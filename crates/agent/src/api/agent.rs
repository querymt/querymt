//! Single agent implementation

#[cfg(feature = "remote")]
use super::mesh::{AgentMesh, Mesh, MeshSpec};
use super::profiles::{AgentProfiles, ProfileRuntimeHandle};
use super::sessions::{AgentSessions, ListSessionsOptions, SessionListPage};
use super::utils::{default_registry, latest_assistant_message, to_absolute_path};
use super::worktrees::{AgentWorktrees, SessionOptions};
use crate::acp::AcpTransport;
use crate::acp::protocol::{ContentBlock, NewSessionRequest, PromptRequest, TextContent};
use crate::acp::stdio::serve_stdio;
use crate::acp::websocket::serve_websocket;
use crate::agent::LocalAgentHandle as AgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::SnapshotPolicy;
use crate::agent::session_mcp::SessionMcpAttachmentSource;
use crate::config::{
    ExecutionPolicy, HooksConfig, McpServerConfig, MiddlewareEntry, SingleAgentConfig,
    SkillsConfig, SlashCommandsConfig, ToolSpec,
};
use crate::event_fanout::EventFanout;
use crate::middleware::Middleware;
use crate::runner::{ChatRunner, ChatSession};
use crate::send_agent::SendAgent;
#[cfg(feature = "api")]
use crate::server::AgentServer;
use crate::session::backend::{StorageBackend, resolve_agent_db_path};
use crate::session::projection::ViewStore;
use crate::session::sqlite_storage::SqliteStorage;
use crate::session::store::SessionStore;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use querymt::LLMParams;
use querymt_agent_runtime::api::AgentSession;
use querymt_agent_runtime::api::callbacks::EventCallbacksState;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Non-serializable infrastructure for agent construction.
///
/// When not provided to a builder, `build()` uses platform defaults:
/// - `plugin_registry`: loaded from `~/.querymt/providers.toml` with Extism + Native loaders
/// - `storage`: SQLite at the default agent db path
///
/// # Example
///
/// ```rust,no_run
/// use querymt_agent::prelude::*;
/// use std::sync::Arc;
///
/// # async fn example(registry: querymt::plugin::host::PluginRegistry, storage: Arc<dyn querymt_agent::session::backend::StorageBackend>) {
/// let agent = Agent::builder()
///     .provider("anthropic", "claude-sonnet-4-20250514")
///     .infra(AgentInfra {
///         plugin_registry: Arc::new(registry),
///         storage: Some(storage),
///         session_mcp_attachment_source: None,
///         event_fanout: None,
///     })
///     .build()
///     .await
///     .unwrap();
/// # }
/// ```
#[derive(Clone)]
pub struct AgentInfra {
    /// Pre-built plugin registry.
    /// Required for iOS/embedded where default loaders are unavailable.
    pub plugin_registry: Arc<querymt::plugin::host::PluginRegistry>,
    /// Pre-opened storage backend.
    /// `None` = create SQLite from the builder/env/default db path.
    pub storage: Option<Arc<dyn StorageBackend>>,
    /// Optional runtime MCP attachment source (e.g., for mobile in-process MCP peers).
    pub session_mcp_attachment_source: Option<Arc<dyn SessionMcpAttachmentSource>>,
    /// Shared live event bus for runtimes that should stream through one UI/ACP connection.
    pub event_fanout: Option<Arc<EventFanout>>,
}

impl AgentInfra {
    /// Build the default shared infrastructure used by profile runtimes.
    pub async fn default_shared() -> Result<Self> {
        Self::shared_with_db_path(None).await
    }

    /// Build shared infrastructure with an optional explicit sessions DB path.
    pub async fn shared_with_db_path(db_path: Option<PathBuf>) -> Result<Self> {
        let registry = Arc::new(default_registry().await?);
        let storage = Arc::new(SqliteStorage::connect(resolve_agent_db_path(db_path)?).await?);
        Ok(Self {
            plugin_registry: registry,
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: Some(Arc::new(EventFanout::new())),
        })
    }
}

/// Type alias for middleware factory closures
type MiddlewareFactory = Box<dyn FnOnce(&AgentHandle) -> Arc<dyn Middleware> + Send>;

pub struct AgentBuilder {
    pub(super) llm_config: Option<LLMParams>,
    pub(super) tools: Vec<ToolSpec>,
    tool_error: Option<crate::config::ToolSpecError>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) snapshot_policy: SnapshotPolicy,
    pub(super) db_path: Option<PathBuf>,
    mode: crate::agent::core::AgentMode,
    permissions: crate::config::Permissions,
    middleware_factories: Vec<MiddlewareFactory>,
    middleware_entries: Vec<MiddlewareEntry>,
    execution: Option<ExecutionPolicy>,
    skills_config: Option<SkillsConfig>,
    slash_commands_config: Option<SlashCommandsConfig>,
    hooks_config: Option<HooksConfig>,
    /// Protocol load options; disabled when omitted.
    dotagents_options: Option<crate::dotagents::DotagentsLoadOptions>,
    /// Optional pre-resolved protocol manifest.
    dotagents_manifest: Option<crate::dotagents::DotagentsManifest>,
    /// Optional host approval mechanism for workspace protocol tasks.
    dotagents_task_approver: Option<Arc<dyn crate::dotagents::DotagentsTaskApprover>>,
    /// Workspace settings from the builder or TOML; service built at `build()`.
    workspace: Option<crate::config::WorkspaceSettings>,
    /// Prebuilt workspace service; wins over `workspace`.
    workspace_service: Option<Arc<crate::worktree::WorkspaceService>>,
    /// Portable repository knowledge settings; disabled when omitted.
    repository_knowledge: Option<crate::config::RepositoryKnowledgeSettings>,
    /// Prebuilt repository knowledge registry; wins over `repository_knowledge`
    /// (hosts and tests that place the index outside the shared config dir).
    repository_knowledge_registry:
        Option<Arc<crate::knowledge::repository::RepositoryKnowledgeRegistry>>,
    /// MCP servers from TOML `[[mcp]]` config, attached to every new session.
    pub(super) mcp_servers: Vec<McpServerConfig>,
    /// Runtime MCP attachment source (e.g., mobile in-process MCP peers).
    session_mcp_attachment_source: Option<Arc<dyn SessionMcpAttachmentSource>>,
    /// Members that already exist (for example tests' handles); listed before
    /// the configured delegates.
    pub(super) preregistered: Vec<crate::delegation::roster::Member>,
    /// The first problem found while configuring delegates; `build` reports it.
    delegate_error: Option<anyhow::Error>,
    /// Members the root can delegate to, built when the agent is.
    pub(super) delegates: Vec<crate::delegation::member::MemberBuildSpec>,
    /// Whether delegation is on (default). It also needs the root's `tools`
    /// to admit `delegate`.
    delegation_enabled: bool,
    pub(super) delegation_settings: crate::config::DelegationSettings,
    /// Set for profile runtimes so each delegation service claims only its own sessions.
    profile_id: Option<String>,
    /// The root's agent ID (`agent` unless set).
    root_id: Option<String>,
    /// Whether the root gets a git snapshot backend whenever the snapshot
    /// policy is not `none`, as `[quorum]` configs always did.
    git_snapshots: bool,
    /// Whether a `skill` tool without configured skills is an error for the root.
    require_skills_for_skill_tool: bool,
    /// Optional pre-built infrastructure (plugin registry + storage).
    infra: Option<AgentInfra>,
    #[cfg(feature = "remote")]
    mesh: Option<Mesh>,
    #[cfg(feature = "remote")]
    mesh_auto_fallback: bool,
    /// Delegates whose LLM calls run on a mesh peer.
    #[cfg(feature = "remote")]
    pub(super) peer_delegates: Vec<crate::delegation::PeerRoute>,
    /// Override: maximum execution steps (forwarded to AgentConfigBuilder).
    max_steps_override: Option<usize>,
    /// Override: maximum prompt bytes (forwarded to AgentConfigBuilder).
    max_prompt_bytes_override: Option<usize>,
    /// Override: execution timeout in seconds (forwarded to AgentConfigBuilder).
    execution_timeout_secs_override: Option<u64>,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            llm_config: None,
            tools: Vec::new(),
            tool_error: None,
            cwd: None,
            snapshot_policy: SnapshotPolicy::Diff,
            db_path: None,
            mode: crate::agent::core::AgentMode::default(),
            permissions: crate::config::Permissions::default(),
            middleware_factories: Vec::new(),
            middleware_entries: Vec::new(),
            execution: None,
            skills_config: None,
            slash_commands_config: None,
            hooks_config: None,
            dotagents_options: None,
            dotagents_manifest: None,
            dotagents_task_approver: None,
            workspace: None,
            workspace_service: None,
            repository_knowledge: None,
            repository_knowledge_registry: None,
            mcp_servers: Vec::new(),
            session_mcp_attachment_source: None,
            preregistered: Vec::new(),
            delegates: Vec::new(),
            delegation_enabled: true,
            delegation_settings: crate::config::DelegationSettings::default(),
            profile_id: None,
            root_id: None,
            git_snapshots: false,
            require_skills_for_skill_tool: false,
            delegate_error: None,
            infra: None,
            #[cfg(feature = "remote")]
            mesh: None,
            #[cfg(feature = "remote")]
            mesh_auto_fallback: false,
            #[cfg(feature = "remote")]
            peer_delegates: Vec::new(),
            max_steps_override: None,
            max_prompt_bytes_override: None,
            execution_timeout_secs_override: None,
        }
    }

    // Helper that lazily initializes LLMParams
    fn with_llm<F>(mut self, f: F) -> Self
    where
        F: FnOnce(LLMParams) -> LLMParams,
    {
        let cfg = self.llm_config.take().unwrap_or_default();
        self.llm_config = Some(f(cfg));
        self
    }

    pub fn provider(self, name: impl Into<String>, model: impl Into<String>) -> Self {
        self.with_llm(|c| c.provider(name).model(model))
    }

    pub fn api_key(self, key: impl Into<String>) -> Self {
        self.with_llm(|c| c.api_key(key))
    }

    pub fn system(self, prompt: impl Into<querymt::params::SystemPrompt>) -> Self {
        self.with_llm(|c| c.system(prompt))
    }

    pub fn parameter<K: Into<String>>(self, key: K, value: impl Into<serde_json::Value>) -> Self {
        self.with_llm(|c| c.parameter(key, value.into()))
    }

    /// Add a member the root agent can delegate to.
    pub fn delegate<F>(mut self, id: impl Into<String>, configure: F) -> Self
    where
        F: FnOnce(super::config::DelegateConfigBuilder) -> super::config::DelegateConfigBuilder,
    {
        let id = id.into();
        let spec = crate::delegation::roster::AgentId::try_from(id.as_str())
            .map_err(|error| anyhow!("delegate '{id}': {error}"))
            .and_then(|agent_id| {
                configure(super::config::DelegateConfigBuilder::new(agent_id)).build()
            });
        match spec {
            Ok(spec) => self.delegates.push(spec),
            Err(error) => {
                self.delegate_error.get_or_insert(error);
            }
        }
        self
    }

    /// Alias of [`delegate`](Self::delegate): add a subagent the root agent
    /// can hand work to.
    pub fn subagent<F>(self, id: impl Into<String>, configure: F) -> Self
    where
        F: FnOnce(super::config::DelegateConfigBuilder) -> super::config::DelegateConfigBuilder,
    {
        self.delegate(id, configure)
    }

    /// Turn delegation on or off (on by default). It also requires the root's
    /// `tools` to admit `delegate`.
    pub fn delegation(mut self, enabled: bool) -> Self {
        self.delegation_enabled = enabled;
        self
    }

    /// How the delegation service of this runtime behaves.
    pub fn delegation_settings(mut self, settings: crate::config::DelegationSettings) -> Self {
        self.delegation_settings = settings;
        self
    }

    /// Mark this runtime as the one of profile `profile_id`.
    pub fn with_profile_id(mut self, profile_id: impl Into<String>) -> Self {
        self.profile_id = Some(profile_id.into());
        self
    }

    /// Lowers configured delegates into members, noting the ones whose LLM
    /// calls run on a mesh peer. The one lowering for `[agent]` and `[quorum]`.
    fn lower_delegates(
        &mut self,
        delegates: Vec<crate::config::DelegateConfig>,
        top_level_mcp: &[McpServerConfig],
    ) -> Result<()> {
        use crate::delegation::member::{MemberBuildSpec, available_local_tool_names};

        let local_tools = available_local_tool_names();
        for delegate in delegates {
            #[allow(
                clippy::redundant_clone,
                reason = "`delegate.peer` is read below when the `remote` feature is on"
            )]
            let spec = MemberBuildSpec::from_delegate_config(
                delegate.clone(),
                top_level_mcp,
                &local_tools,
            )?;
            #[cfg(feature = "remote")]
            if let Some(peer) = delegate.peer {
                self.peer_delegates.push(crate::delegation::PeerRoute {
                    delegate: spec.id.clone(),
                    peer,
                });
            }
            self.delegates.push(spec);
        }
        Ok(())
    }

    pub fn db(mut self, path: impl Into<PathBuf>) -> Self {
        self.db_path = Some(path.into());
        self
    }

    /// Tool selectors (`name`, `server.tool`, `server.*`). A malformed
    /// selector is reported by `build()`.
    pub fn tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        (self.tools, self.tool_error) = crate::delegation::member::parse_tool_specs(tools);
        self
    }

    /// Tool selectors that are already typed.
    pub fn tool_specs(mut self, tools: impl IntoIterator<Item = ToolSpec>) -> Self {
        self.tools = tools.into_iter().collect();
        self.tool_error = None;
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.snapshot_policy = policy;
        self
    }

    /// Configure rate limit retry behavior
    pub fn rate_limit_config(mut self, config: crate::config::RateLimitConfig) -> Self {
        self.execution
            .get_or_insert_with(ExecutionPolicy::default)
            .rate_limit = config;
        self
    }

    /// Inject custom infrastructure (plugin registry, storage).
    ///
    /// Required for environments without default plugin loaders (iOS, embedded).
    /// When not called, `build()` uses platform defaults (Extism + Native loaders,
    /// SQLite at the default db path).
    pub fn infra(mut self, infra: AgentInfra) -> Self {
        self.infra = Some(infra);
        self
    }

    #[cfg(feature = "remote")]
    pub fn mesh(mut self, mesh: Mesh) -> Self {
        self.mesh = if mesh.is_disabled() { None } else { Some(mesh) };
        self
    }

    /// Set the maximum number of execution steps.
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps_override = Some(n);
        self
    }

    /// Set the maximum prompt size in bytes.
    pub fn max_prompt_bytes(mut self, n: usize) -> Self {
        self.max_prompt_bytes_override = Some(n);
        self
    }

    /// Set the execution timeout in seconds.
    pub fn execution_timeout_secs(mut self, secs: u64) -> Self {
        self.execution_timeout_secs_override = Some(secs);
        self
    }

    /// Set the full execution policy.
    pub fn execution_policy(mut self, policy: ExecutionPolicy) -> Self {
        self.execution = Some(policy);
        self
    }

    /// Configure skills.
    pub fn skills(mut self, config: SkillsConfig) -> Self {
        self.skills_config = Some(config);
        self
    }

    /// Configure hooks.
    pub fn hooks(mut self, config: HooksConfig) -> Self {
        self.hooks_config = Some(config);
        self
    }

    /// Configure slash-command discovery.
    pub fn slash_commands(mut self, config: SlashCommandsConfig) -> Self {
        self.slash_commands_config = Some(config);
        self
    }

    /// Configure managed Git worktree workspaces.
    ///
    /// Enabling the service only makes isolation *available*; a session is
    /// isolated only when it asks for it. An injected
    /// [`workspace_service`](Self::workspace_service) takes precedence.
    pub fn workspace(mut self, settings: crate::config::WorkspaceSettings) -> Self {
        self.workspace = Some(settings);
        self
    }

    /// Inject a prebuilt workspace service (wins over [`workspace`](Self::workspace)).
    pub fn workspace_service(mut self, service: Arc<crate::worktree::WorkspaceService>) -> Self {
        self.workspace_service = Some(service);
        self
    }

    /// Configure portable repository knowledge (disabled unless `enabled`).
    ///
    /// Only host configuration can enable the feature. Setting this performs
    /// no repository reads or writes; discovery happens lazily per session.
    pub fn repository_knowledge(
        mut self,
        settings: crate::config::RepositoryKnowledgeSettings,
    ) -> Self {
        self.repository_knowledge = Some(settings);
        self
    }

    /// Use a prebuilt repository knowledge registry instead of building one
    /// from settings, for example one whose index lives in a chosen directory.
    #[cfg(test)]
    pub(crate) fn repository_knowledge_registry(
        mut self,
        registry: Arc<crate::knowledge::repository::RepositoryKnowledgeRegistry>,
    ) -> Self {
        self.repository_knowledge_registry = Some(registry);
        self
    }

    /// Inspect the configured repository knowledge settings, if any.
    pub fn configured_repository_knowledge(
        &self,
    ) -> Option<&crate::config::RepositoryKnowledgeSettings> {
        self.repository_knowledge.as_ref()
    }

    /// Enable `.agents` loading with default discovery options.
    pub fn enable_dotagents(mut self) -> Self {
        self.dotagents_options = Some(crate::dotagents::DotagentsLoadOptions::enabled());
        self
    }

    /// Configure `.agents` loading explicitly.
    /// Supply the workspace used to derive `<workspace>/.agents/`. An explicit
    /// `workspace` or `workspace_root` in the load options always wins.
    pub fn with_dotagents_workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        let options = self
            .dotagents_options
            .take()
            .unwrap_or_else(crate::dotagents::DotagentsLoadOptions::disabled);
        self.dotagents_options = Some(options.with_workspace_fallback(workspace));
        self
    }

    pub fn dotagents_options(mut self, options: crate::dotagents::DotagentsLoadOptions) -> Self {
        self.dotagents_options = Some(options);
        self
    }

    /// Supply a pre-resolved `.agents` manifest.
    pub fn dotagents_manifest(mut self, manifest: crate::dotagents::DotagentsManifest) -> Self {
        self.dotagents_manifest = Some(manifest);
        self
    }

    /// Supply the host approval mechanism for workspace protocol tasks.
    ///
    /// Protocol workspace tasks are treated as untrusted repository content. When
    /// the trust policy is `prompt` and no approver is configured, such tasks
    /// stay pending and inactive, and activation reports them instead of running
    /// them. Configuring an approver lets a CLI, UI, ACP, or embedding host
    /// present the decision.
    pub fn dotagents_task_approver(
        mut self,
        approver: Arc<dyn crate::dotagents::DotagentsTaskApprover>,
    ) -> Self {
        self.dotagents_task_approver = Some(approver);
        self
    }

    /// Inspect configured protocol load options without triggering discovery.
    pub fn configured_dotagents_options(&self) -> Option<&crate::dotagents::DotagentsLoadOptions> {
        self.dotagents_options.as_ref()
    }

    /// Inspect a supplied pre-resolved protocol manifest.
    pub fn configured_dotagents_manifest(&self) -> Option<&crate::dotagents::DotagentsManifest> {
        self.dotagents_manifest.as_ref()
    }

    /// Resolve the effective `.agents` Protocol manifest for inspection.
    ///
    /// Applies the same resolution precedence as [`Self::build`] (a supplied
    /// manifest, then explicit or config-derived load options, then the
    /// builder workspace) but performs no runtime work: no MCP servers are
    /// started, no tasks are scheduled or reconciled, no memories are
    /// imported, and no `.agents` directory is created. Returns `Ok(None)`
    /// when protocol loading is disabled or no protocol layer applies.
    ///
    /// Resolved diagnostics are attached to the returned manifest; in strict
    /// mode the error carries the fully populated manifest for inspection.
    pub fn preview_dotagents_manifest(
        &self,
    ) -> Result<Option<crate::dotagents::DotagentsManifest>, anyhow::Error> {
        let cwd = self.cwd.clone().map(to_absolute_path).transpose()?;
        crate::dotagents::resolve_for_builder(
            self.dotagents_manifest.clone(),
            with_skill_scan(
                self.dotagents_options.clone(),
                self.skills_config.as_ref(),
                &self.tools,
            ),
            cwd.as_deref(),
        )
        .map_err(|error| anyhow::Error::new(*error))
    }

    /// Initial mode of new sessions (default: build).
    pub fn mode(mut self, mode: crate::agent::core::AgentMode) -> Self {
        self.mode = mode;
        self
    }

    /// Tool approval policy: which calls prompt the user and for how long.
    pub fn permissions(mut self, permissions: crate::config::Permissions) -> Self {
        self.permissions = permissions;
        self
    }

    /// Set the runtime MCP attachment source (e.g., for mobile in-process MCP peers).
    pub fn with_session_mcp_attachment_source(
        mut self,
        source: Arc<dyn SessionMcpAttachmentSource>,
    ) -> Self {
        self.session_mcp_attachment_source = Some(source);
        self
    }

    /// Add a middleware to the agent using a factory closure.
    ///
    /// The closure receives a reference to the constructed `AgentHandle`,
    /// allowing access to internal state.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use querymt_agent::api::Agent;
    /// use querymt_agent::middleware::DedupCheckMiddleware;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let agent = Agent::builder()
    ///     .provider("openai", "gpt-4")
    ///     .cwd(".")
    ///     .middleware(|_agent| {
    ///         DedupCheckMiddleware::new()
    ///             .threshold(0.8)
    ///             .min_lines(5)
    ///     })
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn middleware<F, M>(mut self, factory: F) -> Self
    where
        F: FnOnce(&AgentHandle) -> M + Send + 'static,
        M: Middleware + 'static,
    {
        self.middleware_factories
            .push(Box::new(move |agent| Arc::new(factory(agent))));
        self
    }

    /// Add middleware from config entries (used by `from_single_config`).
    ///
    /// This is typically called internally when loading from TOML config files.
    pub fn middleware_from_config(mut self, entries: Vec<MiddlewareEntry>) -> Self {
        self.middleware_entries = entries;
        self
    }

    pub async fn build(mut self) -> Result<Agent> {
        if let Some(error) = self.tool_error.take() {
            return Err(anyhow!("invalid `tools` selector: {error}"));
        }
        if let Some(error) = self.delegate_error.take() {
            return Err(error);
        }
        if self.require_skills_for_skill_tool {
            check_skill_tool(&self.tools, self.skills_config.as_ref(), "planner")?;
        }
        for member in &self.delegates {
            check_skill_tool(
                &member.tools,
                member.skills.as_ref(),
                &format!("delegate '{}'", member.id),
            )?;
        }
        let snapshot_policy = self.snapshot_policy;
        let cwd = if let Some(path) = self.cwd {
            Some(to_absolute_path(path)?)
        } else {
            None
        };

        if !self.delegates.is_empty() {
            let mut required = crate::delegation::member::infer_required_capabilities(&self.tools);
            for member in &self.delegates {
                required.extend(member.required_capabilities.iter().copied());
            }
            if required.contains(&crate::tools::CapabilityRequirement::Filesystem) && cwd.is_none()
            {
                return Err(anyhow!(
                    "Working directory required: one or more agents require filesystem access. Use .cwd() to set one."
                ));
            }
        }

        let mut llm_config = self
            .llm_config
            .ok_or_else(|| anyhow!("LLM configuration is required (call .provider() first)"))?;
        let dotagents_options = with_skill_scan(
            self.dotagents_options.take(),
            self.skills_config.as_ref(),
            &self.tools,
        );
        let dotagents_strictness = dotagents_options
            .as_ref()
            .map(|options| options.strictness())
            .unwrap_or_default();
        let dotagents_manifest = crate::dotagents::resolve_for_builder(
            self.dotagents_manifest.take(),
            dotagents_options.clone(),
            cwd.as_deref(),
        )
        .map_err(|error| anyhow!(error.to_string()))?;
        if let Some(manifest) = &dotagents_manifest {
            // Protocol prompts are additive to explicit system parts, in the
            // order: explicit, `system-prompt.md`, `agents.md`.
            crate::dotagents::compose_prompt(&mut llm_config, manifest);
            for member in &mut self.delegates {
                if let Some(params) = member.llm_config.as_mut() {
                    crate::dotagents::compose_prompt(params, manifest);
                }
            }
            // The selected preset is recorded here and applied after the plugin
            // registry exists, so an unavailable provider is reported instead of
            // leaving a half-applied overlay behind.
            if let Some(preset_name) = dotagents_options
                .as_ref()
                .and_then(|options| options.selected_model_preset())
                && crate::dotagents::select_model_overlay(manifest, preset_name).is_err()
            {
                // An unresolvable preset must not silently discard the explicit
                // base configuration; the diagnostic is re-reported below.
                crate::dotagents::select_model_overlay(manifest, preset_name)
                    .err()
                    .inspect(|diagnostic| log::warn!("dotagents: {diagnostic}"));
            }
        }
        let (plugin_registry, backend, event_fanout): (
            Arc<querymt::plugin::host::PluginRegistry>,
            Arc<dyn StorageBackend>,
            Option<Arc<EventFanout>>,
        ) = match self.infra {
            Some(infra) => {
                let storage = match infra.storage {
                    Some(s) => s,
                    None => {
                        let db_path = resolve_agent_db_path(self.db_path)?;
                        Arc::new(SqliteStorage::connect(db_path).await?)
                    }
                };
                if self.session_mcp_attachment_source.is_none() {
                    self.session_mcp_attachment_source =
                        infra.session_mcp_attachment_source.clone();
                }
                (infra.plugin_registry, storage, infra.event_fanout)
            }
            None => {
                let registry = Arc::new(default_registry().await?);
                let db_path = resolve_agent_db_path(self.db_path)?;
                let storage = Arc::new(SqliteStorage::connect(db_path).await?);
                (registry, storage, None)
            }
        };

        // The plugin registry now exists, so the selected protocol preset can be
        // applied atomically: provider availability is checked first, and every
        // parameter is validated before any field is written. A rejected preset
        // leaves the explicit base configuration completely intact.
        let mut passive_diagnostics: Vec<crate::dotagents::DotagentsDiagnostic> = Vec::new();
        if let (Some(manifest), Some(preset_name)) = (
            dotagents_manifest.as_ref(),
            dotagents_options
                .as_ref()
                .and_then(|options| options.selected_model_preset()),
        ) && let Err(diagnostic) = crate::dotagents::apply_selected_model_preset(
            &plugin_registry,
            manifest,
            preset_name,
            &mut llm_config,
        )
        .await
        {
            match crate::dotagents::classify_activation_unavailable(
                dotagents_strictness,
                crate::dotagents::DotagentsActivationFacility::TargetProfile,
                diagnostic.to_string(),
            ) {
                crate::dotagents::DotagentsActivationDisposition::Fatal(diagnostic) => {
                    return Err(anyhow!(diagnostic.to_string()));
                }
                crate::dotagents::DotagentsActivationDisposition::Diagnostic(diagnostic) => {
                    log::warn!("dotagents: {diagnostic}");
                    passive_diagnostics.push(diagnostic);
                }
            }
        }

        let mut builder =
            AgentConfigBuilder::new(plugin_registry.clone(), backend.clone(), llm_config)
                .with_agent_id(self.root_id.as_deref().unwrap_or("agent"))
                .with_snapshot_policy(snapshot_policy);
        if self.git_snapshots && snapshot_policy != SnapshotPolicy::None {
            builder =
                builder.with_snapshot_backend(Arc::new(crate::snapshot::GitSnapshotBackend::new()));
        }
        if let Some(event_fanout) = event_fanout {
            builder = builder.with_event_fanout(event_fanout);
        }

        // Remote agents from config are registered ahead of the delegates.
        #[allow(unused_mut)]
        let mut preregistered = std::mem::take(&mut self.preregistered);
        #[cfg(feature = "remote")]
        let mesh_handle = match &self.mesh {
            Some(mesh) => Some(mesh.start().await?.handle().as_mesh_handle().clone()),
            None => None,
        };
        // Remote agents from config are listed without contacting their peers.
        #[cfg(feature = "remote")]
        if let Some(mesh) = &self.mesh
            && !mesh.remote_agents().is_empty()
            && let MeshSpec::Toml(cfg) = mesh.spec_for_internal_use()
        {
            let members =
                crate::agent::remote::remote_members_from_config(mesh.remote_agents(), &cfg.peers)?;
            preregistered.extend(members);
        }
        let protocol_plans = dotagents_manifest
            .as_ref()
            .map(crate::dotagents::DotagentsSubAgentPlans::from_manifest);
        let skills_for_members = self.skills_config.clone();

        // Workspace service: an injected instance wins; otherwise build one from
        // the settings. Invalid settings fail here, not at the first session.
        let workspace_service = match self.workspace_service.take() {
            Some(service) => Some(service),
            None => match &self.workspace {
                Some(settings) => {
                    crate::worktree::WorkspaceService::from_settings(settings, backend.as_ref())
                        .map_err(|err| anyhow!("invalid workspace configuration: {err}"))?
                }
                None => None,
            },
        };
        if let Some(service) = workspace_service.clone() {
            builder = builder.with_workspace_service(service);
        }

        // Repository knowledge: a lazy registry only when the host enabled it.
        // Nothing is resolved, created or read until a session uses it.
        let repository_knowledge = self.repository_knowledge_registry.clone().or_else(|| {
            self.repository_knowledge
                .as_ref()
                .and_then(crate::knowledge::host::registry_from_settings)
        });
        builder = builder.with_repository_knowledge(repository_knowledge.clone());

        // Wire schedule repository and knowledge store from the storage backend.
        // These are optional — backends that don't support them return None.
        if let Some(repo) = backend.schedule_repository() {
            builder = builder.with_schedule_repository(repo);
        }
        if let Some(ks) = backend.knowledge_store() {
            builder = builder.with_knowledge_store(ks);
        }

        builder = builder.with_permissions(self.permissions);

        // Apply execution overrides from builder methods
        if let Some(max_steps) = self.max_steps_override {
            builder = builder.with_max_steps(max_steps);
        }
        if let Some(max_prompt_bytes) = self.max_prompt_bytes_override {
            builder = builder.with_max_prompt_bytes(max_prompt_bytes);
        }
        if let Some(execution_timeout_secs) = self.execution_timeout_secs_override {
            builder = builder.with_execution_timeout_secs(execution_timeout_secs);
        }

        let skills_project_root = cwd.as_deref().unwrap_or_else(|| std::path::Path::new("."));
        if let Some(catalog) = crate::skills::SkillCatalog::for_agent(
            self.skills_config.as_ref(),
            &self.tools,
            skills_project_root,
        ) {
            builder = builder.with_skill_catalog(catalog);
        }

        if !self.tools.is_empty() {
            // Restricted tool lists infer the policy from the selectors and
            // attached servers (see `infer_restricted_tool_policy`). This
            // covers config-based MCP servers and preconnected runtime MCP
            // peers (e.g. mobile in-process MCP).
            let policy =
                crate::agent::tools::infer_restricted_tool_policy(&self.tools, &self.mcp_servers);
            builder = builder
                .with_tool_policy(policy)
                .with_allowed_tools(self.tools.clone());
        }

        if !self.mcp_servers.is_empty() {
            builder = builder.with_mcp_servers(self.mcp_servers.clone());
        }

        // Protocol MCP servers convert through the existing stdio and
        // streamable-http transports. They are appended to any explicitly
        // configured servers rather than replacing them, so explicit MCP
        // configuration is preserved.
        if let Some(manifest) = &dotagents_manifest {
            let mcp_plan = crate::dotagents::DotagentsMcpPlan::from_manifest(manifest);
            for diagnostic in &mcp_plan.diagnostics {
                log::warn!("dotagents: {diagnostic}");
            }
            passive_diagnostics.extend(mcp_plan.diagnostics.iter().cloned());
            for member in &mut self.delegates {
                passive_diagnostics.extend(merge_dotagents_mcp_servers(member, &mcp_plan.servers));
            }
            if !mcp_plan.servers.is_empty() {
                let mut servers = self.mcp_servers.clone();
                for server in mcp_plan.servers {
                    if !servers
                        .iter()
                        .any(|existing| existing.name() == server.name())
                    {
                        servers.push(server);
                    } else {
                        let message = format!(
                            "protocol MCP server `{}` collides with an explicit configuration; keeping the explicit server",
                            server.name()
                        );
                        log::warn!("dotagents: {message}");
                        passive_diagnostics.push(crate::dotagents::DotagentsDiagnostic::warning(
                            crate::dotagents::DotagentsDiagnosticCode::Collision,
                            message,
                        ));
                    }
                }
                builder = builder.with_mcp_servers(servers);
            }
        }

        if let Some(source) = self.session_mcp_attachment_source {
            builder = builder.with_session_mcp_attachment_source(source);
        }

        if let Some(ref exec) = self.execution {
            builder = builder.with_snapshot_from_execution(exec);
        }

        builder = builder.with_agent_mode(self.mode);

        if let Some(hooks_config) = self.hooks_config.take() {
            let hooks = crate::hooks::Hooks::new(hooks_config)?;
            builder = builder.with_hooks(hooks);
        }

        if let Some(slash_config) = self.slash_commands_config
            && slash_config.enabled
        {
            let sources = crate::slash_commands::search_paths(
                cwd.as_deref(),
                slash_config.include_global,
                slash_config.include_project,
                &slash_config.paths,
            );
            let (registry, diagnostics) = crate::slash_commands::SlashCommandRegistry::from_sources(
                &sources,
                &slash_config.scripts,
            );
            for diagnostic in &diagnostics {
                log::warn!(
                    "Skipping invalid slash command {}: {}",
                    diagnostic.path.display(),
                    diagnostic.message
                );
            }
            if registry.is_empty() {
                log::info!("No slash commands discovered");
            } else {
                log::info!("Discovered slash commands: {}", registry.names().join(", "));
            }
            builder = builder.with_slash_command_registry(registry);
            // Retain the discovery config so workspace-bound sessions can
            // re-resolve prompt sources from their bound checkout.
            builder = builder.with_slash_commands_config(Some(slash_config));
        }

        // Members, the delegate tool, middleware and the delegation service are
        // all assembled by the runtime's one root builder.
        let members = std::mem::take(&mut self.delegates);
        let handle = crate::delegation::build_root(
            builder,
            crate::delegation::RootParts {
                enabled: self.delegation_enabled,
                settings: self.delegation_settings.clone(),
                profile_id: self.profile_id.clone(),
                cwd: cwd.clone(),
                members,
                member_params: crate::delegation::member::DelegateBuildParams {
                    plugin_registry: plugin_registry.clone(),
                    snapshot_policy,
                    skills_project_root: cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
                    workspace_service,
                    repository_knowledge,
                },
                preregistered,
                protocol_plans,
                skills: skills_for_members,
                middleware_factories: std::mem::take(&mut self.middleware_factories),
                middleware_entries: std::mem::take(&mut self.middleware_entries),
                #[cfg(feature = "remote")]
                peer_delegates: self.peer_delegates.clone(),
                #[cfg(feature = "remote")]
                mesh: mesh_handle.clone(),
            },
        )
        .await?;

        #[cfg(feature = "remote")]
        let mesh_node_name = self.mesh.as_ref().and_then(|mesh| mesh.node_name());

        #[cfg(feature = "remote")]
        if let Some(mesh_handle) = mesh_handle {
            handle.set_mesh_fallback(self.mesh_auto_fallback);
            handle.set_mesh(mesh_handle);
        }

        // Protocol state is retained for post-construction activation. Task and
        // memory reconciliation is intentionally deferred: it needs sessions,
        // profiles, approvals, and a scheduler, some of which do not exist yet.
        let dotagents_state = dotagents_manifest.clone().map(|manifest| {
            AgentDotagentsState::assemble(
                manifest,
                dotagents_options.unwrap_or_else(crate::dotagents::DotagentsLoadOptions::disabled),
                self.dotagents_task_approver.take(),
                passive_diagnostics,
            )
        });

        // Start the scheduler actor if the backend supports scheduling.
        handle.start_scheduler().await;

        let roster = handle
            .config
            .roster()
            .cloned()
            .ok_or_else(|| anyhow!("the root agent was built without a roster"))?;
        let agent = Agent {
            inner: handle,
            roster,
            storage: backend,
            default_session_id: Arc::new(Mutex::new(None)),
            cwd,
            callbacks: Arc::new(EventCallbacksState::new(None)),
            profiles: None,
            dotagents: dotagents_state.map(Arc::new),
        };

        #[cfg(feature = "remote")]
        if self.mesh.is_some() {
            agent.inner.ensure_mesh_published(mesh_node_name).await?;
        }

        Ok(agent)
    }
}

#[derive(Clone)]
pub struct Agent {
    pub(super) inner: Arc<AgentHandle>,
    /// The members the root can delegate to; the roster `build_root` gave the root.
    roster: Arc<crate::delegation::roster::AgentRoster>,
    #[cfg_attr(not(feature = "api"), allow(dead_code))]
    pub(super) storage: Arc<dyn StorageBackend>,
    pub(super) default_session_id: Arc<Mutex<Option<String>>>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) callbacks: Arc<EventCallbacksState>,
    pub(super) profiles: Option<Arc<AgentProfiles>>,
    /// Passive protocol state. Durable task and memory reconciliation is
    /// deliberately *not* performed during `build()`; it runs from
    /// [`Agent::activate_dotagents`] once the runtime topology is complete.
    pub(super) dotagents: Option<Arc<AgentDotagentsState>>,
}

/// Protocol state carried by a built agent for post-construction activation.
pub struct AgentDotagentsState {
    pub(super) manifest: Arc<crate::dotagents::DotagentsManifest>,
    pub(super) options: crate::dotagents::DotagentsLoadOptions,
    pub(super) approver: Option<Arc<dyn crate::dotagents::DotagentsTaskApprover>>,
    /// Diagnostics gathered while applying passive protocol configuration.
    pub(super) passive_diagnostics: Vec<crate::dotagents::DotagentsDiagnostic>,
}

impl AgentDotagentsState {
    /// Assemble the protocol state a runtime carries into activation.
    ///
    /// Shared by the single-agent and quorum builders so both runtimes expose
    /// identical protocol behavior instead of drifting apart.
    pub(super) fn assemble(
        manifest: crate::dotagents::DotagentsManifest,
        options: crate::dotagents::DotagentsLoadOptions,
        approver: Option<Arc<dyn crate::dotagents::DotagentsTaskApprover>>,
        passive_diagnostics: Vec<crate::dotagents::DotagentsDiagnostic>,
    ) -> Self {
        Self {
            manifest: Arc::new(manifest),
            options,
            approver,
            passive_diagnostics,
        }
    }

    /// The resolved protocol manifest.
    pub fn manifest(&self) -> &crate::dotagents::DotagentsManifest {
        &self.manifest
    }

    /// The load options used for resolution.
    pub fn options(&self) -> &crate::dotagents::DotagentsLoadOptions {
        &self.options
    }

    /// Diagnostics emitted while applying passive protocol configuration
    /// (prompts, model presets, MCP plans, skills, delegation targets).
    pub fn passive_diagnostics(&self) -> &[crate::dotagents::DotagentsDiagnostic] {
        &self.passive_diagnostics
    }
}

/// One subagent as listed by [`Agent::subagents`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SubagentInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub origin: crate::delegation::roster::MemberOrigin,
    pub location: crate::delegation::roster::MemberLocation,
}

impl Agent {
    /// Starts building an agent. Every agent can have subagents: add them
    /// here with [`AgentBuilder::subagent`] or later with
    /// [`Agent::add_subagent`].
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    #[deprecated(note = "use `Agent::builder()`; every agent can have subagents")]
    pub fn single() -> AgentBuilder {
        AgentBuilder::new()
    }

    /// Access the underlying `AgentHandle` for advanced configuration.
    ///
    /// The handle provides access to the session registry, event bus, and agent config.
    /// Use this when you need to interact with sessions directly or integrate with
    /// the kameo mesh (e.g., bootstrapping `RemoteNodeManager`).
    pub fn handle(&self) -> Arc<AgentHandle> {
        self.inner.clone()
    }

    #[cfg(feature = "remote")]
    pub fn mesh(&self) -> Option<AgentMesh> {
        self.inner.mesh().map(|mesh| {
            AgentMesh::new(
                crate::agent::remote::MeshRuntimeHandle::from(mesh),
                self.inner.clone(),
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn storage_backend(&self) -> Arc<dyn StorageBackend> {
        self.storage.clone()
    }

    pub async fn chat(&self, prompt: &str) -> Result<String> {
        let session_id = self.ensure_default_session().await?;
        self.chat_with_session(&session_id, prompt).await
    }

    pub async fn chat_session(&self) -> Result<AgentSession> {
        let session_id = self.create_session().await?;
        Ok(AgentSession::new(self.inner.clone(), session_id))
    }

    /// Create a session with explicit [`SessionOptions`].
    ///
    /// `SessionOptions::default()` is identical to [`chat_session`](Self::chat_session):
    /// a local session. Requested isolation fails closed (no workspace service,
    /// unsupported layout, ...) with a typed error carrying a stable
    /// [`code`](crate::worktree::WorktreeOpError::code), and never falls back
    /// to the shared checkout.
    pub async fn chat_session_with(
        &self,
        options: SessionOptions,
    ) -> Result<AgentSession, crate::worktree::WorktreeOpError> {
        use crate::worktree::WorktreeOpError;
        options.validate()?;
        let mut request = match &self.cwd {
            Some(cwd) => NewSessionRequest::new(cwd.clone()),
            None => NewSessionRequest::new(PathBuf::new()),
        };
        if let Some(meta) = options.new_session_meta() {
            request = request.meta(meta);
        }
        let response = self
            .inner
            .new_session(request)
            .await
            .map_err(|err| WorktreeOpError::from_acp(&err))?;
        let session_id = response.session_id.to_string();
        if let Some(name) = options.session_name() {
            self.storage
                .session_store()
                .set_session_name(&session_id, Some(name.to_string()))
                .await?;
        }
        let workspace = super::worktrees::disclosed_workspace(response.meta.as_ref());
        Ok(AgentSession::new(self.inner.clone(), session_id).with_workspace(workspace))
    }

    /// Managed-worktree operations, or `None` when this agent has no
    /// workspace service (see [`AgentBuilder::workspace`]).
    pub fn worktrees(&self) -> Option<AgentWorktrees> {
        let service = self.inner.config.workspace_service()?;
        Some(AgentWorktrees::new(
            crate::worktree::WorktreeOps::new(service),
            self.inner.clone(),
            self.cwd.clone(),
        ))
    }

    pub fn sessions(&self) -> AgentSessions {
        self.api_sessions(
            self.storage
                .view_store()
                .expect("ViewStore is required for Agent::sessions()"),
            self.storage.session_store(),
            self.cwd.clone(),
        )
    }

    pub(crate) fn api_sessions(
        &self,
        view_store: Arc<dyn ViewStore>,
        session_store: Arc<dyn SessionStore>,
        default_cwd: Option<PathBuf>,
    ) -> AgentSessions {
        AgentSessions::new(self.inner.clone(), view_store, session_store, default_cwd)
    }

    pub async fn list_sessions(&self, options: ListSessionsOptions) -> Result<SessionListPage> {
        self.sessions().list(options).await
    }

    pub async fn load_session(&self, session_id: &str) -> Result<AgentSession> {
        self.sessions().load(session_id).await
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        self.sessions().delete(session_id).await
    }

    pub async fn set_provider(&self, provider: &str, model: &str) -> Result<()> {
        let session_id = self.ensure_default_session().await?;
        self.inner
            .set_provider(&session_id, provider, model)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(())
    }

    pub async fn set_llm_config(&self, config: LLMParams) -> Result<()> {
        let session_id = self.ensure_default_session().await?;
        self.inner
            .set_llm_config(&session_id, config)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(())
    }

    pub fn subscribe(
        &self,
    ) -> tokio::sync::broadcast::Receiver<std::sync::Arc<crate::events::EventEnvelope>> {
        self.inner.subscribe_events()
    }

    pub fn on_tool_call<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        self.callbacks.on_tool_call(callback);
        self.callbacks.ensure_listener(self.subscribe());
        self
    }

    pub fn on_tool_complete<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_tool_complete(callback);
        self.callbacks.ensure_listener(self.subscribe());
        self
    }

    pub fn on_message<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_message(callback);
        self.callbacks.ensure_listener(self.subscribe());
        self
    }

    pub fn on_delegation<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_delegation(callback);
        self.callbacks.ensure_listener(self.subscribe());
        self
    }

    /// Subscribes to subagents being added or removed, from any path: the
    /// Rust API, `querymt/agents/add|remove`, or `add_member`. Only changes
    /// made after this call are delivered.
    pub fn subscribe_subagent_changes(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::delegation::roster::RosterChange> {
        self.roster.subscribe_changes()
    }

    /// Calls `callback` for every subagent added or removed from now on, in
    /// order. Call it more than once to register several callbacks. A
    /// callback that falls far behind skips changes (and logs a warning);
    /// it never delays the change itself. Must be called inside a Tokio
    /// runtime.
    pub fn on_subagents_changed<F>(&self, callback: F) -> &Self
    where
        F: Fn(&crate::delegation::roster::RosterChange) + Send + Sync + 'static,
    {
        use tokio::sync::broadcast::error::RecvError;
        let mut changes = self.roster.subscribe_changes();
        tokio::spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(change) => callback(&change),
                    Err(RecvError::Lagged(skipped)) => {
                        log::warn!("on_subagents_changed skipped {skipped} change(s)")
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        self
    }

    pub fn on_error<F>(&self, callback: F) -> &Self
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        self.callbacks.on_error(callback);
        self.callbacks.ensure_listener(self.subscribe());
        self
    }

    pub fn with_profiles(mut self, profiles: AgentProfiles) -> Self {
        let manager = profiles.manager();
        #[cfg(feature = "remote")]
        if let Some(mesh) = self.inner.mesh() {
            manager.set_mesh_handle(mesh);
        }
        self.inner.set_profiles(manager);
        self.profiles = Some(Arc::new(profiles));
        self
    }

    pub fn profiles(&self) -> Option<ProfileRuntimeHandle> {
        self.profiles
            .as_ref()
            .map(|profiles| profiles.manager())
            .or_else(|| self.inner.profiles())
    }

    /// The resolved `.agents` protocol state, when protocol support is enabled.
    pub fn dotagents(&self) -> Option<&Arc<AgentDotagentsState>> {
        self.dotagents.as_ref()
    }

    /// Whether `.agents` protocol support is enabled for this agent.
    pub fn dotagents_enabled(&self) -> bool {
        self.dotagents
            .as_ref()
            .is_some_and(|state| state.options.is_enabled())
    }

    /// Require a host trust decision for protocol workspace tasks at runtime.
    ///
    /// This overrides any approver supplied at build time and is the hook a host
    /// uses when its approval UI is only available after construction.
    pub fn set_dotagents_task_approver(
        &mut self,
        approver: Arc<dyn crate::dotagents::DotagentsTaskApprover>,
    ) {
        if let Some(state) = self.dotagents.take() {
            let mut state = Arc::try_unwrap(state).unwrap_or_else(|arc| {
                // Another clone holds the state; copy the fields we can.
                AgentDotagentsState {
                    manifest: arc.manifest.clone(),
                    options: arc.options.clone(),
                    approver: None,
                    passive_diagnostics: arc.passive_diagnostics.clone(),
                }
            });
            state.approver = Some(approver);
            self.dotagents = Some(Arc::new(state));
        }
    }

    /// Reconcile durable protocol state: memories and repeat tasks.
    ///
    /// This is the side-effectful half of protocol support. It is deliberately
    /// separate from [`Agent::build`] because reconciliation needs storage, a
    /// scheduler, the host's trust decision, and — for profile-scoped tasks — the
    /// profile topology, none of which are guaranteed to exist during
    /// construction.
    ///
    /// Activation is repeatable and idempotent: calling it again after a host
    /// approves a pending workspace task reconciles the newly trusted task
    /// without duplicating schedules, and it reuses the same protocol-owned
    /// automation session across restarts.
    ///
    /// Returns `None` when protocol support is disabled or absent.
    pub async fn activate_dotagents(
        &self,
    ) -> Result<Option<crate::dotagents::DotagentsActivationReport>> {
        let Some(state) = self.dotagents.clone() else {
            return Ok(None);
        };
        if !state.options.is_enabled() {
            return Ok(None);
        }

        let Some(schedules) = self.storage.schedule_repository() else {
            // Without schedule storage there is nowhere durable to reconcile
            // protocol tasks; memories still import independently.
            return Ok(Some(
                self.dotagents_unavailable_report(
                    &state,
                    "protocol task reconciliation was skipped because the storage backend has no schedule repository",
                )
                .await,
            ));
        };

        let Some(task_state) = self.storage.dotagents_task_state_repository() else {
            return Ok(Some(
                self.dotagents_unavailable_report(
                    &state,
                    "protocol task reconciliation was skipped because the storage backend has no task state repository",
                )
                .await,
            ));
        };

        let Some(conn) = self.storage.dotagents_automation_repository() else {
            return Ok(Some(
                self.dotagents_unavailable_report(
                    &state,
                    "protocol task reconciliation was skipped because the storage backend has no automation repository",
                )
                .await,
            ));
        };

        let coordinator = crate::dotagents::DotagentsRuntimeCoordinator::new(
            crate::dotagents::DotagentsActivationContext {
                manifest: state.manifest.clone(),
                options: state.options.clone(),
                sessions: self.storage.session_store(),
                schedules,
                state: task_state,
                automation: conn,
                knowledge: self.storage.knowledge_store(),
                knowledge_scope: self.dotagents_knowledge_scope(),
                approver: state.approver.clone(),
                trigger: self.dotagents_startup_trigger(),
            },
        );

        let resolver = super::dotagents_runtime::SingleAgentTargetResolver::new(
            Arc::downgrade(&self.inner),
            self.profiles(),
        );
        let mut report = coordinator.activate(&resolver).await;
        // Passive diagnostics were gathered during construction and belong in the
        // same activation view for hosts that present protocol status.
        report
            .diagnostics
            .extend(state.passive_diagnostics().iter().cloned());
        report.sort_diagnostics();
        Ok(Some(report))
    }

    async fn dotagents_unavailable_report(
        &self,
        state: &AgentDotagentsState,
        message: &str,
    ) -> crate::dotagents::DotagentsActivationReport {
        let mut report = crate::dotagents::DotagentsActivationReport::default();
        report
            .diagnostics
            .push(crate::dotagents::DotagentsDiagnostic::warning(
                crate::dotagents::DotagentsDiagnosticCode::Other,
                message.to_string(),
            ));
        let memory_report = crate::dotagents::reconcile_memories(
            crate::dotagents::DotagentsMemoryPlan::from_manifest(&state.manifest),
            self.storage.knowledge_store().as_ref(),
            &self.dotagents_knowledge_scope(),
        )
        .await;
        report.memories_reconciled = memory_report.store_available;
        report.diagnostics.extend(memory_report.diagnostics.clone());
        report.memory = Some(memory_report);
        report
            .diagnostics
            .extend(state.passive_diagnostics().iter().cloned());
        report.sort_diagnostics();
        report
    }

    /// The knowledge scope protocol memories import into.
    fn dotagents_knowledge_scope(&self) -> String {
        crate::dotagents::protocol_knowledge_scope(self.cwd.as_deref())
    }

    /// The startup trigger used to fire `runOnStartup` protocol tasks.
    fn dotagents_startup_trigger(
        &self,
    ) -> Option<Arc<dyn crate::dotagents::DotagentsStartupTrigger>> {
        Some(Arc::new(
            super::dotagents_runtime::SchedulerStartupTrigger::new(self.inner.clone()),
        ))
    }

    pub async fn shutdown(&self) {
        if let Some(profiles) = self.profiles() {
            profiles.shutdown().await;
        }
        self.inner.shutdown().await;
    }

    #[cfg(feature = "api")]
    pub fn server(&self) -> AgentServer {
        let server = AgentServer::new(self.inner.clone(), self.storage.clone());
        if let Some(profiles) = self.profiles() {
            server.with_profiles(profiles)
        } else {
            server
        }
    }

    /// Start an ACP server with the specified transport.
    ///
    /// # Transports
    /// - `"stdio"` - Use stdin/stdout for JSON-RPC communication (for subprocess spawning)
    /// - `"ws://host:port"` - Start an ACP WebSocket server at `/acp/ws`
    ///
    /// # Example
    /// ```rust,no_run
    /// use querymt_agent::prelude::*;
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     let agent = Agent::builder()
    ///         .provider("anthropic", "claude-sonnet-4-20250514")
    ///         .cwd("/tmp")
    ///         .tools(["read_tool", "write_file"])
    ///         .build()
    ///         .await?;
    ///     
    ///     agent.acp("stdio").await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Graceful Shutdown
    /// The server handles SIGTERM and SIGINT (Ctrl+C) for graceful shutdown.
    pub async fn acp<T>(&self, transport: T) -> Result<()>
    where
        T: TryInto<AcpTransport>,
        T::Error: std::fmt::Display,
    {
        let transport = transport
            .try_into()
            .map_err(|e| anyhow!("Invalid transport: {}", e))?;

        match transport {
            AcpTransport::Stdio => serve_stdio(self.inner.clone())
                .await
                .map_err(|e| anyhow!("ACP stdio error: {}", e)),
            AcpTransport::WebSocket(addr) => serve_websocket(self.inner.clone(), &addr)
                .await
                .map_err(|e| anyhow!("ACP websocket error: {}", e)),
        }
    }

    pub fn inner(&self) -> Arc<AgentHandle> {
        self.inner.clone()
    }

    /// Returns `true` when the root has subagents it can delegate to. That
    /// includes `.agents` delegation targets, not only configured delegates.
    pub fn has_subagents(&self) -> bool {
        !self.roster.snapshot().is_empty()
    }

    #[deprecated(note = "use `has_subagents()`")]
    pub fn is_multi(&self) -> bool {
        self.has_subagents()
    }

    /// The members the root can delegate to. Read it with `snapshot()`.
    pub fn roster(&self) -> &Arc<crate::delegation::roster::AgentRoster> {
        &self.roster
    }

    /// Adds a member while the agent runs and returns the new roster
    /// generation. Connected clients receive `querymt/agents/changed`. The
    /// change lasts until the agent stops; configuration is not touched.
    pub async fn add_member(
        &self,
        spec: crate::delegation::member::MemberSpec,
    ) -> Result<crate::delegation::roster::RosterGeneration, crate::delegation::roster::RosterError>
    {
        self.inner.add_roster_member(spec).await
    }

    /// Adds a subagent while the agent runs, configured with the same closure
    /// builder as [`AgentBuilder::subagent`]. It is available to the root's
    /// `delegate` tool from the next turn of every session. Returns the new
    /// roster generation. The change lasts until the agent stops.
    ///
    /// Fails (leaving the roster unchanged) when delegation is disabled, the
    /// ID is taken or the subagent cannot be built.
    pub async fn add_subagent<F>(
        &self,
        id: impl Into<String>,
        configure: F,
    ) -> Result<crate::delegation::roster::RosterGeneration, crate::delegation::roster::RosterError>
    where
        F: FnOnce(super::config::DelegateConfigBuilder) -> super::config::DelegateConfigBuilder,
    {
        use crate::delegation::roster::{AgentId, RosterError};
        let id = AgentId::try_from(id.into().as_str())?;
        let spec = configure(super::config::DelegateConfigBuilder::new(id.clone()))
            .build()
            .map_err(|source| RosterError::Build { id, source })?;
        self.inner.add_roster_spec(spec).await
    }

    /// Removes a subagent by ID. A delegation already running on it finishes.
    pub async fn remove_subagent(
        &self,
        id: &str,
    ) -> Result<crate::delegation::roster::RosterGeneration, crate::delegation::roster::RosterError>
    {
        self.remove_member(id).await
    }

    /// The current subagents, ordered by ID. Never builds one.
    pub fn subagents(&self) -> Vec<SubagentInfo> {
        self.roster
            .snapshot()
            .iter()
            .map(|member| SubagentInfo {
                id: member.id().to_string(),
                name: member.info().name.clone(),
                description: member.info().description.clone(),
                capabilities: member.info().capabilities.clone(),
                origin: member.origin(),
                location: member.location(),
            })
            .collect()
    }

    /// How many times the roster has changed since the agent started.
    pub fn roster_generation(&self) -> u64 {
        self.roster.snapshot().generation().get()
    }

    /// Removes a member, whatever its origin. A delegation already running on
    /// it finishes.
    pub async fn remove_member(
        &self,
        id: &str,
    ) -> Result<crate::delegation::roster::RosterGeneration, crate::delegation::roster::RosterError>
    {
        self.inner.remove_roster_member(id).await
    }

    /// MCP servers this agent's runtimes were built with, including those
    /// merged in from `.agents/`: the root's and every local member's.
    pub fn mcp_server_configs(&self) -> Vec<crate::config::McpServerConfig> {
        let mut servers = self.inner.config.mcp_servers.clone();
        for member in self.inner.local_members() {
            servers.extend(member.config.mcp_servers.iter().cloned());
        }
        servers
    }

    /// A member handle by ID (`None` when the root has no such member).
    pub fn delegate(&self, id: &str) -> Option<Arc<AgentHandle>> {
        self.roster.local_member(id)
    }

    async fn ensure_default_session(&self) -> Result<String> {
        if let Some(existing) = self.default_session_id.lock().unwrap().clone() {
            return Ok(existing);
        }
        let session_id = self.create_session().await?;
        *self.default_session_id.lock().unwrap() = Some(session_id.clone());
        Ok(session_id)
    }

    pub(super) async fn create_session(&self) -> Result<String> {
        let request = match &self.cwd {
            Some(cwd) => NewSessionRequest::new(cwd.clone()),
            None => NewSessionRequest::new(PathBuf::new()),
        };
        let response = self
            .inner
            .new_session(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(response.session_id.to_string())
    }

    async fn chat_with_session(&self, session_id: &str, prompt: &str) -> Result<String> {
        let request = PromptRequest::new(
            session_id.to_string(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        self.inner
            .prompt(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let history = self
            .inner
            .config
            .provider
            .history_store()
            .get_history(session_id)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        latest_assistant_message(&history).ok_or_else(|| anyhow!("No assistant response found"))
    }

    /// Create from a serializable config + custom infrastructure.
    ///
    /// This is the primary construction path for FFI callers (iOS, embedded)
    /// who have their own plugin registry and storage backend.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use querymt_agent::prelude::*;
    /// use std::sync::Arc;
    ///
    /// # async fn example(config: SingleAgentConfig, registry: querymt::plugin::host::PluginRegistry, storage: Arc<dyn querymt_agent::session::backend::StorageBackend>) {
    /// let agent = Agent::from_config(config, AgentInfra {
    ///     plugin_registry: Arc::new(registry),
    ///     storage: Some(storage),
    ///     session_mcp_attachment_source: None,
    ///     event_fanout: None,
    /// }).await.unwrap();
    /// agent.chat("hello").await.unwrap();
    /// # }
    /// ```
    pub async fn from_config(config: SingleAgentConfig, infra: AgentInfra) -> Result<Self> {
        Self::from_single_config_with_optional_infra(config, Some(infra)).await
    }

    /// Build an Agent from a single agent config (default infrastructure).
    pub async fn from_single_config(config: SingleAgentConfig) -> Result<Self> {
        Self::from_single_config_with_optional_infra(config, None).await
    }

    /// Build an Agent from a single agent config with injected infrastructure.
    ///
    /// Unlike the old mobile-specific path, this constructor treats
    /// `SingleAgentConfig.mesh` as the single source of truth and performs mesh
    /// setup through the shared remote setup path.
    pub async fn from_single_config_with_infra(
        config: SingleAgentConfig,
        infra: AgentInfra,
    ) -> Result<Self> {
        Self::from_single_config_with_optional_infra(config, Some(infra)).await
    }

    async fn from_single_config_with_optional_infra(
        config: SingleAgentConfig,
        infra: Option<AgentInfra>,
    ) -> Result<Self> {
        let mut builder = Self::builder_from_config(config)?;
        if let Some(infra) = infra {
            builder = builder.infra(infra);
        }
        builder.build().await
    }

    /// The builder for either config shape. `dotagents_workspace` is the
    /// protocol workspace fallback (see `builder_from_config_with_dotagents`).
    pub fn builder_from_config_value(
        config: crate::config::Config,
        dotagents_workspace: Option<&std::path::Path>,
    ) -> Result<AgentBuilder> {
        match config {
            crate::config::Config::Single(config) => {
                Self::builder_from_config_with_dotagents(*config, dotagents_workspace)
            }
            crate::config::Config::Multi(config) => {
                let builder = Self::lower_quorum_config(*config)?;
                Ok(match dotagents_workspace {
                    Some(workspace) => builder.with_dotagents_workspace(workspace),
                    None => builder,
                })
            }
        }
    }

    /// Configure an `AgentBuilder` from a `SingleAgentConfig`.
    ///
    /// Returns the builder before `build()` is called, allowing further
    /// customization (e.g., `.infra()`, `.middleware()`).
    pub fn builder_from_config(config: SingleAgentConfig) -> Result<AgentBuilder> {
        Self::builder_from_config_with_dotagents(config, None)
    }

    /// Configure an `AgentBuilder` from a `SingleAgentConfig` with protocol context.
    ///
    /// `dotagents_workspace` supplies the workspace used to derive the
    /// `<workspace>/.agents/` layer when neither the profile settings nor a
    /// `workspace_root` override select one. This lets profile runtimes pick up
    /// repository protocol files without making the TOML profile catalog parse
    /// protocol profiles itself.
    pub fn builder_from_config_with_dotagents(
        config: SingleAgentConfig,
        dotagents_workspace: Option<&std::path::Path>,
    ) -> Result<AgentBuilder> {
        let dotagents_options = config.dotagents.load_options();
        let dotagents_options = match dotagents_workspace {
            Some(workspace) => dotagents_options.with_workspace_fallback(workspace),
            None => dotagents_options,
        };
        let mut builder = AgentBuilder::new()
            .provider(config.agent.provider, config.agent.model)
            .tool_specs(config.agent.tools)
            .dotagents_options(dotagents_options);

        if let Some(api_key) = config.agent.api_key {
            builder = builder.api_key(api_key);
        }
        for part in config.agent.system {
            if let Some(s) = part.resolved() {
                builder = builder.system(s);
            }
        }
        if let Some(params) = config.agent.parameters {
            for (key, value) in params {
                builder = builder.parameter(key, value);
            }
        }
        if let Some(cwd) = config.agent.cwd {
            builder.cwd = Some(cwd);
        }
        builder.mode = config.agent.mode;
        builder.permissions = config.agent.permissions.to_permissions();
        builder.delegation_settings = config.agent.delegation;
        if let Some(policy) = config.agent.snapshot_policy {
            builder.snapshot_policy = policy.parse().map_err(|error: String| anyhow!(error))?;
        }
        builder.lower_delegates(config.delegates, &config.mcp)?;

        // Apply middleware from config
        if !config.middleware.is_empty() {
            builder = builder.middleware_from_config(config.middleware);
        }

        // Thread through config fields that were previously silently dropped
        builder.execution = Some(config.agent.execution);
        builder.skills_config = Some(config.agent.skills);
        builder.slash_commands_config = Some(config.agent.slash_commands);
        builder.hooks_config = Some(config.agent.hooks);
        if config.agent.workspace.enabled {
            builder.workspace = Some(config.agent.workspace);
        }
        if config.agent.repository_knowledge.enabled {
            builder.repository_knowledge = Some(config.agent.repository_knowledge);
        }

        // Wire MCP servers from TOML `[[mcp]]` config.
        if !config.mcp.is_empty() {
            builder.mcp_servers = config.mcp;
        }

        #[cfg(feature = "remote")]
        if config.mesh.enabled {
            builder.mesh_auto_fallback = config.mesh.auto_fallback;
            builder.mesh =
                Some(Mesh::from_toml(config.mesh).with_remote_agents(config.remote_agents));
        }

        Ok(builder)
    }
}

/// Construction from the deprecated `[quorum]` config shape. It lowers to the
/// same `AgentBuilder` as everything else: the planner is the root and the
/// delegates are its members.
impl Agent {
    /// Create an agent from a serializable quorum config + custom infrastructure.
    #[deprecated(
        note = "`[quorum]` is deprecated: use `[agent]` with `[[delegates]]`, or `Agent::builder().subagent(..)`"
    )]
    pub async fn from_quorum_config_with_infra(
        config: crate::config::QuorumConfig,
        infra: AgentInfra,
    ) -> Result<Self> {
        Self::lower_quorum_config(config)?
            .infra(infra)
            .build()
            .await
    }

    /// Build an Agent from a quorum config (default infrastructure).
    #[deprecated(
        note = "`[quorum]` is deprecated: use `[agent]` with `[[delegates]]`, or `Agent::builder().subagent(..)`"
    )]
    pub async fn from_quorum_config(config: crate::config::QuorumConfig) -> Result<Self> {
        Self::lower_quorum_config(config)?.build().await
    }

    /// Configure an `AgentBuilder` from a `QuorumConfig`.
    ///
    /// Returns the builder before `build()` is called, allowing further
    /// customization (e.g., `.infra()`).
    #[deprecated(
        note = "`[quorum]` is deprecated: use `[agent]` with `[[delegates]]`, or `Agent::builder().subagent(..)`"
    )]
    pub fn builder_from_quorum_config(config: crate::config::QuorumConfig) -> Result<AgentBuilder> {
        Self::lower_quorum_config(config)
    }

    /// The lowering behind the deprecated quorum entry points and `Config::Multi`.
    pub(crate) fn lower_quorum_config(config: crate::config::QuorumConfig) -> Result<AgentBuilder> {
        let snapshot_policy = match config.quorum.snapshot_policy {
            Some(policy) => policy.parse().map_err(|error: String| anyhow!(error))?,
            None => SnapshotPolicy::None,
        };
        let mut builder = AgentBuilder::new()
            .provider(config.planner.provider, config.planner.model)
            .dotagents_options(config.dotagents.load_options());
        builder.root_id = Some("planner".to_string());
        builder.git_snapshots = true;
        builder.require_skills_for_skill_tool = true;
        builder.snapshot_policy = snapshot_policy;
        if let Some(cwd) = config.quorum.cwd {
            builder.cwd = Some(cwd);
        }
        builder.delegation_enabled = config.quorum.delegation;
        builder.delegation_settings = crate::config::DelegationSettings {
            wait_policy: config.quorum.delegation_wait_policy,
            wait_timeout_secs: config.quorum.delegation_wait_timeout_secs,
            cancel_grace_secs: config.quorum.delegation_cancel_grace_secs,
            max_parallel: std::num::NonZeroUsize::new(config.quorum.max_parallel_delegations)
                .ok_or_else(|| anyhow!("`[quorum] max_parallel_delegations` must be at least 1"))?,
            verification: config.quorum.verification,
            summary: config.quorum.delegation_summary,
        };

        let available_local_tool_names = crate::delegation::member::available_local_tool_names();

        for part in config.planner.system {
            if let Some(system) = part.resolved() {
                builder = builder.system(system);
            }
        }
        if let Some(api_key) = config.planner.api_key {
            builder = builder.api_key(api_key);
        }
        for (key, value) in config.planner.parameters.into_iter().flatten() {
            builder = builder.parameter(key, value);
        }

        // Resolve planner tools (validates local tools and prepares for MCP).
        // The runtime tool list keeps the normalized resolved MCP selectors so
        // build() can construct allowlists the runtime filter understands and
        // widen the tool policy for MCP tool advertisement.
        let planner_resolved = crate::config::resolve_tools(
            &config.planner.tools,
            &config.mcp,
            &[],
            &available_local_tool_names,
        )?;
        builder.tools = planner_resolved.runtime_tool_selectors();
        builder.mcp_servers = planner_resolved
            .mcp_servers
            .values()
            .map(|(config, _)| config.clone())
            .collect();
        builder.mode = config.planner.mode;
        builder.permissions = config.planner.permissions.to_permissions();
        builder.middleware_entries = config.planner.middleware;
        builder.execution = Some(config.planner.execution);
        builder.skills_config = Some(config.planner.skills);

        builder.lower_delegates(config.delegates, &config.mcp)?;

        if config.quorum.workspace.enabled {
            builder.workspace = Some(config.quorum.workspace);
        }
        if config.quorum.repository_knowledge.enabled {
            builder.repository_knowledge = Some(config.quorum.repository_knowledge);
        }
        #[cfg(feature = "remote")]
        {
            builder.mesh_auto_fallback = config.mesh.auto_fallback;
            if config.mesh.enabled {
                builder.mesh =
                    Some(Mesh::from_toml(config.mesh).with_remote_agents(config.remote_agents));
            }
        }

        Ok(builder)
    }
}

#[async_trait]
impl ChatRunner for Agent {
    async fn chat(&self, prompt: &str) -> Result<String> {
        Agent::chat(self, prompt).await
    }

    async fn chat_session(&self) -> Result<Box<dyn ChatSession>> {
        let session = Agent::chat_session(self).await?;
        Ok(Box::new(session))
    }

    fn subscribe(
        &self,
    ) -> tokio::sync::broadcast::Receiver<std::sync::Arc<crate::events::EventEnvelope>> {
        Agent::subscribe(self)
    }

    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>) {
        self.callbacks.on_tool_call(callback);
        self.callbacks.ensure_listener(Agent::subscribe(self));
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks.ensure_listener(Agent::subscribe(self));
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks.ensure_listener(Agent::subscribe(self));
    }

    fn on_delegation_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_delegation(callback);
        self.callbacks.ensure_listener(Agent::subscribe(self));
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks.ensure_listener(Agent::subscribe(self));
    }

    #[cfg(feature = "api")]
    fn server(&self) -> AgentServer {
        AgentServer::new(self.inner.clone(), self.storage.clone())
    }
}

/// Scan the manifest's skill list with the settings the agent's skill tool
/// uses; an agent that gets no skill tool lists no skills.
fn with_skill_scan(
    options: Option<crate::dotagents::DotagentsLoadOptions>,
    skills: Option<&SkillsConfig>,
    tools: &[ToolSpec],
) -> Option<crate::dotagents::DotagentsLoadOptions> {
    options.map(|options| {
        options.with_skill_scan(
            match crate::skills::SkillCatalog::skills_for_agent(skills, tools) {
                Some(skills) => crate::dotagents::DotagentsSkillScan::from_config(skills),
                None => crate::dotagents::DotagentsSkillScan::Disabled,
            },
        )
    })
}

/// A `skill` tool needs skills to be configured and enabled for its agent.
fn check_skill_tool(tools: &[ToolSpec], skills: Option<&SkillsConfig>, who: &str) -> Result<()> {
    if !crate::skills::SkillCatalog::names_skill_tool(tools) {
        return Ok(());
    }
    match skills {
        Some(skills) if skills.enabled => Ok(()),
        Some(_) => Err(anyhow!(
            "Tool '{}' requested for {who}, but skills are disabled",
            crate::skills::SkillTool::NAME
        )),
        None => Err(anyhow!(
            "Tool '{}' requested for {who}, but skills are not configured",
            crate::skills::SkillTool::NAME
        )),
    }
}

/// Adds the protocol MCP servers a member does not already configure.
fn merge_dotagents_mcp_servers(
    member: &mut crate::delegation::member::MemberBuildSpec,
    manifest_servers: &[McpServerConfig],
) -> Vec<crate::dotagents::DotagentsDiagnostic> {
    let mut diagnostics = Vec::new();
    for server in manifest_servers {
        if !member
            .mcp_servers
            .iter()
            .any(|existing| existing.name() == server.name())
        {
            member.mcp_servers.push(server.clone());
        } else {
            let message = format!(
                "protocol MCP server `{}` collides with an explicit configuration; keeping the explicit server",
                server.name()
            );
            log::warn!("dotagents: {message}");
            diagnostics.push(crate::dotagents::DotagentsDiagnostic::warning(
                crate::dotagents::DotagentsDiagnosticCode::Collision,
                message,
            ));
        }
    }
    diagnostics
}

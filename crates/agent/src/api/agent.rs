//! Single agent implementation

use super::callbacks::EventCallbacksState;
#[cfg(feature = "remote")]
use super::mesh::{AgentMesh, Mesh, MeshSpec};
use super::profiles::{AgentProfiles, ProfileRuntimeHandle};
use super::quorum::QuorumBuilder;
use super::session::AgentSession;
use super::sessions::{AgentSessions, ListSessionsOptions, SessionListPage};
use super::utils::{default_registry, latest_assistant_message, to_absolute_path};
use crate::acp::AcpTransport;
use crate::acp::protocol::{ContentBlock, NewSessionRequest, PromptRequest, TextContent};
use crate::acp::stdio::serve_stdio;
use crate::acp::websocket::serve_websocket;
use crate::agent::LocalAgentHandle as AgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::{SnapshotPolicy, ToolPolicy};
use crate::agent::session_mcp::SessionMcpAttachmentSource;
use crate::config::{
    ExecutionPolicy, HooksConfig, McpServerConfig, MiddlewareEntry, SingleAgentConfig,
    SkillsConfig, SlashCommandsConfig,
};
use crate::event_fanout::EventFanout;
use crate::middleware::{MIDDLEWARE_REGISTRY, MiddlewareDriver};
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
/// let agent = Agent::single()
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
type MiddlewareFactory = Box<dyn FnOnce(&AgentHandle) -> Arc<dyn MiddlewareDriver> + Send>;

pub struct AgentBuilder {
    pub(super) llm_config: Option<LLMParams>,
    pub(super) tools: Vec<String>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) snapshot_policy: SnapshotPolicy,
    pub(super) db_path: Option<PathBuf>,
    assume_mutating: Option<bool>,
    mutating_tools: Option<Vec<String>>,
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
    /// MCP servers from TOML `[[mcp]]` config, attached to every new session.
    mcp_servers: Vec<McpServerConfig>,
    /// Runtime MCP attachment source (e.g., mobile in-process MCP peers).
    session_mcp_attachment_source: Option<Arc<dyn SessionMcpAttachmentSource>>,
    /// Optional pre-built agent registry (Phase 7: injected by `from_single_config_with_registry`).
    pub(super) agent_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
    /// Optional pre-built infrastructure (plugin registry + storage).
    infra: Option<AgentInfra>,
    #[cfg(feature = "remote")]
    mesh: Option<Mesh>,
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
            cwd: None,
            snapshot_policy: SnapshotPolicy::Diff,
            db_path: None,
            assume_mutating: None,
            mutating_tools: None,
            middleware_factories: Vec::new(),
            middleware_entries: Vec::new(),
            execution: None,
            skills_config: None,
            slash_commands_config: None,
            hooks_config: None,
            dotagents_options: None,
            dotagents_manifest: None,
            dotagents_task_approver: None,
            mcp_servers: Vec::new(),
            session_mcp_attachment_source: None,
            agent_registry: None,
            infra: None,
            #[cfg(feature = "remote")]
            mesh: None,
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

    pub fn system(self, prompt: impl Into<String>) -> Self {
        self.with_llm(|c| c.system(prompt))
    }

    pub fn parameter<K: Into<String>>(self, key: K, value: impl Into<serde_json::Value>) -> Self {
        self.with_llm(|c| c.parameter(key, value.into()))
    }

    pub fn db(mut self, path: impl Into<PathBuf>) -> Self {
        self.db_path = Some(path.into());
        self
    }

    pub fn tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tools = tools.into_iter().map(Into::into).collect();
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

    /// Enable `.agents` loading with default discovery options.
    pub fn enable_dotagents(mut self) -> Self {
        self.dotagents_options = Some(crate::dotagents::DotagentsLoadOptions::enabled());
        self
    }

    /// Configure `.agents` loading explicitly.
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
            self.dotagents_options.clone(),
            cwd.as_deref(),
        )
        .map_err(|error| anyhow::Error::new(*error))
    }

    /// Set whether to assume all tools are mutating.
    pub fn assume_mutating(mut self, yes: bool) -> Self {
        self.assume_mutating = Some(yes);
        self
    }

    /// Set specific tools to be considered mutating.
    pub fn mutating_tools(mut self, tools: Vec<String>) -> Self {
        self.mutating_tools = Some(tools);
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
    /// let agent = Agent::single()
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
        M: MiddlewareDriver + 'static,
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
        let snapshot_policy = self.snapshot_policy;
        let cwd = if let Some(path) = self.cwd {
            Some(to_absolute_path(path)?)
        } else {
            None
        };

        let mut llm_config = self
            .llm_config
            .ok_or_else(|| anyhow!("LLM configuration is required (call .provider() first)"))?;
        let dotagents_options = self.dotagents_options.take();
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
                .with_agent_id("agent")
                .with_snapshot_policy(snapshot_policy);
        if let Some(event_fanout) = event_fanout {
            builder = builder.with_event_fanout(event_fanout);
        }

        // Phase 7: inject pre-populated agent registry (remote agents from config).
        #[cfg(feature = "remote")]
        if let Some(mesh) = &self.mesh
            && !mesh.remote_agents().is_empty()
            && let MeshSpec::Toml(cfg) = mesh.spec_for_internal_use()
        {
            let runtime = mesh.start().await?;
            let registry = crate::agent::remote::register_remote_agents_from_config(
                runtime.handle().as_mesh_handle(),
                mesh.remote_agents(),
                &cfg.peers,
            )
            .await?;
            self.agent_registry = Some(registry);
        }
        let explicit_registry = self.agent_registry.take();
        // Delegation is considered enabled only when the host actually wired a
        // delegation path: the `delegate` tool, or a non-empty registry of
        // pre-registered targets. Protocol loading never changes this.
        let delegation_enabled = self.tools.iter().any(|tool| tool == "delegate")
            || explicit_registry
                .as_ref()
                .is_some_and(|registry| !registry.list_agents().is_empty());

        // Task 3.2: extend a delegation-enabled standalone runtime's registry
        // with lazy protocol targets. Delegation-disabled runtimes never
        // register targets, and protocol loading never enables delegation.
        if delegation_enabled && let Some(manifest) = &dotagents_manifest {
            let plans = crate::dotagents::DotagentsSubAgentPlans::from_manifest(manifest);
            let registry = crate::dotagents::DotagentsTargetRegistry::new(
                &plans,
                explicit_registry,
                Arc::new(super::DotagentsTargetFactory::new(
                    plugin_registry.clone(),
                    backend.clone(),
                )),
            );
            for diagnostic in registry.diagnostics() {
                log::warn!("dotagents: {diagnostic}");
            }
            builder = builder.with_agent_registry(Arc::new(registry));
        } else if let Some(registry) = explicit_registry {
            builder = builder.with_agent_registry(registry);
        }

        // Wire schedule repository and knowledge store from the storage backend.
        // These are optional — backends that don't support them return None.
        if let Some(repo) = backend.schedule_repository() {
            builder = builder.with_schedule_repository(repo);
        }
        if let Some(ks) = backend.knowledge_store() {
            builder = builder.with_knowledge_store(ks);
        }

        if let Some(assume_mutating) = self.assume_mutating {
            builder = builder.with_assume_mutating(assume_mutating);
        }
        if let Some(mutating_tools) = self.mutating_tools {
            builder = builder.with_mutating_tools(mutating_tools);
        }

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

        if let Some(skills_config) = self.skills_config
            && skills_config.enabled
            && (self.tools.is_empty()
                || self
                    .tools
                    .iter()
                    .any(|tool| tool == crate::skills::SkillTool::NAME))
        {
            let project_root = cwd.as_deref().unwrap_or_else(|| std::path::Path::new("."));
            builder
                .tool_registry_mut()
                .add(crate::skills::build_skill_tool(
                    &skills_config,
                    project_root,
                ));
        }

        if !self.tools.is_empty() {
            // Infer tool policy from the tools list:
            // - If any tool spec looks like an external/MCP pattern (contains a
            //   dot separator like "server.*" or "server.tool"), use
            //   BuiltInAndProvider so MCP tool definitions reach the LLM.
            // - Otherwise BuiltInOnly is sufficient.
            //
            // This handles both config-based MCP servers and preconnected
            // runtime MCP peers (e.g. mobile in-process MCP), since the tools
            // list already declares the intended tool scope.
            let has_external_tools = self.tools.iter().any(|t| {
                // MCP-style specs contain a dot: "server.*" or "server.tool_name"
                // while builtins are plain names like "create_task", "read_tool".
                t.contains('.') || self.mcp_servers.iter().any(|m| t == m.name())
            });
            let policy = if has_external_tools || !self.mcp_servers.is_empty() {
                ToolPolicy::BuiltInAndProvider
            } else {
                ToolPolicy::BuiltInOnly
            };
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
        }

        // Build initial config for middleware factories (temporary handle)
        let initial_config = Arc::new(builder.build());
        let temp_handle = Arc::new(AgentHandle::from_config(initial_config.clone()));

        // Apply middleware factories - each factory receives the handle
        let mut middleware_drivers: Vec<Arc<dyn MiddlewareDriver>> = Vec::new();
        for factory in self.middleware_factories {
            let middleware = factory(&temp_handle);
            middleware_drivers.push(middleware);
        }

        // Apply config-based middleware entries
        for entry in &self.middleware_entries {
            match MIDDLEWARE_REGISTRY.create(&entry.middleware_type, &entry.config, &initial_config)
            {
                Ok(middleware) => {
                    middleware_drivers.push(middleware);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("disabled") {
                        return Err(anyhow!(
                            "Failed to create middleware '{}': {}",
                            entry.middleware_type,
                            e
                        ));
                    }
                }
            }
        }

        // Auto-add ContextMiddleware if compaction.auto is true and user didn't provide one
        if initial_config.execution_policy.compaction.auto {
            let already_has = middleware_drivers
                .iter()
                .any(|d| d.name() == "ContextMiddleware");
            if !already_has {
                log::info!("Auto-enabling ContextMiddleware for compaction");
                middleware_drivers.push(Arc::new(crate::middleware::ContextMiddleware::new(
                    crate::middleware::ContextConfig::default().auto_compact(true),
                )));
            }
        }

        // Drop the temporary handle so Arc::try_unwrap can succeed
        // (avoids a full clone of AgentConfig).
        drop(temp_handle);

        // Build final AgentConfig with middleware swapped in
        let initial = Arc::try_unwrap(initial_config).unwrap_or_else(|arc| (*arc).clone());
        let final_config = Arc::new(initial.with_middleware(middleware_drivers));

        let handle = Arc::new(AgentHandle::from_config(final_config));

        #[cfg(feature = "remote")]
        let mesh_node_name = self.mesh.as_ref().and_then(|mesh| mesh.node_name());

        #[cfg(feature = "remote")]
        if let Some(mesh) = &self.mesh {
            let runtime = mesh.start().await?;
            handle.set_mesh(runtime.handle().as_mesh_handle().clone());
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

        let agent = Agent {
            inner: handle,
            storage: backend,
            default_session_id: Arc::new(Mutex::new(None)),
            cwd,
            callbacks: Arc::new(EventCallbacksState::new(None)),
            profiles: None,
            quorum: None,
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
    #[cfg_attr(not(feature = "api"), allow(dead_code))]
    pub(super) storage: Arc<dyn StorageBackend>,
    pub(super) default_session_id: Arc<Mutex<Option<String>>>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) callbacks: Arc<EventCallbacksState>,
    pub(super) profiles: Option<Arc<AgentProfiles>>,
    /// Present when this agent was built with `Agent::multi()`.
    /// Holds the quorum orchestrator for delegate access.
    pub(super) quorum: Option<Arc<crate::quorum::AgentQuorum>>,
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

impl Agent {
    pub fn single() -> AgentBuilder {
        AgentBuilder::new()
    }

    pub fn multi() -> QuorumBuilder {
        QuorumBuilder::new()
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

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::events::EventEnvelope> {
        if let Some(quorum) = &self.quorum {
            quorum.subscribe_events()
        } else {
            self.inner.subscribe_events()
        }
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
        if let Some(quorum) = self.quorum() {
            quorum.shutdown().await;
        }
        self.inner.shutdown().await;
    }

    #[cfg(feature = "api")]
    pub fn server(&self) -> AgentServer {
        let server = AgentServer::new(self.inner.clone(), self.storage.clone(), self.cwd.clone());
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
    ///     let agent = Agent::single()
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

    /// Returns `true` if this agent was built with `Agent::multi()`.
    pub fn is_multi(&self) -> bool {
        self.quorum.is_some()
    }

    /// Access the quorum orchestrator (returns `None` for single agents).
    pub fn quorum(&self) -> Option<&crate::quorum::AgentQuorum> {
        self.quorum.as_deref()
    }

    /// Access the planner handle (returns `None` for single agents).
    ///
    /// For single agents, use `.handle()` instead.
    pub fn planner(&self) -> Option<Arc<dyn crate::agent::handle::AgentHandle>> {
        self.quorum.as_ref().map(|q| q.planner())
    }

    /// Access a delegate handle by ID (returns `None` for single agents or if not found).
    pub fn delegate(&self, id: &str) -> Option<Arc<dyn crate::agent::handle::AgentHandle>> {
        self.quorum.as_ref().and_then(|q| q.delegate(id))
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
        let attachment_source = infra
            .as_ref()
            .and_then(|i| i.session_mcp_attachment_source.clone());

        #[cfg(feature = "remote")]
        {
            if config.mesh.enabled {
                let auto_fallback = config.mesh.auto_fallback;
                let mesh_cfg = config.mesh.clone();
                let remote_agents = config.remote_agents.clone();
                let mut builder = Self::builder_from_config(config, None)?;
                if let Some(infra) = infra {
                    builder = builder.infra(infra);
                }
                if let Some(source) = attachment_source {
                    builder = builder.with_session_mcp_attachment_source(source);
                }
                let agent = builder
                    .mesh(Mesh::from_toml(mesh_cfg).with_remote_agents(remote_agents))
                    .build()
                    .await?;
                agent.inner.set_mesh_fallback(auto_fallback);
                return Ok(agent);
            }
        }

        let mut builder = Self::builder_from_config(config, None)?;
        if let Some(infra) = infra {
            builder = builder.infra(infra);
        }
        if let Some(source) = attachment_source {
            builder = builder.with_session_mcp_attachment_source(source);
        }
        builder.build().await
    }

    /// Configure an `AgentBuilder` from a `SingleAgentConfig`.
    ///
    /// Returns the builder before `build()` is called, allowing further
    /// customization (e.g., `.infra()`, `.middleware()`).
    pub fn builder_from_config(
        config: SingleAgentConfig,
        initial_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
    ) -> Result<AgentBuilder> {
        Self::builder_from_config_with_dotagents(config, initial_registry, None)
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
        initial_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
        dotagents_workspace: Option<&std::path::Path>,
    ) -> Result<AgentBuilder> {
        let dotagents_options = config.dotagents.load_options();
        let dotagents_options = match dotagents_workspace {
            Some(workspace) => dotagents_options.with_workspace_fallback(workspace),
            None => dotagents_options,
        };
        let mut builder = AgentBuilder::new()
            .provider(config.agent.provider, config.agent.model)
            .tools(config.agent.tools)
            .dotagents_options(dotagents_options);

        if let Some(api_key) = config.agent.api_key {
            builder = builder.api_key(api_key);
        }
        for part in config.agent.system {
            if let crate::config::SystemPart::Inline(s) = part {
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
        builder.assume_mutating = Some(config.agent.assume_mutating);
        builder.mutating_tools = Some(config.agent.mutating_tools);

        // Inject pre-populated registry (Phase 7).
        if let Some(registry) = initial_registry {
            builder.agent_registry = Some(registry);
        }

        // Apply middleware from config
        if !config.middleware.is_empty() {
            builder = builder.middleware_from_config(config.middleware);
        }

        // Thread through config fields that were previously silently dropped
        builder.execution = Some(config.agent.execution);
        builder.skills_config = Some(config.agent.skills);
        builder.slash_commands_config = Some(config.agent.slash_commands);
        builder.hooks_config = Some(config.agent.hooks);

        // Wire MCP servers from TOML `[[mcp]]` config.
        if !config.mcp.is_empty() {
            builder.mcp_servers = config.mcp;
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
        if let Some(quorum) = &self.quorum {
            let session_id = self.create_session().await?;
            let session = super::quorum::QuorumSession::new(
                quorum.planner(),
                quorum.event_fanout(),
                quorum.store(),
                session_id,
                self.cwd.clone(),
            );
            Ok(Box::new(session))
        } else {
            let session = Agent::chat_session(self).await?;
            Ok(Box::new(session))
        }
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::events::EventEnvelope> {
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
        AgentServer::new(self.inner.clone(), self.storage.clone(), self.cwd.clone())
    }
}

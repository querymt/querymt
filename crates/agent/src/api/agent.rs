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
    ExecutionPolicy, HooksConfig, McpServerConfig, MiddlewareEntry, SingleAgentConfig, SkillsConfig,
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
    hooks_config: Option<HooksConfig>,
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
            hooks_config: None,
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

        let llm_config = self
            .llm_config
            .ok_or_else(|| anyhow!("LLM configuration is required (call .provider() first)"))?;

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

        let mut builder = AgentConfigBuilder::new(plugin_registry, backend.clone(), llm_config)
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
        if let Some(registry) = self.agent_registry {
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

        // Initialize skills system if enabled
        if let Some(skills_config) = self.skills_config {
            if skills_config.enabled {
                use crate::skills::{SkillRegistry, SkillTool, default_search_paths};
                use std::sync::Mutex;

                let project_root = cwd.as_deref().unwrap_or_else(|| std::path::Path::new("."));
                let mut search_paths = default_search_paths(project_root);
                for custom_path in &skills_config.paths {
                    search_paths.push(crate::skills::types::SkillSource::Configured(
                        custom_path.clone(),
                    ));
                }

                let mut skill_registry = SkillRegistry::new();
                match skill_registry
                    .load_from_sources(&search_paths, skills_config.include_external)
                {
                    Ok(count) => {
                        if count > 0 {
                            let compatible_skills =
                                skill_registry.compatible_with(&skills_config.agent_id);
                            let compatible_names: Vec<_> = compatible_skills
                                .iter()
                                .map(|s| s.metadata.name.clone())
                                .collect();
                            log::info!(
                                "Skills system initialized: {} skills discovered, {} compatible with agent '{}'",
                                count,
                                compatible_names.len(),
                                skills_config.agent_id
                            );
                            if !compatible_names.is_empty() {
                                log::debug!("Compatible skills: {}", compatible_names.join(", "));
                            }
                        } else {
                            log::debug!(
                                "Skills system enabled but no skills found in {} search paths",
                                search_paths.len()
                            );
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to discover skills: {}. Skills system will be unavailable.",
                            e
                        );
                    }
                }

                let registry_arc = Arc::new(Mutex::new(skill_registry));
                let skill_tool = SkillTool::new(
                    registry_arc,
                    Some(skills_config.agent_id.clone()),
                    Arc::new(skills_config.permissions.clone()),
                );
                builder.tool_registry_mut().add(Arc::new(skill_tool));
            } else {
                log::debug!("Skills system disabled in configuration");
            }
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
        };

        #[cfg(feature = "remote")]
        if self.mesh.is_some() {
            agent.inner.ensure_mesh_published(mesh_node_name).await?;
        }

        Ok(agent)
    }
}

pub struct Agent {
    pub(super) inner: Arc<AgentHandle>,
    #[cfg_attr(not(feature = "api"), allow(dead_code))]
    pub(super) storage: Arc<dyn StorageBackend>,
    pub(super) default_session_id: Arc<Mutex<Option<String>>>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) callbacks: Arc<EventCallbacksState>,
    pub(super) profiles: Option<AgentProfiles>,
    /// Present when this agent was built with `Agent::multi()`.
    /// Holds the quorum orchestrator for delegate access.
    pub(super) quorum: Option<crate::quorum::AgentQuorum>,
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
        self.profiles = Some(profiles);
        self
    }

    pub fn profiles(&self) -> Option<ProfileRuntimeHandle> {
        self.profiles.as_ref().map(AgentProfiles::manager)
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
    /// - `"ws://host:port"` - Start a WebSocket server (not yet implemented, use .server() instead)
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
        self.quorum.as_ref()
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
        let mut builder = AgentBuilder::new()
            .provider(config.agent.provider, config.agent.model)
            .tools(config.agent.tools);

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

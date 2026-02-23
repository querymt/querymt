//! Single agent implementation

use super::callbacks::EventCallbacksState;
use super::quorum::QuorumBuilder;
use super::session::AgentSession;
use super::utils::{default_registry, latest_assistant_message, to_absolute_path};
use crate::acp::AcpTransport;
use crate::acp::stdio::serve_stdio;
use crate::acp::websocket::serve_websocket;
use crate::agent::AgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::{SnapshotPolicy, ToolPolicy};
use crate::config::{
    ExecutionPolicy, McpServerConfig, MiddlewareEntry, SingleAgentConfig, SkillsConfig,
};
use crate::events::AgentEvent;
use crate::middleware::{MIDDLEWARE_REGISTRY, MiddlewareDriver};
use crate::runner::{ChatRunner, ChatSession};
use crate::send_agent::SendAgent;
#[cfg(feature = "dashboard")]
use crate::server::AgentServer;
use crate::session::backend::{StorageBackend, default_agent_db_path};
use crate::session::sqlite_storage::SqliteStorage;
use agent_client_protocol::{ContentBlock, NewSessionRequest, PromptRequest, TextContent};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use querymt::LLMParams;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
    /// MCP servers from TOML `[[mcp]]` config, attached to every new session.
    mcp_servers: Vec<McpServerConfig>,
    /// Optional pre-built agent registry (Phase 7: injected by `from_single_config_with_registry`).
    pub(super) agent_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
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
            mcp_servers: Vec::new(),
            agent_registry: None,
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

    /// Add a middleware to the agent using a factory closure.
    ///
    /// The closure receives a reference to the constructed `QueryMTAgent`,
    /// allowing access to internal state like `event_bus()`.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use querymt_agent::simple::Agent;
    /// use querymt_agent::middleware::DedupCheckMiddleware;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let agent = Agent::single()
    ///     .provider("openai", "gpt-4")
    ///     .cwd(".")
    ///     .middleware(|agent| {
    ///         DedupCheckMiddleware::new()
    ///             .threshold(0.8)
    ///             .min_lines(5)
    ///             .with_event_bus(agent.event_bus())
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

    pub async fn build(self) -> Result<Agent> {
        let snapshot_policy = self.snapshot_policy;
        let cwd = if let Some(path) = self.cwd {
            Some(to_absolute_path(path)?)
        } else {
            None
        };

        let llm_config = self
            .llm_config
            .ok_or_else(|| anyhow!("LLM configuration is required (call .provider() first)"))?;

        let plugin_registry = Arc::new(default_registry().await?);
        let db_path = match self.db_path {
            Some(path) => path,
            None => default_agent_db_path()?,
        };
        let backend = SqliteStorage::connect(db_path).await?;

        let mut builder =
            AgentConfigBuilder::new(plugin_registry, backend.session_store(), llm_config)
                .with_snapshot_policy(snapshot_policy);

        // Phase 7: inject pre-populated agent registry (remote agents from config).
        if let Some(registry) = self.agent_registry {
            builder = builder.with_agent_registry(registry);
        }

        if let Some(assume_mutating) = self.assume_mutating {
            builder = builder.with_assume_mutating(assume_mutating);
        }
        if let Some(mutating_tools) = self.mutating_tools {
            builder = builder.with_mutating_tools(mutating_tools);
        }

        builder.add_observer(backend.event_observer());

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
            // Use BuiltInAndProvider when MCP servers are also configured so
            // that MCP tool definitions reach the LLM.  With BuiltInOnly the
            // collect_tools() gate strips them out entirely.
            let policy = if !self.mcp_servers.is_empty() {
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

        if let Some(exec) = self.execution {
            use crate::config::RuntimeExecutionPolicy;
            builder = builder.with_execution_policy(RuntimeExecutionPolicy::from(&exec));
            match exec.snapshot.backend.as_str() {
                "git" => {
                    use crate::snapshot::git::GitSnapshotBackend;
                    builder = builder.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
                }
                "none" | "" => {}
                other => {
                    log::warn!("Unknown snapshot backend '{}', ignoring", other);
                }
            }
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

        // Build final AgentConfig with all middleware appended
        let final_config = Arc::new(crate::agent::AgentConfig {
            provider: initial_config.provider.clone(),
            event_bus: initial_config.event_bus.clone(),
            agent_registry: initial_config.agent_registry.clone(),
            workspace_manager_actor: initial_config.workspace_manager_actor.clone(),
            default_mode: initial_config.default_mode.clone(),
            tool_config: initial_config.tool_config.clone(),
            tool_registry: initial_config.tool_registry.clone(),
            middleware_drivers,
            auth_methods: initial_config.auth_methods.clone(),
            max_steps: initial_config.max_steps,
            snapshot_policy: initial_config.snapshot_policy,
            assume_mutating: initial_config.assume_mutating,
            mutating_tools: initial_config.mutating_tools.clone(),
            max_prompt_bytes: initial_config.max_prompt_bytes,
            execution_timeout_secs: initial_config.execution_timeout_secs,
            delegation_wait_policy: initial_config.delegation_wait_policy.clone(),
            delegation_wait_timeout_secs: initial_config.delegation_wait_timeout_secs,
            delegation_cancel_grace_secs: initial_config.delegation_cancel_grace_secs,
            execution_policy: initial_config.execution_policy.clone(),
            compaction: initial_config.compaction.clone(),
            snapshot_backend: initial_config.snapshot_backend.clone(),
            snapshot_gc_config: initial_config.snapshot_gc_config.clone(),
            delegation_context_config: initial_config.delegation_context_config.clone(),
            pending_elicitations: initial_config.pending_elicitations.clone(),
            mcp_servers: initial_config.mcp_servers.clone(),
        });

        let handle = Arc::new(AgentHandle::from_config(final_config));

        Ok(Agent {
            inner: handle,
            storage: Arc::new(backend),
            default_session_id: Arc::new(Mutex::new(None)),
            cwd,
            callbacks: Arc::new(EventCallbacksState::new(None)),
        })
    }
}

pub struct Agent {
    pub(super) inner: Arc<AgentHandle>,
    #[cfg_attr(not(feature = "dashboard"), allow(dead_code))]
    pub(super) storage: Arc<dyn StorageBackend>,
    default_session_id: Arc<Mutex<Option<String>>>,
    pub(super) cwd: Option<PathBuf>,
    callbacks: Arc<EventCallbacksState>,
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

    pub async fn chat(&self, prompt: &str) -> Result<String> {
        let session_id = self.ensure_default_session().await?;
        self.chat_with_session(&session_id, prompt).await
    }

    pub async fn chat_session(&self) -> Result<AgentSession> {
        let session_id = self.create_session().await?;
        Ok(AgentSession::new(self.inner.clone(), session_id))
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

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        self.inner.subscribe_events()
    }

    pub fn on_tool_call<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
        self
    }

    pub fn on_tool_complete<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
        self
    }

    pub fn on_message<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
        self
    }

    pub fn on_delegation<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_delegation(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
        self
    }

    pub fn on_error<F>(&self, callback: F) -> &Self
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
        self
    }

    #[cfg(feature = "dashboard")]
    pub fn dashboard(&self) -> AgentServer {
        AgentServer::new(self.inner.clone(), self.storage.clone(), self.cwd.clone())
    }

    /// Start an ACP server with the specified transport.
    ///
    /// # Transports
    /// - `"stdio"` - Use stdin/stdout for JSON-RPC communication (for subprocess spawning)
    /// - `"ws://host:port"` - Start a WebSocket server (not yet implemented, use .dashboard() instead)
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

    /// Build an Agent from a single agent config
    pub async fn from_single_config(config: SingleAgentConfig) -> Result<Self> {
        let builder = Self::builder_from_config(config, None)?;
        builder.build().await
    }

    /// Build an Agent from a single agent config, optionally injecting a pre-populated
    /// agent registry (Phase 7: for remote agents discovered from `[[remote_agents]]`).
    ///
    /// When `initial_registry` is `Some`, it is used as the agent registry instead of the
    /// default empty `DefaultAgentRegistry`.  When `mesh` is `Some`, the `MeshHandle` is
    /// stored on the resulting `AgentHandle` via `set_mesh()`. `mesh_auto_fallback`
    /// controls whether `provider_node = None` may resolve providers from mesh peers.
    #[cfg(feature = "remote")]
    pub async fn from_single_config_with_registry(
        config: SingleAgentConfig,
        initial_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
        mesh: Option<crate::agent::remote::MeshHandle>,
        mesh_auto_fallback: bool,
    ) -> Result<Self> {
        let builder = Self::builder_from_config(config, initial_registry)?;
        let agent = builder.build().await?;

        agent.inner.set_mesh_fallback(mesh_auto_fallback);

        if let Some(mesh) = mesh {
            agent.inner.set_mesh(mesh);
        }

        Ok(agent)
    }

    /// Shared helper: configure an `AgentBuilder` from a `SingleAgentConfig`.
    fn builder_from_config(
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
        if let Some(db) = config.agent.db {
            builder.db_path = Some(db);
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
        let session = Agent::chat_session(self).await?;
        Ok(Box::new(session))
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        Agent::subscribe(self)
    }

    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>) {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_delegation_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_delegation(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    #[cfg(feature = "dashboard")]
    fn dashboard(&self) -> AgentServer {
        Agent::dashboard(self)
    }
}

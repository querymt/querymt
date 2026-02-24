//! Multi-agent quorum implementation

use super::callbacks::EventCallbacksState;
use super::config::{AgentConfig, DelegateConfigBuilder, PlannerConfigBuilder};
use super::utils::{
    build_llm_config, default_registry, infer_required_capabilities, latest_assistant_message,
    to_absolute_path,
};
use crate::AgentQuorumBuilder;
use crate::agent::AgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::SnapshotPolicy;
use crate::agent::core::ToolPolicy;
use crate::config::{MiddlewareEntry, QuorumConfig, resolve_tools};
use crate::delegation::AgentInfo;

use crate::middleware::MIDDLEWARE_REGISTRY;
use crate::quorum::AgentQuorum;
use crate::runner::{ChatRunner, ChatSession};
use crate::send_agent::SendAgent;
#[cfg(feature = "dashboard")]
use crate::server::AgentServer;
use crate::session::backend::default_agent_db_path;
use crate::session::store::SessionStore;
use crate::session::{SqliteStorage, StorageBackend};
use crate::snapshot::GitSnapshotBackend;
use crate::tools::CapabilityRequirement;
use crate::tools::builtins::all_builtin_tools;
use agent_client_protocol::{ContentBlock, NewSessionRequest, PromptRequest, TextContent};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Builder for multi-agent quorum with closure-based configuration
pub struct QuorumBuilder {
    pub(super) cwd: Option<PathBuf>,
    pub(super) db_path: Option<PathBuf>,
    pub(super) planner_config: Option<AgentConfig>,
    pub(super) delegates: Vec<AgentConfig>,
    pub(super) delegation_enabled: bool,
    pub(super) verification_enabled: bool,
    pub(super) snapshot_policy: SnapshotPolicy,
    pub(super) delegation_summary_config: Option<crate::config::DelegationSummaryConfig>,
    pub(super) delegation_wait_policy: crate::config::DelegationWaitPolicy,
    pub(super) delegation_wait_timeout_secs: u64,
    pub(super) delegation_cancel_grace_secs: u64,
    pub(super) max_parallel_delegations: usize,
    /// Pre-built registry entries to merge before building (Phase 7: remote agents).
    ///
    /// When `Some`, the entries in this registry are merged with the local delegate agents
    /// before the planner is built, so the planner sees both local and remote agents as
    /// delegation targets.
    pub(super) initial_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
    /// Optional mesh handle to store on the planner's `AgentHandle` after build (Phase 7).
    #[cfg(feature = "remote")]
    pub(super) mesh: Option<crate::agent::remote::MeshHandle>,
    /// Controls whether provider lookup may fall back to mesh peers when
    /// `provider_node_id` is not explicitly set.
    #[cfg(feature = "remote")]
    pub(super) mesh_auto_fallback: bool,
}

impl Default for QuorumBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl QuorumBuilder {
    pub fn new() -> Self {
        Self {
            cwd: None,
            db_path: None,
            planner_config: None,
            delegates: Vec::new(),
            delegation_enabled: true,
            verification_enabled: false,
            snapshot_policy: SnapshotPolicy::None,
            delegation_summary_config: None,
            delegation_wait_policy: crate::config::DelegationWaitPolicy::default(),
            delegation_wait_timeout_secs: 120,
            delegation_cancel_grace_secs: 5,
            max_parallel_delegations: 5,
            initial_registry: None,
            #[cfg(feature = "remote")]
            mesh: None,
            #[cfg(feature = "remote")]
            mesh_auto_fallback: false,
        }
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn db(mut self, path: impl Into<PathBuf>) -> Self {
        self.db_path = Some(path.into());
        self
    }

    /// Configure the planner agent using a closure
    pub fn planner<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(PlannerConfigBuilder) -> PlannerConfigBuilder,
    {
        let builder = PlannerConfigBuilder::new();
        self.planner_config = Some(configure(builder).build());
        self
    }

    /// Add a delegate agent configured via closure
    pub fn delegate<F>(mut self, id: impl Into<String>, configure: F) -> Self
    where
        F: FnOnce(DelegateConfigBuilder) -> DelegateConfigBuilder,
    {
        let builder = DelegateConfigBuilder::new(id);
        self.delegates.push(configure(builder).build());
        self
    }

    pub fn with_delegation(mut self, enabled: bool) -> Self {
        self.delegation_enabled = enabled;
        self
    }

    pub fn with_verification(mut self, enabled: bool) -> Self {
        self.verification_enabled = enabled;
        self
    }

    pub fn with_snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.snapshot_policy = policy;
        self
    }

    pub fn with_defaults(mut self) -> Self {
        self.delegation_enabled = true;
        self.verification_enabled = true;
        self
    }

    pub async fn build(self) -> Result<Quorum> {
        let planner_config = self
            .planner_config
            .ok_or_else(|| anyhow!("Planner configuration is required"))?;

        // Convert cwd to absolute path if provided
        let cwd = self.cwd.map(to_absolute_path).transpose()?;

        // Capability validation
        let mut all_required = HashSet::new();
        all_required.extend(infer_required_capabilities(&planner_config.tools));
        for delegate in &self.delegates {
            all_required.extend(&delegate.required_capabilities);
        }

        if all_required.contains(&CapabilityRequirement::Filesystem) && cwd.is_none() {
            return Err(anyhow!(
                "Working directory required: one or more agents require filesystem access. Use .cwd() to set one."
            ));
        }

        let registry = Arc::new(default_registry().await?);

        let path = match self.db_path {
            Some(path) => path,
            None => default_agent_db_path()?,
        };
        let backend = Arc::new(SqliteStorage::connect(path).await?);
        let mut builder = AgentQuorumBuilder::from_backend(backend.clone());

        if let Some(cwd_path) = cwd.clone() {
            builder = builder.cwd(cwd_path);
        }

        // Phase 7: inject pre-registered remote agents into the quorum's delegate registry.
        if let Some(ref initial_reg) = self.initial_registry {
            for info in initial_reg.list_agents() {
                if let Some(instance) = initial_reg.get_agent_instance(&info.id) {
                    let handle = initial_reg.get_agent_handle(&info.id);
                    log::debug!(
                        "QuorumBuilder: pre-registering remote agent '{}' in quorum",
                        info.id
                    );
                    builder = builder.preregister_agent_with_handle(info, instance, handle);
                }
            }
        }

        for delegate in self.delegates {
            let agent_info = AgentInfo {
                id: delegate.id.clone(),
                name: delegate.id.clone(),
                description: delegate.description.clone().unwrap_or_default(),
                capabilities: delegate.capabilities.clone(),
                required_capabilities: delegate.required_capabilities.clone(),
                meta: None,
            };
            let llm_config = build_llm_config(&delegate)?;
            let tools = delegate.tools.clone();
            let middleware_entries = delegate.middleware.clone();
            let exec = delegate.execution.clone();
            let registry = registry.clone();
            let snapshot_policy_for_delegate = self.snapshot_policy;
            let delegation_wait_policy_for_delegate = self.delegation_wait_policy.clone();
            let delegation_wait_timeout_for_delegate = self.delegation_wait_timeout_secs;
            let delegation_cancel_grace_for_delegate = self.delegation_cancel_grace_secs;
            builder = builder.add_delegate_agent(agent_info, move |store, event_journal| {
                use crate::config::RuntimeExecutionPolicy;
                let mut b = AgentConfigBuilder::new(
                    registry.clone(),
                    store.clone(),
                    event_journal.clone(),
                    llm_config.clone(),
                )
                .with_tool_policy(ToolPolicy::BuiltInOnly)
                .with_snapshot_policy(snapshot_policy_for_delegate);

                if snapshot_policy_for_delegate != SnapshotPolicy::None {
                    b = b.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
                }

                if !tools.is_empty() {
                    b = b.with_allowed_tools(tools.clone());
                }

                let auto_compact = exec.compaction.auto;
                b = b
                    .with_execution_policy(RuntimeExecutionPolicy::from(&exec))
                    .with_delegation_wait_policy(delegation_wait_policy_for_delegate.clone())
                    .with_delegation_wait_timeout_secs(delegation_wait_timeout_for_delegate)
                    .with_delegation_cancel_grace_secs(delegation_cancel_grace_for_delegate);
                apply_middleware_from_config(&mut b, &middleware_entries, auto_compact);

                match exec.snapshot.backend.as_str() {
                    "git" => {
                        b = b.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
                    }
                    "none" | "" => {}
                    other => {
                        log::warn!(
                            "Unknown snapshot backend '{}' for delegate, ignoring",
                            other
                        );
                    }
                }

                let config = Arc::new(b.build());
                Arc::new(AgentHandle::from_config(config)) as Arc<dyn SendAgent>
            });
        }

        let planner_llm = build_llm_config(&planner_config)?;
        let planner_tools = planner_config.tools.clone();
        let planner_middleware = planner_config.middleware.clone();
        let planner_exec = planner_config.execution.clone();
        let registry_for_planner = registry.clone();
        let snapshot_policy_for_planner = self.snapshot_policy;
        let delegation_wait_policy_for_planner = self.delegation_wait_policy.clone();
        let delegation_wait_timeout_for_planner = self.delegation_wait_timeout_secs;
        let delegation_cancel_grace_for_planner = self.delegation_cancel_grace_secs;
        builder = builder.with_planner(move |store, event_journal, agent_registry| {
            use crate::config::RuntimeExecutionPolicy;
            let mut b = AgentConfigBuilder::new(
                registry_for_planner.clone(),
                store.clone(),
                event_journal.clone(),
                planner_llm.clone(),
            )
            .with_agent_registry(agent_registry)
            .with_snapshot_policy(snapshot_policy_for_planner);

            if snapshot_policy_for_planner != SnapshotPolicy::None {
                b = b.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
            }

            if !planner_tools.is_empty() {
                b = b
                    .with_tool_policy(ToolPolicy::BuiltInOnly)
                    .with_allowed_tools(planner_tools.clone());
            }

            let auto_compact = planner_exec.compaction.auto;
            b = b
                .with_execution_policy(RuntimeExecutionPolicy::from(&planner_exec))
                .with_delegation_wait_policy(delegation_wait_policy_for_planner.clone())
                .with_delegation_wait_timeout_secs(delegation_wait_timeout_for_planner)
                .with_delegation_cancel_grace_secs(delegation_cancel_grace_for_planner);
            apply_middleware_from_config(&mut b, &planner_middleware, auto_compact);

            match planner_exec.snapshot.backend.as_str() {
                "git" => {
                    b = b.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
                }
                "none" | "" => {}
                other => {
                    log::warn!("Unknown snapshot backend '{}' for planner, ignoring", other);
                }
            }

            let config = Arc::new(b.build());
            Arc::new(AgentHandle::from_config(config)) as Arc<dyn SendAgent>
        });

        builder = builder
            .with_delegation(self.delegation_enabled)
            .with_verification(self.verification_enabled)
            .with_wait_policy(self.delegation_wait_policy)
            .with_wait_timeout_secs(self.delegation_wait_timeout_secs)
            .with_cancel_grace_secs(self.delegation_cancel_grace_secs)
            .with_max_parallel_delegations(self.max_parallel_delegations);

        // Build delegation summarizer if configured
        if let Some(ref summary_config) = self.delegation_summary_config {
            if summary_config.enabled {
                match crate::delegation::DelegationSummarizer::from_config(
                    summary_config,
                    registry.clone(),
                )
                .await
                {
                    Ok(summarizer) => {
                        log::info!(
                            "Delegation summarizer enabled with provider: {}, model: {}",
                            summary_config.provider,
                            summary_config.model
                        );
                        builder = builder.with_delegation_summarizer(Some(Arc::new(summarizer)));
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to build delegation summarizer: {}. Proceeding without summary.",
                            e
                        );
                    }
                }
            } else {
                log::debug!("Delegation summarizer disabled in config");
            }
        }

        let quorum = builder.build().map_err(|e| anyhow!(e.to_string()))?;

        // Extract planner handle for dashboard use
        let planner = quorum.planner();
        let planner_handle = planner
            .as_any()
            .downcast_ref::<AgentHandle>()
            .ok_or_else(|| anyhow!("Planner is not an AgentHandle"))?;
        let planner_handle = Arc::new(AgentHandle::from_config(planner_handle.config.clone()));

        // Phase 7: apply mesh routing policy and store the mesh handle.
        #[cfg(feature = "remote")]
        planner_handle.set_mesh_fallback(self.mesh_auto_fallback);

        #[cfg(feature = "remote")]
        if let Some(mesh) = self.mesh {
            planner_handle.set_mesh(mesh);
        }

        Ok(Quorum {
            inner: quorum,
            storage: backend,
            planner_handle,
            planner_session_id: Arc::new(Mutex::new(None)),
            cwd,
            callbacks: Arc::new(EventCallbacksState::new(None)),
        })
    }
}

pub struct Quorum {
    inner: AgentQuorum,
    #[cfg_attr(not(feature = "dashboard"), allow(dead_code))]
    storage: Arc<dyn StorageBackend>,
    #[cfg_attr(not(feature = "dashboard"), allow(dead_code))]
    planner_handle: Arc<AgentHandle>,
    planner_session_id: Arc<Mutex<Option<String>>>,
    cwd: Option<PathBuf>,
    callbacks: Arc<EventCallbacksState>,
}

impl Quorum {
    pub async fn chat(&self, prompt: &str) -> Result<String> {
        let session_id = self.ensure_planner_session().await?;
        let request = PromptRequest::new(
            session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        let planner = self.inner.planner();
        planner
            .prompt(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let history = self
            .inner
            .store()
            .get_history(&session_id)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        latest_assistant_message(&history).ok_or_else(|| anyhow!("No assistant response found"))
    }

    pub fn inner(&self) -> &AgentQuorum {
        &self.inner
    }

    /// Access the planner's `AgentHandle` for advanced configuration.
    ///
    /// The handle provides access to the session registry, event bus, and agent config.
    /// Use this when you need to interact with sessions directly or integrate with
    /// the kameo mesh (e.g., bootstrapping `RemoteNodeManager`).
    pub fn handle(&self) -> Arc<crate::agent::AgentHandle> {
        self.planner_handle.clone()
    }

    pub fn planner(&self) -> Arc<dyn SendAgent> {
        self.inner.planner()
    }

    pub fn delegate(&self, id: &str) -> Option<Arc<dyn SendAgent>> {
        self.inner.delegate(id)
    }

    #[cfg(feature = "dashboard")]
    pub fn dashboard(&self) -> AgentServer {
        AgentServer::new(
            self.planner_handle.clone(),
            self.storage.clone(),
            self.cwd.clone(),
        )
    }

    /// Start an ACP server with the specified transport.
    ///
    /// # Arguments
    /// * `transport` - Either "stdio" for stdin/stdout, or "ip:port" for WebSocket
    ///
    /// # Example
    /// ```rust,no_run
    /// # use querymt_agent::prelude::*;
    /// # #[tokio::main]
    /// # async fn main() -> anyhow::Result<()> {
    /// let quorum = Agent::multi()
    ///     .planner(|p| p.provider("openai", "gpt-4"))
    ///     .build()
    ///     .await?;
    ///     
    /// quorum.acp("stdio").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn acp(&self, transport: &str) -> Result<()> {
        match transport {
            "stdio" => crate::acp::serve_stdio(self.planner_handle.clone())
                .await
                .map_err(|e| anyhow!("ACP stdio error: {}", e)),
            addr if addr.contains(':') => Err(anyhow!(
                "WebSocket ACP not yet implemented for Quorum. Use .dashboard().run(\"{}\") for web access.",
                addr
            )),
            _ => Err(anyhow!(
                "Invalid ACP transport '{}'. Use 'stdio' or 'ip:port' format.",
                transport
            )),
        }
    }

    async fn ensure_planner_session(&self) -> Result<String> {
        if let Some(existing) = self.planner_session_id.lock().unwrap().clone() {
            return Ok(existing);
        }
        let planner = self.inner.planner();
        let request = match &self.cwd {
            Some(cwd) => NewSessionRequest::new(cwd.clone()),
            None => NewSessionRequest::new(PathBuf::new()),
        };
        let response = planner
            .new_session(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let session_id = response.session_id.to_string();
        *self.planner_session_id.lock().unwrap() = Some(session_id.clone());
        Ok(session_id)
    }

    async fn create_new_planner_session(&self) -> Result<String> {
        let planner = self.inner.planner();
        let request = match &self.cwd {
            Some(cwd) => NewSessionRequest::new(cwd.clone()),
            None => NewSessionRequest::new(PathBuf::new()),
        };
        let response = planner
            .new_session(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(response.session_id.to_string())
    }

    /// Build a Quorum from a quorum config
    pub async fn from_quorum_config(config: QuorumConfig) -> Result<Self> {
        let builder = Self::builder_from_quorum_config(config, None)?;
        builder.build().await
    }

    /// Build a `Quorum` from a quorum config, optionally injecting a pre-populated
    /// agent registry and mesh handle (Phase 7: config-driven remote agents).
    ///
    /// When `initial_registry` is `Some`, the remote agent entries from the registry
    /// are pre-registered in the quorum's delegation registry *before* local delegates,
    /// so local delegates with the same ID take precedence.
    ///
    /// When `mesh` is `Some`, the `MeshHandle` is stored on the planner's `AgentHandle`
    /// via `set_mesh()` so that mesh-aware methods (`list_remote_nodes`,
    /// `create_remote_session`, etc.) work.
    #[cfg(feature = "remote")]
    pub async fn from_quorum_config_with_registry(
        config: QuorumConfig,
        initial_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
        mesh: Option<crate::agent::remote::MeshHandle>,
        mesh_auto_fallback: bool,
    ) -> Result<Self> {
        let mut builder = Self::builder_from_quorum_config(config, initial_registry)?;
        builder.mesh = mesh;
        builder.mesh_auto_fallback = mesh_auto_fallback;
        builder.build().await
    }

    /// Shared helper: configure a `QuorumBuilder` from a `QuorumConfig`.
    fn builder_from_quorum_config(
        config: QuorumConfig,
        initial_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
    ) -> Result<QuorumBuilder> {
        let mut builder = QuorumBuilder::new();

        if let Some(cwd) = config.quorum.cwd {
            builder = builder.cwd(cwd);
        }
        if let Some(db) = config.quorum.db {
            builder = builder.db(db);
        }

        builder.delegation_enabled = config.quorum.delegation;
        builder.verification_enabled = config.quorum.verification;
        builder.delegation_summary_config = config.quorum.delegation_summary;
        builder.delegation_wait_policy = config.quorum.delegation_wait_policy;
        builder.delegation_wait_timeout_secs = config.quorum.delegation_wait_timeout_secs;
        builder.delegation_cancel_grace_secs = config.quorum.delegation_cancel_grace_secs;
        builder.max_parallel_delegations = config.quorum.max_parallel_delegations;

        // Parse snapshot policy
        let snapshot_policy = parse_snapshot_policy(config.quorum.snapshot_policy)?;

        // Build the set of builtin tool names for validation
        let builtin_names: HashSet<String> = all_builtin_tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();

        // Configure planner with tool resolution
        let mut planner_config = AgentConfig::new("planner");
        let mut llm = querymt::LLMParams::new()
            .provider(config.planner.provider)
            .model(config.planner.model);
        for part in config.planner.system {
            if let crate::config::SystemPart::Inline(s) = part {
                llm = llm.system(s);
            }
        }
        if let Some(api_key) = config.planner.api_key {
            llm = llm.api_key(api_key);
        }
        if let Some(params) = config.planner.parameters {
            for (key, value) in params {
                llm = llm.parameter(key, value);
            }
        }
        planner_config.llm_config = Some(llm);

        // Resolve planner tools (validates builtin tools and prepares for MCP)
        let planner_resolved =
            resolve_tools(&config.planner.tools, &config.mcp, &[], &builtin_names)?;
        planner_config.tools = planner_resolved.builtins;

        // Note: MCP tools are not yet supported in the simple Quorum API.
        if !planner_resolved.mcp_servers.is_empty() {
            log::warn!(
                "MCP servers configured for planner, but MCP is not yet supported in Quorum. Only builtin tools will be available."
            );
        }

        planner_config.middleware = config.planner.middleware;
        planner_config.execution = config.planner.execution;

        builder.planner_config = Some(planner_config);

        // Configure delegates with tool resolution
        for delegate in config.delegates {
            let mut delegate_config = AgentConfig::new(delegate.id.clone());
            let mut llm = querymt::LLMParams::new()
                .provider(delegate.provider)
                .model(delegate.model);
            for part in delegate.system {
                if let crate::config::SystemPart::Inline(s) = part {
                    llm = llm.system(s);
                }
            }
            if let Some(api_key) = delegate.api_key {
                llm = llm.api_key(api_key);
            }
            if let Some(params) = delegate.parameters {
                for (key, value) in params {
                    llm = llm.parameter(key, value);
                }
            }
            delegate_config.llm_config = Some(llm);
            delegate_config.description = delegate.description;
            delegate_config.capabilities = delegate.capabilities;

            let delegate_resolved =
                resolve_tools(&delegate.tools, &config.mcp, &delegate.mcp, &builtin_names)?;
            delegate_config.tools = delegate_resolved.builtins;

            if !delegate_resolved.mcp_servers.is_empty() {
                log::warn!(
                    "MCP servers configured for delegate '{}', but MCP is not yet supported in Quorum. Only builtin tools will be available.",
                    delegate.id
                );
            }

            delegate_config.required_capabilities =
                infer_required_capabilities(&delegate_config.tools)
                    .into_iter()
                    .collect();

            delegate_config.middleware = delegate.middleware;
            delegate_config.execution = delegate.execution;

            builder.delegates.push(delegate_config);
        }

        builder.snapshot_policy = snapshot_policy;
        builder.initial_registry = initial_registry;

        Ok(builder)
    }
}

#[async_trait]
impl ChatRunner for Quorum {
    async fn chat(&self, prompt: &str) -> Result<String> {
        Quorum::chat(self, prompt).await
    }

    async fn chat_session(&self) -> Result<Box<dyn ChatSession>> {
        let session_id = self.create_new_planner_session().await?;
        let session = QuorumSession::new(
            self.inner.planner(),
            self.inner.event_fanout(),
            self.inner.store(),
            session_id,
            self.cwd.clone(),
        );
        Ok(Box::new(session))
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::events::EventEnvelope> {
        self.inner.subscribe_events()
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
        Quorum::dashboard(self)
    }
}

/// A session for interacting with a Quorum's planner agent
pub struct QuorumSession {
    planner: Arc<dyn SendAgent>,
    session_id: String,
    callbacks: Arc<EventCallbacksState>,
    event_fanout: Arc<crate::event_fanout::EventFanout>,
    store: Arc<dyn SessionStore>,
    #[allow(dead_code)]
    cwd: Option<PathBuf>,
}

impl QuorumSession {
    fn new(
        planner: Arc<dyn SendAgent>,
        event_fanout: Arc<crate::event_fanout::EventFanout>,
        store: Arc<dyn SessionStore>,
        session_id: String,
        cwd: Option<PathBuf>,
    ) -> Self {
        let callbacks = Arc::new(EventCallbacksState::new(Some(session_id.clone())));
        Self {
            planner,
            session_id,
            callbacks,
            event_fanout,
            store,
            cwd,
        }
    }
}

#[async_trait]
impl ChatSession for QuorumSession {
    fn id(&self) -> &str {
        &self.session_id
    }

    async fn chat(&self, prompt: &str) -> Result<String> {
        let request = PromptRequest::new(
            self.session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        self.planner
            .prompt(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let history = self
            .store
            .get_history(&self.session_id)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        latest_assistant_message(&history).ok_or_else(|| anyhow!("No assistant response found"))
    }

    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>) {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.event_fanout.subscribe());
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.event_fanout.subscribe());
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.event_fanout.subscribe());
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.event_fanout.subscribe());
    }
}

/// Helper to parse snapshot policy string to enum
fn parse_snapshot_policy(policy: Option<String>) -> Result<SnapshotPolicy> {
    match policy.as_deref() {
        None => Ok(SnapshotPolicy::None),
        Some("none") => Ok(SnapshotPolicy::None),
        Some("metadata") => Ok(SnapshotPolicy::Metadata),
        Some("diff") => Ok(SnapshotPolicy::Diff),
        Some(other) => Err(anyhow!(
            "Invalid snapshot_policy '{}'. Valid options: 'none', 'metadata', 'diff'",
            other
        )),
    }
}

/// Helper to apply middleware from config entries to a builder.
///
/// `factory_config` is a snapshot of the config built so far (before middleware
/// are appended). Middleware factories only need `compaction_config` and
/// `event_journal` from it.
fn apply_middleware_from_config(
    builder: &mut AgentConfigBuilder,
    entries: &[MiddlewareEntry],
    auto_compact: bool,
) {
    // Build a lightweight AgentConfig snapshot that factory closures can read.
    // Middleware factories only consult compaction_config and event_journal.
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::config::RuntimeExecutionPolicy;
    use std::sync::Arc;

    let ephemeral_policy = RuntimeExecutionPolicy {
        compaction: builder.compaction_config().clone(),
        ..Default::default()
    };

    let factory_config = Arc::new(
        AgentConfigBuilder::from_provider(builder.provider().clone(), builder.event_journal())
            .with_agent_registry(builder.agent_registry())
            .with_execution_policy(ephemeral_policy)
            .build(),
    );

    for entry in entries {
        match MIDDLEWARE_REGISTRY.create(&entry.middleware_type, &entry.config, &factory_config) {
            Ok(middleware) => {
                builder.push_middleware(middleware);
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("disabled") {
                    log::warn!(
                        "Failed to create middleware '{}': {}",
                        entry.middleware_type,
                        e
                    );
                }
            }
        }
    }

    if auto_compact {
        log::info!("Auto-enabling ContextMiddleware for compaction");
        builder.push_middleware(Arc::new(crate::middleware::ContextMiddleware::new(
            crate::middleware::ContextConfig::default().auto_compact(true),
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::{AgentActorHandle, AgentInfo, DefaultAgentRegistry};
    use crate::test_utils::empty_plugin_registry;
    use querymt::LLMParams;
    use std::sync::Arc;

    #[test]
    fn test_quorum_delegate_builder_system() {
        let builder = QuorumBuilder::new()
            .cwd(PathBuf::from("/tmp"))
            .planner(|p| {
                p.provider("openai", "gpt-4")
                    .system("Planner system prompt")
                    .tools(["delegate"])
            })
            .delegate("coder", |d| {
                d.provider("ollama", "model")
                    .system("Coder system prompt")
                    .tools(["shell"])
            });

        // Test that the delegate was added with correct system prompt
        assert_eq!(builder.delegates.len(), 1);
        let delegate = &builder.delegates[0];
        assert_eq!(
            delegate.llm_config.as_ref().map(|c| c.system.clone()),
            Some(vec!["Coder system prompt".to_string()])
        );
    }

    #[test]
    fn test_parse_snapshot_policy() {
        assert_eq!(parse_snapshot_policy(None).unwrap(), SnapshotPolicy::None);
        assert_eq!(
            parse_snapshot_policy(Some("none".to_string())).unwrap(),
            SnapshotPolicy::None
        );
        assert_eq!(
            parse_snapshot_policy(Some("metadata".to_string())).unwrap(),
            SnapshotPolicy::Metadata
        );
        assert_eq!(
            parse_snapshot_policy(Some("diff".to_string())).unwrap(),
            SnapshotPolicy::Diff
        );
        assert!(parse_snapshot_policy(Some("invalid".to_string())).is_err());
    }

    #[test]
    fn test_quorum_builder_snapshot_policy() {
        let builder = QuorumBuilder::new().with_snapshot_policy(SnapshotPolicy::Diff);
        assert_eq!(builder.snapshot_policy, SnapshotPolicy::Diff);
    }

    #[tokio::test]
    async fn test_quorum_build_preserves_initial_registry_handles() {
        let (plugin_registry, _temp_dir) = empty_plugin_registry().unwrap();
        let plugin_registry = Arc::new(plugin_registry);

        let delegate_storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let delegate_config = Arc::new(
            AgentConfigBuilder::new(
                plugin_registry,
                delegate_storage.session_store(),
                delegate_storage.event_journal(),
                LLMParams::new().provider("mock").model("mock-model"),
            )
            .build(),
        );
        let delegate_handle = Arc::new(AgentHandle::from_config(delegate_config));

        let mut initial_registry = DefaultAgentRegistry::new();
        initial_registry.register_with_handle(
            AgentInfo {
                id: "remote-coder".to_string(),
                name: "Remote Coder".to_string(),
                description: "Remote coding agent".to_string(),
                capabilities: vec!["coding".to_string()],
                required_capabilities: vec![],
                meta: None,
            },
            delegate_handle.clone() as Arc<dyn SendAgent>,
            AgentActorHandle::Local {
                config: delegate_handle.config.clone(),
                registry: delegate_handle.registry.clone(),
            },
        );

        let mut builder = QuorumBuilder::new().planner(|p| p.provider("mock", "mock-model"));
        builder.initial_registry = Some(Arc::new(initial_registry));

        let quorum = builder.build().await.unwrap();

        assert!(
            quorum
                .inner()
                .registry()
                .get_agent_handle("remote-coder")
                .is_some()
        );
    }
}

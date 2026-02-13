//! Multi-agent quorum implementation

use super::callbacks::EventCallbacksState;
use super::config::{AgentConfig, DelegateConfigBuilder, PlannerConfigBuilder};
use super::utils::{
    build_llm_config, default_registry, infer_required_capabilities, latest_assistant_message,
    to_absolute_path,
};
use crate::agent::AgentHandle;
use crate::agent::builder::AgentBuilderExt;
use crate::agent::core::{QueryMTAgent, SnapshotPolicy, ToolPolicy};
use crate::config::{MiddlewareEntry, QuorumConfig, resolve_tools};
use crate::delegation::AgentInfo;
use crate::event_bus::EventBus;
use crate::events::AgentEvent;
use crate::middleware::MIDDLEWARE_REGISTRY;
use crate::quorum::AgentQuorum;
use crate::runner::{ChatRunner, ChatSession};
use crate::send_agent::SendAgent;
#[cfg(feature = "dashboard")]
use crate::server::AgentServer;
use crate::session::projection::ViewStore;
use crate::session::store::SessionStore;
use crate::snapshot::GitSnapshotBackend;
use crate::tools::CapabilityRequirement;
use crate::tools::builtins::all_builtin_tools;
use agent_client_protocol::{ContentBlock, NewSessionRequest, PromptRequest, TextContent};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Mutex as StdMutex};

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

        let mut builder = AgentQuorum::builder(self.db_path)
            .await
            .map_err(|e| anyhow!(format!("Failed to build quorum: {e}")))?;

        if let Some(cwd_path) = cwd.clone() {
            builder = builder.cwd(cwd_path);
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
            let tool_output_config = delegate.tool_output.clone();
            let pruning_config = delegate.pruning.clone();
            let compaction_config = delegate.compaction.clone();
            let snapshot_backend_config = delegate.snapshot.clone();
            let rate_limit_config = delegate.rate_limit.clone();
            let registry = registry.clone();
            let snapshot_policy_for_delegate = self.snapshot_policy;
            let _cwd_for_delegate = cwd.clone();
            builder = builder.add_delegate_agent(agent_info, move |store, event_bus| {
                let mut agent =
                    QueryMTAgent::new(registry.clone(), store.clone(), llm_config.clone())
                        .with_event_bus(event_bus.clone())
                        .with_tool_policy(ToolPolicy::BuiltInOnly)
                        .with_snapshot_policy(snapshot_policy_for_delegate);

                // Set snapshot backend if snapshot policy is enabled
                if snapshot_policy_for_delegate != SnapshotPolicy::None {
                    agent = agent.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
                }

                if !tools.is_empty() {
                    agent = agent.with_allowed_tools(tools.clone());
                }

                // Set compaction config BEFORE applying middleware so factory can read it
                agent = agent
                    .with_tool_output_config(tool_output_config)
                    .with_pruning_config(pruning_config)
                    .with_compaction_config(compaction_config.clone())
                    .with_rate_limit_config(rate_limit_config);

                // Apply middleware from config (after compaction_config is set)
                apply_middleware_from_config(
                    &mut agent,
                    &middleware_entries,
                    compaction_config.auto,
                );

                // Handle snapshot backend from config (can override the policy-based one above)
                match snapshot_backend_config.backend.as_str() {
                    "git" => {
                        agent = agent.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
                    }
                    "none" | "" => {
                        // Explicitly disable snapshot backend or use default
                    }
                    other => {
                        log::warn!(
                            "Unknown snapshot backend '{}' for delegate, ignoring",
                            other
                        );
                    }
                }

                // Convert QueryMTAgent to AgentHandle
                let config = agent.agent_config();
                let registry = agent.kameo_registry();
                let handle = AgentHandle {
                    config,
                    registry,
                    client_state: agent.client_state.clone(),
                    client: agent.client.clone(),
                    bridge: agent.bridge.clone(),
                    default_mode: StdMutex::new(
                        agent
                            .default_mode
                            .lock()
                            .map(|m| *m)
                            .unwrap_or(crate::agent::core::AgentMode::Build),
                    ),
                };
                Arc::new(handle) as Arc<dyn SendAgent>
            });
        }

        let planner_llm = build_llm_config(&planner_config)?;
        let planner_tools = planner_config.tools.clone();
        let planner_middleware = planner_config.middleware.clone();
        let planner_tool_output = planner_config.tool_output.clone();
        let planner_pruning = planner_config.pruning.clone();
        let planner_compaction = planner_config.compaction.clone();
        let planner_snapshot = planner_config.snapshot.clone();
        let planner_rate_limit = planner_config.rate_limit.clone();
        let registry_for_planner = registry.clone();
        let snapshot_policy_for_planner = self.snapshot_policy;
        let _cwd_for_planner = cwd.clone();
        builder = builder.with_planner(move |store, event_bus, agent_registry| {
            let mut agent = QueryMTAgent::new(
                registry_for_planner.clone(),
                store.clone(),
                planner_llm.clone(),
            )
            .with_event_bus(event_bus.clone())
            .with_agent_registry(agent_registry)
            .with_snapshot_policy(snapshot_policy_for_planner);

            // Set snapshot root and backend if snapshot policy is enabled and cwd is available
            // Set snapshot backend if snapshot policy is enabled
            if snapshot_policy_for_planner != SnapshotPolicy::None {
                agent = agent.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
            }

            if !planner_tools.is_empty() {
                agent = agent
                    .with_tool_policy(ToolPolicy::BuiltInOnly)
                    .with_allowed_tools(planner_tools.clone());
            }

            // Set compaction config BEFORE applying middleware so factory can read it
            agent = agent
                .with_tool_output_config(planner_tool_output)
                .with_pruning_config(planner_pruning)
                .with_compaction_config(planner_compaction.clone())
                .with_rate_limit_config(planner_rate_limit);

            // Apply middleware from config (after compaction_config is set)
            apply_middleware_from_config(&mut agent, &planner_middleware, planner_compaction.auto);

            // Handle snapshot backend from config (can override the policy-based one above)
            match planner_snapshot.backend.as_str() {
                "git" => {
                    agent = agent.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
                }
                "none" | "" => {
                    // Explicitly disable snapshot backend or use default
                }
                other => {
                    log::warn!("Unknown snapshot backend '{}' for planner, ignoring", other);
                }
            }

            // Convert QueryMTAgent to AgentHandle
            let config = agent.agent_config();
            let registry = agent.kameo_registry();
            let handle = AgentHandle {
                config,
                registry,
                client_state: agent.client_state.clone(),
                client: agent.client.clone(),
                bridge: agent.bridge.clone(),
                default_mode: StdMutex::new(
                    agent
                        .default_mode
                        .lock()
                        .map(|m| *m)
                        .unwrap_or(crate::agent::core::AgentMode::Build),
                ),
            };
            Arc::new(handle) as Arc<dyn SendAgent>
        });

        builder = builder
            .with_delegation(self.delegation_enabled)
            .with_verification(self.verification_enabled);

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
        let view_store = quorum.view_store();

        // Extract planner handle for dashboard use
        let planner = quorum.planner();
        let planner_handle = planner
            .as_any()
            .downcast_ref::<AgentHandle>()
            .ok_or_else(|| anyhow!("Planner is not an AgentHandle"))?;
        let planner_handle = Arc::new(AgentHandle {
            config: planner_handle.config.clone(),
            registry: planner_handle.registry.clone(),
            client_state: planner_handle.client_state.clone(),
            client: planner_handle.client.clone(),
            bridge: planner_handle.bridge.clone(),
            default_mode: StdMutex::new(
                planner_handle
                    .default_mode
                    .lock()
                    .map(|m| *m)
                    .unwrap_or(crate::agent::core::AgentMode::Build),
            ),
        });

        Ok(Quorum {
            inner: quorum,
            view_store,
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
    view_store: Arc<dyn ViewStore>,
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
            self.view_store.clone(),
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
        // MCP servers would need to be started when sessions are created,
        // similar to how it's done in protocol.rs. For now, we only validate
        // and extract builtin tools.
        if !planner_resolved.mcp_servers.is_empty() {
            log::warn!(
                "MCP servers configured for planner, but MCP is not yet supported in Quorum. Only builtin tools will be available."
            );
        }

        // Copy middleware config for planner
        planner_config.middleware = config.planner.middleware;

        // Copy compaction/pruning/tool_output/snapshot/rate_limit configs for planner
        planner_config.tool_output = config.planner.tool_output;
        planner_config.pruning = config.planner.pruning;
        planner_config.compaction = config.planner.compaction;
        planner_config.snapshot = config.planner.snapshot;
        planner_config.rate_limit = config.planner.rate_limit;

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

            // Resolve delegate tools
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

            // Copy middleware config for this delegate
            delegate_config.middleware = delegate.middleware;

            // Copy compaction/pruning/tool_output/snapshot/rate_limit configs for this delegate
            delegate_config.tool_output = delegate.tool_output;
            delegate_config.pruning = delegate.pruning;
            delegate_config.compaction = delegate.compaction;
            delegate_config.snapshot = delegate.snapshot;
            delegate_config.rate_limit = delegate.rate_limit;

            builder.delegates.push(delegate_config);
        }

        builder.snapshot_policy = snapshot_policy;

        builder.build().await
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
            self.inner.event_bus(),
            self.inner.store(),
            session_id,
            self.cwd.clone(),
        );
        Ok(Box::new(session))
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        self.inner.event_bus().subscribe()
    }

    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>) {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.inner.event_bus().subscribe());
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.inner.event_bus().subscribe());
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.inner.event_bus().subscribe());
    }

    fn on_delegation_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_delegation(callback);
        self.callbacks
            .ensure_listener(self.inner.event_bus().subscribe());
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.inner.event_bus().subscribe());
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
    event_bus: Arc<EventBus>,
    store: Arc<dyn SessionStore>,
    #[allow(dead_code)]
    cwd: Option<PathBuf>,
}

impl QuorumSession {
    fn new(
        planner: Arc<dyn SendAgent>,
        event_bus: Arc<EventBus>,
        store: Arc<dyn SessionStore>,
        session_id: String,
        cwd: Option<PathBuf>,
    ) -> Self {
        let callbacks = Arc::new(EventCallbacksState::new(Some(session_id.clone())));
        Self {
            planner,
            session_id,
            callbacks,
            event_bus,
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
        self.callbacks.ensure_listener(self.event_bus.subscribe());
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks.ensure_listener(self.event_bus.subscribe());
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks.ensure_listener(self.event_bus.subscribe());
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks.ensure_listener(self.event_bus.subscribe());
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

/// Helper to apply middleware from config entries to an agent
fn apply_middleware_from_config(
    agent: &mut QueryMTAgent,
    entries: &[MiddlewareEntry],
    auto_compact: bool,
) {
    // Build a temporary AgentConfig for middleware factories
    let factory_config = agent.agent_config();
    for entry in entries {
        match MIDDLEWARE_REGISTRY.create(&entry.middleware_type, &entry.config, &factory_config) {
            Ok(middleware) => {
                agent.middleware_drivers.lock().unwrap().push(middleware);
            }
            Err(e) => {
                // Skip if middleware is disabled, otherwise log warning
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

    // Auto-add ContextMiddleware if compaction.auto is true and user didn't provide one
    if auto_compact {
        let mut drivers = agent.middleware_drivers.lock().unwrap();
        let already_has = drivers.iter().any(|d| d.name() == "ContextMiddleware");
        if !already_has {
            log::info!("Auto-enabling ContextMiddleware for compaction");
            let context_middleware = crate::middleware::ContextMiddleware::new(
                crate::middleware::ContextConfig::default().auto_compact(true),
            );
            drivers.push(Arc::new(context_middleware));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

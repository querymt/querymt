//! Core agent structures and basic implementations

use crate::acp::client_bridge::ClientBridgeSender;
use crate::config::{CompactionConfig, PruningConfig, ToolOutputConfig};
use crate::delegation::{AgentRegistry, DefaultAgentRegistry};
use crate::event_bus::EventBus;
use crate::events::AgentEvent;
use crate::index::{WorkspaceIndexManager, WorkspaceIndexManagerConfig};
use crate::middleware::{CompositeDriver, MiddlewareDriver};
use crate::session::compaction::SessionCompaction;
use crate::session::provider::SessionProvider;
use crate::session::store::{LLMConfig, SessionStore};
use crate::tools::ToolRegistry;
use agent_client_protocol::{
    AuthMethod, ClientCapabilities, Error, Implementation, ProtocolVersion, SessionId,
    SessionNotification, SessionUpdate,
};
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, OnceCell, broadcast, watch};

/// Runtime operating mode for the agent.
/// Modes control what the agent is allowed to do and what system reminders are injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AgentMode {
    /// Full read/write mode - all tools available, no restrictions
    Build = 0,
    /// Planning mode - read-only, agent observes and plans without making changes
    Plan = 1,
    /// Review mode - read-only, agent reviews code and provides feedback
    Review = 2,
}

impl AgentMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => AgentMode::Plan,
            2 => AgentMode::Review,
            _ => AgentMode::Build,
        }
    }

    /// Cycle to next mode: Build -> Plan -> Review -> Build
    pub fn next(self) -> Self {
        match self {
            AgentMode::Build => AgentMode::Plan,
            AgentMode::Plan => AgentMode::Review,
            AgentMode::Review => AgentMode::Build,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AgentMode::Build => "build",
            AgentMode::Plan => "plan",
            AgentMode::Review => "review",
        }
    }

    /// Whether this mode is read-only (agent should not make file changes)
    pub fn is_read_only(&self) -> bool {
        matches!(self, AgentMode::Plan | AgentMode::Review)
    }
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for AgentMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "build" => Ok(AgentMode::Build),
            "plan" => Ok(AgentMode::Plan),
            "review" => Ok(AgentMode::Review),
            _ => Err(format!(
                "unknown agent mode: '{}'. Valid modes: build, plan, review",
                s
            )),
        }
    }
}

/// Main agent implementation that coordinates LLM interactions, tool execution,
/// session management, and protocol compliance.
pub struct QueryMTAgent {
    pub(crate) provider: Arc<SessionProvider>,
    pub(crate) active_sessions: Arc<Mutex<HashMap<String, watch::Sender<bool>>>>,
    pub(crate) session_runtime: Arc<Mutex<HashMap<String, Arc<SessionRuntime>>>>,
    pub(crate) max_steps: Option<usize>,
    pub(crate) snapshot_policy: SnapshotPolicy,
    pub(crate) assume_mutating: bool,
    pub(crate) mutating_tools: HashSet<String>,
    pub(crate) max_prompt_bytes: Option<usize>,
    pub(crate) tool_config: Arc<StdMutex<ToolConfig>>,
    pub(crate) tool_registry: Arc<StdMutex<ToolRegistry>>,
    pub(crate) middleware_drivers: Arc<std::sync::Mutex<Vec<Arc<dyn MiddlewareDriver>>>>,
    pub(crate) agent_mode: Arc<std::sync::atomic::AtomicU8>,
    pub(crate) event_bus: Arc<EventBus>,
    pub(crate) client_state: Arc<StdMutex<Option<ClientState>>>,
    pub(crate) auth_methods: Arc<StdMutex<Vec<AuthMethod>>>,
    pub(crate) client: Arc<StdMutex<Option<Arc<dyn agent_client_protocol::Client + Send + Sync>>>>,
    pub(crate) bridge: Arc<StdMutex<Option<ClientBridgeSender>>>,
    pub(crate) agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
    pub(crate) delegation_context_config: DelegationContextConfig,
    pub(crate) workspace_index_manager: Arc<WorkspaceIndexManager>,

    // Compaction system (3-layer)
    /// Tool output truncation configuration (Layer 1)
    pub(crate) tool_output_config: ToolOutputConfig,
    /// Pruning configuration (Layer 2) - runs after every turn
    pub(crate) pruning_config: PruningConfig,
    /// AI compaction configuration (Layer 3) - runs on context overflow
    pub(crate) compaction_config: CompactionConfig,
    /// Session compaction service for AI summaries
    pub(crate) compaction: SessionCompaction,

    // Rate limiting
    /// Rate limit retry configuration
    pub(crate) rate_limit_config: crate::config::RateLimitConfig,

    // Snapshot system for undo/redo
    /// Snapshot backend implementation (e.g., git-based)
    pub(crate) snapshot_backend: Option<Arc<dyn crate::snapshot::SnapshotBackend>>,
    /// GC configuration for snapshot cleanup
    pub(crate) snapshot_gc_config: crate::snapshot::GcConfig,

    // Question handling for interactive tools
    /// Pending elicitation requests from tools and MCP servers (elicitation_id -> response sender)
    pub(crate) pending_elicitations: crate::elicitation::PendingElicitationMap,
}

/// Configuration for when and how delegation context is injected into conversations.
#[derive(Debug, Clone)]
pub struct DelegationContextConfig {
    pub timing: DelegationContextTiming,
    pub auto_inject: bool,
}

/// Timing options for delegation context injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationContextTiming {
    FirstTurnOnly,
    EveryTurn,
    Disabled,
}

/// Client state for protocol compliance.
#[derive(Clone)]
pub struct ClientState {
    pub protocol_version: ProtocolVersion,
    pub client_capabilities: ClientCapabilities,
    pub client_info: Option<Implementation>,
    pub authenticated: bool,
}

/// Runtime state for an active session.
pub struct SessionRuntime {
    pub cwd: Option<std::path::PathBuf>,
    pub _mcp_services: HashMap<
        String,
        rmcp::service::RunningService<rmcp::RoleClient, crate::elicitation::ElicitationHandler>,
    >,
    pub mcp_tools: HashMap<String, Arc<querymt::mcp::adapter::McpToolAdapter>>,
    pub mcp_tool_defs: Vec<querymt::chat::Tool>,
    pub permission_cache: StdMutex<HashMap<String, bool>>,
    /// Hash of currently available tools (for change detection)
    pub current_tools_hash: StdMutex<Option<crate::hash::RapidHash>>,
    /// Function index for duplicate code detection (built asynchronously on session start)
    pub function_index: Arc<OnceCell<Arc<tokio::sync::RwLock<crate::index::FunctionIndex>>>>,
    /// Turn snapshot: (turn_id, snapshot_id) taken at the start of the turn for undo/redo
    pub turn_snapshot: StdMutex<Option<(String, String)>>,
    /// Accumulated changed file paths across the entire turn (for end-of-turn dedup check)
    pub turn_diffs: StdMutex<crate::index::DiffPaths>,
}

/// Policy for tool usage and availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolPolicy {
    BuiltInOnly,
    ProviderOnly,
    #[default]
    BuiltInAndProvider,
}

/// Configuration for tool access control.
#[derive(Clone, Default)]
pub struct ToolConfig {
    pub policy: ToolPolicy,
    pub allowlist: Option<HashSet<String>>,
    pub denylist: HashSet<String>,
}

/// Policy for filesystem snapshotting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotPolicy {
    None,
    Metadata,
    Diff,
}

impl std::fmt::Display for SnapshotPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            SnapshotPolicy::None => "none",
            SnapshotPolicy::Metadata => "metadata",
            SnapshotPolicy::Diff => "diff",
        };
        write!(f, "{}", value)
    }
}

impl QueryMTAgent {
    /// Creates a new agent instance with the specified plugin registry and session store.
    pub fn new(
        plugin_registry: Arc<PluginRegistry>,
        store: Arc<dyn SessionStore>,
        initial_config: LLMParams,
    ) -> Self {
        let session_provider =
            Arc::new(SessionProvider::new(plugin_registry, store, initial_config));
        let mut tool_registry = ToolRegistry::new();

        // Register built-in tools
        // File operations
        tool_registry.add(Arc::new(crate::tools::builtins::ReadFileTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::WriteFileTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::DeleteFileTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::EditTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::MultiEditTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::ApplyPatchTool::new()));

        // Search and navigation
        tool_registry.add(Arc::new(crate::tools::builtins::SearchTextTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::GlobTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::ListTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::MdqTool::new()));

        // Execution and external
        tool_registry.add(Arc::new(crate::tools::builtins::ShellTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::WebFetchTool::new()));

        // Task management
        tool_registry.add(Arc::new(crate::tools::builtins::CreateTaskTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::DelegateTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::TodoWriteTool::new()));
        tool_registry.add(Arc::new(crate::tools::builtins::TodoReadTool::new()));

        // User interaction
        tool_registry.add(Arc::new(crate::tools::builtins::QuestionTool::new()));

        Self {
            provider: session_provider,
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
            session_runtime: Arc::new(Mutex::new(HashMap::new())),
            max_steps: None,
            snapshot_policy: SnapshotPolicy::None,
            assume_mutating: true,
            mutating_tools: HashSet::new(),
            max_prompt_bytes: None,
            tool_config: Arc::new(StdMutex::new(ToolConfig::default())),
            tool_registry: Arc::new(StdMutex::new(tool_registry)),
            middleware_drivers: Arc::new(std::sync::Mutex::new(Vec::new())),
            agent_mode: Arc::new(std::sync::atomic::AtomicU8::new(AgentMode::Build as u8)),
            event_bus: Arc::new(EventBus::new()),
            client_state: Arc::new(StdMutex::new(None)),
            auth_methods: Arc::new(StdMutex::new(Vec::new())),
            client: Arc::new(StdMutex::new(None)),
            bridge: Arc::new(StdMutex::new(None)),
            agent_registry: Arc::new(DefaultAgentRegistry::new()),
            delegation_context_config: DelegationContextConfig {
                timing: DelegationContextTiming::FirstTurnOnly,
                auto_inject: true,
            },
            workspace_index_manager: Arc::new(WorkspaceIndexManager::new(
                WorkspaceIndexManagerConfig::default(),
            )),
            // Compaction system (3-layer) - all defaults
            tool_output_config: ToolOutputConfig::default(),
            pruning_config: PruningConfig::default(),
            compaction_config: CompactionConfig::default(),
            compaction: SessionCompaction::new(),
            // Rate limiting - default config
            rate_limit_config: crate::config::RateLimitConfig::default(),
            // Snapshot system - disabled by default
            snapshot_backend: None,
            snapshot_gc_config: crate::snapshot::GcConfig::default(),
            // Elicitation handling - empty by default
            pending_elicitations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn workspace_index_manager(&self) -> Arc<WorkspaceIndexManager> {
        self.workspace_index_manager.clone()
    }

    /// Creates a CompositeDriver from the configured middleware drivers
    pub(crate) fn create_driver(&self) -> CompositeDriver {
        use crate::middleware::{LimitsConfig, LimitsMiddleware};

        let mut drivers: Vec<Arc<dyn MiddlewareDriver>> = Vec::new();

        // Add LimitsMiddleware if configured
        if let Some(max_steps) = self.max_steps {
            drivers.push(Arc::new(LimitsMiddleware::new(
                LimitsConfig::default().max_steps(max_steps),
            )));
        }

        // Add all user-configured middleware drivers
        if let Ok(middleware_drivers) = self.middleware_drivers.lock() {
            for driver in middleware_drivers.iter() {
                drivers.push(driver.clone());
            }
        }

        CompositeDriver::new(drivers)
    }

    /// Returns the session limits from configured middleware
    pub fn get_session_limits(&self) -> Option<crate::events::SessionLimits> {
        self.create_driver().get_limits()
    }

    /// Builds delegation metadata for ACP AgentCapabilities._meta field
    pub(crate) fn build_delegation_meta(
        &self,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        let agents = self.agent_registry.list_agents();
        if agents.is_empty() {
            return None;
        }

        let delegation_value = serde_json::json!({
            "version": "1",
            "available": true,
            "agents": agents.iter().map(|agent| {
                serde_json::json!({
                    "id": agent.id,
                    "name": agent.name,
                    "description": agent.description,
                    "capabilities": agent.capabilities,
                })
            }).collect::<Vec<_>>()
        });

        let mut meta = serde_json::Map::new();
        meta.insert("mt.query.agent.delegation".to_string(), delegation_value);
        Some(meta)
    }

    /// Gets a snapshot of the current tool configuration.
    pub(crate) fn tool_config_snapshot(&self) -> ToolConfig {
        self.tool_config
            .lock()
            .map(|config| config.clone())
            .unwrap_or_default()
    }

    /// Checks if a tool requires permission for execution.
    pub(crate) fn requires_permission_for_tool(&self, tool_name: &str) -> bool {
        self.mutating_tools.contains(tool_name)
            || matches!(
                crate::agent::utils::tool_kind_for_tool(tool_name),
                agent_client_protocol::ToolKind::Edit
                    | agent_client_protocol::ToolKind::Delete
                    | agent_client_protocol::ToolKind::Execute
            )
    }

    /// Sets the agent operating mode.
    pub fn set_agent_mode(&self, mode: AgentMode) {
        self.agent_mode
            .store(mode as u8, std::sync::atomic::Ordering::Relaxed);
    }

    /// Gets the current agent mode.
    pub fn get_agent_mode(&self) -> AgentMode {
        AgentMode::from_u8(self.agent_mode.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Gets the agent mode atomic for sharing with middleware.
    pub fn agent_mode_flag(&self) -> Arc<std::sync::atomic::AtomicU8> {
        self.agent_mode.clone()
    }

    /// Subscribes to agent events.
    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_bus.subscribe()
    }

    /// Adds an event observer after agent creation.
    pub fn add_observer(&self, observer: Arc<dyn crate::events::EventObserver>) {
        self.event_bus.add_observer(observer);
    }

    /// Access the underlying event bus.
    pub fn event_bus(&self) -> Arc<EventBus> {
        self.event_bus.clone()
    }

    /// Access the session runtime map.
    ///
    /// This is primarily used by middleware that needs access to per-session state
    /// like the function index for duplicate code detection.
    pub fn session_runtime(&self) -> Arc<Mutex<HashMap<String, Arc<SessionRuntime>>>> {
        self.session_runtime.clone()
    }

    /// Access the agent registry.
    pub fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        self.agent_registry.clone()
    }

    /// Access the tool registry for built-in tool execution.
    pub fn tool_registry(&self) -> Arc<crate::tools::ToolRegistry> {
        let registry = self.tool_registry.lock().unwrap();
        Arc::new(registry.clone())
    }

    /// Access the pending elicitations map for resolving tool and MCP server elicitation requests.
    pub fn pending_elicitations(&self) -> crate::elicitation::PendingElicitationMap {
        self.pending_elicitations.clone()
    }

    /// Sets the client for protocol communication.
    pub fn set_client(&self, client: Arc<dyn agent_client_protocol::Client + Send + Sync>) {
        if let Ok(mut handle) = self.client.lock() {
            *handle = Some(client);
        }
    }

    /// Sets the client bridge for ACP stdio communication.
    ///
    /// This is used internally by the ACP stdio server to enable
    /// agent→client communication through the Send/!Send boundary.
    pub fn set_bridge(&self, bridge: ClientBridgeSender) {
        if let Ok(mut handle) = self.bridge.lock() {
            *handle = Some(bridge);
        }
    }

    /// Gets a clone of the bridge sender if available.
    ///
    /// Returns None if no bridge has been set (e.g., not running in ACP stdio mode).
    pub(crate) fn bridge(&self) -> Option<ClientBridgeSender> {
        self.bridge.lock().ok().and_then(|b| b.clone())
    }

    /// Emits an event for external observers.
    pub fn emit_event(&self, session_id: &str, kind: crate::events::AgentEventKind) {
        self.event_bus.publish(session_id, kind);
    }

    /// Switch provider and model for a session (simple form)
    pub async fn set_provider(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
    ) -> Result<(), Error> {
        // Preserve the system prompt when switching models
        let system_prompt = self.get_session_system_prompt(session_id).await;

        let mut config = LLMParams::new().provider(provider).model(model);

        // Add system prompt to config
        for prompt_part in system_prompt {
            config = config.system(prompt_part);
        }

        self.set_llm_config(session_id, config).await
    }

    /// Helper method to extract system prompt from current session config
    pub(crate) async fn get_session_system_prompt(&self, session_id: &str) -> Vec<String> {
        // Try to get the current session's LLM config
        if let Ok(Some(current_config)) = self
            .provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
        {
            // Try to extract system prompt from params JSON
            if let Some(params) = &current_config.params
                && let Some(system_array) = params.get("system").and_then(|v| v.as_array())
            {
                // Parse the array of strings
                let mut system_parts = Vec::new();
                for item in system_array {
                    if let Some(s) = item.as_str() {
                        system_parts.push(s.to_string());
                    }
                }
                if !system_parts.is_empty() {
                    return system_parts;
                }
            }
        }

        // Fall back to initial_config system prompt
        self.provider.initial_config().system.clone()
    }

    /// Switch provider configuration for a session (advanced form)
    pub async fn set_llm_config(&self, session_id: &str, config: LLMParams) -> Result<(), Error> {
        let provider_name = config
            .provider
            .as_ref()
            .ok_or_else(|| Error::new(-32000, "Provider is required in config".to_string()))?;

        if self
            .provider
            .plugin_registry()
            .get(provider_name)
            .await
            .is_none()
        {
            return Err(Error::new(
                -32000,
                format!("Unknown provider: {}", provider_name),
            ));
        }
        let llm_config = self
            .provider
            .history_store()
            .create_or_get_llm_config(&config)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;
        self.provider
            .history_store()
            .set_session_llm_config(session_id, llm_config.id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        // Fetch context limit from model info
        let context_limit =
            crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                .and_then(|m| m.context_limit());

        self.emit_event(
            session_id,
            crate::events::AgentEventKind::ProviderChanged {
                provider: llm_config.provider.clone(),
                model: llm_config.model.clone(),
                config_id: llm_config.id,
                context_limit,
            },
        );
        Ok(())
    }

    /// Get current LLM config for a session
    pub async fn get_session_llm_config(
        &self,
        session_id: &str,
    ) -> Result<Option<LLMConfig>, Error> {
        self.provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))
    }

    /// Get LLM config by ID
    pub async fn get_llm_config(&self, config_id: i64) -> Result<Option<LLMConfig>, Error> {
        self.provider
            .history_store()
            .get_llm_config(config_id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))
    }

    /// Sends a session update notification to the client.
    ///
    /// Uses the client bridge if available (ACP stdio mode), otherwise no-op.
    /// The notification is sent asynchronously (fire-and-forget) to avoid blocking.
    pub(crate) fn send_session_update(&self, session_id: &str, update: SessionUpdate) {
        if let Some(bridge) = self.bridge() {
            let notification =
                SessionNotification::new(SessionId::from(session_id.to_string()), update);
            // Fire-and-forget - spawn to avoid blocking the caller
            tokio::spawn(async move {
                if let Err(e) = bridge.notify(notification).await {
                    log::debug!("Failed to send session update: {}", e);
                }
            });
        }
        // If no bridge, silently ignore (backward compatible with WebSocket mode)
    }

    /// Gracefully shutdown the agent and all background tasks.
    ///
    /// This method:
    /// 1. Signals all active sessions to cancel
    /// 2. Shuts down the event bus (aborting observer tasks)
    /// 3. Waits briefly for cleanup
    pub async fn shutdown(&self) {
        log::info!("QueryMTAgent: Starting graceful shutdown");

        // 1. Cancel all active sessions
        let sessions: Vec<_> = {
            let mut active = self.active_sessions.lock().await;
            active.drain().collect()
        };
        log::debug!(
            "QueryMTAgent: Cancelling {} active sessions",
            sessions.len()
        );
        for (_id, tx) in sessions {
            let _ = tx.send(true); // Signal cancellation
        }

        // 2. Shutdown event bus (abort all observer tasks)
        self.event_bus.shutdown().await;

        // 3. Wait briefly for cleanup
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        log::info!("QueryMTAgent: Shutdown complete");
    }
}

//! Core agent structures and basic implementations

use crate::acp::client_bridge::ClientBridgeSender;
use crate::config::{CompactionConfig, PruningConfig, ToolOutputConfig};
use crate::delegation::{AgentRegistry, DefaultAgentRegistry};
use crate::event_bus::EventBus;
use crate::index::{WorkspaceIndexManager, WorkspaceIndexManagerConfig};
use crate::middleware::MiddlewareDriver;
use crate::session::compaction::SessionCompaction;
use crate::session::provider::SessionProvider;
use crate::session::store::SessionStore;
use crate::tools::ToolRegistry;
use agent_client_protocol::{AuthMethod, ClientCapabilities, Implementation, ProtocolVersion};
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, OnceCell};

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
pub(crate) struct QueryMTAgent {
    pub(crate) provider: Arc<SessionProvider>,
    pub(crate) default_mode: Arc<StdMutex<AgentMode>>,
    pub(crate) max_steps: Option<usize>,
    pub(crate) snapshot_policy: SnapshotPolicy,
    pub(crate) assume_mutating: bool,
    pub(crate) mutating_tools: HashSet<String>,
    pub(crate) max_prompt_bytes: Option<usize>,
    pub(crate) tool_config: Arc<StdMutex<ToolConfig>>,
    pub(crate) tool_registry: Arc<StdMutex<ToolRegistry>>,
    pub(crate) middleware_drivers: Arc<std::sync::Mutex<Vec<Arc<dyn MiddlewareDriver>>>>,
    pub(crate) event_bus: Arc<EventBus>,
    pub(crate) client_state: Arc<StdMutex<Option<ClientState>>>,
    pub(crate) auth_methods: Arc<StdMutex<Vec<AuthMethod>>>,
    pub(crate) client: Arc<StdMutex<Option<Arc<dyn agent_client_protocol::Client + Send + Sync>>>>,
    pub(crate) bridge: Arc<StdMutex<Option<ClientBridgeSender>>>,
    pub(crate) agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
    pub(crate) delegation_context_config: DelegationContextConfig,
    pub(crate) workspace_index_manager: Arc<WorkspaceIndexManager>,
    /// Maximum time to wait for session execution permit (seconds).
    /// If a session is busy and doesn't become available within this time,
    /// the prompt will fail with a timeout error. Default: 300 (5 minutes)
    pub(crate) execution_timeout_secs: u64,

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

    // ── Kameo actor infrastructure (lazily initialized) ──────────────
    /// Cached AgentConfig built from this agent's fields.
    /// Lazily constructed on first access to avoid breaking existing construction patterns.
    pub(crate) kameo_config: std::sync::OnceLock<std::sync::Arc<crate::agent::AgentConfig>>,

    /// Session registry for kameo actor management.
    /// Lazily constructed alongside kameo_config.
    pub(crate) kameo_registry:
        std::sync::OnceLock<std::sync::Arc<Mutex<crate::agent::SessionRegistry>>>,
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
    /// Execution permit ensuring only one prompt runs at a time for this session.
    /// Uses a semaphore with capacity 1 to guarantee FIFO ordering of concurrent prompts.
    /// This prevents race conditions where user messages are inserted between
    /// tool_use and tool_result blocks, which violates LLM API requirements.
    ///
    /// When a prompt is cancelled via `cancel_session()`, the execution permit is held
    /// until the cancelled operation fully cleans up (removes from active_sessions,
    /// emits final events, etc.). This ensures the next queued prompt doesn't race
    /// with cancellation cleanup.
    pub execution_permit: Arc<tokio::sync::Semaphore>,
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

        // Register all built-in tools from the canonical source
        tool_registry.extend(crate::tools::builtins::all_builtin_tools());

        Self {
            provider: session_provider,
            default_mode: Arc::new(StdMutex::new(AgentMode::Build)),
            max_steps: None,
            snapshot_policy: SnapshotPolicy::None,
            assume_mutating: true,
            mutating_tools: HashSet::new(),
            max_prompt_bytes: None,
            tool_config: Arc::new(StdMutex::new(ToolConfig::default())),
            tool_registry: Arc::new(StdMutex::new(tool_registry)),
            middleware_drivers: Arc::new(std::sync::Mutex::new(Vec::new())),
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
            execution_timeout_secs: 300, // 5 minutes default
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

            // Kameo infrastructure - lazily initialized
            kameo_config: std::sync::OnceLock::new(),
            kameo_registry: std::sync::OnceLock::new(),
        }
    }

    /// Get or build the `AgentConfig` from this agent's current fields.
    ///
    /// This provides a bridge to the kameo actor architecture during the
    /// build process. The config is built once and cached.
    ///
    /// Note: `default_mode` is passed as an Arc<Mutex> (live shared reference),
    /// not a snapshot, so session actors always read the current value.
    pub fn agent_config(&self) -> std::sync::Arc<crate::agent::AgentConfig> {
        self.kameo_config
            .get_or_init(|| {
                let tool_config = self
                    .tool_config
                    .lock()
                    .map(|c| c.clone())
                    .unwrap_or_default();
                let tool_registry = self.tool_registry.lock().unwrap().clone();
                let middleware_drivers = self
                    .middleware_drivers
                    .lock()
                    .map(|d| d.clone())
                    .unwrap_or_default();
                let auth_methods = self
                    .auth_methods
                    .lock()
                    .map(|m| m.clone())
                    .unwrap_or_default();

                std::sync::Arc::new(crate::agent::AgentConfig {
                    provider: self.provider.clone(),
                    event_bus: self.event_bus.clone(),
                    agent_registry: self.agent_registry.clone(),
                    workspace_index_manager: self.workspace_index_manager.clone(),
                    default_mode: self.default_mode.clone(),
                    tool_config,
                    tool_registry,
                    middleware_drivers,
                    auth_methods,
                    max_steps: self.max_steps,
                    snapshot_policy: self.snapshot_policy,
                    assume_mutating: self.assume_mutating,
                    mutating_tools: self.mutating_tools.clone(),
                    max_prompt_bytes: self.max_prompt_bytes,
                    execution_timeout_secs: self.execution_timeout_secs,
                    tool_output_config: self.tool_output_config.clone(),
                    pruning_config: self.pruning_config.clone(),
                    compaction_config: self.compaction_config.clone(),
                    compaction: self.compaction.clone(),
                    rate_limit_config: self.rate_limit_config.clone(),
                    snapshot_backend: self.snapshot_backend.clone(),
                    snapshot_gc_config: self.snapshot_gc_config.clone(),
                    delegation_context_config: self.delegation_context_config.clone(),
                    pending_elicitations: self.pending_elicitations.clone(),
                })
            })
            .clone()
    }

    /// Get or create the `SessionRegistry` backed by kameo actors.
    pub fn kameo_registry(&self) -> std::sync::Arc<Mutex<crate::agent::SessionRegistry>> {
        self.kameo_registry
            .get_or_init(|| {
                let config = self.agent_config();
                std::sync::Arc::new(Mutex::new(crate::agent::SessionRegistry::new(config)))
            })
            .clone()
    }

    /// Adds an event observer after agent creation.
    pub fn add_observer(&self, observer: Arc<dyn crate::events::EventObserver>) {
        self.event_bus.add_observer(observer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_all_builtin_tools_registered() {
        // Verify that all tools from all_builtin_tools() are registered
        let all_tools = crate::tools::builtins::all_builtin_tools();

        // Create a minimal agent to test tool registration
        use crate::session::backend::StorageBackend;
        use crate::session::sqlite_storage::SqliteStorage;
        use crate::test_utils::empty_plugin_registry;

        let (plugin_registry, _temp_dir) = empty_plugin_registry().unwrap();
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let llm_config = querymt::LLMParams::new()
            .provider("test")
            .model("test-model");

        let agent = QueryMTAgent::new(
            Arc::new(plugin_registry),
            storage.session_store(),
            llm_config,
        );
        let registry = agent.tool_registry.lock().unwrap();

        // Check that all tools are present
        let registered_names = registry.names();
        for tool in &all_tools {
            assert!(
                registered_names.contains(&tool.name().to_string()),
                "Tool '{}' from all_builtin_tools() should be registered",
                tool.name()
            );
        }

        // Specifically verify semantic_edit is registered
        assert!(
            registry.find("semantic_edit").is_some(),
            "semantic_edit tool should be registered"
        );

        // Verify count matches
        assert_eq!(
            registered_names.len(),
            all_tools.len(),
            "Number of registered tools should match all_builtin_tools()"
        );
    }
}

//! Core agent structures and basic implementations

use agent_client_protocol::{ClientCapabilities, Implementation, ProtocolVersion};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use tokio::sync::OnceCell;

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

/// Shared, live MCP tool state for a session.
///
/// Holds the tool maps and change-detection hash behind interior-mutability
/// locks so that `McpClientHandler::on_tool_list_changed` can refresh them
/// at any time without requiring exclusive ownership of `SessionRuntime`.
///
/// `tools` and `tool_defs` use `std::sync::RwLock` because all read paths are
/// synchronous (no `.await` while the lock is held) and writes only occur on
/// `tools/list_changed` notifications (rare).  `tools_hash` reuses the
/// `Mutex` pattern already established by other `SessionRuntime` fields.
pub struct McpToolState {
    pub tools: RwLock<HashMap<String, Arc<querymt::mcp::adapter::McpToolAdapter>>>,
    pub tool_defs: RwLock<Vec<querymt::chat::Tool>>,
    /// Hash of the currently available tool set used for change detection.
    /// Cleared by `McpClientHandler::on_tool_list_changed` so the next turn
    /// unconditionally re-emits a `ToolsAvailable` event.
    pub tools_hash: StdMutex<Option<crate::hash::RapidHash>>,
}

impl McpToolState {
    pub fn new(
        tools: HashMap<String, Arc<querymt::mcp::adapter::McpToolAdapter>>,
        tool_defs: Vec<querymt::chat::Tool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tools: RwLock::new(tools),
            tool_defs: RwLock::new(tool_defs),
            tools_hash: StdMutex::new(None),
        })
    }

    pub fn empty() -> Arc<Self> {
        Self::new(HashMap::new(), Vec::new())
    }
}

/// Type alias for the in-flight pre-turn snapshot task handle.
type PreTurnSnapshotTask =
    StdMutex<Option<tokio::task::JoinHandle<(String, Result<String, String>)>>>;

/// Runtime state for an active session.
pub struct SessionRuntime {
    pub cwd: Option<std::path::PathBuf>,
    pub _mcp_services: HashMap<
        String,
        rmcp::service::RunningService<rmcp::RoleClient, crate::elicitation::McpClientHandler>,
    >,
    /// Live MCP tool state — refreshed in-place when a server sends
    /// `tools/list_changed`.
    pub mcp_tool_state: Arc<McpToolState>,
    pub permission_cache: StdMutex<HashMap<String, bool>>,
    /// Workspace handle for duplicate code detection (built asynchronously on session start)
    pub workspace_handle: Arc<OnceCell<crate::index::WorkspaceHandle>>,
    /// Turn snapshot: (turn_id, snapshot_id) taken at the start of the turn for undo/redo
    pub turn_snapshot: StdMutex<Option<(String, String)>>,
    /// In-flight pre-turn snapshot task, resolved before first assistant response or post-turn use.
    pub pre_turn_snapshot_task: PreTurnSnapshotTask,
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

impl SessionRuntime {
    /// Construct a new `SessionRuntime`, filling all bookkeeping fields with
    /// their default initial values.
    pub fn new(
        cwd: Option<std::path::PathBuf>,
        mcp_services: HashMap<
            String,
            rmcp::service::RunningService<rmcp::RoleClient, crate::elicitation::McpClientHandler>,
        >,
        mcp_tool_state: Arc<McpToolState>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cwd,
            _mcp_services: mcp_services,
            mcp_tool_state,
            permission_cache: StdMutex::new(HashMap::new()),
            workspace_handle: Arc::new(OnceCell::new()),
            turn_snapshot: StdMutex::new(None),
            pre_turn_snapshot_task: StdMutex::new(None),
            turn_diffs: StdMutex::new(Default::default()),
            execution_permit: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }
}

/// Policy for tool usage and availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_all_builtin_tools_registered() {
        // Verify that all tools from all_builtin_tools() are registered
        let all_tools = crate::tools::builtins::all_builtin_tools();

        use crate::agent::agent_config_builder::AgentConfigBuilder;
        use crate::session::backend::StorageBackend;
        use crate::session::sqlite_storage::SqliteStorage;
        use crate::test_utils::empty_plugin_registry;
        use std::sync::Arc;

        let (plugin_registry, _temp_dir) = empty_plugin_registry().unwrap();
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let llm_config = querymt::LLMParams::new()
            .provider("test")
            .model("test-model");

        let builder = AgentConfigBuilder::new(
            Arc::new(plugin_registry),
            storage.session_store(),
            storage.event_journal(),
            llm_config,
        );
        let config = builder.build();

        // Check that all tools are present
        let registered_names = config.tool_registry.names();
        for tool in &all_tools {
            assert!(
                registered_names.contains(&tool.name().to_string()),
                "Tool '{}' from all_builtin_tools() should be registered",
                tool.name()
            );
        }

        // Specifically verify semantic_edit is registered
        assert!(
            config.tool_registry.find("semantic_edit").is_some(),
            "semantic_edit tool should be registered"
        );

        // Experimental tools may be registered outside all_builtin_tools().
        assert!(
            registered_names.len() >= all_tools.len(),
            "Registered tool count should include at least all_builtin_tools()"
        );
    }
}

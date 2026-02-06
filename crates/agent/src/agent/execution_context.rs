//! Execution context bundling all per-run state for agent execution

use crate::agent::core::SessionRuntime;
use crate::session::runtime::RuntimeContext;
use crate::tools::AgentToolContext;
use std::path::Path;
use std::sync::Arc;

/// Bundles all execution state needed for a single agent run.
///
/// This struct consolidates what was previously passed as separate parameters:
/// - `session_id: &str`
/// - `runtime: Option<&SessionRuntime>` (infrastructure state)
/// - `runtime_context: &RuntimeContext` (domain state)
///
/// By combining these into a single struct, we eliminate redundant parameters
/// and make the execution pipeline cleaner.
pub(crate) struct ExecutionContext {
    /// The session ID for this execution
    pub session_id: String,

    /// Per-session runtime infrastructure (cwd, MCP tools, snapshots, etc.)
    pub runtime: Arc<SessionRuntime>,

    /// Per-run domain state (active task, intent, progress recording)
    pub state: RuntimeContext,
}

impl ExecutionContext {
    /// Create a new execution context
    pub fn new(session_id: String, runtime: Arc<SessionRuntime>, state: RuntimeContext) -> Self {
        Self {
            session_id,
            runtime,
            state,
        }
    }

    /// Get the current working directory, if set
    pub fn cwd(&self) -> Option<&Path> {
        self.runtime.cwd.as_deref()
    }

    /// Create a tool execution context from this execution context
    ///
    /// This centralizes the construction of `AgentToolContext` from the execution state,
    /// extracting the necessary fields (session_id, cwd, agent_registry) in one place.
    pub fn tool_context(
        &self,
        agent_registry: Arc<dyn crate::delegation::AgentRegistry>,
        elicitation_tx: Option<tokio::sync::mpsc::Sender<crate::tools::ElicitationRequest>>,
    ) -> AgentToolContext {
        AgentToolContext::new(
            self.session_id.clone(),
            self.cwd().map(|p| p.to_path_buf()),
            Some(agent_registry),
            elicitation_tx,
        )
    }
}

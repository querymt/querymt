//! Execution context bundling all per-run state for agent execution

use crate::agent::core::SessionRuntime;
use crate::model::AgentMessage;
use crate::session::error::SessionResult;
use crate::session::provider::SessionHandle;
use crate::session::runtime::RuntimeContext;
use crate::session::store::LLMConfig;
use crate::tools::AgentToolContext;
use std::path::Path;
use std::sync::Arc;

/// Bundles all execution state needed for a single agent run.
///
/// This struct consolidates what was previously passed as separate parameters:
/// - `session_id: &str`
/// - `runtime: Option<&SessionRuntime>` (infrastructure state)
/// - `runtime_context: &RuntimeContext` (domain state)
/// - `session_handle: SessionHandle` (turn-pinned session + provider)
///
/// The `session_handle` is resolved once at the start of `run_prompt` and reused
/// for the entire turn. This eliminates redundant `with_session()` calls (DB
/// lookups) that previously happened on every state-machine transition.
pub(crate) struct ExecutionContext {
    /// The session ID for this execution
    pub session_id: String,

    /// Per-session runtime infrastructure (cwd, MCP tools, snapshots, etc.)
    pub runtime: Arc<SessionRuntime>,

    /// Per-run domain state (active task, intent, progress recording)
    pub state: RuntimeContext,

    /// Turn-pinned session handle. Created once per `run_prompt` and reused
    /// throughout execution — avoids repeated DB lookups for session/config.
    pub session_handle: SessionHandle,
}

impl ExecutionContext {
    /// Create a new execution context
    pub fn new(
        session_id: String,
        runtime: Arc<SessionRuntime>,
        state: RuntimeContext,
        session_handle: SessionHandle,
    ) -> Self {
        Self {
            session_id,
            runtime,
            state,
            session_handle,
        }
    }

    /// Get the current working directory, if set
    pub fn cwd(&self) -> Option<&Path> {
        self.runtime.cwd.as_deref()
    }

    /// Persist an `AgentMessage` via the turn-pinned session handle.
    pub async fn add_message(&self, message: AgentMessage) -> SessionResult<()> {
        self.session_handle.add_message(message).await
    }

    /// Get the turn-pinned LLM config (provider name, model, params).
    pub fn llm_config(&self) -> Option<&LLMConfig> {
        self.session_handle.llm_config()
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

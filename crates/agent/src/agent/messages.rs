//! Message types for SessionActor.
//!
//! Each message is a separate struct. Messages no longer carry `session_id`
//! because the actor IS the session.

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::core::AgentMode;
use agent_client_protocol::{
    ExtNotification as AcpExtNotification, ExtRequest, PromptRequest, SetSessionModelRequest,
};
use querymt::LLMParams;

// ══════════════════════════════════════════════════════════════════════════
//  Execution
// ══════════════════════════════════════════════════════════════════════════

/// Run a prompt on this session.
///
/// The handler uses `ctx.spawn()` to execute the state machine in a
/// detached task so the actor stays responsive for `Cancel`, `SetMode`, etc.
pub struct Prompt {
    pub req: PromptRequest,
}

/// Signal cancellation of the running prompt.
pub struct Cancel;

/// Internal message sent by the spawned prompt task when it finishes.
/// Resets the `prompt_running` flag on the actor.
pub(crate) struct PromptFinished;

// ══════════════════════════════════════════════════════════════════════════
//  Configuration
// ══════════════════════════════════════════════════════════════════════════

/// Set the agent mode for this session.
pub struct SetMode {
    pub mode: AgentMode,
}

/// Get the current agent mode for this session.
pub struct GetMode;

/// Switch provider and model for this session (simple form).
pub struct SetProvider {
    pub provider: String,
    pub model: String,
}

/// Switch provider configuration for this session (advanced form).
pub struct SetLlmConfig {
    pub config: LLMParams,
}

/// Set session model via ACP protocol.
pub struct SetSessionModel {
    pub req: SetSessionModelRequest,
}

/// Set the tool policy for this session.
pub struct SetToolPolicy {
    pub policy: crate::agent::core::ToolPolicy,
}

/// Set the allowed tools whitelist.
pub struct SetAllowedTools {
    pub tools: Vec<String>,
}

/// Clear the allowed tools whitelist.
pub struct ClearAllowedTools;

/// Set the denied tools blacklist.
pub struct SetDeniedTools {
    pub tools: Vec<String>,
}

/// Clear the denied tools blacklist.
pub struct ClearDeniedTools;

// ══════════════════════════════════════════════════════════════════════════
//  State Queries
// ══════════════════════════════════════════════════════════════════════════

/// Get session limits from configured middleware.
pub struct GetSessionLimits;

/// Get current LLM config for this session.
pub struct GetLlmConfig;

// ══════════════════════════════════════════════════════════════════════════
//  Undo / Redo
// ══════════════════════════════════════════════════════════════════════════

/// Undo filesystem changes back to a specific message.
pub struct Undo {
    pub message_id: String,
}

/// Redo: restore to pre-undo state.
pub struct Redo;

// ══════════════════════════════════════════════════════════════════════════
//  Extensions
// ══════════════════════════════════════════════════════════════════════════

/// Handle extension method calls.
pub struct ExtMethod {
    pub req: ExtRequest,
}

/// Handle extension notifications.
pub struct ExtNotification {
    pub notif: AcpExtNotification,
}

// ══════════════════════════════════════════════════════════════════════════
//  Lifecycle
// ══════════════════════════════════════════════════════════════════════════

/// Set the client bridge for SessionUpdate notifications.
pub struct SetBridge {
    pub bridge: ClientBridgeSender,
}

/// Stop this session actor gracefully.
pub struct Shutdown;

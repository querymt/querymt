//! Message types for SessionActor.
//!
//! Each message is a separate struct. Messages no longer carry `session_id`
//! because the actor IS the session.
//!
//! ## Serialization
//!
//! Public messages that may be sent across the kameo mesh (when the `remote`
//! feature is enabled) derive `Serialize + Deserialize`. Internal-only messages
//! (`PromptFinished`, `SetBridge`, `Shutdown`) do NOT — they are local-only and
//! contain non-serializable types.

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::core::AgentMode;
use agent_client_protocol::{
    ExtNotification as AcpExtNotification, ExtRequest, PromptRequest, SetSessionModelRequest,
};
use querymt::LLMParams;
use serde::{Deserialize, Serialize};

// ══════════════════════════════════════════════════════════════════════════
//  Execution
// ══════════════════════════════════════════════════════════════════════════

/// Run a prompt on this session.
///
/// The handler uses `ctx.spawn()` to execute the state machine in a
/// detached task so the actor stays responsive for `Cancel`, `SetMode`, etc.
#[derive(Serialize, Deserialize)]
pub struct Prompt {
    pub req: PromptRequest,
}

/// Signal cancellation of the running prompt.
#[derive(Serialize, Deserialize)]
pub struct Cancel;

/// Internal message sent by the spawned prompt task when it finishes.
/// Resets the `prompt_running` flag on the actor.
///
/// NOT serializable — only sent by spawned task within the same actor.
pub(crate) struct PromptFinished;

// ══════════════════════════════════════════════════════════════════════════
//  Configuration
// ══════════════════════════════════════════════════════════════════════════

/// Set the agent mode for this session.
#[derive(Serialize, Deserialize)]
pub struct SetMode {
    pub mode: AgentMode,
}

/// Get the current agent mode for this session.
#[derive(Serialize, Deserialize)]
pub struct GetMode;

/// Switch provider and model for this session (simple form).
#[derive(Serialize, Deserialize)]
pub struct SetProvider {
    pub provider: String,
    pub model: String,
}

/// Switch provider configuration for this session (advanced form).
#[derive(Serialize, Deserialize)]
pub struct SetLlmConfig {
    pub config: LLMParams,
}

/// Set session model via ACP protocol.
#[derive(Serialize, Deserialize)]
pub struct SetSessionModel {
    pub req: SetSessionModelRequest,
    /// Mesh node that owns the provider. `None` = local, `Some(hostname)` = remote node.
    #[serde(default)]
    pub provider_node: Option<String>,
}

/// Set the tool policy for this session.
#[derive(Serialize, Deserialize)]
pub struct SetToolPolicy {
    pub policy: crate::agent::core::ToolPolicy,
}

/// Set the allowed tools whitelist.
#[derive(Serialize, Deserialize)]
pub struct SetAllowedTools {
    pub tools: Vec<String>,
}

/// Clear the allowed tools whitelist.
#[derive(Serialize, Deserialize)]
pub struct ClearAllowedTools;

/// Set the denied tools blacklist.
#[derive(Serialize, Deserialize)]
pub struct SetDeniedTools {
    pub tools: Vec<String>,
}

/// Clear the denied tools blacklist.
#[derive(Serialize, Deserialize)]
pub struct ClearDeniedTools;

// ══════════════════════════════════════════════════════════════════════════
//  State Queries
// ══════════════════════════════════════════════════════════════════════════

/// Get session limits from configured middleware.
#[derive(Serialize, Deserialize)]
pub struct GetSessionLimits;

/// Get current LLM config for this session.
#[derive(Serialize, Deserialize)]
pub struct GetLlmConfig;

// ══════════════════════════════════════════════════════════════════════════
//  Undo / Redo
// ══════════════════════════════════════════════════════════════════════════

/// Undo filesystem changes back to a specific message.
#[derive(Serialize, Deserialize)]
pub struct Undo {
    pub message_id: String,
}

/// Redo: restore to pre-undo state.
#[derive(Serialize, Deserialize)]
pub struct Redo;

// ══════════════════════════════════════════════════════════════════════════
//  Extensions
// ══════════════════════════════════════════════════════════════════════════

/// Handle extension method calls.
#[derive(Serialize, Deserialize)]
pub struct ExtMethod {
    pub req: ExtRequest,
}

/// Handle extension notifications.
#[derive(Serialize, Deserialize)]
pub struct ExtNotification {
    pub notif: AcpExtNotification,
}

// ══════════════════════════════════════════════════════════════════════════
//  Lifecycle (local-only, NOT serializable)
// ══════════════════════════════════════════════════════════════════════════

/// Set the client bridge for SessionUpdate notifications.
///
/// NOT serializable — contains `ClientBridgeSender` (mpsc channel).
pub struct SetBridge {
    pub bridge: ClientBridgeSender,
}

/// Stop this session actor gracefully.
///
/// NOT serializable — local lifecycle only.
pub struct Shutdown;

// ══════════════════════════════════════════════════════════════════════════
//  Remote-ready messages (new for kameo mesh support)
// ══════════════════════════════════════════════════════════════════════════

/// Get the full message history for this session.
///
/// Reply: `Result<Vec<AgentMessage>, Error>`
///
/// For local sessions, reads from the local `SessionStore`.
/// For remote sessions, the reply is serialized and sent back over the mesh.
#[derive(Serialize, Deserialize)]
pub struct GetHistory;

/// Subscribe a remote `EventRelayActor` to this session's events.
///
/// Reply: `Result<(), Error>`
///
/// When handled, the session registers an `EventForwarder` on its local
/// `EventBus` that forwards events to the specified relay actor.
#[derive(Serialize, Deserialize)]
pub struct SubscribeEvents {
    pub relay_actor_id: u64,
}

/// Unsubscribe a previously registered event relay.
///
/// Reply: `Result<(), Error>`
#[derive(Serialize, Deserialize)]
pub struct UnsubscribeEvents {
    pub relay_actor_id: u64,
}

/// Set planning context on a delegate session.
///
/// Reply: `Result<(), Error>`
///
/// Used by the delegation orchestrator to inject the parent session's
/// planning summary into a child session's system prompt, without
/// requiring direct access to the child's `SessionStore`.
#[derive(Serialize, Deserialize)]
pub struct SetPlanningContext {
    pub summary: String,
}

// ══════════════════════════════════════════════════════════════════════════
//  File Proxy (remote file index / file reads)
// ══════════════════════════════════════════════════════════════════════════

/// Get the file index for this session's workspace.
///
/// Reply: `Result<GetFileIndexResponse, FileProxyError>`
///
/// Used by the dashboard to serve `@` autocomplete when the session lives on
/// a remote node. The handler reads from `runtime.workspace_handle` on
/// whichever node owns the actor.
#[derive(Serialize, Deserialize)]
pub struct GetFileIndex;

/// Read a file or directory on this session's node.
///
/// Reply: `Result<ReadRemoteFileResponse, FileProxyError>`
///
/// Used by the `@` mention expansion pipeline to inline file content for
/// remote sessions. The handler resolves the path relative to the session's
/// `cwd` and enforces the workspace-root sandbox.
#[derive(Serialize, Deserialize)]
pub struct ReadRemoteFile {
    /// Path to read — absolute or relative to the session's cwd.
    pub path: String,
    /// Line offset for paged reads (0-based).
    pub offset: usize,
    /// Max lines to return.
    pub limit: usize,
}

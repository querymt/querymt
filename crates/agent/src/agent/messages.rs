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
///
/// Carries the prompt generation that completed so the actor can ignore stale
/// completions from older queued tasks.
///
/// NOT serializable — only sent by spawned task within the same actor.
pub(crate) struct PromptFinished {
    pub generation: u64,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::SetSessionModelRequest;

    // ── Serialization round-trips ────────────────────────────────────────────

    #[test]
    fn prompt_message_serializes() {
        // PromptRequest uses #[serde(rename_all = "camelCase")], so session_id → sessionId
        // and the content field is called "prompt" in the protocol schema.
        let json = r#"{"req":{"sessionId":"sess-1","prompt":[]}}"#;
        let rt: Prompt = serde_json::from_str(json).unwrap();
        let back = serde_json::to_string(&rt).unwrap();
        assert!(back.contains("sess-1"));
    }

    #[test]
    fn cancel_message_serializes() {
        let msg = Cancel;
        let json = serde_json::to_string(&msg).unwrap();
        let _rt: Cancel = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn set_mode_message_serializes() {
        let msg = SetMode {
            mode: AgentMode::Plan,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: SetMode = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.mode, AgentMode::Plan);
    }

    #[test]
    fn get_mode_message_serializes() {
        let msg = GetMode;
        let json = serde_json::to_string(&msg).unwrap();
        let _rt: GetMode = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn set_provider_message_serializes() {
        let msg = SetProvider {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: SetProvider = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.provider, "openai");
        assert_eq!(rt.model, "gpt-4");
    }

    #[test]
    fn set_session_model_serializes() {
        let req = SetSessionModelRequest::new("s1".to_string(), "anthropic/claude-3".to_string());
        let msg = SetSessionModel {
            req,
            provider_node: Some("node-1".to_string()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: SetSessionModel = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.req.session_id, "s1".into());
        assert_eq!(rt.provider_node.as_deref(), Some("node-1"));
    }

    #[test]
    fn set_tool_policy_serializes() {
        let msg = SetToolPolicy {
            policy: crate::agent::core::ToolPolicy::BuiltInOnly,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: SetToolPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.policy, crate::agent::core::ToolPolicy::BuiltInOnly);
    }

    #[test]
    fn set_allowed_tools_serializes() {
        let msg = SetAllowedTools {
            tools: vec!["shell".to_string(), "read_tool".to_string()],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: SetAllowedTools = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.tools.len(), 2);
        assert!(rt.tools.contains(&"shell".to_string()));
    }

    #[test]
    fn clear_allowed_tools_serializes() {
        let msg = ClearAllowedTools;
        let json = serde_json::to_string(&msg).unwrap();
        let _rt: ClearAllowedTools = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn set_denied_tools_serializes() {
        let msg = SetDeniedTools {
            tools: vec!["dangerous_tool".to_string()],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: SetDeniedTools = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.tools, vec!["dangerous_tool".to_string()]);
    }

    #[test]
    fn clear_denied_tools_serializes() {
        let msg = ClearDeniedTools;
        let json = serde_json::to_string(&msg).unwrap();
        let _rt: ClearDeniedTools = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn get_session_limits_serializes() {
        let msg = GetSessionLimits;
        let json = serde_json::to_string(&msg).unwrap();
        let _rt: GetSessionLimits = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn get_llm_config_serializes() {
        let msg = GetLlmConfig;
        let json = serde_json::to_string(&msg).unwrap();
        let _rt: GetLlmConfig = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn undo_message_serializes() {
        let msg = Undo {
            message_id: "msg-xyz".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: Undo = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.message_id, "msg-xyz");
    }

    #[test]
    fn redo_message_serializes() {
        let msg = Redo;
        let json = serde_json::to_string(&msg).unwrap();
        let _rt: Redo = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn get_history_message_serializes() {
        let msg = GetHistory;
        let json = serde_json::to_string(&msg).unwrap();
        let _rt: GetHistory = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn subscribe_events_message_serializes() {
        let msg = SubscribeEvents { relay_actor_id: 42 };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: SubscribeEvents = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.relay_actor_id, 42);
    }

    #[test]
    fn unsubscribe_events_message_serializes() {
        let msg = UnsubscribeEvents { relay_actor_id: 99 };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: UnsubscribeEvents = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.relay_actor_id, 99);
    }

    #[test]
    fn set_planning_context_message_serializes() {
        let msg = SetPlanningContext {
            summary: "The plan is to do X then Y".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: SetPlanningContext = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.summary, "The plan is to do X then Y");
    }

    #[test]
    fn get_file_index_message_serializes() {
        let msg = GetFileIndex;
        let json = serde_json::to_string(&msg).unwrap();
        let _rt: GetFileIndex = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn read_remote_file_message_serializes() {
        let msg = ReadRemoteFile {
            path: "/tmp/test.txt".to_string(),
            offset: 10,
            limit: 100,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let rt: ReadRemoteFile = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.path, "/tmp/test.txt");
        assert_eq!(rt.offset, 10);
        assert_eq!(rt.limit, 100);
    }

    #[test]
    fn set_llm_config_message_serializes() {
        let config = querymt::LLMParams::new().provider("openai").model("gpt-4");
        let msg = SetLlmConfig { config };
        let json = serde_json::to_string(&msg).unwrap();
        let _rt: SetLlmConfig = serde_json::from_str(&json).unwrap();
    }
}

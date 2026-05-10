//! FFI error codes, opaque handle types, and JSON utility types.

use serde::{Deserialize, Serialize};

// ─── Error Codes ─────────────────────────────────────────────────────────────

/// FFI function return codes.
///
/// Every public FFI function returns one of these `int32_t` values.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiErrorCode {
    /// Success
    Ok = 0,
    /// Bad parameter (invalid JSON, null pointer where disallowed, etc.)
    InvalidArgument = 1,
    /// Object, session, or node not found
    NotFound = 2,
    /// Internal or async runtime error
    RuntimeError = 3,
    /// Operation not supported by this build/config
    Unsupported = 4,
    /// Duplicate registration
    AlreadyExists = 5,
    /// Operation blocked by active call
    Busy = 6,
    /// Invalid lifecycle/background state
    InvalidState = 7,
}

impl FfiErrorCode {
    pub fn name(self) -> &'static str {
        match self {
            FfiErrorCode::Ok => "QMT_MOBILE_OK",
            FfiErrorCode::InvalidArgument => "QMT_MOBILE_INVALID_ARGUMENT",
            FfiErrorCode::NotFound => "QMT_MOBILE_NOT_FOUND",
            FfiErrorCode::RuntimeError => "QMT_MOBILE_RUNTIME_ERROR",
            FfiErrorCode::Unsupported => "QMT_MOBILE_UNSUPPORTED",
            FfiErrorCode::AlreadyExists => "QMT_MOBILE_ALREADY_EXISTS",
            FfiErrorCode::Busy => "QMT_MOBILE_BUSY",
            FfiErrorCode::InvalidState => "QMT_MOBILE_INVALID_STATE",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_error_code_names_match_public_constants() {
        assert_eq!(FfiErrorCode::Ok.name(), "QMT_MOBILE_OK");
        assert_eq!(
            FfiErrorCode::InvalidArgument.name(),
            "QMT_MOBILE_INVALID_ARGUMENT"
        );
        assert_eq!(FfiErrorCode::NotFound.name(), "QMT_MOBILE_NOT_FOUND");
        assert_eq!(
            FfiErrorCode::RuntimeError.name(),
            "QMT_MOBILE_RUNTIME_ERROR"
        );
        assert_eq!(FfiErrorCode::Unsupported.name(), "QMT_MOBILE_UNSUPPORTED");
        assert_eq!(
            FfiErrorCode::AlreadyExists.name(),
            "QMT_MOBILE_ALREADY_EXISTS"
        );
        assert_eq!(FfiErrorCode::Busy.name(), "QMT_MOBILE_BUSY");
        assert_eq!(
            FfiErrorCode::InvalidState.name(),
            "QMT_MOBILE_INVALID_STATE"
        );
    }

    #[test]
    fn error_code_values_match_names() {
        assert_eq!(FfiErrorCode::Ok as i32, 0);
        assert_eq!(FfiErrorCode::InvalidArgument as i32, 1);
        assert_eq!(FfiErrorCode::NotFound as i32, 2);
        assert_eq!(FfiErrorCode::RuntimeError as i32, 3);
        assert_eq!(FfiErrorCode::Unsupported as i32, 4);
        assert_eq!(FfiErrorCode::AlreadyExists as i32, 5);
        assert_eq!(FfiErrorCode::Busy as i32, 6);
        assert_eq!(FfiErrorCode::InvalidState as i32, 7);
    }

    #[test]
    fn session_options_parse_partial() {
        let json = r#"{"provider": "anthropic", "model": "claude-3"}"#;
        let opts: SessionOptions = serde_json::from_str(json).expect("should parse");
        assert_eq!(opts.provider, Some("anthropic".into()));
        assert_eq!(opts.model, Some("claude-3".into()));
        assert!(opts.cwd.is_none());
    }

    #[test]
    fn invite_options_defaults() {
        let json = r#"{"mesh_name": "test-mesh"}"#;
        let opts: InviteOptions = serde_json::from_str(json).expect("should parse");
        assert_eq!(opts.mesh_name, Some("test-mesh".into()));
        assert_eq!(opts.expires_at, 0);
        assert_eq!(opts.max_uses, None);
        assert!(!opts.can_invite);
    }

    #[test]
    fn session_list_response_serializes() {
        let response = SessionListResponse {
            sessions: vec![SessionSummary {
                session_id: "sess-1".into(),
                title: "Test".into(),
                created_at: 1000,
                updated_at: 2000,
                runtime_state: "idle".into(),
                is_remote: false,
                node_id: None,
            }],
            next_cursor: None,
        };
        let json = serde_json::to_string(&response).expect("should serialize");
        assert!(json.contains("sess-1"));
        assert!(json.contains("Test"));
    }

    #[test]
    fn event_envelope_serializes() {
        let envelope = EventEnvelope {
            session_id: Some("sess-1".into()),
            session_handle: Some(42),
            is_remote: false,
            node_id: None,
            request_id: None,
            event: serde_json::json!({"type": "message", "content": "hello"}),
        };
        let json = serde_json::to_string(&envelope).expect("should serialize");
        assert!(json.contains("sess-1"));
        assert!(json.contains("\"type\":\"message\""));
    }
}

// ─── Handle Types ────────────────────────────────────────────────────────────

/// Opaque handle type for agent instances.
pub type AgentHandle = u64;

/// Opaque handle type for session instances.
pub type SessionHandle = u64;

// ─── Session Options ─────────────────────────────────────────────────────────

/// JSON options for session creation.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionOptions {
    /// Working directory for the session. Must be absolute when provided.
    pub cwd: Option<String>,
    /// Provider override. If omitted, agent defaults are used.
    pub provider: Option<String>,
    /// Model override. If omitted, agent defaults are used.
    pub model: Option<String>,
}

// ─── Invite Options ──────────────────────────────────────────────────────────

/// JSON options for invite creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteOptions {
    /// Display/group name for the mesh.
    pub mesh_name: Option<String>,
    /// Unix timestamp in seconds; 0 means no expiry.
    #[serde(default)]
    pub expires_at: u64,
    /// Maximum joins; 0 means unlimited, omitted defaults to 1.
    pub max_uses: Option<u32>,
    /// Whether the joining node can create further invites.
    #[serde(default)]
    pub can_invite: bool,
}

// ─── Response Types ──────────────────────────────────────────────────────────

/// Response for session listing.
#[derive(Debug, Clone, Serialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionSummary>,
    pub next_cursor: Option<String>,
}

/// A single session summary.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub runtime_state: String,
    pub is_remote: bool,
    pub node_id: Option<String>,
}

/// Response for mesh node listing.
#[derive(Debug, Clone, Serialize)]
pub struct NodeListResponse {
    pub enabled: bool,
    pub local_node_id: String,
    pub nodes: Vec<NodeInfo>,
}

/// A single mesh node.
#[derive(Debug, Clone, Serialize)]
pub struct NodeInfo {
    pub node_id: String,
    pub label: String,
    pub hostname: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub active_sessions: u32,
    pub is_local: bool,
    pub is_reachable: bool,
}

/// Response for remote session listing.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteSessionListResponse {
    pub node_id: String,
    pub sessions: Vec<RemoteSessionSummary>,
}

/// A single remote session summary.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteSessionSummary {
    pub session_id: String,
    pub actor_id: u64,
    pub cwd: Option<String>,
    pub created_at: i64,
    pub title: String,
    pub peer_label: String,
    pub runtime_state: String,
}

/// Response for invite creation.
#[derive(Debug, Clone, Serialize)]
pub struct InviteCreateResponse {
    pub token: String,
    pub invite_id: String,
    pub inviter_peer_id: String,
    pub mesh_name: Option<String>,
    pub expires_at: u64,
    pub max_uses: u32,
    pub can_invite: bool,
}

/// Response for joining a mesh.
#[derive(Debug, Clone, Serialize)]
pub struct JoinMeshResponse {
    pub joined: bool,
    pub peer_id: String,
    pub mesh_name: Option<String>,
    pub inviter_peer_id: String,
}

/// Mesh status response.
#[derive(Debug, Clone, Serialize)]
pub struct MeshStatusResponse {
    pub enabled: bool,
    pub peer_id: Option<String>,
    pub transport: String,
    pub backgrounded: bool,
    pub known_peer_count: usize,
    pub has_invite_store: bool,
    pub has_membership_store: bool,
    /// The listen multiaddr used at bootstrap (diagnostic).
    pub listen: Option<String>,
    /// The discovery mode used at bootstrap (diagnostic).
    pub discovery: Option<String>,
    /// OTLP telemetry endpoint in use (diagnostic).
    pub telemetry_endpoint: Option<String>,
}

/// Event envelope wrapping a QueryMT event with routing metadata.
#[derive(Debug, Clone, Serialize)]
pub struct EventEnvelope {
    pub session_id: Option<String>,
    pub session_handle: Option<u64>,
    pub is_remote: bool,
    pub node_id: Option<String>,
    pub request_id: Option<String>,
    pub event: serde_json::Value,
}

/// Session history response.
#[derive(Debug, Clone, Serialize)]
pub struct SessionHistoryResponse {
    pub messages: Vec<SessionMessage>,
}

/// A single message in session history.
///
/// `parts` contains ACP `ContentBlock` values (not raw `MessagePart`).
#[derive(Debug, Clone, Serialize)]
pub struct SessionMessage {
    pub role: String,
    /// ACP ContentBlock values serialized as JSON.
    pub parts: Vec<serde_json::Value>,
    pub created_at: i64,
    pub message_id: String,
}

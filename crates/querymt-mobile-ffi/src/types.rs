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
    fn mobile_init_config_minimal_parses() {
        let json = r#"{"agent": {"provider": "anthropic", "model": "claude-3-5-sonnet-20241022"}}"#;
        let config: MobileInitConfig = serde_json::from_str(json).expect("should parse");
        assert_eq!(config.agent.provider, "anthropic");
        assert_eq!(config.agent.model, "claude-3-5-sonnet-20241022");
        assert!(!config.mesh.enabled);
    }

    #[test]
    fn mobile_init_config_with_mesh_parses() {
        let json = r#"{
            "agent": {"provider": "openai", "model": "gpt-4o"},
            "mesh": {"enabled": true, "transport": "lan"}
        }"#;
        let config: MobileInitConfig = serde_json::from_str(json).expect("should parse");
        assert_eq!(config.agent.provider, "openai");
        assert_eq!(config.mesh.enabled, true);
        assert_eq!(config.mesh.transport, "lan");
    }

    #[test]
    fn mobile_init_config_with_iroh_transport_parses() {
        let json = r#"{
            "agent": {"provider": "openai", "model": "gpt-4o"},
            "mesh": {"enabled": true, "transport": "iroh", "identity_file": "/tmp/id"}
        }"#;
        let config: MobileInitConfig = serde_json::from_str(json).expect("should parse");
        assert_eq!(config.mesh.transport, "iroh");
        assert_eq!(config.mesh.identity_file, Some("/tmp/id".into()));
    }

    #[test]
    fn mobile_mesh_config_peers_and_listen_parse() {
        let json = r#"{
            "agent": {"provider": "openai", "model": "gpt-4o"},
            "mesh": {
                "enabled": true,
                "listen": "/ip4/0.0.0.0/tcp/0",
                "discovery": "mdns",
                "peers": [
                    {"name": "desktop", "addr": "/ip4/192.168.1.100/tcp/9001"}
                ],
                "request_timeout_secs": 120,
                "stream_reconnect_grace_secs": 60
            }
        }"#;
        let config: MobileInitConfig = serde_json::from_str(json).expect("should parse");
        assert_eq!(config.mesh.listen.as_deref(), Some("/ip4/0.0.0.0/tcp/0"));
        assert_eq!(config.mesh.discovery, "mdns");
        assert_eq!(config.mesh.peers.len(), 1);
        assert_eq!(config.mesh.peers[0].name, "desktop");
        assert_eq!(config.mesh.peers[0].addr, "/ip4/192.168.1.100/tcp/9001");
        assert_eq!(config.mesh.request_timeout_secs, 120);
        assert_eq!(config.mesh.stream_reconnect_grace_secs, 60);
    }

    #[test]
    fn mobile_mesh_config_defaults() {
        let json =
            r#"{"agent": {"provider": "openai", "model": "gpt-4o"}, "mesh": {"enabled": true}}"#;
        let config: MobileInitConfig = serde_json::from_str(json).expect("should parse");
        assert_eq!(config.mesh.listen.as_deref(), Some("/ip4/0.0.0.0/tcp/0"));
        assert_eq!(config.mesh.discovery, "mdns");
        assert!(config.mesh.peers.is_empty());
        assert_eq!(config.mesh.request_timeout_secs, 300);
        assert_eq!(config.mesh.stream_reconnect_grace_secs, 120);
    }

    #[test]
    fn mobile_init_config_fails_if_provider_missing() {
        let json = r#"{"agent": {"model": "gpt-4o"}}"#;
        let result: Result<MobileInitConfig, _> = serde_json::from_str(json);
        assert!(result.is_err(), "should fail without provider");
    }

    #[test]
    fn mobile_init_config_with_remote_agents_parses() {
        let json = r#"{
            "agent": {"provider": "openai", "model": "gpt-4o"},
            "remote_agents": [
                {"id": "agent1", "name": "Alpha", "description": "test agent", "peer": "12D3KooW..."}
            ]
        }"#;
        let config: MobileInitConfig = serde_json::from_str(json).expect("should parse");
        assert_eq!(config.remote_agents.len(), 1);
        assert_eq!(config.remote_agents[0].id, "agent1");
        assert_eq!(config.remote_agents[0].peer, "12D3KooW...");
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

// ─── Config Types (JSON deserialization targets) ────────────────────────────

/// The JSON config passed to `qmt_mobile_init_agent`.
#[derive(Debug, Clone, Deserialize)]
pub struct MobileInitConfig {
    /// Core agent settings.
    #[serde(default)]
    pub agent: MobileAgentSettings,

    /// Mesh configuration.
    #[serde(default)]
    pub mesh: MobileMeshConfig,

    /// Remote agent declarations.
    #[serde(default, rename = "remote_agents")]
    pub remote_agents: Vec<MobileRemoteAgentConfig>,

    /// Telemetry (OTLP) configuration.
    #[serde(default)]
    pub telemetry: MobileTelemetryConfig,
}

/// Agent settings within the mobile init config.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MobileAgentSettings {
    /// LLM provider name.
    pub provider: String,

    /// Model identifier.
    pub model: String,

    /// SQLite database path. If absent, a platform data-dir fallback is used.
    pub db: Option<String>,

    /// Working directory for the agent.
    pub cwd: Option<String>,

    /// Allowed tool names.
    #[serde(default)]
    pub tools: Vec<String>,

    /// Extra LLM parameters (temperature, max_tokens, top_p, etc.).
    #[serde(default)]
    pub parameters: Option<std::collections::HashMap<String, serde_json::Value>>,

    /// System prompt.
    #[serde(default)]
    pub system: Vec<String>,

    /// API key for the provider.
    pub api_key: Option<String>,
}

/// Mesh configuration within the mobile init config.
#[derive(Debug, Clone, Deserialize)]
pub struct MobileMeshConfig {
    /// Whether mesh networking is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Transport: "iroh" for internet-capable, absent/"lan" for LAN-only.
    #[serde(default = "default_transport")]
    pub transport: String,

    /// Whether provider_node_id = None may fall back to mesh provider discovery.
    #[serde(default)]
    pub auto_fallback: bool,

    /// Multiaddr to listen on. Defaults to "/ip4/0.0.0.0/tcp/0" (OS picks
    /// a random free port) so mobile nodes don't collide with desktop agents.
    #[serde(default = "default_listen")]
    pub listen: Option<String>,

    /// Peer discovery strategy: "mdns" (default), "kademlia", or "none".
    #[serde(default = "default_discovery")]
    pub discovery: String,

    /// Explicit bootstrap peers to dial at startup.
    /// Each entry has a `name` and a libp2p multiaddr `addr`.
    #[serde(default)]
    pub peers: Vec<MobileMeshPeerConfig>,

    /// Timeout in seconds for non-streaming mesh requests. Default: 300.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,

    /// Grace period (seconds) for mesh reconnection. Default: 120.
    #[serde(default = "default_stream_reconnect_grace_secs")]
    pub stream_reconnect_grace_secs: u64,

    /// Path to the persistent ed25519 identity file for the mesh node.
    pub identity_file: Option<String>,

    /// Invite token to join an existing mesh at startup.
    pub invite: Option<String>,

    /// Human-readable node name advertised to mesh peers.
    /// When absent, falls back to OS hostname (which is often "unknown" on mobile).
    #[serde(default)]
    pub node_name: Option<String>,
}

/// A single mesh bootstrap peer.
#[derive(Debug, Clone, Deserialize)]
pub struct MobileMeshPeerConfig {
    /// Human-readable label.
    pub name: String,
    /// Libp2p multiaddr, e.g. "/ip4/192.168.1.100/tcp/9000".
    pub addr: String,
}

fn default_transport() -> String {
    "lan".to_string()
}

fn default_listen() -> Option<String> {
    Some("/ip4/0.0.0.0/tcp/0".to_string())
}

fn default_discovery() -> String {
    "mdns".to_string()
}

fn default_request_timeout_secs() -> u64 {
    300
}

fn default_stream_reconnect_grace_secs() -> u64 {
    120
}

impl Default for MobileMeshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: default_transport(),
            auto_fallback: false,
            listen: default_listen(),
            discovery: default_discovery(),
            peers: Vec::new(),
            request_timeout_secs: default_request_timeout_secs(),
            stream_reconnect_grace_secs: default_stream_reconnect_grace_secs(),
            identity_file: None,
            invite: None,
            node_name: None,
        }
    }
}

/// Telemetry (OTLP) configuration within the mobile init config.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MobileTelemetryConfig {
    /// Whether to enable OTLP telemetry export. Default: false.
    ///
    /// When enabled, the endpoint is read from `OTEL_EXPORTER_OTLP_ENDPOINT`
    /// (falling back to the default in `querymt-utils`) and the level from
    /// `QMT_TELEMETRY_LEVEL` (default `info`).
    #[serde(default)]
    pub enabled: bool,
}

/// Remote agent config within the mobile init config.
#[derive(Debug, Clone, Deserialize)]
pub struct MobileRemoteAgentConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub peer: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

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
#[derive(Debug, Clone, Serialize)]
pub struct SessionMessage {
    pub role: String,
    pub parts: Vec<serde_json::Value>,
    pub created_at: i64,
    pub message_id: String,
}

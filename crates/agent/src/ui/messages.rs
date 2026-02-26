//! Wire protocol types for UI WebSocket communication.
//!
//! Contains all message types exchanged between the UI client and server,
//! as well as supporting DTOs for sessions, models, and agents.

use crate::events::EventEnvelope;
use crate::index::FileIndexEntry;
use crate::session::projection::AuditView;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiPromptBlock {
    Text {
        text: String,
    },
    ResourceLink {
        name: String,
        uri: String,
        #[serde(default)]
        description: Option<String>,
    },
}

/// Information about an available agent for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct UiAgentInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
}

/// Routing mode for message distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    Single,
    Broadcast,
}

/// Cached model list entry with canonical identity.
#[derive(Debug, Clone, Serialize)]
pub struct ModelEntry {
    /// Canonical internal identifier (e.g., "hf:repo:file.gguf", "file:/path/to/model.gguf", or provider-specific ID)
    pub id: String,
    /// Human-readable display label
    pub label: String,
    /// Model source: "preset", "cached", "custom", "catalog"
    pub source: String,
    /// Provider name
    pub provider: String,
    /// Original model identifier (for backwards compatibility)
    pub model: String,
    /// Stable node id where this provider lives. `None` = local node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Human-readable node label for display purposes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_label: Option<String>,
    /// Model family/repo for grouping (e.g., "Qwen2.5-Coder-32B-Instruct")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Quantization level (e.g., "Q8_0", "Q6_K", "unknown")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCapabilityEntry {
    pub provider: String,
    pub supports_custom_models: bool,
}

/// Recent model usage entry from event history.
#[derive(Debug, Clone, Serialize)]
pub struct RecentModelEntry {
    pub provider: String,
    pub model: String,
    pub last_used: String, // ISO 8601 timestamp
    pub use_count: u32,
}

/// OAuth authentication status for a provider.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthStatus {
    NotAuthenticated,
    Expired,
    Connected,
}

// Re-export from the canonical definition in querymt-utils (always available,
// regardless of the `oauth` feature flag).
pub use querymt_utils::OAuthFlowKind;

/// OAuth-capable provider entry for dashboard auth UI.
#[derive(Debug, Clone, Serialize)]
pub struct AuthProviderEntry {
    pub provider: String,
    pub display_name: String,
    pub status: OAuthStatus,
}

/// Summary of a session for listing.
#[derive(Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Public ID of the parent session (if this is a child session)
    pub parent_session_id: Option<String>,
    /// Fork origin: "user" or "delegation"
    pub fork_origin: Option<String>,
    /// Whether this session has child sessions
    pub has_children: bool,
    /// Node label where this session lives. "local" for local sessions,
    /// peer hostname/label for remote sessions (display only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// Information about a remote node discovered in the kameo mesh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteNodeInfo {
    /// Stable mesh node id (PeerId string).
    pub id: String,
    /// Human-readable label / hostname
    pub label: String,
    /// Node capabilities
    pub capabilities: Vec<String>,
    /// Number of active sessions on the node
    pub active_sessions: usize,
}

/// Group of sessions by working directory.
#[derive(Serialize)]
pub struct SessionGroup {
    pub cwd: Option<String>,
    pub sessions: Vec<SessionSummary>,
    pub latest_activity: Option<String>,
}

/// Messages from UI client to server.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiClientMessage {
    Init,
    SetActiveAgent {
        agent_id: String,
    },
    SetRoutingMode {
        mode: RoutingMode,
    },
    NewSession {
        cwd: Option<String>,
        #[serde(default)]
        request_id: Option<String>,
    },
    Prompt {
        prompt: Vec<UiPromptBlock>,
    },
    ListSessions,
    LoadSession {
        session_id: String,
    },
    ListAllModels {
        #[serde(default)]
        refresh: bool,
    },
    SetSessionModel {
        session_id: String,
        model_id: String,
        /// Optional mesh node id (PeerId string) that owns the provider. `None` = local.
        #[serde(default, alias = "node")]
        node_id: Option<String>,
    },
    /// Get recent models from event history
    GetRecentModels {
        #[serde(default)]
        limit_per_workspace: Option<u32>,
    },
    /// Request file index for @ mentions
    GetFileIndex,
    /// Request LLM config details by config_id
    GetLlmConfig {
        config_id: i64,
    },
    /// Cancel the active session for the current agent
    CancelSession,
    /// Undo filesystem changes to a specific message point
    Undo {
        message_id: String,
    },
    /// Redo: restore filesystem to pre-undo state
    Redo,
    /// Subscribe to a session's event stream
    SubscribeSession {
        session_id: String,
        #[serde(default)]
        agent_id: Option<String>,
    },
    /// Unsubscribe from a session's event stream
    UnsubscribeSession {
        session_id: String,
    },
    /// Respond to an elicitation request
    ElicitationResponse {
        elicitation_id: String,
        action: String,
        content: Option<serde_json::Value>,
    },
    /// List configured OAuth-capable providers and their auth status
    ListAuthProviders,
    /// Start OAuth login flow for provider
    #[serde(rename = "start_oauth_login")]
    StartOAuthLogin {
        provider: String,
    },
    /// Complete OAuth login flow using pasted callback URL/code
    #[serde(rename = "complete_oauth_login")]
    CompleteOAuthLogin {
        flow_id: String,
        response: String,
    },
    /// Disconnect OAuth credentials for provider
    #[serde(rename = "disconnect_oauth")]
    DisconnectOAuth {
        provider: String,
    },
    /// Set the agent's operating mode (build/plan/review)
    SetAgentMode {
        mode: String,
    },
    /// Get the current agent mode
    GetAgentMode,
    /// List remote nodes discovered in the kameo mesh
    ListRemoteNodes,
    /// List sessions on a specific remote node
    ListRemoteSessions {
        /// Stable node id (PeerId string) identifying the target node
        #[serde(alias = "node")]
        node_id: String,
    },
    /// Create a new session on a specific remote node
    CreateRemoteSession {
        /// Stable node id (PeerId string) identifying the target node
        #[serde(alias = "node")]
        node_id: String,
        /// Working directory on the remote machine (optional)
        cwd: Option<String>,
        /// Client-generated request ID for correlating the response
        #[serde(default)]
        request_id: Option<String>,
    },
    /// Attach an existing remote session to the local dashboard
    AttachRemoteSession {
        /// Stable node id (PeerId string) identifying the target node
        #[serde(alias = "node")]
        node_id: String,
        /// Session ID to attach
        session_id: String,
    },
    AddCustomModelFromHf {
        provider: String,
        repo: String,
        filename: String,
        #[serde(default)]
        display_name: Option<String>,
    },
    AddCustomModelFromFile {
        provider: String,
        file_path: String,
        #[serde(default)]
        display_name: Option<String>,
    },
    DeleteCustomModel {
        provider: String,
        model_id: String,
    },
    /// Trigger an update of all OCI provider plugins.
    UpdatePlugins,
}

#[derive(Debug, Clone, Serialize)]
pub struct UndoStackFrame {
    pub message_id: String,
}

/// Result of updating a single OCI plugin, reported in `PluginUpdateComplete`.
#[derive(Debug, Clone, Serialize)]
pub struct PluginUpdateResult {
    pub plugin_name: String,
    pub success: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamCursor {
    #[serde(default)]
    pub local_seq: u64,
    #[serde(default)]
    pub remote_seq_by_source: HashMap<String, u64>,
}

/// Messages from server to UI client.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiServerMessage {
    State {
        routing_mode: RoutingMode,
        active_agent_id: String,
        active_session_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        default_cwd: Option<String>,
        agents: Vec<UiAgentInfo>,
        sessions_by_agent: HashMap<String, String>,
        agent_mode: String,
    },
    SessionCreated {
        agent_id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    Event {
        agent_id: String,
        session_id: String,
        event: EventEnvelope,
    },
    SessionEvents {
        session_id: String,
        agent_id: String,
        events: Vec<EventEnvelope>,
        cursor: StreamCursor,
    },
    Error {
        message: String,
    },
    SessionList {
        groups: Vec<SessionGroup>,
    },
    SessionLoaded {
        session_id: String,
        agent_id: String,
        audit: AuditView,
        undo_stack: Vec<UndoStackFrame>,
        cursor: StreamCursor,
    },
    WorkspaceIndexStatus {
        session_id: String,
        status: String,
        message: Option<String>,
    },
    AllModelsList {
        models: Vec<ModelEntry>,
    },
    /// Recent models from event history, grouped by workspace
    RecentModels {
        by_workspace: HashMap<String, Vec<RecentModelEntry>>,
    },
    ProviderCapabilities {
        providers: Vec<ProviderCapabilityEntry>,
    },
    /// File index for autocomplete
    FileIndex {
        files: Vec<FileIndexEntry>,
        generated_at: u64,
    },
    /// LLM config details response
    LlmConfig {
        config_id: i64,
        provider: String,
        model: String,
        params: Option<Value>,
    },
    /// Result of an undo operation
    UndoResult {
        success: bool,
        message: Option<String>,
        reverted_files: Vec<String>,
        message_id: Option<String>,
        undo_stack: Vec<UndoStackFrame>,
    },
    /// Result of a redo operation
    RedoResult {
        success: bool,
        message: Option<String>,
        undo_stack: Vec<UndoStackFrame>,
    },
    /// Current agent mode notification
    AgentMode {
        mode: String,
    },
    /// OAuth-capable providers and current authentication status
    AuthProviders {
        providers: Vec<AuthProviderEntry>,
    },
    /// OAuth flow started; frontend should open authorization_url
    #[serde(rename = "oauth_flow_started")]
    OAuthFlowStarted {
        flow_id: String,
        provider: String,
        authorization_url: String,
        flow_kind: OAuthFlowKind,
    },
    /// OAuth flow completion result
    #[serde(rename = "oauth_result")]
    OAuthResult {
        provider: String,
        success: bool,
        message: String,
    },
    /// List of remote nodes discovered in the kameo mesh
    RemoteNodes {
        nodes: Vec<RemoteNodeInfo>,
    },
    /// Sessions available on a specific remote node
    RemoteSessions {
        /// Stable node id (PeerId string)
        node_id: String,
        /// Sessions on that node
        sessions: Vec<crate::agent::remote::RemoteSessionInfo>,
    },
    ModelDownloadStatus {
        provider: String,
        model_id: String,
        status: String,
        bytes_downloaded: u64,
        bytes_total: Option<u64>,
        percent: Option<f32>,
        speed_bps: Option<u64>,
        eta_seconds: Option<u64>,
        message: Option<String>,
    },
    /// Progress update for an OCI plugin update operation.
    PluginUpdateStatus {
        plugin_name: String,
        image_reference: String,
        phase: String,
        bytes_downloaded: u64,
        bytes_total: Option<u64>,
        percent: Option<f32>,
        message: Option<String>,
    },
    /// All OCI plugin updates have completed.
    PluginUpdateComplete {
        results: Vec<PluginUpdateResult>,
    },
}

impl UiServerMessage {
    /// Returns the message type name for logging purposes.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::State { .. } => "state",
            Self::SessionCreated { .. } => "session_created",
            Self::Event { .. } => "event",
            Self::Error { .. } => "error",
            Self::SessionList { .. } => "session_list",
            Self::SessionLoaded { .. } => "session_loaded",
            Self::WorkspaceIndexStatus { .. } => "workspace_index_status",
            Self::AllModelsList { .. } => "all_models_list",
            Self::RecentModels { .. } => "recent_models",
            Self::ProviderCapabilities { .. } => "provider_capabilities",
            Self::FileIndex { .. } => "file_index",
            Self::LlmConfig { .. } => "llm_config",
            Self::SessionEvents { .. } => "session_events",
            Self::UndoResult { .. } => "undo_result",
            Self::RedoResult { .. } => "redo_result",
            Self::AgentMode { .. } => "agent_mode",
            Self::AuthProviders { .. } => "auth_providers",
            Self::OAuthFlowStarted { .. } => "oauth_flow_started",
            Self::OAuthResult { .. } => "oauth_result",
            Self::RemoteNodes { .. } => "remote_nodes",
            Self::RemoteSessions { .. } => "remote_sessions",
            Self::ModelDownloadStatus { .. } => "model_download_status",
            Self::PluginUpdateStatus { .. } => "plugin_update_status",
            Self::PluginUpdateComplete { .. } => "plugin_update_complete",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OAuthFlowKind, PluginUpdateResult, UiClientMessage, UiServerMessage};
    use serde_json::json;

    #[test]
    fn deserializes_start_oauth_login_tag() {
        let current: UiClientMessage = serde_json::from_value(json!({
            "type": "start_oauth_login",
            "provider": "openai"
        }))
        .expect("current start_oauth_login tag should deserialize");

        match current {
            UiClientMessage::StartOAuthLogin { provider } => assert_eq!(provider, "openai"),
            _ => panic!("expected StartOAuthLogin variant"),
        }
    }

    #[test]
    fn deserializes_complete_oauth_login_tag() {
        let current: UiClientMessage = serde_json::from_value(json!({
            "type": "complete_oauth_login",
            "flow_id": "flow-1",
            "response": "code"
        }))
        .expect("current complete_oauth_login tag should deserialize");

        match current {
            UiClientMessage::CompleteOAuthLogin { flow_id, response } => {
                assert_eq!(flow_id, "flow-1");
                assert_eq!(response, "code");
            }
            _ => panic!("expected CompleteOAuthLogin variant"),
        }
    }

    #[test]
    fn deserializes_disconnect_oauth_tag() {
        let current: UiClientMessage = serde_json::from_value(json!({
            "type": "disconnect_oauth",
            "provider": "openai"
        }))
        .expect("current disconnect_oauth tag should deserialize");

        match current {
            UiClientMessage::DisconnectOAuth { provider } => assert_eq!(provider, "openai"),
            _ => panic!("expected DisconnectOAuth variant"),
        }
    }

    // ── Remote message node/node_id alias tests ──────────────────────────

    #[test]
    fn create_remote_session_accepts_node_id_field() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "create_remote_session",
            "node_id": "peer-abc",
            "cwd": "/tmp"
        }))
        .expect("node_id field should deserialize");

        match msg {
            UiClientMessage::CreateRemoteSession { node_id, cwd, .. } => {
                assert_eq!(node_id, "peer-abc");
                assert_eq!(cwd.as_deref(), Some("/tmp"));
            }
            _ => panic!("expected CreateRemoteSession"),
        }
    }

    #[test]
    fn create_remote_session_accepts_node_alias() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "create_remote_session",
            "node": "peer-abc",
            "cwd": "/tmp"
        }))
        .expect("node alias should deserialize to node_id");

        match msg {
            UiClientMessage::CreateRemoteSession { node_id, cwd, .. } => {
                assert_eq!(node_id, "peer-abc");
                assert_eq!(cwd.as_deref(), Some("/tmp"));
            }
            _ => panic!("expected CreateRemoteSession"),
        }
    }

    #[test]
    fn list_remote_sessions_accepts_node_alias() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "list_remote_sessions",
            "node": "peer-xyz"
        }))
        .expect("node alias should deserialize to node_id");

        match msg {
            UiClientMessage::ListRemoteSessions { node_id } => {
                assert_eq!(node_id, "peer-xyz");
            }
            _ => panic!("expected ListRemoteSessions"),
        }
    }

    #[test]
    fn attach_remote_session_accepts_node_alias() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "attach_remote_session",
            "node": "peer-xyz",
            "session_id": "sess-1"
        }))
        .expect("node alias should deserialize to node_id");

        match msg {
            UiClientMessage::AttachRemoteSession {
                node_id,
                session_id,
            } => {
                assert_eq!(node_id, "peer-xyz");
                assert_eq!(session_id, "sess-1");
            }
            _ => panic!("expected AttachRemoteSession"),
        }
    }

    #[test]
    fn set_session_model_accepts_node_alias() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "set_session_model",
            "session_id": "sess-1",
            "model_id": "claude-3-opus",
            "node": "peer-abc"
        }))
        .expect("node alias should deserialize to node_id");

        match msg {
            UiClientMessage::SetSessionModel {
                session_id,
                model_id,
                node_id,
            } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(model_id, "claude-3-opus");
                assert_eq!(node_id.as_deref(), Some("peer-abc"));
            }
            _ => panic!("expected SetSessionModel"),
        }
    }

    #[test]
    fn remote_sessions_server_msg_serializes_node_id() {
        let msg = UiServerMessage::RemoteSessions {
            node_id: "peer-abc".to_string(),
            sessions: Vec::new(),
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        assert_eq!(json["type"], "remote_sessions");
        assert_eq!(json["node_id"], "peer-abc");
    }

    // ── Plugin update message tests (RED→GREEN) ───────────────────────────────

    #[test]
    fn update_plugins_client_message_deserializes() {
        let msg: UiClientMessage = serde_json::from_value(json!({
            "type": "update_plugins"
        }))
        .expect("update_plugins should deserialize");
        assert!(matches!(msg, UiClientMessage::UpdatePlugins));
    }

    #[test]
    fn plugin_update_status_server_message_serializes() {
        let msg = UiServerMessage::PluginUpdateStatus {
            plugin_name: "my-plugin".to_string(),
            image_reference: "ghcr.io/org/plugin:latest".to_string(),
            phase: "downloading".to_string(),
            bytes_downloaded: 1024,
            bytes_total: Some(4096),
            percent: Some(25.0),
            message: None,
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        assert_eq!(json["type"], "plugin_update_status");
        assert_eq!(json["plugin_name"], "my-plugin");
        assert_eq!(json["phase"], "downloading");
        assert_eq!(json["bytes_downloaded"], 1024);
        assert_eq!(json["bytes_total"], 4096);
        assert!((json["percent"].as_f64().unwrap() - 25.0).abs() < 0.01);
        assert!(json["message"].is_null());
    }

    #[test]
    fn plugin_update_complete_server_message_serializes() {
        let msg = UiServerMessage::PluginUpdateComplete {
            results: vec![
                PluginUpdateResult {
                    plugin_name: "ok-plugin".to_string(),
                    success: true,
                    message: None,
                },
                PluginUpdateResult {
                    plugin_name: "bad-plugin".to_string(),
                    success: false,
                    message: Some("network error".to_string()),
                },
            ],
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        assert_eq!(json["type"], "plugin_update_complete");
        let results = json["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["plugin_name"], "ok-plugin");
        assert_eq!(results[0]["success"], true);
        assert_eq!(results[1]["success"], false);
        assert_eq!(results[1]["message"], "network error");
    }

    #[test]
    fn plugin_update_status_with_failed_message_serializes() {
        let msg = UiServerMessage::PluginUpdateStatus {
            plugin_name: "err-plugin".to_string(),
            image_reference: "ghcr.io/org/plugin:v1".to_string(),
            phase: "failed".to_string(),
            bytes_downloaded: 512,
            bytes_total: None,
            percent: None,
            message: Some("connection refused".to_string()),
        };
        let json = serde_json::to_value(&msg).expect("should serialize");
        assert_eq!(json["type"], "plugin_update_status");
        assert_eq!(json["message"], "connection refused");
        assert!(json["bytes_total"].is_null());
        assert!(json["percent"].is_null());
    }

    #[test]
    fn serializes_oauth_server_tags_without_extra_underscore() {
        let flow_started = serde_json::to_value(UiServerMessage::OAuthFlowStarted {
            flow_id: "flow-1".to_string(),
            provider: "openai".to_string(),
            authorization_url: "https://example.com".to_string(),
            flow_kind: OAuthFlowKind::RedirectCode,
        })
        .expect("OAuthFlowStarted should serialize");
        assert_eq!(flow_started["type"], "oauth_flow_started");
        assert_eq!(flow_started["flow_kind"], "redirect_code");

        let result = serde_json::to_value(UiServerMessage::OAuthResult {
            provider: "openai".to_string(),
            success: true,
            message: "ok".to_string(),
        })
        .expect("OAuthResult should serialize");
        assert_eq!(result["type"], "oauth_result");
    }
}

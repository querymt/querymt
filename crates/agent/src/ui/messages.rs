//! Wire protocol types for UI WebSocket communication.
//!
//! Contains all message types exchanged between the UI client and server,
//! as well as supporting DTOs for sessions, models, and agents.

use crate::events::AgentEvent;
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

/// Cached model list entry.
#[derive(Debug, Clone, Serialize)]
pub struct ModelEntry {
    pub provider: String,
    pub model: String,
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
}

#[derive(Debug, Clone, Serialize)]
pub struct UndoStackFrame {
    pub message_id: String,
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
        event: AgentEvent,
    },
    SessionEvents {
        session_id: String,
        agent_id: String,
        events: Vec<AgentEvent>,
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
        by_workspace: HashMap<Option<String>, Vec<RecentModelEntry>>,
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
    },
    /// OAuth flow completion result
    #[serde(rename = "oauth_result")]
    OAuthResult {
        provider: String,
        success: bool,
        message: String,
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
            Self::FileIndex { .. } => "file_index",
            Self::LlmConfig { .. } => "llm_config",
            Self::SessionEvents { .. } => "session_events",
            Self::UndoResult { .. } => "undo_result",
            Self::RedoResult { .. } => "redo_result",
            Self::AgentMode { .. } => "agent_mode",
            Self::AuthProviders { .. } => "auth_providers",
            Self::OAuthFlowStarted { .. } => "oauth_flow_started",
            Self::OAuthResult { .. } => "oauth_result",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{UiClientMessage, UiServerMessage};
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

    #[test]
    fn serializes_oauth_server_tags_without_extra_underscore() {
        let flow_started = serde_json::to_value(UiServerMessage::OAuthFlowStarted {
            flow_id: "flow-1".to_string(),
            provider: "openai".to_string(),
            authorization_url: "https://example.com".to_string(),
        })
        .expect("OAuthFlowStarted should serialize");
        assert_eq!(flow_started["type"], "oauth_flow_started");

        let result = serde_json::to_value(UiServerMessage::OAuthResult {
            provider: "openai".to_string(),
            success: true,
            message: "ok".to_string(),
        })
        .expect("OAuthResult should serialize");
        assert_eq!(result["type"], "oauth_result");
    }
}

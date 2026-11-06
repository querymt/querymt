use serde::{Deserialize, Serialize};
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshNodesChangedNotification {
    pub peer_id: String,
    pub change: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshJoinedNotification {
    pub peer_id: String,
    pub transport: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshPeerExpiredNotification {
    pub peer_id: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsChangedNotification {
    pub reason: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulesChangedNotification {
    pub node_id: Option<String>,
    pub session_id: Option<String>,
    pub schedule_public_id: String,
    pub change: String,
    pub schedule: Option<crate::control::schedules::ScheduleInfo>,
}

pub const SESSION_INPUT_STATE_VERSION: u32 = 1;

#[typeshare]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionInputDelivery {
    Steer,
    Queue,
}

#[typeshare]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionInputState {
    Accepted,
    Queued,
    Applied,
    Started,
    Discarded,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInputStateNotification {
    pub version: u32,
    pub session_id: String,
    pub input_id: String,
    pub delivery: SessionInputDelivery,
    pub state: SessionInputState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "number")]
    pub latency_ms: Option<u64>,
}

pub(crate) async fn emit_session_pins_changed_notification(
    agent: &crate::LocalAgentHandle,
    session_id: &str,
) {
    let params = match serde_json::value::RawValue::from_string(
        serde_json::json!({ "session_id": session_id }).to_string(),
    ) {
        Ok(raw) => std::sync::Arc::from(raw),
        Err(_) => return,
    };
    let notification = crate::acp::protocol::ExtNotification::new(
        crate::acp::shared::QMT_NOTIFICATION_SESSION_PINS_CHANGED,
        params,
    );
    agent.broadcast_ext_notification(notification.clone());

    let bridge = agent.bridge.lock().ok().and_then(|guard| guard.clone());
    if let Some(bridge) = bridge
        && let Err(error) = bridge.notify_ext(notification).await
    {
        log::warn!("Failed to send session-pins-changed notification: {error}");
    }
}

/// An MCP server that was left out of a session because its authorization is
/// missing or could not be checked. The session itself still started.
///
/// Carried by the `McpServerAuthRequired` event, by session responses
/// (`_meta.querymt.skippedMcpServers`) and, with the session id, by the
/// `querymt/session/mcpAuthRequired` notification.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedMcpServer {
    pub server_name: String,
    /// Provider id to log in with (`mcp:<server>`).
    pub provider_id: String,
    /// Why authorization could not be used, when it is not simply "no login
    /// yet" (e.g. the authorization server is unreachable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SkippedMcpServer {
    /// No usable credentials: the user has to log in.
    pub fn auth_required(server: &str) -> Self {
        Self {
            server_name: server.to_string(),
            provider_id: crate::mcp_auth::provider_id(server),
            error: None,
        }
    }

    /// Authorization could not be checked; logging in may not help until the
    /// underlying problem is fixed.
    pub fn auth_unavailable(server: &str, reason: &str) -> Self {
        Self {
            error: Some(reason.to_string()),
            ..Self::auth_required(server)
        }
    }
}

pub const MCP_AUTH_REQUIRED_VERSION: u32 = 1;

/// Params of the `querymt/session/mcpAuthRequired` notification.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuthRequiredNotification {
    pub version: u32,
    pub session_id: String,
    pub server_name: String,
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod mcp_auth_tests {
    use super::*;

    #[test]
    fn auth_required_params_keep_their_wire_format() {
        let n = McpAuthRequiredNotification {
            version: MCP_AUTH_REQUIRED_VERSION,
            session_id: "s1".into(),
            server_name: "linear".into(),
            provider_id: "mcp:linear".into(),
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            serde_json::json!({
                "version": 1, "session_id": "s1",
                "server_name": "linear", "provider_id": "mcp:linear",
            })
        );
        let n = McpAuthRequiredNotification {
            error: Some("idp unreachable".into()),
            ..n
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            serde_json::json!({
                "version": 1, "session_id": "s1",
                "server_name": "linear", "provider_id": "mcp:linear",
                "error": "idp unreachable",
            })
        );
    }

    #[test]
    fn skipped_server_wire_format() {
        assert_eq!(
            serde_json::to_value(SkippedMcpServer::auth_required("linear")).unwrap(),
            serde_json::json!({"server_name": "linear", "provider_id": "mcp:linear"})
        );
        assert_eq!(
            serde_json::to_value(SkippedMcpServer::auth_unavailable("corp", "down")).unwrap(),
            serde_json::json!({
                "server_name": "corp", "provider_id": "mcp:corp", "error": "down"
            })
        );
    }
}

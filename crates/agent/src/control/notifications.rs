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

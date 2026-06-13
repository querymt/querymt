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

//! Delegation update notification types (the projector stays in `control`).

use serde::{Deserialize, Serialize};
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationUpdateState {
    Requested,
    Forked,
    Completed,
    Failed,
    Cancelled,
}

impl DelegationUpdateState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStage {
    Queued,
    Executing,
    Verifying,
    Finalizing,
    CancelRequested,
}

#[typeshare]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationActivity {
    Live,
    #[default]
    Historical,
}

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationUpdateNotification {
    pub version: u32,
    pub session_id: String,
    pub delegation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub state: DelegationUpdateState,
    pub target_agent_id: String,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    /// Confirmed child model, not the parent's current preference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_provider_node_id: Option<String>,
    #[typeshare(serialized_as = "number")]
    pub requested_at: i64,
    #[typeshare(serialized_as = "Option<number>")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_at: Option<i64>,
    #[typeshare(serialized_as = "Option<number>")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[typeshare(serialized_as = "number")]
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub activity: DelegationActivity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<DelegationStage>,
    #[typeshare(serialized_as = "Option<number>")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_seq: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_epoch: Option<String>,
}

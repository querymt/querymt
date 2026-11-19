//! Session input-state notifications (queued and steering inputs).

use serde::{Deserialize, Serialize};
use typeshare::typeshare;

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

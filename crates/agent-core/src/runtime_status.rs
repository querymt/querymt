//! Session runtime status types.

use serde::{Deserialize, Serialize};
use typeshare::typeshare;

/// High-level runtime state for stop/resume and steering orchestration.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntimeStatus {
    pub phase: SessionRuntimePhase,
    pub active_run_id: Option<String>,
    pub steerable: bool,
    pub pending_steering_count: u32,
    pub queued_input_count: u32,
    #[typeshare(serialized_as = "Option<number>")]
    pub run_started_at_ms: Option<u64>,
    pub observation: RuntimeObservation,
    #[serde(default)]
    pub delegation_activity: Vec<crate::delegation_update::DelegationUpdateNotification>,
}

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeObservation {
    pub availability: RuntimeAvailability,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    #[typeshare(serialized_as = "Option<number>")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAvailability {
    Live,
    Inactive,
    Unavailable,
}

impl RuntimeObservation {
    pub const INACTIVE: Self = Self {
        availability: RuntimeAvailability::Inactive,
        epoch: None,
        revision: None,
        reason: None,
    };
}

#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntimePhase {
    Unknown,
    Idle,
    Starting,
    Model,
    Tools,
    Waiting,
    Closing,
    CancelRequested,
}

impl SessionRuntimeStatus {
    pub fn unavailable(reason: &str) -> Self {
        Self {
            phase: SessionRuntimePhase::Unknown,
            observation: RuntimeObservation {
                availability: RuntimeAvailability::Unavailable,
                reason: Some(reason.to_string()),
                ..RuntimeObservation::INACTIVE
            },
            ..Self::Idle
        }
    }

    #[allow(non_upper_case_globals)]
    pub const Idle: Self = Self {
        phase: SessionRuntimePhase::Idle,
        active_run_id: None,
        steerable: false,
        pending_steering_count: 0,
        queued_input_count: 0,
        run_started_at_ms: None,
        observation: RuntimeObservation::INACTIVE,
        delegation_activity: Vec::new(),
    };

    #[allow(non_upper_case_globals)]
    pub const Running: Self = Self {
        phase: SessionRuntimePhase::Model,
        active_run_id: None,
        steerable: false,
        pending_steering_count: 0,
        queued_input_count: 0,
        run_started_at_ms: None,
        observation: RuntimeObservation::INACTIVE,
        delegation_activity: Vec::new(),
    };

    #[allow(non_upper_case_globals)]
    pub const Waiting: Self = Self {
        phase: SessionRuntimePhase::Waiting,
        active_run_id: None,
        steerable: false,
        pending_steering_count: 0,
        queued_input_count: 0,
        run_started_at_ms: None,
        observation: RuntimeObservation::INACTIVE,
        delegation_activity: Vec::new(),
    };

    #[allow(non_upper_case_globals)]
    pub const CancelRequested: Self = Self {
        phase: SessionRuntimePhase::CancelRequested,
        active_run_id: None,
        steerable: false,
        pending_steering_count: 0,
        queued_input_count: 0,
        run_started_at_ms: None,
        observation: RuntimeObservation::INACTIVE,
        delegation_activity: Vec::new(),
    };
}

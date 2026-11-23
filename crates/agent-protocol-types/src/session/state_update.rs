//! Run state notifications (`querymt/session/stateUpdate`).
//!
//! Shaped on ACP v2 `state_update`: the foreground work of a session is
//! `running`, `requires_action` or `idle`. The agent derives it from the event
//! stream (run start and end, blocking permission and elicitation waits), not
//! from the coalescing runtime status, so every transition is delivered.

use crate::protocol::StopReason;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

/// Extension notification method carrying a [`SessionStateUpdate`].
pub const SESSION_STATE_UPDATE_METHOD: &str = "querymt/session/stateUpdate";

/// Foreground work state of a session.
#[typeshare]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionRunState {
    /// A run is executing.
    Running,
    /// The run waits for the user (permission or question).
    RequiresAction,
    /// No run is executing.
    Idle,
}

/// Params of `querymt/session/stateUpdate`.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateUpdate {
    pub session_id: String,
    pub state: SessionRunState,
    pub run_id: String,
    /// ACP stop reason of the run; set with `idle` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "string")]
    pub stop_reason: Option<StopReason>,
    /// Host time of the transition in milliseconds since the epoch (the
    /// event's timestamp). Absent on the current-state sync after a load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<number>")]
    pub at_ms: Option<i64>,
}

impl SessionStateUpdate {
    /// Set the host time of the transition.
    pub fn at(mut self, at_ms: i64) -> Self {
        self.at_ms = Some(at_ms);
        self
    }

    pub fn running(session_id: &str, run_id: &str) -> Self {
        Self::new(session_id, SessionRunState::Running, run_id, None)
    }

    pub fn requires_action(session_id: &str, run_id: &str) -> Self {
        Self::new(session_id, SessionRunState::RequiresAction, run_id, None)
    }

    pub fn idle(session_id: &str, run_id: &str, stop_reason: StopReason) -> Self {
        Self::new(session_id, SessionRunState::Idle, run_id, Some(stop_reason))
    }

    fn new(
        session_id: &str,
        state: SessionRunState,
        run_id: &str,
        stop_reason: Option<StopReason>,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            state,
            run_id: run_id.to_string(),
            stop_reason,
            at_ms: None,
        }
    }
}

/// Map a run's recorded outcome to the ACP stop reason reported with `idle`.
pub fn stop_reason_for_outcome(
    outcome: &str,
    stop_type: Option<crate::events::StopType>,
) -> StopReason {
    match outcome {
        "cancelled" => StopReason::Cancelled,
        "stopped" => stop_type.map_or(StopReason::EndTurn, StopReason::from),
        // completed, failed and interrupted runs end the turn.
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::StopType;

    #[test]
    fn wire_shape_is_camel_case_with_snake_case_state() {
        let update = SessionStateUpdate::idle("s1", "r1", StopReason::EndTurn);
        assert_eq!(
            serde_json::to_value(&update).unwrap(),
            serde_json::json!({
                "sessionId": "s1", "state": "idle", "runId": "r1", "stopReason": "end_turn"
            })
        );
        let waiting = SessionStateUpdate::requires_action("s1", "r1");
        assert_eq!(
            serde_json::to_value(&waiting).unwrap(),
            serde_json::json!({"sessionId": "s1", "state": "requires_action", "runId": "r1"})
        );
        let timed = SessionStateUpdate::running("s1", "r1").at(10_000);
        assert_eq!(
            serde_json::to_value(&timed).unwrap(),
            serde_json::json!({"sessionId": "s1", "state": "running", "runId": "r1", "atMs": 10000})
        );
    }

    #[test]
    fn outcomes_map_to_stop_reasons() {
        assert_eq!(
            stop_reason_for_outcome("completed", None),
            StopReason::EndTurn
        );
        assert_eq!(
            stop_reason_for_outcome("cancelled", None),
            StopReason::Cancelled
        );
        assert_eq!(
            stop_reason_for_outcome("stopped", Some(StopType::PriceLimit)),
            StopReason::MaxTokens
        );
        assert_eq!(stop_reason_for_outcome("failed", None), StopReason::EndTurn);
    }
}

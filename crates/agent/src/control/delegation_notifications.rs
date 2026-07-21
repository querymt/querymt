use crate::events::{AgentEvent, AgentEventKind, EventEnvelope};
use crate::session::domain::ForkOrigin;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use typeshare::typeshare;

const TERMINAL_SUMMARY_MAX_CHARS: usize = 512;

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

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationUpdateNotification {
    pub version: u32,
    pub session_id: String,
    pub delegation_id: String,
    pub tool_call_id: Option<String>,
    pub state: DelegationUpdateState,
    pub target_agent_id: String,
    pub objective: String,
    pub child_session_id: Option<String>,
    #[typeshare(serialized_as = "number")]
    pub requested_at: i64,
    #[typeshare(serialized_as = "Option<number>")]
    pub forked_at: Option<i64>,
    #[typeshare(serialized_as = "Option<number>")]
    pub finished_at: Option<i64>,
    #[typeshare(serialized_as = "number")]
    pub updated_at: i64,
    pub result_summary: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DelegationUpdateProjector {
    snapshots: HashMap<String, DelegationUpdateNotification>,
    pending: HashMap<String, Vec<PendingDelegationTransition>>,
}

#[derive(Debug, Clone)]
enum PendingDelegationTransition {
    Forked {
        session_id: String,
        child_session_id: String,
        timestamp: i64,
    },
    Finished {
        timestamp: i64,
        state: DelegationUpdateState,
        result_summary: Option<String>,
        error: Option<String>,
    },
}

impl DelegationUpdateProjector {
    pub fn project_envelope(
        &mut self,
        event: &EventEnvelope,
    ) -> Option<DelegationUpdateNotification> {
        self.project(event.session_id(), event.timestamp(), event.kind())
    }

    pub fn project_event(&mut self, event: &AgentEvent) -> Option<DelegationUpdateNotification> {
        self.project(&event.session_id, event.timestamp, &event.kind)
    }

    pub fn snapshots(&self) -> Vec<DelegationUpdateNotification> {
        let mut snapshots = self.snapshots.values().cloned().collect::<Vec<_>>();
        snapshots.sort_by(|left, right| {
            left.requested_at
                .cmp(&right.requested_at)
                .then_with(|| left.delegation_id.cmp(&right.delegation_id))
        });
        snapshots
    }

    fn project(
        &mut self,
        session_id: &str,
        timestamp: i64,
        kind: &AgentEventKind,
    ) -> Option<DelegationUpdateNotification> {
        match kind {
            AgentEventKind::DelegationRequested {
                delegation,
                tool_call_id,
            } => {
                if let Some(snapshot) = self.snapshots.get_mut(&delegation.public_id) {
                    if snapshot.tool_call_id.is_none() {
                        snapshot.tool_call_id = tool_call_id.clone();
                    }
                    return Some(snapshot.clone());
                }

                let snapshot = DelegationUpdateNotification {
                    version: 1,
                    session_id: session_id.to_string(),
                    delegation_id: delegation.public_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    state: DelegationUpdateState::Requested,
                    target_agent_id: delegation.target_agent_id.clone(),
                    objective: delegation.objective.clone(),
                    child_session_id: None,
                    requested_at: timestamp,
                    forked_at: None,
                    finished_at: None,
                    updated_at: timestamp,
                    result_summary: None,
                    error: None,
                };
                self.snapshots
                    .insert(delegation.public_id.clone(), snapshot.clone());
                if let Some(transitions) = self.pending.remove(&delegation.public_id) {
                    for transition in transitions {
                        self.apply_transition(&delegation.public_id, transition);
                    }
                }
                self.snapshots.get(&delegation.public_id).cloned()
            }
            AgentEventKind::SessionForked {
                parent_session_id,
                child_session_id,
                origin: ForkOrigin::Delegation,
                fork_point_ref,
                ..
            } => {
                let transition = PendingDelegationTransition::Forked {
                    session_id: parent_session_id.clone(),
                    child_session_id: child_session_id.clone(),
                    timestamp,
                };
                if !self.apply_transition(fork_point_ref, transition.clone()) {
                    self.pending
                        .entry(fork_point_ref.clone())
                        .or_default()
                        .push(transition);
                    return None;
                }
                self.snapshots.get(fork_point_ref).cloned()
            }
            AgentEventKind::DelegationCompleted {
                delegation_id,
                result,
            } => self.finish(
                delegation_id,
                timestamp,
                DelegationUpdateState::Completed,
                result.as_deref().map(bounded_summary),
                None,
            ),
            AgentEventKind::DelegationFailed {
                delegation_id,
                error,
            } => self.finish(
                delegation_id,
                timestamp,
                DelegationUpdateState::Failed,
                None,
                Some(bounded_summary(error)),
            ),
            AgentEventKind::DelegationCancelled { delegation_id } => self.finish(
                delegation_id,
                timestamp,
                DelegationUpdateState::Cancelled,
                None,
                None,
            ),
            _ => None,
        }
    }

    fn finish(
        &mut self,
        delegation_id: &str,
        timestamp: i64,
        state: DelegationUpdateState,
        result_summary: Option<String>,
        error: Option<String>,
    ) -> Option<DelegationUpdateNotification> {
        let transition = PendingDelegationTransition::Finished {
            timestamp,
            state,
            result_summary,
            error,
        };
        if !self.apply_transition(delegation_id, transition.clone()) {
            self.pending
                .entry(delegation_id.to_string())
                .or_default()
                .push(transition);
            return None;
        }
        self.snapshots.get(delegation_id).cloned()
    }

    fn apply_transition(
        &mut self,
        delegation_id: &str,
        transition: PendingDelegationTransition,
    ) -> bool {
        let Some(snapshot) = self.snapshots.get_mut(delegation_id) else {
            return false;
        };
        match transition {
            PendingDelegationTransition::Forked {
                session_id,
                child_session_id,
                timestamp,
            } => {
                snapshot.session_id = session_id;
                snapshot.state = DelegationUpdateState::Forked;
                snapshot.child_session_id = Some(child_session_id);
                snapshot.forked_at.get_or_insert(timestamp);
                snapshot.finished_at = None;
                snapshot.updated_at = timestamp;
                snapshot.result_summary = None;
                snapshot.error = None;
            }
            PendingDelegationTransition::Finished {
                timestamp,
                state,
                result_summary,
                error,
            } => {
                snapshot.state = state;
                snapshot.finished_at = Some(timestamp);
                snapshot.updated_at = timestamp;
                snapshot.result_summary = result_summary;
                snapshot.error = error;
            }
        }
        true
    }
}

pub fn delegation_updates_from_events(events: &[AgentEvent]) -> Vec<DelegationUpdateNotification> {
    let mut projector = DelegationUpdateProjector::default();
    for event in events {
        projector.project_event(event);
    }
    projector.snapshots()
}

fn bounded_summary(value: &str) -> String {
    let mut chars = value.chars();
    let summary = chars
        .by_ref()
        .take(TERMINAL_SUMMARY_MAX_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        let mut bounded = summary
            .chars()
            .take(TERMINAL_SUMMARY_MAX_CHARS.saturating_sub(3))
            .collect::<String>();
        bounded.push_str("...");
        bounded
    } else {
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{AgentEvent, EventOrigin};
    use crate::session::domain::{Delegation, DelegationStatus, ForkPointType};
    use time::OffsetDateTime;

    fn delegation() -> Delegation {
        Delegation {
            id: 0,
            public_id: "delegation-1".into(),
            session_id: 0,
            task_id: None,
            target_agent_id: "coder".into(),
            objective: "Implement it".into(),
            objective_hash: crate::hash::RapidHash::default(),
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: None,
            planning_summary: None,
            status: DelegationStatus::Requested,
            retry_count: 0,
            created_at: OffsetDateTime::UNIX_EPOCH,
            completed_at: None,
        }
    }

    fn event(timestamp: i64, kind: AgentEventKind) -> AgentEvent {
        AgentEvent {
            seq: timestamp,
            timestamp,
            session_id: "parent-1".into(),
            origin: EventOrigin::Local,
            source_node: None,
            kind,
        }
    }

    #[test]
    fn projects_full_idempotent_lifecycle_snapshots() {
        let requested = event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: Some("call-1".into()),
            },
        );
        let forked = event(
            2,
            AgentEventKind::SessionForked {
                parent_session_id: "parent-1".into(),
                child_session_id: "child-1".into(),
                target_agent_id: "coder".into(),
                origin: ForkOrigin::Delegation,
                fork_point_type: ForkPointType::MessageIndex,
                fork_point_ref: "delegation-1".into(),
                instructions: None,
            },
        );
        let completed = event(
            3,
            AgentEventKind::DelegationCompleted {
                delegation_id: "delegation-1".into(),
                result: Some("done".into()),
            },
        );
        let mut projector = DelegationUpdateProjector::default();

        let first = projector.project_event(&requested).unwrap();
        assert_eq!(first.state, DelegationUpdateState::Requested);
        assert_eq!(first.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(projector.project_event(&requested), Some(first));

        let forked = projector.project_event(&forked).unwrap();
        assert_eq!(forked.state, DelegationUpdateState::Forked);
        assert_eq!(forked.child_session_id.as_deref(), Some("child-1"));
        assert_eq!(forked.target_agent_id, "coder");

        let completed = projector.project_event(&completed).unwrap();
        assert_eq!(completed.state, DelegationUpdateState::Completed);
        assert_eq!(completed.child_session_id.as_deref(), Some("child-1"));
        assert_eq!(completed.result_summary.as_deref(), Some("done"));
        assert_eq!(completed.finished_at, Some(3));
        assert_eq!(projector.project_event(&requested), Some(completed));
    }

    #[test]
    fn concurrent_delegations_with_same_target_remain_distinct() {
        let mut second = delegation();
        second.public_id = "delegation-2".into();
        let mut projector = DelegationUpdateProjector::default();
        projector.project_event(&event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: Some("call-1".into()),
            },
        ));
        projector.project_event(&event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: second,
                tool_call_id: Some("call-2".into()),
            },
        ));

        let snapshots = projector.snapshots();
        assert_eq!(snapshots.len(), 2);
        assert_ne!(snapshots[0].delegation_id, snapshots[1].delegation_id);
        assert_ne!(snapshots[0].tool_call_id, snapshots[1].tool_call_id);
    }

    #[test]
    fn terminal_summaries_are_utf8_safe_and_bounded() {
        let mut projector = DelegationUpdateProjector::default();
        projector.project_event(&event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: None,
            },
        ));
        let update = projector
            .project_event(&event(
                2,
                AgentEventKind::DelegationFailed {
                    delegation_id: "delegation-1".into(),
                    error: "界".repeat(600),
                },
            ))
            .unwrap();

        let error = update.error.unwrap();
        assert_eq!(error.chars().count(), TERMINAL_SUMMARY_MAX_CHARS);
        assert!(error.ends_with("..."));
    }

    #[test]
    fn projection_recovers_when_fanout_delivers_fork_before_request() {
        let mut projector = DelegationUpdateProjector::default();
        projector.project_event(&event(
            2,
            AgentEventKind::SessionForked {
                parent_session_id: "parent-1".into(),
                child_session_id: "child-1".into(),
                target_agent_id: "coder".into(),
                origin: ForkOrigin::Delegation,
                fork_point_type: ForkPointType::ProgressEntry,
                fork_point_ref: "delegation-1".into(),
                instructions: None,
            },
        ));

        let update = projector
            .project_event(&event(
                1,
                AgentEventKind::DelegationRequested {
                    delegation: delegation(),
                    tool_call_id: Some("call-1".into()),
                },
            ))
            .unwrap();

        assert_eq!(update.state, DelegationUpdateState::Forked);
        assert_eq!(update.child_session_id.as_deref(), Some("child-1"));
        assert_eq!(update.tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn load_snapshot_projection_keeps_latest_state_only() {
        let events = vec![
            event(
                1,
                AgentEventKind::DelegationRequested {
                    delegation: delegation(),
                    tool_call_id: Some("call-1".into()),
                },
            ),
            event(
                2,
                AgentEventKind::SessionForked {
                    parent_session_id: "parent-1".into(),
                    child_session_id: "child-1".into(),
                    target_agent_id: "coder".into(),
                    origin: ForkOrigin::Delegation,
                    fork_point_type: ForkPointType::ProgressEntry,
                    fork_point_ref: "delegation-1".into(),
                    instructions: None,
                },
            ),
            event(
                3,
                AgentEventKind::DelegationCompleted {
                    delegation_id: "delegation-1".into(),
                    result: Some("done".into()),
                },
            ),
        ];

        let updates = delegation_updates_from_events(&events);

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].state, DelegationUpdateState::Completed);
        assert_eq!(updates[0].child_session_id.as_deref(), Some("child-1"));
        assert_eq!(updates[0].tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn live_translator_emits_camel_case_extension_notification() {
        let envelope = EventEnvelope::from(event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: Some("call-1".into()),
            },
        ));
        let mut translator = crate::acp::shared::AcpLiveEventTranslator::new();

        let notification = translator.translate_notification(&envelope).unwrap();

        assert_eq!(
            notification["method"],
            crate::acp::shared::QMT_NOTIFICATION_DELEGATION_UPDATE
        );
        assert_eq!(notification["params"]["delegationId"], "delegation-1");
        assert_eq!(notification["params"]["toolCallId"], "call-1");
        assert_eq!(notification["params"]["state"], "requested");
        assert!(notification["params"]["childSessionId"].is_null());
        assert!(notification["params"]["resultSummary"].is_null());
    }

    #[test]
    fn historical_requested_event_without_tool_call_id_deserializes() {
        let value = serde_json::to_value(AgentEventKind::DelegationRequested {
            delegation: delegation(),
            tool_call_id: None,
        })
        .unwrap();
        let mut value = value.as_object().unwrap().clone();
        value.remove("tool_call_id");

        let decoded: AgentEventKind =
            serde_json::from_value(serde_json::Value::Object(value)).unwrap();
        assert!(matches!(
            decoded,
            AgentEventKind::DelegationRequested {
                tool_call_id: None,
                ..
            }
        ));
    }
}

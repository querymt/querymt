use crate::events::{AgentEvent, AgentEventKind, EventEnvelope};
use crate::session::domain::ForkOrigin;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use typeshare::typeshare;

const TERMINAL_SUMMARY_MAX_CHARS: usize = 512;
const LIVE_MAX_TRACKED_DELEGATIONS: usize = 1024;

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
    fn is_terminal(self) -> bool {
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

#[derive(Debug, Clone, Default)]
pub struct DelegationUpdateProjector {
    snapshots: HashMap<String, DelegationUpdateNotification>,
    pending: HashMap<String, Vec<PendingDelegationTransition>>,
    stage_order: HashMap<String, (String, i64)>,
    max_tracked: Option<usize>,
}

#[derive(Debug, Clone)]
enum PendingDelegationTransition {
    Forked {
        session_id: String,
        child_session_id: String,
        selected_model_id: Option<String>,
        selected_provider_node_id: Option<String>,
        timestamp: i64,
    },
    Stage {
        timestamp: i64,
        stage: Option<DelegationStage>,
        source_epoch: String,
    },
    Finished {
        timestamp: i64,
        state: DelegationUpdateState,
        result_summary: Option<String>,
        error: Option<String>,
    },
}

impl PendingDelegationTransition {
    fn timestamp(&self) -> i64 {
        match self {
            Self::Forked { timestamp, .. }
            | Self::Finished { timestamp, .. }
            | Self::Stage { timestamp, .. } => *timestamp,
        }
    }

    fn same_kind(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Forked { .. }, Self::Forked { .. })
                | (Self::Finished { .. }, Self::Finished { .. })
                | (Self::Stage { .. }, Self::Stage { .. })
        )
    }
}

impl DelegationUpdateProjector {
    pub fn for_live_stream() -> Self {
        Self {
            max_tracked: Some(LIVE_MAX_TRACKED_DELEGATIONS),
            ..Self::default()
        }
    }

    pub fn project_envelope(
        &mut self,
        event: &EventEnvelope,
    ) -> Option<DelegationUpdateNotification> {
        let id = match event.kind() {
            AgentEventKind::DelegationRequested { delegation, .. } => {
                Some(delegation.public_id.as_str())
            }
            AgentEventKind::SessionForked {
                origin: ForkOrigin::Delegation,
                fork_point_ref,
                ..
            } => Some(fork_point_ref.as_str()),
            AgentEventKind::DelegationStageChanged { delegation_id, .. }
            | AgentEventKind::DelegationCompleted { delegation_id, .. }
            | AgentEventKind::DelegationFailed { delegation_id, .. }
            | AgentEventKind::DelegationCancelled { delegation_id } => Some(delegation_id.as_str()),
            _ => None,
        };
        // Replayed history cannot alter an already newer owned observation at the same order.
        if let Some(current) = id.and_then(|id| self.snapshots.get(id))
            && current.source_epoch.is_some()
            && event.session_seq() > 0
            && current
                .session_seq
                .is_some_and(|seq| event.session_seq() <= seq)
        {
            return None;
        }
        if let AgentEventKind::DelegationStageChanged {
            delegation_id,
            source_epoch,
            ..
        } = event.kind()
        {
            if event.session_seq() <= 0
                || self
                    .stage_order
                    .get(delegation_id)
                    .is_some_and(|(epoch, seq)| {
                        epoch != source_epoch || event.session_seq() <= *seq
                    })
            {
                return None;
            }
            self.stage_order.insert(
                delegation_id.clone(),
                (source_epoch.clone(), event.session_seq()),
            );
        }
        let update = self.project(event.session_id(), event.timestamp(), event.kind())?;
        if let Some(snapshot) = self.snapshots.get_mut(&update.delegation_id) {
            let stage_seq = self
                .stage_order
                .get(&update.delegation_id)
                .map_or(0, |(_, seq)| *seq);
            let seq = event
                .session_seq()
                .max(stage_seq)
                .max(snapshot.session_seq.unwrap_or(0));
            if seq > 0 {
                snapshot.session_seq = Some(seq);
            }
            return Some(snapshot.clone());
        }
        Some(update)
    }

    pub fn project_event(&mut self, event: &AgentEvent) -> Option<DelegationUpdateNotification> {
        self.project_envelope(&event.clone().into())
    }

    /// Hydrate durable lifecycle facts without treating the latest live stage
    /// watermark as evidence that older, not-yet-seeded metadata was observed.
    pub(crate) fn seed_history(&mut self, session_id: &str, events: &[AgentEvent]) {
        for event in events.iter().filter(|event| event.session_id == session_id) {
            if let AgentEventKind::SessionForked {
                parent_session_id, ..
            } = &event.kind
                && parent_session_id != session_id
            {
                continue;
            }
            let id = match &event.kind {
                AgentEventKind::DelegationRequested { delegation, .. } => &delegation.public_id,
                AgentEventKind::SessionForked {
                    origin: ForkOrigin::Delegation,
                    fork_point_ref,
                    ..
                } => fork_point_ref,
                AgentEventKind::DelegationCompleted { delegation_id, .. }
                | AgentEventKind::DelegationFailed { delegation_id, .. }
                | AgentEventKind::DelegationCancelled { delegation_id } => delegation_id,
                _ => continue,
            };
            let previous = self.snapshots.get(id).cloned();
            if let Some(current) = &previous {
                if current.session_id != event.session_id {
                    continue;
                }
                match &event.kind {
                    AgentEventKind::DelegationRequested {
                        delegation,
                        tool_call_id,
                        ..
                    } if current.target_agent_id != delegation.target_agent_id
                        || current.objective != delegation.objective
                        || current
                            .tool_call_id
                            .as_ref()
                            .zip(tool_call_id.as_ref())
                            .is_some_and(|(current, incoming)| current != incoming) =>
                    {
                        continue;
                    }
                    AgentEventKind::SessionForked {
                        child_session_id,
                        target_agent_id,
                        ..
                    } if current.target_agent_id != *target_agent_id
                        || current
                            .child_session_id
                            .as_ref()
                            .is_some_and(|id| id != child_session_id) =>
                    {
                        continue;
                    }
                    _ => {}
                }
            }
            // This path accepts only persisted lifecycle history. Live envelopes
            // still use project_envelope's sequence and epoch rejection guards.
            self.project(&event.session_id, event.timestamp, &event.kind);
            if let Some(snapshot) = self.snapshots.get_mut(id) {
                if let Some(previous) = previous {
                    snapshot.child_session_id = previous
                        .child_session_id
                        .or(snapshot.child_session_id.take());
                    snapshot.selected_model_id = previous
                        .selected_model_id
                        .or(snapshot.selected_model_id.take());
                    snapshot.selected_provider_node_id = previous
                        .selected_provider_node_id
                        .or(snapshot.selected_provider_node_id.take());
                }
                let stage_seq = self.stage_order.get(id).map_or(0, |(_, seq)| *seq);
                let seq = event
                    .session_seq
                    .max(stage_seq)
                    .max(snapshot.session_seq.unwrap_or(0));
                if seq > 0 {
                    snapshot.session_seq = Some(seq);
                }
            }
        }
    }

    pub(crate) fn restore_observation(&mut self, update: DelegationUpdateNotification) {
        if self
            .snapshots
            .get(&update.delegation_id)
            .is_some_and(|current| {
                current.session_seq.unwrap_or(0) > update.session_seq.unwrap_or(0)
            })
        {
            return;
        }
        if let (Some(epoch), Some(seq)) = (&update.source_epoch, update.session_seq) {
            self.stage_order
                .insert(update.delegation_id.clone(), (epoch.clone(), seq));
        }
        self.snapshots.insert(update.delegation_id.clone(), update);
        self.prune_snapshots();
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
                ..
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
                    selected_model_id: None,
                    selected_provider_node_id: None,
                    requested_at: timestamp,
                    forked_at: None,
                    finished_at: None,
                    updated_at: timestamp,
                    result_summary: None,
                    error: None,
                    activity: DelegationActivity::Historical,
                    stage: None,
                    session_seq: None,
                    source_epoch: None,
                };
                self.snapshots
                    .insert(delegation.public_id.clone(), snapshot.clone());
                if let Some(mut transitions) = self.pending.remove(&delegation.public_id) {
                    transitions.sort_by_key(PendingDelegationTransition::timestamp);
                    for transition in transitions {
                        self.apply_transition(&delegation.public_id, transition);
                    }
                }
                let update = self.snapshots.get(&delegation.public_id).cloned();
                self.prune_snapshots();
                update
            }
            AgentEventKind::SessionForked {
                parent_session_id,
                child_session_id,
                origin: ForkOrigin::Delegation,
                fork_point_ref,
                selected_model_id,
                selected_provider_node_id,
                ..
            } => {
                let transition = PendingDelegationTransition::Forked {
                    session_id: parent_session_id.clone(),
                    child_session_id: child_session_id.clone(),
                    selected_model_id: selected_model_id.clone(),
                    selected_provider_node_id: selected_provider_node_id.clone(),
                    timestamp,
                };
                if !self.apply_transition(fork_point_ref, transition.clone()) {
                    self.queue_pending(fork_point_ref, transition);
                    return None;
                }
                self.snapshots.get(fork_point_ref).cloned()
            }
            AgentEventKind::DelegationStageChanged {
                delegation_id,
                stage,
                source_epoch,
            } => {
                let transition = PendingDelegationTransition::Stage {
                    timestamp,
                    stage: *stage,
                    source_epoch: source_epoch.clone(),
                };
                if !self.apply_transition(delegation_id, transition.clone()) {
                    self.queue_pending(delegation_id, transition);
                    return None;
                }
                self.snapshots.get(delegation_id).cloned()
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
            self.queue_pending(delegation_id, transition);
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
            PendingDelegationTransition::Stage {
                timestamp,
                stage,
                source_epoch,
            } => {
                snapshot.stage = stage;
                snapshot.activity = if stage.is_some() {
                    DelegationActivity::Live
                } else {
                    DelegationActivity::Historical
                };
                snapshot.source_epoch = Some(source_epoch);
                snapshot.updated_at = snapshot.updated_at.max(timestamp);
            }
            PendingDelegationTransition::Forked {
                session_id,
                child_session_id,
                selected_model_id,
                selected_provider_node_id,
                timestamp,
            } => {
                if timestamp < snapshot.requested_at
                    || snapshot
                        .finished_at
                        .is_some_and(|finished_at| timestamp > finished_at)
                {
                    return true;
                }
                snapshot.session_id = session_id;
                snapshot.child_session_id = Some(child_session_id);
                snapshot.selected_model_id = selected_model_id;
                snapshot.selected_provider_node_id = selected_provider_node_id;
                snapshot.forked_at = Some(
                    snapshot
                        .forked_at
                        .map_or(timestamp, |current| current.min(timestamp)),
                );
                if !snapshot.state.is_terminal() {
                    snapshot.state = DelegationUpdateState::Forked;
                    snapshot.updated_at = snapshot.updated_at.max(timestamp);
                }
            }
            PendingDelegationTransition::Finished {
                timestamp,
                state,
                result_summary,
                error,
            } => {
                if timestamp < snapshot.requested_at || snapshot.state.is_terminal() {
                    return true;
                }
                snapshot.state = state;
                snapshot.finished_at = Some(timestamp);
                snapshot.updated_at = snapshot.updated_at.max(timestamp);
                snapshot.result_summary = result_summary;
                snapshot.error = error;
            }
        }
        true
    }

    fn queue_pending(&mut self, delegation_id: &str, transition: PendingDelegationTransition) {
        let transitions = self.pending.entry(delegation_id.to_string()).or_default();
        if let Some(existing) = transitions
            .iter_mut()
            .find(|existing| existing.same_kind(&transition))
        {
            if matches!(transition, PendingDelegationTransition::Stage { .. })
                || transition.timestamp() < existing.timestamp()
            {
                *existing = transition;
            }
        } else {
            transitions.push(transition);
        }
        self.prune_pending();
    }

    fn prune_snapshots(&mut self) {
        let Some(max_tracked) = self.max_tracked else {
            return;
        };
        while self.snapshots.len() > max_tracked {
            let Some(oldest) = self
                .snapshots
                .iter()
                .filter(|(_, snapshot)| snapshot.activity != DelegationActivity::Live)
                .min_by_key(|(_, snapshot)| snapshot.updated_at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.snapshots.remove(&oldest);
            self.stage_order.remove(&oldest);
        }
    }

    fn prune_pending(&mut self) {
        let Some(max_tracked) = self.max_tracked else {
            return;
        };
        while self.pending.len() > max_tracked {
            let Some(oldest) = self
                .pending
                .iter()
                .min_by_key(|(_, transitions)| {
                    transitions
                        .iter()
                        .map(PendingDelegationTransition::timestamp)
                        .max()
                        .unwrap_or(i64::MIN)
                })
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.pending.remove(&oldest);
        }
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
            isolation: Default::default(),
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
            session_seq: 0,
            seq: timestamp,
            timestamp,
            session_id: "parent-1".into(),
            origin: EventOrigin::Local,
            source_node: None,
            kind,
        }
    }

    #[test]
    fn status_consistency_terminal_update_advances_stage_order() {
        let mut projector = DelegationUpdateProjector::for_live_stream();
        projector.project_event(&event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: None,
                batch_id: None,
                cycle_id: None,
            },
        ));
        let mut stage = event(
            2,
            AgentEventKind::DelegationStageChanged {
                delegation_id: "delegation-1".into(),
                stage: Some(DelegationStage::Finalizing),
                source_epoch: "workflow-1".into(),
            },
        );
        stage.session_seq = 10;
        let before = projector.project_envelope(&stage.into()).unwrap();
        let mut fork = event(
            2,
            AgentEventKind::SessionForked {
                parent_session_id: "parent-1".into(),
                child_session_id: "child-1".into(),
                target_agent_id: "coder".into(),
                origin: ForkOrigin::Delegation,
                fork_point_type: ForkPointType::MessageIndex,
                fork_point_ref: "delegation-1".into(),
                instructions: None,
                selected_model_id: None,
                selected_provider_node_id: None,
            },
        );
        fork.session_seq = 11;
        let linked = projector.project_envelope(&fork.clone().into()).unwrap();
        assert!(linked.session_seq > before.session_seq);
        assert_eq!(linked.child_session_id.as_deref(), Some("child-1"));
        let mut terminal = event(
            2,
            AgentEventKind::DelegationCompleted {
                delegation_id: "delegation-1".into(),
                result: Some("done".into()),
            },
        );
        terminal.session_seq = 12;
        let after = projector.project_envelope(&terminal.into()).unwrap();
        assert!(
            after.session_seq > before.session_seq,
            "terminal result cannot share stale stage ordering"
        );
        assert_eq!(after.activity, DelegationActivity::Live);
        assert!(after.session_seq > linked.session_seq);
        assert!(
            projector.project_event(&fork).is_none(),
            "history cannot emit changed data at a stale/equal live order"
        );
        assert_eq!(projector.snapshots()[0], after);
    }

    #[test]
    fn late_stage_cannot_resurrect_cleaned_workflow() {
        let mut projector = DelegationUpdateProjector::for_live_stream();
        projector.project_event(&event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: None,
                batch_id: None,
                cycle_id: None,
            },
        ));
        let stage = |seq, value| {
            EventEnvelope::Ephemeral(crate::events::EphemeralEvent {
                session_id: "parent-1".into(),
                session_seq: seq,
                timestamp: 2,
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::DelegationStageChanged {
                    delegation_id: "delegation-1".into(),
                    stage: value,
                    source_epoch: "workflow-1".into(),
                },
            })
        };
        projector.project_envelope(&stage(10, Some(DelegationStage::Verifying)));
        projector.project_event(&event(
            2,
            AgentEventKind::DelegationCancelled {
                delegation_id: "delegation-1".into(),
            },
        ));
        assert_eq!(
            projector.snapshots()[0].activity,
            DelegationActivity::Live,
            "terminal record must not hide owned cleanup"
        );
        projector.project_envelope(&stage(12, None));
        assert_eq!(
            projector.snapshots()[0].activity,
            DelegationActivity::Historical
        );
        projector.project_envelope(&stage(11, Some(DelegationStage::Verifying)));
        assert_eq!(
            projector.snapshots()[0].activity,
            DelegationActivity::Historical,
            "late same-timestamp stage must not revive cleanup"
        );
        assert_eq!(projector.snapshots()[0].session_seq, Some(12));
    }

    #[test]
    fn projects_full_idempotent_lifecycle_snapshots() {
        let requested = event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: Some("call-1".into()),
                batch_id: None,
                cycle_id: None,
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
                selected_model_id: Some("provider/model".into()),
                selected_provider_node_id: Some("node".into()),
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
        assert_eq!(forked.selected_model_id.as_deref(), Some("provider/model"));
        assert_eq!(forked.selected_provider_node_id.as_deref(), Some("node"));
        assert_eq!(forked.target_agent_id, "coder");

        let completed = projector.project_event(&completed).unwrap();
        assert_eq!(completed.state, DelegationUpdateState::Completed);
        assert_eq!(completed.child_session_id.as_deref(), Some("child-1"));
        assert_eq!(completed.result_summary.as_deref(), Some("done"));
        assert_eq!(
            completed.selected_model_id.as_deref(),
            Some("provider/model")
        );
        assert_eq!(completed.selected_provider_node_id.as_deref(), Some("node"));
        assert_eq!(completed.finished_at, Some(3));
        assert_eq!(projector.project_event(&requested), Some(completed));
    }

    #[test]
    fn historical_fork_without_model_provenance_projects_unknown() {
        let mut projector = DelegationUpdateProjector::default();
        projector.project_event(&event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: None,
                batch_id: None,
                cycle_id: None,
            },
        ));
        let update = projector
            .project_event(&event(
                2,
                AgentEventKind::SessionForked {
                    parent_session_id: "parent-1".into(),
                    child_session_id: "child-1".into(),
                    target_agent_id: "coder".into(),
                    origin: ForkOrigin::Delegation,
                    fork_point_type: ForkPointType::ProgressEntry,
                    fork_point_ref: "delegation-1".into(),
                    instructions: None,
                    selected_model_id: None,
                    selected_provider_node_id: None,
                },
            ))
            .unwrap();
        assert!(update.selected_model_id.is_none());
        assert!(update.selected_provider_node_id.is_none());
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
                batch_id: None,
                cycle_id: None,
            },
        ));
        projector.project_event(&event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: second,
                tool_call_id: Some("call-2".into()),
                batch_id: None,
                cycle_id: None,
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
                batch_id: None,
                cycle_id: None,
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
                selected_model_id: Some("provider/model".into()),
                selected_provider_node_id: Some("node".into()),
            },
        ));

        let update = projector
            .project_event(&event(
                1,
                AgentEventKind::DelegationRequested {
                    delegation: delegation(),
                    tool_call_id: Some("call-1".into()),
                    batch_id: None,
                    cycle_id: None,
                },
            ))
            .unwrap();

        assert_eq!(update.state, DelegationUpdateState::Forked);
        assert_eq!(update.child_session_id.as_deref(), Some("child-1"));
        assert_eq!(update.tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn pending_transitions_are_applied_in_timestamp_order() {
        let mut projector = DelegationUpdateProjector::default();
        projector.project_event(&event(
            3,
            AgentEventKind::DelegationCompleted {
                delegation_id: "delegation-1".into(),
                result: Some("done".into()),
            },
        ));
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
                selected_model_id: Some("provider/model".into()),
                selected_provider_node_id: Some("node".into()),
            },
        ));

        let update = projector
            .project_event(&event(
                1,
                AgentEventKind::DelegationRequested {
                    delegation: delegation(),
                    tool_call_id: Some("call-1".into()),
                    batch_id: None,
                    cycle_id: None,
                },
            ))
            .unwrap();

        assert_eq!(update.state, DelegationUpdateState::Completed);
        assert_eq!(update.child_session_id.as_deref(), Some("child-1"));
        assert_eq!(update.finished_at, Some(3));
        assert_eq!(update.updated_at, 3);
    }

    #[test]
    fn terminal_state_is_not_regressed_by_out_of_order_fork() {
        let mut projector = DelegationUpdateProjector::default();
        projector.project_event(&event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: Some("call-1".into()),
                batch_id: None,
                cycle_id: None,
            },
        ));
        let completed = projector
            .project_event(&event(
                3,
                AgentEventKind::DelegationCompleted {
                    delegation_id: "delegation-1".into(),
                    result: Some("done".into()),
                },
            ))
            .unwrap();

        let update = projector
            .project_event(&event(
                2,
                AgentEventKind::SessionForked {
                    parent_session_id: "parent-1".into(),
                    child_session_id: "child-1".into(),
                    target_agent_id: "coder".into(),
                    origin: ForkOrigin::Delegation,
                    fork_point_type: ForkPointType::ProgressEntry,
                    fork_point_ref: "delegation-1".into(),
                    instructions: None,
                    selected_model_id: Some("provider/model".into()),
                    selected_provider_node_id: Some("node".into()),
                },
            ))
            .unwrap();

        assert_eq!(update.state, DelegationUpdateState::Completed);
        assert_eq!(update.finished_at, completed.finished_at);
        assert_eq!(update.result_summary, completed.result_summary);
        assert_eq!(update.child_session_id.as_deref(), Some("child-1"));
        assert_eq!(update.selected_model_id.as_deref(), Some("provider/model"));
        assert_eq!(update.selected_provider_node_id.as_deref(), Some("node"));
    }

    #[test]
    fn live_projector_bounds_snapshots_and_pending_transitions() {
        let mut projector = DelegationUpdateProjector {
            max_tracked: Some(2),
            ..DelegationUpdateProjector::default()
        };

        for id in 1..=3 {
            let mut requested = delegation();
            requested.public_id = format!("delegation-{id}");
            projector.project_event(&event(
                id,
                AgentEventKind::DelegationRequested {
                    delegation: requested,
                    tool_call_id: None,
                    batch_id: None,
                    cycle_id: None,
                },
            ));
        }
        assert_eq!(projector.snapshots.len(), 2);
        assert!(!projector.snapshots.contains_key("delegation-1"));

        for id in 4..=6 {
            projector.project_event(&event(
                id,
                AgentEventKind::DelegationCancelled {
                    delegation_id: format!("delegation-{id}"),
                },
            ));
        }
        assert_eq!(projector.pending.len(), 2);
        assert!(!projector.pending.contains_key("delegation-4"));
    }

    #[test]
    fn load_snapshot_projection_keeps_latest_state_only() {
        let events = vec![
            event(
                1,
                AgentEventKind::DelegationRequested {
                    delegation: delegation(),
                    tool_call_id: Some("call-1".into()),
                    batch_id: None,
                    cycle_id: None,
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
                    selected_model_id: Some("provider/model".into()),
                    selected_provider_node_id: Some("node".into()),
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
        assert_eq!(
            updates[0].selected_model_id.as_deref(),
            Some("provider/model")
        );
        assert_eq!(
            updates[0].selected_provider_node_id.as_deref(),
            Some("node")
        );
    }

    #[test]
    fn live_translator_emits_camel_case_extension_notification() {
        let envelope = EventEnvelope::from(event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: Some("call-1".into()),
                batch_id: None,
                cycle_id: None,
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
        assert!(notification["params"].get("childSessionId").is_none());
        assert!(notification["params"].get("resultSummary").is_none());
    }

    #[test]
    fn seeding_history_enriches_lifecycle_behind_live_stage_watermark() {
        let mut requested = event(
            1,
            AgentEventKind::DelegationRequested {
                delegation: delegation(),
                tool_call_id: Some("call-1".into()),
                batch_id: None,
                cycle_id: None,
            },
        );
        requested.session_seq = 1;
        let mut forked = event(
            2,
            AgentEventKind::SessionForked {
                parent_session_id: "parent-1".into(),
                child_session_id: "child-1".into(),
                target_agent_id: "coder".into(),
                origin: ForkOrigin::Delegation,
                fork_point_type: ForkPointType::ProgressEntry,
                fork_point_ref: "delegation-1".into(),
                instructions: None,
                selected_model_id: Some("confirmed-model".into()),
                selected_provider_node_id: None,
            },
        );
        forked.session_seq = 2;
        let mut completed = event(
            3,
            AgentEventKind::DelegationCompleted {
                delegation_id: "delegation-1".into(),
                result: Some("durable outcome".into()),
            },
        );
        completed.session_seq = 3;
        let mut stage = event(
            50,
            AgentEventKind::DelegationStageChanged {
                delegation_id: "delegation-1".into(),
                stage: Some(DelegationStage::Verifying),
                source_epoch: "current-owner".into(),
            },
        );
        stage.session_seq = 50;
        for (terminal, expected) in [
            (None, DelegationUpdateState::Forked),
            (
                Some(completed.kind.clone()),
                DelegationUpdateState::Completed,
            ),
            (
                Some(AgentEventKind::DelegationFailed {
                    delegation_id: "delegation-1".into(),
                    error: "durable failure".into(),
                }),
                DelegationUpdateState::Failed,
            ),
            (
                Some(AgentEventKind::DelegationCancelled {
                    delegation_id: "delegation-1".into(),
                }),
                DelegationUpdateState::Cancelled,
            ),
        ] {
            let mut translator = crate::acp::shared::AcpLiveEventTranslator::new();
            assert!(
                translator
                    .translate_notification(&EventEnvelope::from(stage.clone()))
                    .is_none()
            );
            let mut history = vec![requested.clone(), forked.clone()];
            if let Some(kind) = terminal {
                let mut terminal = completed.clone();
                terminal.kind = kind;
                history.push(terminal);
            }
            let meta = serde_json::Map::from_iter([(
                "querymt/sessionLoadSnapshot.v1".into(),
                serde_json::json!({"audit":{"events":history}}),
            )]);
            let recovered = translator
                .seed_history_from_meta("parent-1", Some(&meta))
                .unwrap();
            assert_eq!(
                recovered.len(),
                1,
                "actual seed_history must deliver enriched lifecycle behind a live watermark, including a live fork"
            );
            assert_eq!(recovered[0].state, expected);
            assert_eq!(
                recovered[0].result_summary.as_deref(),
                (expected == DelegationUpdateState::Completed).then_some("durable outcome")
            );
            assert_eq!(
                recovered[0].error.as_deref(),
                (expected == DelegationUpdateState::Failed).then_some("durable failure")
            );
            assert_eq!(recovered[0].child_session_id.as_deref(), Some("child-1"));
            assert_eq!(
                recovered[0].selected_model_id.as_deref(),
                Some("confirmed-model")
            );
            assert_eq!(recovered[0].session_seq, Some(50));
            assert_eq!(recovered[0].stage, Some(DelegationStage::Verifying));
            let mut next_stage = stage.clone();
            next_stage.session_seq = 51;
            let observed = translator
                .translate_notification(&EventEnvelope::from(next_stage.clone()))
                .unwrap();
            assert_eq!(
                observed["params"]["childSessionId"], "child-1",
                "queued stage must not prevent fork identity hydration"
            );
            assert_eq!(observed["params"]["selectedModelId"], "confirmed-model");
            assert_eq!(
                observed["params"]["state"],
                serde_json::to_value(expected).unwrap()
            );
            assert_eq!(observed["params"]["stage"], "verifying");
            assert_eq!(observed["params"]["sourceEpoch"], "current-owner");
            assert!(
                translator
                    .translate_notification(&EventEnvelope::from(stage.clone()))
                    .is_none(),
                "real stale live stage must still be rejected"
            );
            let mut wrong_epoch = next_stage.clone();
            wrong_epoch.session_seq = 100;
            if let AgentEventKind::DelegationStageChanged { source_epoch, .. } =
                &mut wrong_epoch.kind
            {
                *source_epoch = "other-owner".into();
            }
            assert!(
                translator
                    .translate_notification(&EventEnvelope::from(wrong_epoch))
                    .is_none(),
                "history must not weaken epoch rejection"
            );
            next_stage.session_seq = 52;
            if let AgentEventKind::DelegationStageChanged { stage, .. } = &mut next_stage.kind {
                *stage = None;
            }
            let cleaned = translator
                .translate_notification(&EventEnvelope::from(next_stage))
                .unwrap();
            assert_eq!(cleaned["params"]["activity"], "historical");
            assert!(
                translator
                    .seed_history_from_meta("parent-1", Some(&meta))
                    .unwrap()
                    .is_empty(),
                "repeat history must not revive activity or reemit terminal"
            );
        }
    }

    #[test]
    fn seeding_connection_history_preserves_live_delegation_completion() {
        let history = vec![
            event(
                1,
                AgentEventKind::DelegationRequested {
                    delegation: delegation(),
                    tool_call_id: Some("call-1".into()),
                    batch_id: None,
                    cycle_id: None,
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
                    selected_model_id: None,
                    selected_provider_node_id: None,
                },
            ),
        ];
        let meta = serde_json::Map::from_iter([(
            "querymt/sessionLoadSnapshot.v1".into(),
            serde_json::json!({ "audit": { "events": history } }),
        )]);
        let completed = EventEnvelope::from(event(
            3,
            AgentEventKind::DelegationCompleted {
                delegation_id: "delegation-1".into(),
                result: Some("done".into()),
            },
        ));
        // Completion can arrive while history is being read, before the request is seeded.
        let mut translator = crate::acp::shared::AcpLiveEventTranslator::new();
        assert!(translator.translate_notification(&completed).is_none());
        let recovered = translator
            .seed_history_from_meta("parent-1", Some(&meta))
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].state, DelegationUpdateState::Completed);
        assert_eq!(recovered[0].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(recovered[0].child_session_id.as_deref(), Some("child-1"));

        // Repeated loads must not roll newer terminal state back to the historical fork.
        assert!(
            translator
                .seed_history_from_meta("parent-1", Some(&meta))
                .unwrap()
                .is_empty()
        );
        let repeated = translator
            .translate_notification(&EventEnvelope::from(history[0].clone()))
            .unwrap();
        assert_eq!(repeated["params"]["state"], "completed");

        let mut cleanup = event(
            50,
            AgentEventKind::DelegationStageChanged {
                delegation_id: "delegation-1".into(),
                stage: None,
                source_epoch: "current-owner".into(),
            },
        );
        cleanup.session_seq = 50;
        translator.translate_notification(&cleanup.into()).unwrap();
        let mut enriched_fork = history[1].clone();
        if let AgentEventKind::SessionForked {
            selected_model_id,
            selected_provider_node_id,
            ..
        } = &mut enriched_fork.kind
        {
            *selected_model_id = Some("confirmed-model".into());
            *selected_provider_node_id = Some("confirmed-node".into());
        }
        let mut conflicts = Vec::new();
        let mut foreign = enriched_fork.clone();
        foreign.session_id = "other-parent".into();
        conflicts.push(foreign);
        let mut wrong_parent = enriched_fork.clone();
        if let AgentEventKind::SessionForked {
            parent_session_id, ..
        } = &mut wrong_parent.kind
        {
            *parent_session_id = "other-parent".into();
        }
        conflicts.push(wrong_parent);
        let mut wrong_child = enriched_fork.clone();
        if let AgentEventKind::SessionForked {
            child_session_id, ..
        } = &mut wrong_child.kind
        {
            *child_session_id = "other-child".into();
        }
        conflicts.push(wrong_child);
        let mut wrong_request = history[0].clone();
        if let AgentEventKind::DelegationRequested {
            delegation,
            tool_call_id,
            ..
        } = &mut wrong_request.kind
        {
            delegation.target_agent_id = "other-agent".into();
            *tool_call_id = Some("other-tool".into());
        }
        conflicts.push(wrong_request);
        let bad_meta = serde_json::Map::from_iter([(
            "querymt/sessionLoadSnapshot.v1".into(),
            serde_json::json!({"audit":{"events":conflicts}}),
        )]);
        assert!(
            translator
                .seed_history_from_meta("parent-1", Some(&bad_meta))
                .unwrap()
                .is_empty(),
            "conflicting identity is not enrichment"
        );
        let enrichment = serde_json::Map::from_iter([(
            "querymt/sessionLoadSnapshot.v1".into(),
            serde_json::json!({"audit":{"events":[enriched_fork, event(4, AgentEventKind::DelegationFailed { delegation_id: "delegation-1".into(), error: "must not replace outcome".into() })]}}),
        )]);
        let recovered = translator
            .seed_history_from_meta("parent-1", Some(&enrichment))
            .unwrap();
        assert_eq!(
            recovered.len(),
            1,
            "late missing metadata must be delivered even after cleanup"
        );
        assert_eq!(recovered[0].state, DelegationUpdateState::Completed);
        assert_eq!(recovered[0].result_summary.as_deref(), Some("done"));
        assert_eq!(recovered[0].error, None);
        assert_eq!(recovered[0].child_session_id.as_deref(), Some("child-1"));
        assert_eq!(
            recovered[0].selected_model_id.as_deref(),
            Some("confirmed-model")
        );
        assert_eq!(
            recovered[0].selected_provider_node_id.as_deref(),
            Some("confirmed-node")
        );
        assert_eq!(recovered[0].session_seq, Some(50));
        assert_eq!(recovered[0].source_epoch.as_deref(), Some("current-owner"));
        assert_eq!(recovered[0].activity, DelegationActivity::Historical);
        assert_eq!(recovered[0].stage, None);
        assert!(
            translator
                .seed_history_from_meta("parent-1", Some(&meta))
                .unwrap()
                .is_empty(),
            "old missing model metadata must not erase current identity"
        );
        assert!(
            translator
                .seed_history_from_meta("parent-1", Some(&enrichment))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn load_delivers_completion_buffered_before_history_once() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let session_id = fixture.create_session().await;
        let stream = fixture.config.event_sink.session_stream(&session_id);
        stream.emit(AgentEventKind::DelegationRequested {
            delegation: delegation(),
            tool_call_id: Some("call-1".into()),
            batch_id: None,
            cycle_id: None,
        });
        stream.emit(AgentEventKind::SessionForked {
            parent_session_id: session_id.clone(),
            child_session_id: "child-1".into(),
            target_agent_id: "coder".into(),
            origin: ForkOrigin::Delegation,
            fork_point_type: ForkPointType::ProgressEntry,
            fork_point_ref: "delegation-1".into(),
            instructions: None,
            selected_model_id: None,
            selected_provider_node_id: None,
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while stream.published_tip() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut completed = event(
            i64::MAX,
            AgentEventKind::DelegationCompleted {
                delegation_id: "delegation-1".into(),
                result: Some("done".into()),
            },
        );
        completed.session_id = session_id.clone();
        // Load uses metadata; extension attachment uses the event-stream fallback.
        for method in ["session/load", "querymt/session/runtimeState"] {
            let translator = std::sync::Arc::new(std::sync::Mutex::new(
                crate::acp::shared::AcpLiveEventTranslator::new(),
            ));
            assert!(
                translator
                    .lock()
                    .unwrap()
                    .translate_notification(&EventEnvelope::from(completed.clone()))
                    .is_none()
            );
            let owners = crate::acp::shared::SessionOwnerMap::default();
            let pending = crate::acp::shared::PendingElicitationMap::default();
            let (tx, mut rx) = tokio::sync::mpsc::channel(16);
            let context = crate::acp::shared::RpcDispatchContext {
                translator: Some(translator),
                session_bridge: Some(crate::acp::client_bridge::ClientBridgeSender::new(tx)),
                session_load: Some(std::sync::Arc::new(std::sync::Mutex::new(
                    crate::acp::shared::AcpSessionLoadPreferences {
                        snapshot_history: true,
                    },
                ))),
                ..Default::default()
            };
            for expected_count in [1, 0] {
                let output = crate::acp::shared::handle_rpc_message_with_context(
                    fixture.handle.as_ref(), &owners, &pending, "connection-1",
                    crate::acp::shared::RpcMessage {
                        jsonrpc: "2.0".into(), id: Some(serde_json::json!(1)), method: method.into(),
                        params: if method == "session/load" {
                            serde_json::json!({"sessionId": session_id, "cwd": "/tmp", "mcpServers": []})
                        } else { serde_json::json!({"session_id": session_id}) },
                    }, context.clone(),
                ).await;
                assert!(output.response.unwrap().error.is_none());
                let mut updates: Vec<_> = output
                    .notifications
                    .into_iter()
                    .filter(|notification| {
                        notification["method"] == "querymt/session/delegationUpdate"
                    })
                    .map(|notification| notification["params"].clone())
                    .collect();
                while let Ok(message) = rx.try_recv() {
                    if let crate::acp::client_bridge::ClientBridgeMessage::ExtNotification(
                        notification,
                    ) = message
                    {
                        updates.push(
                            serde_json::from_str::<serde_json::Value>(notification.params.get())
                                .unwrap(),
                        );
                    }
                }
                assert_eq!(updates.len(), expected_count, "{method}");
                if let Some(update) = updates.first() {
                    assert_eq!(update["state"], "completed");
                    assert_eq!(update["toolCallId"], "call-1");
                    assert_eq!(update["childSessionId"], "child-1");
                }
            }
        }
    }

    #[test]
    fn historical_requested_event_without_tool_call_id_deserializes() {
        let value = serde_json::to_value(AgentEventKind::DelegationRequested {
            delegation: delegation(),
            tool_call_id: None,
            batch_id: None,
            cycle_id: None,
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
                batch_id: None,
                cycle_id: None,
                ..
            }
        ));
    }
}

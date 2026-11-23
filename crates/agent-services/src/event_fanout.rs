//! Transport-only event fanout.
//!
//! `EventFanout` delivers events to live subscribers. It does NOT write to
//! storage. This is the "transport boundary" from the refactor plan.

use querymt_agent_core::events::{AgentEventKind, EventEnvelope};
use std::collections::HashMap;
use tokio::sync::broadcast;

const FANOUT_BUFFER: usize = 1024;

/// Capacity of the live broadcast channel: the most events a subscriber can
/// fall behind by before it lags. A connection holding live events during a
/// session load is bounded by the same limit.
pub const LIVE_EVENT_CAPACITY: usize = FANOUT_BUFFER;

/// Reasoning text streamed for one part of an unstored message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflightReasoning {
    pub part_id: Option<String>,
    pub text: String,
}

/// Text streamed for one assistant message that has no stored event yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflightMessage {
    pub message_id: String,
    /// Reasoning parts in arrival order.
    pub reasoning: Vec<InflightReasoning>,
    /// Assistant text streamed so far.
    pub text: String,
}

/// The in-flight text of a session at one point of its event order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflightText {
    /// Highest `session_seq` of a streamed delta folded in: every delta at or
    /// before it is part of `messages`. Other events never move it.
    pub through_seq: i64,
    pub messages: Vec<InflightMessage>,
}

/// Folds streamed deltas per session, so a connection that loads a session
/// mid-stream can be given the text deltas it never saw. Deltas are not
/// journaled; this is the only place the streamed-so-far text exists.
#[derive(Default)]
struct InflightAccumulator {
    sessions: HashMap<String, InflightText>,
}

impl InflightAccumulator {
    fn observe(&mut self, envelope: &EventEnvelope) {
        let session_id = envelope.session_id();
        let seq = envelope.session_seq();
        match envelope.kind() {
            AgentEventKind::AssistantContentDelta {
                content,
                message_id,
            } if !content.is_empty() => {
                let message = self.message(session_id, message_id, seq);
                message.text.push_str(content);
            }
            AgentEventKind::AssistantThinkingDelta {
                content,
                message_id,
                part_id,
            } if !content.is_empty() => {
                let message = self.message(session_id, message_id, seq);
                let part_id = part_id.clone().filter(|id| !id.is_empty());
                let index = match &part_id {
                    Some(id) => message
                        .reasoning
                        .iter()
                        .position(|part| part.part_id.as_ref() == Some(id)),
                    // A chunk without a part id joins the last part.
                    None => message.reasoning.len().checked_sub(1),
                };
                match index {
                    Some(index) => message.reasoning[index].text.push_str(content),
                    None => message.reasoning.push(InflightReasoning {
                        part_id,
                        text: content.clone(),
                    }),
                }
            }
            AgentEventKind::AssistantMessageStored { message_id, .. } => {
                match message_id {
                    Some(message_id) => {
                        if let Some(inflight) = self.sessions.get_mut(session_id) {
                            inflight.messages.retain(|m| &m.message_id != message_id);
                            if inflight.messages.is_empty() {
                                self.sessions.remove(session_id);
                            }
                        }
                    }
                    // A message without an id cannot be matched to what
                    // streamed; the turn's text is stored, so it is over.
                    None => {
                        self.sessions.remove(session_id);
                    }
                }
            }
            // The run ended: whatever was still streaming is over.
            AgentEventKind::RunCompleted { .. } | AgentEventKind::Cancelled => {
                self.sessions.remove(session_id);
            }
            // Only streamed deltas move `through_seq`; other events are not
            // part of the in-flight text.
            _ => {}
        }
    }

    fn message(&mut self, session_id: &str, message_id: &str, seq: i64) -> &mut InflightMessage {
        let inflight = self
            .sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| InflightText {
                through_seq: 0,
                messages: Vec::new(),
            });
        if seq > 0 {
            inflight.through_seq = inflight.through_seq.max(seq);
        }
        let index = match inflight
            .messages
            .iter()
            .position(|m| m.message_id == message_id)
        {
            Some(index) => index,
            None => {
                inflight.messages.push(InflightMessage {
                    message_id: message_id.to_owned(),
                    reasoning: Vec::new(),
                    text: String::new(),
                });
                inflight.messages.len() - 1
            }
        };
        &mut inflight.messages[index]
    }
}

/// Transport-only event fanout.
///
/// Delivers `EventEnvelope` (durable or ephemeral) to live subscribers.
/// No persistence behavior.
pub struct EventFanout {
    sender: broadcast::Sender<EventEnvelope>,
    delegation_sender: broadcast::Sender<(EventEnvelope, Option<tracing::Span>)>,
    delegation_activity:
        parking_lot::Mutex<crate::control::delegation_notifications::DelegationUpdateProjector>,
    inflight: parking_lot::Mutex<InflightAccumulator>,
}

impl EventFanout {
    /// Create a new fanout with a bounded broadcast channel.
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(FANOUT_BUFFER);
        let (delegation_sender, _) = broadcast::channel(FANOUT_BUFFER);
        Self {
            sender,
            delegation_sender,
            delegation_activity: parking_lot::Mutex::new(crate::control::delegation_notifications::DelegationUpdateProjector::for_live_stream()),
            inflight: parking_lot::Mutex::new(InflightAccumulator::default()),
        }
    }

    /// The text a session has streamed for messages that are not stored yet,
    /// or `None` when nothing is streaming. It is folded at publish time, in
    /// the same order subscribers receive events, so `through_seq` tells a
    /// subscriber exactly which deltas the text already contains.
    pub fn inflight_text(&self, session_id: &str) -> Option<InflightText> {
        self.inflight.lock().sessions.get(session_id).cloned()
    }

    /// Subscribe to the live event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.sender.subscribe()
    }

    /// Publish an event envelope to all live subscribers.
    pub fn publish(&self, envelope: EventEnvelope) {
        self.publish_with_context(envelope, None);
    }

    pub(crate) fn is_delegation_event(kind: &AgentEventKind) -> bool {
        matches!(
            kind,
            AgentEventKind::DelegationRequested { .. }
                | AgentEventKind::DelegationCancelRequested { .. }
                | AgentEventKind::Cancelled
        )
    }

    #[doc(hidden)]
    pub fn subscribe_delegation(
        &self,
    ) -> broadcast::Receiver<(EventEnvelope, Option<tracing::Span>)> {
        self.delegation_sender.subscribe()
    }

    // Context belongs only to this bounded live channel, never the journal or public wire.
    pub(crate) fn publish_with_context(
        &self,
        envelope: EventEnvelope,
        parent: Option<tracing::Span>,
    ) {
        self.delegation_activity.lock().project_envelope(&envelope);
        // Folded before the send, under the same order subscribers see.
        self.inflight.lock().observe(&envelope);
        if Self::is_delegation_event(envelope.kind()) {
            let _ = self.delegation_sender.send((envelope.clone(), parent));
        }
        let _ = self.sender.send(envelope);
    }

    #[doc(hidden)]
    pub fn live_delegation_activity(
        &self,
        session_id: &str,
    ) -> Vec<crate::control::delegation_notifications::DelegationUpdateNotification> {
        self.delegation_activity
            .lock()
            .snapshots()
            .into_iter()
            .filter(|update| {
                update.session_id == session_id
                    && update.activity
                        == crate::control::delegation_notifications::DelegationActivity::Live
            })
            .collect()
    }

    /// The running delegation whose child session is `child_session_id`, or
    /// `None` when the session is not the child of a live, unfinished
    /// delegation observed by this node.
    #[doc(hidden)]
    pub fn running_delegation_for_child(
        &self,
        child_session_id: &str,
    ) -> Option<crate::control::delegation_notifications::DelegationUpdateNotification> {
        self.delegation_activity
            .lock()
            .snapshot_for_child(child_session_id)
            .filter(|update| {
                update.activity
                    == crate::control::delegation_notifications::DelegationActivity::Live
                    && !update.state.is_terminal()
            })
    }

    #[doc(hidden)]
    pub fn delegation_snapshots(
        &self,
        session_ids: &[String],
    ) -> Vec<crate::control::delegation_notifications::DelegationUpdateNotification> {
        self.delegation_activity
            .lock()
            .snapshots()
            .into_iter()
            .filter(|update| session_ids.contains(&update.session_id))
            .collect()
    }

    /// Number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count() + self.delegation_sender.receiver_count()
    }
}

impl Default for EventFanout {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt_agent_core::events::{
        AgentEventKind, DurableEvent, EphemeralEvent, EventEnvelope, EventOrigin,
    };

    #[tokio::test]
    async fn new_creates_working_fanout() {
        let fanout = EventFanout::new();
        assert_eq!(fanout.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn subscribe_receives_durable_event() {
        let fanout = EventFanout::new();
        let mut rx = fanout.subscribe();

        let durable = EventEnvelope::Durable(DurableEvent {
            session_seq: 0,
            event_id: "e1".into(),
            stream_seq: 1,
            session_id: "s1".into(),
            timestamp: 100,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        });

        fanout.publish(durable);

        let received = tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        assert!(received.is_durable());
        assert_eq!(received.session_id(), "s1");
    }

    #[tokio::test]
    async fn subscribe_receives_ephemeral_event() {
        let fanout = EventFanout::new();
        let mut rx = fanout.subscribe();

        let ephemeral = EventEnvelope::Ephemeral(EphemeralEvent {
            session_seq: 0,
            session_id: "s2".into(),
            timestamp: 200,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantContentDelta {
                content: "hello".into(),
                message_id: "m1".into(),
            },
        });

        fanout.publish(ephemeral);

        let received = tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        assert!(received.is_ephemeral());
        assert_eq!(received.session_id(), "s2");
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive() {
        let fanout = EventFanout::new();
        let mut rx1 = fanout.subscribe();
        let mut rx2 = fanout.subscribe();

        fanout.publish(EventEnvelope::Durable(DurableEvent {
            session_seq: 0,
            event_id: "e1".into(),
            stream_seq: 1,
            session_id: "s1".into(),
            timestamp: 100,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        }));

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert_eq!(e1.session_id(), e2.session_id());
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_does_not_panic() {
        let fanout = EventFanout::new();
        fanout.publish(EventEnvelope::Ephemeral(EphemeralEvent {
            session_seq: 0,
            session_id: "s1".into(),
            timestamp: 1,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::Cancelled,
        }));
    }

    #[tokio::test]
    async fn subscriber_count_tracks_subscribers() {
        let fanout = EventFanout::new();
        assert_eq!(fanout.subscriber_count(), 0);

        let _rx1 = fanout.subscribe();
        assert_eq!(fanout.subscriber_count(), 1);

        let _rx2 = fanout.subscribe();
        assert_eq!(fanout.subscriber_count(), 2);

        drop(_rx1);
        // Broadcast channel count may not immediately reflect drops,
        // but at least we have 1 remaining.
    }

    fn ephemeral(seq: i64, kind: AgentEventKind) -> EventEnvelope {
        EventEnvelope::Ephemeral(EphemeralEvent {
            session_seq: seq,
            session_id: "s1".into(),
            timestamp: 0,
            origin: EventOrigin::Local,
            source_node: None,
            kind,
        })
    }

    fn content(seq: i64, message_id: &str, text: &str) -> EventEnvelope {
        ephemeral(
            seq,
            AgentEventKind::AssistantContentDelta {
                content: text.into(),
                message_id: message_id.into(),
            },
        )
    }

    fn thinking(seq: i64, message_id: &str, part: Option<&str>, text: &str) -> EventEnvelope {
        ephemeral(
            seq,
            AgentEventKind::AssistantThinkingDelta {
                content: text.into(),
                message_id: message_id.into(),
                part_id: part.map(str::to_owned),
            },
        )
    }

    #[test]
    fn inflight_text_folds_deltas_per_message_and_part() {
        let fanout = EventFanout::new();
        fanout.publish(thinking(1, "m1", Some("p1"), "think "));
        fanout.publish(thinking(2, "m1", Some("p1"), "more"));
        fanout.publish(thinking(3, "m1", Some("p2"), "second"));
        fanout.publish(content(4, "m1", "hel"));
        fanout.publish(content(5, "m1", "lo"));

        let inflight = fanout.inflight_text("s1").expect("text is streaming");
        assert_eq!(inflight.through_seq, 5);
        assert_eq!(inflight.messages.len(), 1);
        let message = &inflight.messages[0];
        assert_eq!(message.text, "hello");
        let parts: Vec<_> = message
            .reasoning
            .iter()
            .map(|p| (p.part_id.as_deref(), p.text.as_str()))
            .collect();
        assert_eq!(parts, [(Some("p1"), "think more"), (Some("p2"), "second")]);
        assert_eq!(fanout.inflight_text("other"), None);
    }

    #[test]
    fn inflight_text_ends_with_the_stored_message_or_the_run() {
        let fanout = EventFanout::new();
        fanout.publish(content(1, "m1", "a"));
        fanout.publish(content(2, "m2", "b"));
        fanout.publish(ephemeral(
            3,
            AgentEventKind::AssistantMessageStored {
                content: "a".into(),
                thinking: None,
                reasoning_parts: Vec::new(),
                message_id: Some("m1".into()),
            },
        ));
        let inflight = fanout.inflight_text("s1").unwrap();
        let ids: Vec<_> = inflight
            .messages
            .iter()
            .map(|m| m.message_id.as_str())
            .collect();
        assert_eq!(ids, ["m2"]);
        // only streamed deltas move the position, not the stored message
        assert_eq!(inflight.through_seq, 2);

        fanout.publish(ephemeral(
            4,
            AgentEventKind::RunCompleted {
                run_id: "r".into(),
                outcome: "completed".into(),
                stop_type: None,
                reason: None,
            },
        ));
        assert_eq!(fanout.inflight_text("s1"), None);
    }

    #[test]
    fn inflight_through_seq_ignores_other_events_and_idless_store_ends_the_text() {
        let fanout = EventFanout::new();
        fanout.publish(content(1, "m1", "a"));
        fanout.publish(ephemeral(
            7,
            AgentEventKind::WorktreeCommandOutput { text: "out".into() },
        ));
        assert_eq!(fanout.inflight_text("s1").unwrap().through_seq, 1);

        fanout.publish(ephemeral(
            8,
            AgentEventKind::AssistantMessageStored {
                content: "a".into(),
                thinking: None,
                reasoning_parts: Vec::new(),
                message_id: None,
            },
        ));
        assert_eq!(fanout.inflight_text("s1"), None);
    }

    #[tokio::test]
    async fn default_creates_same_as_new() {
        let f1 = EventFanout::new();
        let f2 = EventFanout::default();
        assert_eq!(f1.subscriber_count(), f2.subscriber_count());
    }
}

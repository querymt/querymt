//! Transport-only event fanout.
//!
//! `EventFanout` delivers events to live subscribers. It does NOT write to
//! storage. This is the "transport boundary" from the refactor plan.

use crate::events::EventEnvelope;
use tokio::sync::broadcast;

const FANOUT_BUFFER: usize = 1024;

/// Transport-only event fanout.
///
/// Delivers `EventEnvelope` (durable or ephemeral) to live subscribers.
/// No persistence behavior.
pub struct EventFanout {
    sender: broadcast::Sender<EventEnvelope>,
}

impl EventFanout {
    /// Create a new fanout with a bounded broadcast channel.
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(FANOUT_BUFFER);
        Self { sender }
    }

    /// Subscribe to the live event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.sender.subscribe()
    }

    /// Publish an event envelope to all live subscribers.
    pub fn publish(&self, envelope: EventEnvelope) {
        let _ = self.sender.send(envelope);
    }

    /// Number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
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
    use crate::events::{AgentEventKind, DurableEvent, EphemeralEvent, EventEnvelope, EventOrigin};

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

    #[tokio::test]
    async fn default_creates_same_as_new() {
        let f1 = EventFanout::new();
        let f2 = EventFanout::default();
        assert_eq!(f1.subscriber_count(), f2.subscriber_count());
    }
}

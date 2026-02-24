//! Event relay actor — receives events from remote sessions and republishes
//! them to the local event bus.
//!
//! Lives on the **local** machine. A remote `SessionActor` forwards events to
//! this actor via `EventForwarder`. This actor then republishes them on the
//! local `EventBus`, making remote session events visible to the local dashboard
//! and other local observers.

use crate::event_sink::EventSink;
use crate::events::{AgentEvent, Durability, EventOrigin, classify_durability};
use kameo::Actor;
use kameo::message::{Context, Message};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Event relay actor — receives events from remote sessions and routes
/// them through `EventSink` for proper persistence and fanout.
///
/// Durable events are persisted to the local journal with Remote origin.
/// Ephemeral events are published to fanout only.
#[derive(Actor, Clone)]
pub struct EventRelayActor {
    /// Local event sink for persist + fanout
    event_sink: Arc<EventSink>,
    /// Label for the source (e.g., "dev-gpu"), used for logging/debugging
    source_label: String,
    /// Running count of RelayedEvent messages received — used for loop diagnosis.
    received: u64,
}

impl EventRelayActor {
    /// Create a new event relay actor that routes through EventSink.
    pub fn new(event_sink: Arc<EventSink>, source_label: String) -> Self {
        Self {
            event_sink,
            source_label,
            received: 0,
        }
    }
}

/// Message containing an event relayed from a remote session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelayedEvent {
    /// The event to relay
    pub event: AgentEvent,
}

impl Message<RelayedEvent> for EventRelayActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: RelayedEvent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.received += 1;
        tracing::trace!(
            target: "remote::event_relay",
            source = %self.source_label,
            received_count = self.received,
            seq = msg.event.seq,
            session_id = %msg.event.session_id,
            kind = ?msg.event.kind,
            "relaying event through EventSink"
        );
        log::debug!(
            "EventRelayActor({}): received RelayedEvent #{} — \
             original_seq={} session={} kind={:?}",
            self.source_label,
            self.received,
            msg.event.seq,
            msg.event.session_id,
            msg.event.kind,
        );

        // Set remote provenance metadata.
        let mut event = msg.event;
        event.origin = EventOrigin::Remote;
        if event.source_node.is_none() {
            event.source_node = Some(self.source_label.clone());
        }

        let source_node = event.source_node.clone();

        // Route through EventSink: durable events are persisted + published,
        // ephemeral events are published to fanout only.
        match classify_durability(&event.kind) {
            Durability::Durable => {
                if let Err(e) = self
                    .event_sink
                    .emit_durable_with_origin(
                        &event.session_id,
                        event.kind,
                        EventOrigin::Remote,
                        source_node,
                    )
                    .await
                {
                    log::warn!(
                        "EventRelayActor({}): failed to persist relayed durable event: {}",
                        self.source_label,
                        e
                    );
                }
            }
            Durability::Ephemeral => {
                self.event_sink.emit_ephemeral_with_origin(
                    &event.session_id,
                    event.kind,
                    EventOrigin::Remote,
                    source_node,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_fanout::EventFanout;
    use crate::event_sink::EventSink;
    use crate::events::EventEnvelope;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;

    async fn make_relay_with_sink(
        _label: &str,
    ) -> (
        Arc<EventSink>,
        Arc<dyn crate::session::projection::EventJournal>,
    ) {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let journal = storage.event_journal();
        let fanout = Arc::new(EventFanout::new());
        let sink = Arc::new(EventSink::new(journal.clone(), fanout));
        (sink, journal)
    }

    #[tokio::test]
    async fn test_event_relay_republishes_to_fanout() {
        let (sink, _journal) = make_relay_with_sink("test-remote").await;
        let relay = EventRelayActor::new(sink.clone(), "test-remote".to_string());
        let relay_ref = <EventRelayActor as kameo::actor::Spawn>::spawn(relay);

        // Subscribe to the fanout
        let mut rx = sink.fanout().subscribe();

        // Send a relayed durable event
        let test_event = AgentEvent {
            seq: 42,
            timestamp: 1234567890,
            session_id: "test-session".to_string(),
            origin: EventOrigin::Remote,
            source_node: Some("remote-a".to_string()),
            kind: crate::events::AgentEventKind::SessionCreated,
        };

        relay_ref
            .tell(RelayedEvent {
                event: test_event.clone(),
            })
            .await
            .expect("tell should succeed");

        // Should receive the event on the fanout
        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("Should receive event within timeout")
            .expect("Should successfully receive event");

        assert_eq!(received.session_id(), "test-session");
        if let EventEnvelope::Durable(de) = &received {
            assert!(matches!(de.origin, EventOrigin::Remote));
            assert_eq!(de.source_node.as_deref(), Some("remote-a"));
            assert!(matches!(
                de.kind,
                crate::events::AgentEventKind::SessionCreated
            ));
        } else {
            panic!("expected durable event envelope");
        }
    }

    /// Remote durable events relayed through EventRelayActor must be persisted
    /// to the local journal with origin=Remote and source_node preserved.
    #[tokio::test]
    async fn relayed_durable_event_persisted_to_journal_with_remote_origin() {
        let (sink, journal) = make_relay_with_sink("gpu-box").await;
        let relay = EventRelayActor::new(sink, "gpu-box".to_string());
        let relay_ref = <EventRelayActor as kameo::actor::Spawn>::spawn(relay);

        let test_event = AgentEvent {
            seq: 99,
            timestamp: 1700000000,
            session_id: "remote-sess".to_string(),
            origin: EventOrigin::Remote,
            source_node: Some("gpu-box".to_string()),
            kind: crate::events::AgentEventKind::SessionCreated,
        };

        relay_ref
            .tell(RelayedEvent { event: test_event })
            .await
            .expect("tell should succeed");

        // Allow async persistence to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Event must be in the journal with remote origin
        let events = journal
            .load_session_stream("remote-sess", None, None)
            .await
            .unwrap();
        assert_eq!(events.len(), 1, "relayed event must be in journal");
        assert!(matches!(events[0].origin, EventOrigin::Remote));
        assert_eq!(events[0].source_node.as_deref(), Some("gpu-box"));
    }

    /// Ephemeral events relayed through EventRelayActor must NOT appear in the journal.
    /// They should only be published to the fanout.
    #[tokio::test]
    async fn relayed_ephemeral_event_not_persisted_to_journal() {
        let (sink, journal) = make_relay_with_sink("eph-test").await;
        let mut rx = sink.fanout().subscribe();
        let relay = EventRelayActor::new(sink, "eph-test".to_string());
        let relay_ref = <EventRelayActor as kameo::actor::Spawn>::spawn(relay);

        let ephemeral_event = AgentEvent {
            seq: 0,
            timestamp: 1700000000,
            session_id: "s-eph".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: crate::events::AgentEventKind::AssistantContentDelta {
                content: "token".into(),
                message_id: "m1".into(),
            },
        };

        relay_ref
            .tell(RelayedEvent {
                event: ephemeral_event,
            })
            .await
            .expect("tell should succeed");

        // Should arrive on fanout
        let env = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout — ephemeral should arrive on fanout")
            .expect("recv");
        assert!(env.is_ephemeral());

        // Must NOT be in journal
        let events = journal
            .load_session_stream("s-eph", None, None)
            .await
            .unwrap();
        assert!(
            events.is_empty(),
            "ephemeral events must never appear in journal"
        );
    }

    /// When the incoming event has no source_node, the relay should use
    /// its own source_label as the source_node.
    #[tokio::test]
    async fn relayed_event_defaults_source_node_to_label() {
        let (sink, journal) = make_relay_with_sink("my-gpu").await;
        let relay = EventRelayActor::new(sink, "my-gpu".to_string());
        let relay_ref = <EventRelayActor as kameo::actor::Spawn>::spawn(relay);

        let event = AgentEvent {
            seq: 1,
            timestamp: 100,
            session_id: "s-default".to_string(),
            origin: EventOrigin::Local,
            source_node: None, // no source_node set
            kind: crate::events::AgentEventKind::SessionCreated,
        };

        relay_ref
            .tell(RelayedEvent { event })
            .await
            .expect("tell should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = journal
            .load_session_stream("s-default", None, None)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].source_node.as_deref(),
            Some("my-gpu"),
            "source_node should default to the relay's source_label"
        );
        assert!(matches!(events[0].origin, EventOrigin::Remote));
    }

    /// When the incoming event already has a source_node, it should be preserved.
    #[tokio::test]
    async fn relayed_event_preserves_existing_source_node() {
        let (sink, journal) = make_relay_with_sink("relay-x").await;
        let relay = EventRelayActor::new(sink, "relay-x".to_string());
        let relay_ref = <EventRelayActor as kameo::actor::Spawn>::spawn(relay);

        let event = AgentEvent {
            seq: 1,
            timestamp: 100,
            session_id: "s-preserve".to_string(),
            origin: EventOrigin::Remote,
            source_node: Some("original-peer".to_string()),
            kind: crate::events::AgentEventKind::SessionCreated,
        };

        relay_ref
            .tell(RelayedEvent { event })
            .await
            .expect("tell should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = journal
            .load_session_stream("s-preserve", None, None)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].source_node.as_deref(),
            Some("original-peer"),
            "existing source_node must be preserved, not overwritten by relay label"
        );
    }
}

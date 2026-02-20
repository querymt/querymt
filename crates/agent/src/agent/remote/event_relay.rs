//! Event relay actor — receives events from remote sessions and republishes
//! them to the local event bus.
//!
//! Lives on the **local** machine. A remote `SessionActor` forwards events to
//! this actor via `EventForwarder`. This actor then republishes them on the
//! local `EventBus`, making remote session events visible to the local dashboard
//! and other local observers.

use crate::event_bus::EventBus;
use crate::events::AgentEvent;
use kameo::Actor;
use kameo::message::{Context, Message};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Event relay actor — receives events from remote sessions and republishes
/// them to the local event bus.
#[derive(Actor, Clone)]
pub struct EventRelayActor {
    /// Local event bus where we republish received events
    local_event_bus: Arc<EventBus>,
    /// Label for the source (e.g., "dev-gpu"), used for logging/debugging
    source_label: String,
    /// Running count of RelayedEvent messages received — used for loop diagnosis.
    received: u64,
}

impl EventRelayActor {
    /// Create a new event relay actor for a specific remote source.
    pub fn new(local_event_bus: Arc<EventBus>, source_label: String) -> Self {
        Self {
            local_event_bus,
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
        log::debug!(
            "EventRelayActor({}): received RelayedEvent #{} — \
             original_seq={} session={} kind={:?}",
            self.source_label,
            self.received,
            msg.event.seq,
            msg.event.session_id,
            msg.event.kind,
        );

        // Republish the event on the local event bus
        // The event already has its original session_id, seq, timestamp, and kind
        // We just need to publish the kind under the original session_id
        self.local_event_bus
            .publish(&msg.event.session_id, msg.event.kind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_relay_republishes_to_local_bus() {
        let local_bus = Arc::new(EventBus::new());
        let relay = EventRelayActor::new(local_bus.clone(), "test-remote".to_string());
        let relay_ref = <EventRelayActor as kameo::actor::Spawn>::spawn(relay);

        // Subscribe to the local bus
        let mut rx = local_bus.subscribe();

        // Send a relayed event
        let test_event = AgentEvent {
            seq: 42,
            timestamp: 1234567890,
            session_id: "test-session".to_string(),
            kind: crate::events::AgentEventKind::SessionCreated,
        };

        relay_ref
            .tell(RelayedEvent {
                event: test_event.clone(),
            })
            .await
            .expect("tell should succeed");

        // Should receive the event on the local bus
        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("Should receive event within timeout")
            .expect("Should successfully receive event");

        assert_eq!(received.session_id, "test-session");
        assert!(matches!(
            received.kind,
            crate::events::AgentEventKind::SessionCreated
        ));
    }
}

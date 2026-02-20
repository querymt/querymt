//! Event forwarder — observes events from a remote session and forwards them
//! to a local EventRelayActor.
//!
//! Lives on the **remote** machine. Registered as an `EventObserver` on the
//! remote session's `EventBus`. When the remote session emits events, this
//! forwarder sends them to the local `EventRelayActor` via kameo remote
//! messaging.

#[cfg(feature = "remote")]
use crate::events::{AgentEvent, EventObserver};
#[cfg(feature = "remote")]
use async_trait::async_trait;
#[cfg(feature = "remote")]
use querymt::error::LLMError;

#[cfg(feature = "remote")]
use kameo::actor::RemoteActorRef;

#[cfg(feature = "remote")]
use super::event_relay::{EventRelayActor, RelayedEvent};

/// Event forwarder that sends events to a remote EventRelayActor.
///
/// This is registered as an observer on the remote session's EventBus.
/// When events are emitted, they are forwarded to the local relay actor.
#[cfg(feature = "remote")]
pub struct EventForwarder {
    /// Reference to the local EventRelayActor
    relay_ref: RemoteActorRef<EventRelayActor>,
    /// Label for logging/debugging
    source_label: String,
}

#[cfg(feature = "remote")]
impl EventForwarder {
    /// Create a new event forwarder that sends events to the given relay actor.
    pub fn new(relay_ref: RemoteActorRef<EventRelayActor>, source_label: String) -> Self {
        Self {
            relay_ref,
            source_label,
        }
    }
}

#[cfg(feature = "remote")]
#[async_trait]
impl EventObserver for EventForwarder {
    async fn on_event(&self, event: &AgentEvent) -> Result<(), LLMError> {
        log::trace!(
            "EventForwarder({}): forwarding seq={} session={} kind={:?}",
            self.source_label,
            event.seq,
            event.session_id,
            event.kind
        );

        // Forward the event to the local relay actor.
        // Use tell (fire-and-forget) to avoid blocking the event publisher.
        if let Err(e) = self
            .relay_ref
            .tell(&RelayedEvent {
                event: event.clone(),
            })
            .send()
        {
            log::warn!(
                "EventForwarder({}): Failed to forward event: {}",
                self.source_label,
                e
            );
            return Err(LLMError::GenericError(format!(
                "Failed to forward event: {}",
                e
            )));
        }

        Ok(())
    }
}

#[cfg(not(feature = "remote"))]
/// Stub for when remote feature is not enabled
pub struct EventForwarder;

#[cfg(not(feature = "remote"))]
impl EventForwarder {
    /// This stub should never be constructed without the remote feature
    #[allow(dead_code)]
    pub fn new(_relay_ref: (), _source_label: String) -> Self {
        panic!("EventForwarder requires the 'remote' feature to be enabled")
    }
}

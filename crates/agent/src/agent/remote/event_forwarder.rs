//! Event forwarder — subscribes to the EventFanout and forwards events
//! to a local EventRelayActor.
//!
//! Lives on the **remote** machine. Subscribes to the session's `EventFanout`
//! broadcast channel. When events are published, this forwarder sends them to
//! the local `EventRelayActor` via kameo remote messaging.

#[cfg(feature = "remote")]
use crate::events::{AgentEvent, EventEnvelope};
#[cfg(feature = "remote")]
use std::sync::Arc;

#[cfg(feature = "remote")]
use kameo::actor::RemoteActorRef;

#[cfg(feature = "remote")]
use super::event_relay::{EventRelayActor, RelayedEvent};

/// Event forwarder that subscribes to an EventFanout and sends events to a
/// remote EventRelayActor.
///
/// Spawns a background task that reads from the fanout receiver and forwards
/// each event to the relay actor. The task is cancelled when the returned
/// `tokio::task::JoinHandle` is aborted or the fanout sender is dropped.
#[cfg(feature = "remote")]
pub struct EventForwarder;

#[cfg(feature = "remote")]
impl EventForwarder {
    /// Start forwarding events from the given fanout to the relay actor.
    ///
    /// Returns a `JoinHandle` that can be used to abort the forwarder task.
    pub fn start(
        fanout: Arc<crate::event_fanout::EventFanout>,
        relay_ref: RemoteActorRef<EventRelayActor>,
        source_label: String,
    ) -> tokio::task::JoinHandle<()> {
        let mut rx = fanout.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(envelope) => {
                        let event: AgentEvent = match &envelope {
                            EventEnvelope::Durable(de) => de.clone().into(),
                            EventEnvelope::Ephemeral(ee) => ee.clone().into(),
                        };
                        tracing::trace!(
                            target: "remote::event_forwarder",
                            source = %source_label,
                            session_id = %event.session_id,
                            kind = ?event.kind,
                            "forwarding event to relay actor"
                        );

                        if let Err(e) = relay_ref
                            .tell(&RelayedEvent {
                                event: event.clone(),
                            })
                            .send()
                        {
                            tracing::warn!(
                                target: "remote::event_forwarder",
                                source = %source_label,
                                error = %e,
                                "failed to forward event to relay actor"
                            );
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            target: "remote::event_forwarder",
                            source = %source_label,
                            skipped = n,
                            "forwarder lagged behind fanout"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::debug!(
                            target: "remote::event_forwarder",
                            source = %source_label,
                            "fanout closed, stopping forwarder"
                        );
                        break;
                    }
                }
            }
        })
    }
}

#[cfg(not(feature = "remote"))]
/// Stub for when remote feature is not enabled
pub struct EventForwarder;

#[cfg(not(feature = "remote"))]
impl EventForwarder {
    /// This stub should never be constructed without the remote feature
    #[allow(dead_code)]
    pub fn start(
        _fanout: std::sync::Arc<crate::event_fanout::EventFanout>,
        _relay_ref: (),
        _source_label: String,
    ) -> tokio::task::JoinHandle<()> {
        panic!("EventForwarder requires the 'remote' feature to be enabled")
    }
}

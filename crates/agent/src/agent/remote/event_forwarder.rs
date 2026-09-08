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
use kameo::error::{Infallible, RemoteSendError};

#[cfg(feature = "remote")]
use super::event_relay::{EventRelayActor, RelayedEvent};

/// An owned event-forwarder task. Dropping the handle cancels the task.
pub struct EventForwarderHandle {
    task: tokio::task::JoinHandle<()>,
}

impl EventForwarderHandle {
    pub fn abort(&self) {
        self.task.abort();
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for EventForwarderHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Event forwarder that subscribes to an EventFanout and sends events to a
/// remote EventRelayActor.
#[cfg(feature = "remote")]
pub struct EventForwarder;

#[cfg(feature = "remote")]
fn destination_is_gone(error: &RemoteSendError<Infallible>) -> bool {
    matches!(
        error,
        RemoteSendError::ActorNotRunning
            | RemoteSendError::ActorStopped
            | RemoteSendError::UnknownActor { .. }
            | RemoteSendError::UnknownMessage { .. }
            | RemoteSendError::BadActorType
    )
}

#[cfg(feature = "remote")]
impl EventForwarder {
    /// Start forwarding events from the given fanout to the relay actor.
    ///
    /// Only events whose `session_id` matches `filter_session_id` are
    /// forwarded.  All other events on the (global) fanout are silently
    /// skipped.  This prevents N-times duplication when multiple remote
    /// sessions share the same `EventFanout`.
    ///
    /// Returns an owned handle which cancels the forwarder when dropped.
    pub fn start(
        fanout: Arc<crate::event_fanout::EventFanout>,
        relay_ref: RemoteActorRef<EventRelayActor>,
        source_label: String,
        filter_session_id: String,
    ) -> EventForwarderHandle {
        let mut rx = fanout.subscribe();
        let relay_actor_id = relay_ref.id();
        let task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(envelope) => {
                        // Only forward events belonging to our session.
                        if envelope.session_id() != filter_session_id {
                            continue;
                        }

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

                        if let Err(error) = relay_ref
                            .tell(&RelayedEvent {
                                event: event.clone(),
                            })
                            .mailbox_timeout(std::time::Duration::from_secs(3))
                            .send_ack()
                            .await
                        {
                            let terminal = destination_is_gone(&error);
                            tracing::warn!(
                                target: "remote::event_forwarder",
                                source = %source_label,
                                session_id = %filter_session_id,
                                relay_actor_id = %relay_actor_id,
                                terminal,
                                error = %error,
                                "failed to forward event to relay actor"
                            );
                            if terminal {
                                break;
                            }
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
        });
        EventForwarderHandle { task }
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
        _filter_session_id: String,
    ) -> EventForwarderHandle {
        panic!("EventForwarder requires the 'remote' feature to be enabled")
    }
}

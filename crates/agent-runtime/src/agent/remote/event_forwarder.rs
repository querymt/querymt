//! Event forwarder — subscribes to the EventFanout and forwards events
//! to a local EventRelayActor.
//!
//! Lives on the **remote** machine. Subscribes to the session's `EventFanout`
//! broadcast channel. When events are published, this forwarder sends them to
//! the local `EventRelayActor` via kameo remote messaging.

#[cfg(feature = "remote")]
use querymt_agent_core::events::{AgentEvent, EventEnvelope};
#[cfg(feature = "remote")]
use std::sync::Arc;

#[cfg(feature = "remote")]
use kameo::actor::RemoteActorRef;
#[cfg(feature = "remote")]
use kameo::error::RemoteSendError;

#[cfg(feature = "remote")]
use super::event_relay::{EventRelayActor, RelayLagged, RelayedEvent};
#[cfg(feature = "remote")]
use querymt_remote::{
    MeshHandle,
    mesh_ingress::PeerRequest,
    peer_access::{GrantStore, PolicyChange, PolicyEffect, SharingProtocol},
};

#[cfg(feature = "remote")]
pub(crate) struct PeerSubscription {
    pub(crate) egress: MeshHandle,
    pub(crate) store: Arc<dyn GrantStore>,
    pub(crate) origin: PeerRequest,
    pub(crate) changes: Option<tokio::sync::broadcast::Receiver<PolicyChange>>,
}

#[cfg(feature = "remote")]
impl PeerSubscription {
    pub(crate) async fn is_current(&mut self) -> bool {
        use tokio::sync::broadcast::error::TryRecvError;

        if !self.has_sessions().await {
            return false;
        }
        let Some(changes) = &mut self.changes else {
            return true;
        };
        loop {
            match changes.try_recv() {
                Ok(change) if lost_sessions(&self.origin, &change) => return false,
                Ok(_) => {}
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Lagged(_) | TryRecvError::Closed) => return false,
            }
        }
    }

    async fn has_sessions(&self) -> bool {
        matches!(self.store.grant(self.origin.key()).await, Ok(state)
            if state.grant.allows_sessions(self.origin.protocol()))
    }

    async fn wait_for_loss(
        &self,
        mut changes: Option<tokio::sync::broadcast::Receiver<PolicyChange>>,
    ) {
        if let Some(changes) = &mut changes {
            // The actor already checked the current grant and drained this
            // receiver. Consume later changes before attempting delivery.
            while let Ok(change) = changes.recv().await {
                if lost_sessions(&self.origin, &change) {
                    break;
                }
            }
        } else {
            while self.has_sessions().await {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
}

#[cfg(feature = "remote")]
fn lost_sessions(origin: &PeerRequest, change: &PolicyChange) -> bool {
    &change.key == origin.key()
        && matches!(&change.effect, PolicyEffect::Grant(grant) if !grant.allows_sessions(origin.protocol()))
}

#[cfg(feature = "remote")]
async fn deliver<M>(
    relay: &RemoteActorRef<EventRelayActor>,
    message: &M,
    subscriber: Option<&PeerSubscription>,
) -> Result<(), (String, bool)>
where
    EventRelayActor: kameo::remote::RemoteMessage<M> + kameo::message::Message<M, Reply = ()>,
    M: serde::Serialize + Send + Sync + 'static,
{
    use querymt_remote::request_timeout::{SharingAskError, ask_sharing_v1};
    let timeout = std::time::Duration::from_secs(3);
    if let Some(subscriber) = subscriber
        && subscriber.origin.protocol() == SharingProtocol::V1
    {
        return ask_sharing_v1::<_, _, (), crate::error::AgentError>(
            &subscriber.egress,
            relay,
            message,
            timeout,
        )
        .await
        .map_err(|error| {
            let terminal = matches!(
                error,
                SharingAskError::Actor(RemoteSendError::HandlerError(_))
            ) || matches!(&error, SharingAskError::Actor(error) if destination_is_gone(error));
            (error.to_string(), terminal)
        });
    }
    relay
        .tell(message)
        .mailbox_timeout(timeout)
        .send_ack()
        .await
        .map_err(|error| (error.to_string(), destination_is_gone(&error)))
}

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
fn destination_is_gone<E>(error: &RemoteSendError<E>) -> bool {
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
        fanout: Arc<querymt_agent_services::event_fanout::EventFanout>,
        relay_ref: RemoteActorRef<EventRelayActor>,
        source_label: String,
        filter_session_id: String,
    ) -> EventForwarderHandle {
        Self::start_with_policy(fanout, relay_ref, source_label, filter_session_id, None)
    }

    pub(crate) fn start_with_policy(
        fanout: Arc<querymt_agent_services::event_fanout::EventFanout>,
        relay_ref: RemoteActorRef<EventRelayActor>,
        source_label: String,
        filter_session_id: String,
        mut subscriber: Option<PeerSubscription>,
    ) -> EventForwarderHandle {
        let mut rx = fanout.subscribe();
        let relay_actor_id = relay_ref.id();
        let changes = subscriber
            .as_mut()
            .and_then(|subscriber| subscriber.changes.take());
        let task = tokio::spawn(async move {
            let revoked = async {
                if let Some(subscriber) = &subscriber {
                    subscriber.wait_for_loss(changes).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(revoked);
            loop {
                let next = tokio::select! {
                    biased;
                    _ = &mut revoked => break,
                    next = rx.recv() => next,
                };
                if next
                    .as_ref()
                    .is_ok_and(|event| event.session_id() != filter_session_id)
                {
                    continue;
                }
                match next {
                    Ok(envelope) => {
                        // The envelope is shared with the other subscribers; this
                        // takes the single copy `AgentEvent` needs (and none at
                        // all if this receiver holds the last reference).
                        let event: AgentEvent = match Arc::unwrap_or_clone(envelope) {
                            EventEnvelope::Durable(de) => de.into(),
                            EventEnvelope::Ephemeral(ee) => ee.into(),
                        };
                        tracing::trace!(
                            target: "remote::event_forwarder",
                            source = %source_label,
                            session_id = %event.session_id,
                            kind = ?event.kind,
                            "forwarding event to relay actor"
                        );

                        let message = RelayedEvent { event };
                        let result = tokio::select! {
                            biased;
                            _ = &mut revoked => break,
                            result = deliver(&relay_ref, &message, subscriber.as_ref()) => result,
                        };
                        if let Err((error, terminal)) = result {
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
                        // Tell the relay so it fetches the durable tail now;
                        // it cannot see a loss at the end of a burst itself.
                        // Best effort: a later event still reveals the gap.
                        let message = RelayLagged {
                            session_id: filter_session_id.clone(),
                            skipped: n,
                        };
                        let result = tokio::select! {
                            biased;
                            _ = &mut revoked => break,
                            result = deliver(&relay_ref, &message, subscriber.as_ref()) => result,
                        };
                        if let Err((error, terminal)) = result {
                            if terminal {
                                break;
                            }
                            tracing::debug!(
                                target: "remote::event_forwarder",
                                source = %source_label,
                                %error,
                                "failed to send lag signal to relay actor"
                            );
                        }
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
        _fanout: std::sync::Arc<querymt_agent_services::event_fanout::EventFanout>,
        _relay_ref: (),
        _source_label: String,
        _filter_session_id: String,
    ) -> EventForwarderHandle {
        panic!("EventForwarder requires the 'remote' feature to be enabled")
    }
}

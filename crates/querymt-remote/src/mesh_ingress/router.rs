//! Explicit local dispatch for a guarded mesh. A Kameo registration alone never
//! makes an actor reachable through this router.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use kameo::actor::ActorId;
use kameo::error::RemoteSendError;
use kameo::remote::{
    RemoteActor, RemoteMessage, messaging::SwarmRequest, messaging::SwarmResponse,
};
use libp2p::PeerId;
use parking_lot::{Mutex, RwLock};
use serde::{Serialize, de::DeserializeOwned};

use super::{MeshIngress, SharingRejection};
use crate::NodeId;
use crate::peer_access::{
    GrantStore, PeerGrant, PeerGrantKey, PolicyChange, PolicyEffect, SharingProtocol,
};
use tokio::sync::broadcast;

#[cfg(test)]
mod tests;

type AskResult = Result<Vec<u8>, RemoteSendError<Vec<u8>>>;
type Handler =
    dyn Fn(PeerRequest, Vec<u8>) -> Pin<Box<dyn Future<Output = AskResult> + Send>> + Send + Sync;
type RouteKey = (&'static str, &'static str);
type Routes = HashMap<ActorId, HashMap<RouteKey, Arc<Route>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Delivery {
    Ask,
    Tell,
}

/// The wire envelopes accepted by a route. The payload decoder is shared.
#[derive(Debug, Clone, Copy)]
pub enum InboundDelivery {
    /// Ask on either protocol.
    Ask,
    /// Ask only on the legacy protocol.
    LegacyAsk,
    /// Ask only on the grant-aware protocol.
    V1Ask,
    /// Legacy Tell acknowledgment or grant-aware Ask reply for the same message.
    LegacyTellV1Ask,
}

impl InboundDelivery {
    fn accepts(self, protocol: SharingProtocol, delivery: Delivery) -> bool {
        use Delivery::{Ask, Tell};
        use SharingProtocol::{Legacy, V1};
        matches!(
            (self, protocol, delivery),
            (Self::Ask, _, Ask)
                | (Self::LegacyAsk, Legacy, Ask)
                | (Self::V1Ask | Self::LegacyTellV1Ask, V1, Ask)
                | (Self::LegacyTellV1Ask, Legacy, Tell)
        )
    }
}

/// Trusted local metadata captured at the network boundary. It is not wire data.
#[derive(Debug, Clone)]
pub struct PeerRequest {
    key: PeerGrantKey,
    protocol: SharingProtocol,
    grant: Option<PeerGrant>,
    policy_changes: Arc<Mutex<Option<broadcast::Receiver<PolicyChange>>>>,
}

impl PeerRequest {
    /// Grant authorized at ingress, absent for identity-only routes.
    pub fn grant(&self) -> Option<&PeerGrant> {
        self.grant.as_ref()
    }

    /// Host and authenticated caller in the direction of resource access.
    pub fn key(&self) -> &PeerGrantKey {
        &self.key
    }

    /// Request's sharing contract, not a peer's claimed capability.
    pub fn protocol(&self) -> SharingProtocol {
        self.protocol
    }

    /// Transfer the admission-time policy history to a resource-specific handler.
    /// Only one consumer can take it, even when this request is cloned.
    pub fn take_policy_changes(&self) -> Option<broadcast::Receiver<PolicyChange>> {
        self.policy_changes.lock().take()
    }
}

/// Initial access check before payload decode. Resource-specific handlers must
/// still check their inputs and retain the peer across queued or deferred work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundAccess {
    /// Identity-only discovery or an admission check that validates its payload.
    Identity,
    /// The caller can read only the effective grant addressed to itself.
    PeerAccess,
    /// Full access, including on the grant-aware contract.
    Full,
    /// Grant-aware session operations (not sufficient for tool execution).
    Sessions,
    /// Grant-aware workspace operations.
    Filesystem,
    /// At least one provider is shared. The handler must check the actual binding.
    Providers,
}

impl InboundAccess {
    fn permits(self, grant: &PeerGrant, protocol: SharingProtocol) -> bool {
        if self == Self::Identity {
            return true;
        }
        if protocol == SharingProtocol::Legacy {
            return grant == &PeerGrant::Full;
        }
        match self {
            Self::Identity | Self::PeerAccess => true,
            Self::Full => grant == &PeerGrant::Full,
            Self::Sessions => grant.allows_sessions(protocol),
            Self::Filesystem => grant.allows_filesystem(protocol),
            Self::Providers => match grant {
                PeerGrant::Full => true,
                PeerGrant::Custom { providers, .. } => !providers.is_empty(),
                PeerGrant::None => false,
            },
        }
    }
}

struct Route {
    delivery: InboundDelivery,
    access: InboundAccess,
    handler: Box<Handler>,
}

/// Host-bound registry for explicitly supported guarded messages. Unknown routes
/// and remote actor lifecycle operations are never forwarded to Kameo.
pub struct InboundRouter {
    host: PeerId,
    store: Arc<dyn GrantStore>,
    routes: RwLock<Routes>,
}

impl InboundRouter {
    /// Create an empty, deny-by-default registry for one host identity.
    pub fn new(host: PeerId, store: Arc<dyn GrantStore>) -> Self {
        Self {
            host,
            store,
            routes: RwLock::new(HashMap::new()),
        }
    }

    /// Install an explicit wire message handler. Decode and reply encoding match
    /// Kameo 0.22, but the handler receives local peer metadata as a separate value.
    /// The handler must not copy remote authority from any field in `M`.
    /// Callback handlers must validate the authenticated sender and active context.
    pub fn register<A, M, R, E, F, Fut>(
        &self,
        actor_id: ActorId,
        delivery: InboundDelivery,
        access: InboundAccess,
        handler: F,
    ) where
        A: RemoteActor + RemoteMessage<M>,
        M: DeserializeOwned + Send + 'static,
        R: Serialize + Send,
        E: Serialize + Send,
        F: Fn(PeerRequest, M) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<R, E>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let handler: Box<Handler> = Box::new(move |peer, bytes| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                let message = rmp_serde::from_slice(&bytes)
                    .map_err(|error| RemoteSendError::DeserializeMessage(error.to_string()))?;
                match handler(peer, message).await {
                    Ok(reply) => rmp_serde::to_vec_named(&reply)
                        .map_err(|error| RemoteSendError::SerializeReply(error.to_string())),
                    Err(error) => match rmp_serde::to_vec_named(&error) {
                        Ok(encoded) => Err(RemoteSendError::HandlerError(encoded)),
                        Err(error) => {
                            Err(RemoteSendError::SerializeHandlerError(error.to_string()))
                        }
                    },
                }
            })
        });
        self.routes.write().entry(actor_id).or_default().insert(
            (
                <A as RemoteActor>::REMOTE_ID,
                <A as RemoteMessage<M>>::REMOTE_ID,
            ),
            Arc::new(Route {
                delivery,
                access,
                handler,
            }),
        );
    }

    /// Withdraw all guarded handlers for an actor when its exported reference ends.
    pub fn withdraw_actor(&self, actor: ActorId) {
        self.routes.write().remove(&actor);
    }

    async fn authorize(
        &self,
        peer: &PeerRequest,
        access: InboundAccess,
    ) -> Result<Option<PeerGrant>, SharingRejection> {
        if access == InboundAccess::Identity {
            return Ok(None);
        }
        match self.store.grant(peer.key()).await {
            Ok(state) if access.permits(&state.grant, peer.protocol) => Ok(Some(state.grant)),
            Ok(_) => Err(SharingRejection::Denied),
            Err(error) => {
                tracing::warn!(%error, "cannot authorize mesh request");
                Err(SharingRejection::Unavailable)
            }
        }
    }

    async fn dispatch(
        &self,
        peer: PeerId,
        request: SwarmRequest,
        protocol: SharingProtocol,
    ) -> Result<AskResult, SharingRejection> {
        let (delivery, actor_id, actor_remote_id, message_remote_id, payload) = match request {
            SwarmRequest::Ask {
                actor_id,
                actor_remote_id,
                message_remote_id,
                payload,
                ..
            } => (
                Delivery::Ask,
                actor_id,
                actor_remote_id,
                message_remote_id,
                payload,
            ),
            SwarmRequest::Tell {
                actor_id,
                actor_remote_id,
                message_remote_id,
                payload,
                ..
            } => (
                Delivery::Tell,
                actor_id,
                actor_remote_id,
                message_remote_id,
                payload,
            ),
            _ => return Ok(Err(RemoteSendError::BadActorType)),
        };
        if actor_id.peer_id() != Some(&self.host) {
            return Ok(Err(RemoteSendError::BadActorType));
        }
        let route_key = (actor_remote_id.as_ref(), message_remote_id.as_ref());
        let Some(route) = self
            .routes
            .read()
            .get(&actor_id)
            .and_then(|routes| routes.get(&route_key))
            .filter(|route| route.delivery.accepts(protocol, delivery))
            .cloned()
        else {
            return Ok(Err(RemoteSendError::UnknownMessage {
                actor_remote_id,
                message_remote_id,
            }));
        };
        let changes = (route.access != InboundAccess::Identity)
            .then(|| self.store.subscribe_changes())
            .flatten();
        let mut origin = PeerRequest {
            key: PeerGrantKey {
                host: NodeId::from_peer_id(self.host),
                peer: NodeId::from_peer_id(peer),
            },
            protocol,
            grant: None,
            policy_changes: Arc::new(Mutex::new(
                changes.as_ref().map(broadcast::Receiver::resubscribe),
            )),
        };
        origin.grant = self.authorize(&origin, route.access).await?;
        let is_current = {
            self.routes
                .read()
                .get(&actor_id)
                .and_then(|routes| routes.get(&route_key))
                .is_some_and(|current| Arc::ptr_eq(current, &route))
        };
        if !is_current {
            return Ok(Err(RemoteSendError::ActorNotRunning));
        }
        let result = (route.handler)(origin.clone(), payload).await;
        if origin.grant.is_some() && self.authorize(&origin, route.access).await.is_err() {
            return Err(SharingRejection::Changed);
        }
        // Restoring a grant must not resume delivery of a revoked request.
        if let Some(mut changes) = changes {
            use tokio::sync::broadcast::error::TryRecvError;
            loop {
                match changes.try_recv() {
                    Ok(change)
                        if change.key == origin.key
                            && matches!(&change.effect, PolicyEffect::Grant(grant) if !route.access.permits(grant, protocol)) =>
                    {
                        return Err(SharingRejection::Changed);
                    }
                    Ok(_) => {}
                    Err(TryRecvError::Lagged(_) | TryRecvError::Closed) => {
                        return Err(SharingRejection::Changed);
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
        }
        Ok(result)
    }

    async fn ask(&self, peer: PeerId, request: SwarmRequest) -> AskResult {
        self.dispatch(peer, request, SharingProtocol::Legacy)
            .await
            .map_err(legacy_denied)?
    }
}

// Legacy Kameo has no wire access-denied variant. Use an existing terminal
// error shape rather than making a refusal look like a reconnect.
fn legacy_denied(rejection: SharingRejection) -> RemoteSendError<Vec<u8>> {
    if rejection == SharingRejection::Changed {
        RemoteSendError::SerializeReply(rejection.to_string())
    } else {
        RemoteSendError::DeserializeMessage(rejection.to_string())
    }
}

#[async_trait]
impl MeshIngress for InboundRouter {
    async fn handle_v1(
        &self,
        peer: PeerId,
        request: SwarmRequest,
    ) -> Result<SwarmResponse, SharingRejection> {
        match request {
            ask @ SwarmRequest::Ask { .. } => self
                .dispatch(peer, ask, SharingProtocol::V1)
                .await
                .map(SwarmResponse::Ask),
            _ => Err(SharingRejection::UnsupportedOperation),
        }
    }

    async fn handle_legacy(&self, peer: PeerId, request: SwarmRequest) -> SwarmResponse {
        match request {
            ask @ SwarmRequest::Ask { .. } => SwarmResponse::Ask(self.ask(peer, ask).await),
            tell @ SwarmRequest::Tell { .. } => {
                let result = self
                    .ask(peer, tell)
                    .await
                    .map(|_| ())
                    .map_err(|error| match error {
                        RemoteSendError::HandlerError(_) => {
                            RemoteSendError::DeserializeMessage("mesh notification rejected".into())
                        }
                        error => error.map_err(|_| unreachable!("handler error handled above")),
                    });
                SwarmResponse::Tell(result)
            }
            SwarmRequest::Link { .. } => SwarmResponse::Link(Err(RemoteSendError::BadActorType)),
            SwarmRequest::Unlink { .. } => {
                SwarmResponse::Unlink(Err(RemoteSendError::BadActorType))
            }
            SwarmRequest::SignalLinkDied { .. } => {
                SwarmResponse::SignalLinkDied(Err(RemoteSendError::BadActorType))
            }
        }
    }
}

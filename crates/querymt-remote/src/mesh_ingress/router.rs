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
use parking_lot::RwLock;
use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;

use super::MeshIngress;
use crate::NodeId;
use crate::peer_access::{GrantStore, PeerGrant, PeerGrantKey, SharingProtocol};

#[cfg(test)]
mod tests;

type AskResult = Result<Vec<u8>, RemoteSendError<Vec<u8>>>;
type Handler =
    dyn Fn(PeerRequest, Vec<u8>) -> Pin<Box<dyn Future<Output = AskResult> + Send>> + Send + Sync;
type Routes = HashMap<ActorId, HashMap<(&'static str, &'static str), Arc<Route>>>;

/// Trusted local metadata captured at the network boundary. It is not wire data.
#[derive(Debug, Clone)]
pub struct PeerRequest {
    key: PeerGrantKey,
    protocol: SharingProtocol,
}

impl PeerRequest {
    /// Host and authenticated caller in the direction of resource access.
    pub fn key(&self) -> &PeerGrantKey {
        &self.key
    }

    /// Request's sharing contract, not a peer's claimed capability.
    pub fn protocol(&self) -> SharingProtocol {
        self.protocol
    }
}

/// Initial access check before payload decode. Resource-specific handlers must
/// still check their inputs and retain the peer across queued or deferred work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundAccess {
    /// Identity-only discovery or an admission check that validates its payload.
    Identity,
    /// Full access on the legacy contract, regardless of advertised peer version.
    LegacyFull,
}

struct Route {
    access: InboundAccess,
    handler: Box<Handler>,
}

/// Host-bound registry for explicitly supported guarded asks. Unknown routes,
/// tells and actor lifecycle operations are never forwarded to Kameo.
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
    pub fn register_ask<A, M, R, E, F, Fut>(
        &self,
        actor_id: ActorId,
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
            Arc::new(Route { access, handler }),
        );
    }

    /// Withdraw all guarded handlers for an actor when its exported reference ends.
    pub fn withdraw_actor(&self, actor: ActorId) {
        self.routes.write().remove(&actor);
    }

    async fn full_grant_revision(
        &self,
        peer: &PeerRequest,
    ) -> Result<Uuid, RemoteSendError<Vec<u8>>> {
        match self.store.grant(peer.key()).await {
            Ok(state) if state.grant == PeerGrant::Full => Ok(state.revision),
            Ok(_) => Err(legacy_denied()),
            Err(error) => {
                tracing::warn!(%error, "cannot authorize legacy mesh request");
                Err(legacy_denied())
            }
        }
    }

    async fn ask(&self, peer: PeerId, request: SwarmRequest) -> AskResult {
        let SwarmRequest::Ask {
            actor_id,
            actor_remote_id,
            message_remote_id,
            payload,
            ..
        } = request
        else {
            return Err(RemoteSendError::BadActorType);
        };
        if actor_id.peer_id() != Some(&self.host) {
            return Err(RemoteSendError::BadActorType);
        }
        let Some(route) = self
            .routes
            .read()
            .get(&actor_id)
            .and_then(|routes| routes.get(&(actor_remote_id.as_ref(), message_remote_id.as_ref())))
            .cloned()
        else {
            return Err(RemoteSendError::UnknownMessage {
                actor_remote_id,
                message_remote_id,
            });
        };
        let origin = PeerRequest {
            key: PeerGrantKey {
                host: NodeId::from_peer_id(self.host),
                peer: NodeId::from_peer_id(peer),
            },
            protocol: SharingProtocol::Legacy,
        };
        let revision = match route.access {
            InboundAccess::Identity => None,
            InboundAccess::LegacyFull => Some(self.full_grant_revision(&origin).await?),
        };
        let is_current = || {
            self.routes
                .read()
                .get(&actor_id)
                .and_then(|routes| {
                    routes.get(&(actor_remote_id.as_ref(), message_remote_id.as_ref()))
                })
                .is_some_and(|current| Arc::ptr_eq(current, &route))
        };
        if !is_current() {
            return Err(RemoteSendError::ActorNotRunning);
        }
        let result = (route.handler)(origin.clone(), payload).await;
        if let Some(revision) = revision
            && !matches!(self.full_grant_revision(&origin).await, Ok(current) if current == revision)
        {
            return Err(RemoteSendError::SerializeReply(
                "mesh grant changed before reply".into(),
            ));
        }
        if !is_current() {
            return Err(RemoteSendError::SerializeReply(
                "mesh route changed before reply".into(),
            ));
        }
        result
    }
}

// Legacy Kameo has no wire access-denied variant. Use an existing terminal
// error shape rather than making a refusal look like a reconnect.
fn legacy_denied() -> RemoteSendError<Vec<u8>> {
    RemoteSendError::DeserializeMessage("mesh access denied".into())
}

#[async_trait]
impl MeshIngress for InboundRouter {
    async fn handle_legacy(&self, peer: PeerId, request: SwarmRequest) -> SwarmResponse {
        match request {
            ask @ SwarmRequest::Ask { .. } => SwarmResponse::Ask(self.ask(peer, ask).await),
            SwarmRequest::Tell { .. } => SwarmResponse::Tell(Err(RemoteSendError::BadActorType)),
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

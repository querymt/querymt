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
type ReplyCheck = fn(&[u8], &PeerGrant, SharingProtocol) -> bool;
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

/// What a `Handler` route's handler chose as the authority of its request.
#[derive(Debug, Default)]
enum Selection {
    /// The handler has not authorized anything; the request is refused.
    #[default]
    Unset,
    Refused(SharingRejection),
    Chosen {
        key: PeerGrantKey,
        access: InboundAccess,
        changes: Option<broadcast::Receiver<PolicyChange>>,
    },
}

struct StoreRef(Arc<dyn GrantStore>);

impl std::fmt::Debug for StoreRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GrantStore")
    }
}

/// Trusted local metadata captured at the network boundary. It is not wire data.
#[derive(Debug, Clone)]
pub struct PeerRequest {
    /// The peer whose grant applies: the sender, or the issuer of a model pass.
    key: PeerGrantKey,
    /// The transport-authenticated sender of the request.
    sender: NodeId,
    protocol: SharingProtocol,
    grant: Option<PeerGrant>,
    policy_changes: Arc<Mutex<Option<broadcast::Receiver<PolicyChange>>>>,
    store: Arc<StoreRef>,
    selection: Arc<Mutex<Selection>>,
}

impl PeerRequest {
    /// The transport-authenticated sender, which owns callbacks and streams of
    /// this request even when [`Self::key`] names a model pass issuer.
    pub fn sender(&self) -> &NodeId {
        &self.sender
    }

    /// Host and sender in the direction of stream and callback ownership.
    pub fn sender_key(&self) -> PeerGrantKey {
        PeerGrantKey {
            host: self.key.host.clone(),
            peer: self.sender.clone(),
        }
    }

    /// Refuse a request on a `Handler` route before any authority is chosen.
    pub(crate) fn refuse(&self, rejection: SharingRejection) {
        let mut selection = self.selection.lock();
        if !matches!(&*selection, Selection::Chosen { .. }) {
            *selection = Selection::Refused(rejection);
        }
    }

    /// Authorize this request for `access` and make the result its authority.
    /// Only for `InboundAccess::Handler` routes, and at most once per request.
    /// `authority` names the peer whose grant applies: `None` for the sender, or
    /// the issuer of a model pass the handler has verified. A pass requires the
    /// grant-aware contract. A refusal is final for the request, never a reason
    /// to evaluate another peer's grant.
    pub(crate) async fn authorize_as(
        &self,
        authority: Option<&NodeId>,
        access: InboundAccess,
    ) -> Result<PeerRequest, SharingRejection> {
        match self.select(authority, access).await {
            Ok(request) => Ok(request),
            Err(rejection) => {
                let mut selection = self.selection.lock();
                if !matches!(&*selection, Selection::Chosen { .. }) {
                    *selection = Selection::Refused(rejection);
                }
                Err(rejection)
            }
        }
    }

    async fn select(
        &self,
        authority: Option<&NodeId>,
        access: InboundAccess,
    ) -> Result<PeerRequest, SharingRejection> {
        let key = match authority {
            None => self.sender_key(),
            Some(_) if self.protocol != SharingProtocol::V1 => {
                return Err(SharingRejection::Denied);
            }
            Some(issuer) => PeerGrantKey {
                host: self.key.host.clone(),
                peer: issuer.clone(),
            },
        };
        let changes = self.store.0.subscribe_peer_changes(&key);
        // The router's receiver must start before the grant read suspends: a
        // `resubscribe` afterwards would skip a revoke and restore that happen
        // during the read, and the router's final check would miss them.
        let router_changes = changes.as_ref().map(broadcast::Receiver::resubscribe);
        let state = self.store.0.grant(&key).await.map_err(|error| {
            tracing::warn!(%error, "cannot authorize mesh request");
            SharingRejection::Unavailable
        })?;
        if !access.permits(&state.grant, self.protocol) {
            return Err(SharingRejection::Denied);
        }
        *self.selection.lock() = Selection::Chosen {
            key: key.clone(),
            access,
            changes: router_changes,
        };
        Ok(PeerRequest {
            key,
            grant: Some(state.grant),
            policy_changes: Arc::new(Mutex::new(changes)),
            ..self.clone()
        })
    }

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
    /// The handler picks the authority of the request with
    /// [`PeerRequest::authorize_as`], for routes that accept a model pass. The
    /// router refuses a request whose handler authorized nothing, and rechecks
    /// the chosen authority after the handler as it does for other routes.
    Handler,
}

impl InboundAccess {
    fn permits(self, grant: &PeerGrant, protocol: SharingProtocol) -> bool {
        if matches!(self, Self::Identity | Self::Handler) {
            return true;
        }
        if protocol == SharingProtocol::Legacy {
            return grant == &PeerGrant::Full;
        }
        match self {
            Self::Identity | Self::Handler | Self::PeerAccess => true,
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
    reply_check: Option<ReplyCheck>,
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
        self.register_checked::<A, M, R, E, F, Fut>(actor_id, delivery, access, handler, None);
    }

    pub(crate) fn register_checked<A, M, R, E, F, Fut>(
        &self,
        actor_id: ActorId,
        delivery: InboundDelivery,
        access: InboundAccess,
        handler: F,
        reply_check: Option<ReplyCheck>,
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
                reply_check,
            }),
        );
    }

    /// Withdraw all guarded handlers for an actor when its exported reference ends.
    pub fn withdraw_actor(&self, actor: ActorId) {
        self.routes.write().remove(&actor);
    }

    /// The messages reachable on `actor`, by `message_remote_id`, with the
    /// access each route requires at ingress.
    pub fn registered_messages(&self, actor: ActorId) -> Vec<(&'static str, InboundAccess)> {
        self.routes
            .read()
            .get(&actor)
            .map(|routes| {
                routes
                    .iter()
                    .map(|((_, message), route)| (*message, route.access))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn authorize(
        &self,
        key: &PeerGrantKey,
        protocol: SharingProtocol,
        access: InboundAccess,
    ) -> Result<Option<PeerGrant>, SharingRejection> {
        if matches!(access, InboundAccess::Identity | InboundAccess::Handler) {
            return Ok(None);
        }
        match self.store.grant(key).await {
            Ok(state) if access.permits(&state.grant, protocol) => Ok(Some(state.grant)),
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
        let route = {
            let routes = self.routes.read();
            let Some(actor_routes) = routes.get(&actor_id) else {
                // No exported actor has this ID: it stopped, or the reference
                // predates a restart of this node. Report it as gone so the
                // caller re-resolves the actor instead of seeing a version
                // mismatch.
                tracing::debug!(
                    %peer,
                    actor = %actor_remote_id,
                    message_id = %message_remote_id,
                    "mesh request for an actor that is not exported"
                );
                return Ok(Err(RemoteSendError::ActorNotRunning));
            };
            actor_routes
                .get(&route_key)
                .filter(|route| route.delivery.accepts(protocol, delivery))
                .cloned()
        };
        let Some(route) = route else {
            // A version mismatch or a missing registration; both need operator action.
            tracing::warn!(
                %peer,
                actor = %actor_remote_id,
                message_id = %message_remote_id,
                ?protocol,
                ?delivery,
                "mesh request has no guarded route"
            );
            return Ok(Err(RemoteSendError::UnknownMessage {
                actor_remote_id,
                message_remote_id,
            }));
        };
        let key = PeerGrantKey {
            host: NodeId::from_peer_id(self.host),
            peer: NodeId::from_peer_id(peer),
        };
        let changes = (!matches!(
            route.access,
            InboundAccess::Identity | InboundAccess::Handler
        ))
        .then(|| self.store.subscribe_peer_changes(&key))
        .flatten();
        let mut origin = PeerRequest {
            sender: key.peer.clone(),
            key,
            protocol,
            grant: None,
            policy_changes: Arc::new(Mutex::new(
                changes.as_ref().map(broadcast::Receiver::resubscribe),
            )),
            store: Arc::new(StoreRef(self.store.clone())),
            selection: Arc::default(),
        };
        origin.grant = match self.authorize(&origin.key, protocol, route.access).await {
            Ok(grant) => grant,
            Err(rejection) => {
                tracing::info!(
                    %peer,
                    actor = %actor_remote_id,
                    message_id = %message_remote_id,
                    access = ?route.access,
                    ?protocol,
                    ?rejection,
                    "mesh request refused"
                );
                return Err(rejection);
            }
        };
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
        let permits_reply = |grant: &PeerGrant| match (route.reply_check, &result) {
            (Some(check), Ok(reply)) => check(reply, grant, protocol),
            _ => true,
        };
        // The authority checked after the handler is the one it chose.
        let (auth_key, auth_access, authorized, mut changes) =
            if route.access == InboundAccess::Handler {
                match std::mem::take(&mut *origin.selection.lock()) {
                    Selection::Chosen {
                        key,
                        access,
                        changes,
                    } => (key, access, true, changes),
                    Selection::Refused(rejection) => {
                        tracing::info!(
                            %peer,
                            actor = %actor_remote_id,
                            message_id = %message_remote_id,
                            ?protocol,
                            ?rejection,
                            "mesh request refused"
                        );
                        return Err(rejection);
                    }
                    Selection::Unset => {
                        tracing::warn!(
                            %peer,
                            actor = %actor_remote_id,
                            message_id = %message_remote_id,
                            "mesh handler route authorized nothing"
                        );
                        return Err(SharingRejection::Denied);
                    }
                }
            } else {
                (
                    origin.key.clone(),
                    route.access,
                    origin.grant.is_some(),
                    changes,
                )
            };
        if authorized {
            let grant = self
                .authorize(&auth_key, protocol, auth_access)
                .await
                .map_err(|_| SharingRejection::Changed)?;
            if grant.as_ref().is_some_and(|grant| !permits_reply(grant)) {
                return Err(SharingRejection::Changed);
            }
        }
        // Restoring a grant must not resume delivery of a revoked request.
        if let Some(changes) = changes.as_mut() {
            use tokio::sync::broadcast::error::TryRecvError;
            loop {
                match changes.try_recv() {
                    Ok(change)
                        if change.key == auth_key
                            && matches!(&change.effect, PolicyEffect::Grant(grant) if !auth_access.permits(grant, protocol) || !permits_reply(grant)) =>
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

    async fn present_trust(&self, peer: PeerId, tokens: Vec<String>) -> Vec<String> {
        let Some(defaults) = self.store.access_defaults() else {
            return Vec::new();
        };
        // Signature checks are CPU work; keep them off the async workers.
        tokio::task::spawn_blocking(move || defaults.present(&NodeId::from_peer_id(peer), &tokens))
            .await
            .unwrap_or_default()
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

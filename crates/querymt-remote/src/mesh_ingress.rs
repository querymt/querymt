//! Authenticated inbound boundary for legacy and grant-aware mesh requests.
//!
//! Kameo 0.22 drops caller identity before dispatch. A guarded mesh disables its
//! original inbound messaging substreams. The legacy listener and the separate
//! grant-aware protocol deliver authenticated caller identity to the same gate.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use kameo::error::RemoteSendError;
use kameo::remote::{
    self,
    messaging::{SwarmRequest, SwarmResponse},
};
use libp2p::core::{Endpoint, transport::PortUse, upgrade::DeniedUpgrade};
use libp2p::request_response::{self as rpc, ProtocolSupport, cbor};
use libp2p::swarm::handler::{
    ConnectionEvent, ConnectionHandlerEvent, ConnectionHandlerSelect, SubstreamProtocol,
};
use libp2p::swarm::{
    ConnectionDenied, ConnectionHandler, ConnectionId, FromSwarm, NetworkBehaviour, THandler,
    THandlerInEvent, THandlerOutEvent, ToSwarm,
};
use libp2p::{Multiaddr, PeerId, StreamProtocol};
use serde::{Serialize, de::DeserializeOwned};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::{Instant, sleep_until};

use crate::provider_transport::MESH_MESSAGE_SIZE_MAXIMUM;

const LEGACY_PROTOCOL: StreamProtocol = StreamProtocol::new("/kameo/messaging/1.0.0");
const SHARING_PROTOCOL: StreamProtocol = StreamProtocol::new("/querymt/mesh-sharing/1");
const MAX_IN_FLIGHT: usize = 100;

mod router;
mod wire;
pub use router::{InboundAccess, InboundDelivery, InboundRouter, PeerRequest};
pub(crate) use wire::{ResponseSender, SharingRequest, SharingResponse, SharingWire};
pub use wire::{SharingCallError, SharingRejection, is_legacy_access_denial};

#[cfg(test)]
mod tests;

/// Entry point for protected actor requests. The caller is authenticated by the
/// transport, not read from the serialized envelope. Implementations must treat
/// every request as untrusted until authorization and must not forward the
/// request through another unguarded network path.
#[async_trait]
pub trait MeshIngress: Send + Sync + 'static {
    /// Handle a request received on the legacy wire protocol.
    async fn handle_legacy(&self, peer: PeerId, request: SwarmRequest) -> SwarmResponse;

    /// Handle an actor operation negotiated on the grant-aware protocol.
    async fn handle_v1(
        &self,
        _peer: PeerId,
        _request: SwarmRequest,
    ) -> Result<SwarmResponse, SharingRejection> {
        Err(SharingRejection::UnsupportedOperation)
    }
}

pub(crate) enum InboundRequest<Q, R> {
    Dispatch(Q),
    Reply(R),
}

pub(crate) trait InboundProtocol: Default + 'static {
    type Request: Serialize + DeserializeOwned + Send + 'static;
    type ActorRequest: Send + 'static;
    type Response: Serialize + DeserializeOwned + Send + 'static;

    const PROTOCOL: StreamProtocol;
    const SUPPORT: ProtocolSupport;
    const TASK_FAILURE: &'static str;

    fn receive(request: Self::Request) -> InboundRequest<Self::ActorRequest, Self::Response>;
    fn dispatch(
        gate: &dyn MeshIngress,
        peer: PeerId,
        request: Self::ActorRequest,
    ) -> impl Future<Output = Self::Response> + Send;
    fn overloaded(request: Self::ActorRequest) -> Self::Response;

    fn outbound_result(
        &mut self,
        _peer: PeerId,
        _id: rpc::OutboundRequestId,
        _result: Result<Self::Response, rpc::OutboundFailure>,
    ) {
    }
}

type Wire<P> = cbor::Behaviour<<P as InboundProtocol>::Request, <P as InboundProtocol>::Response>;
type ReplyTask<R> = (rpc::InboundRequestId, Option<(rpc::ResponseChannel<R>, R)>);

/// Each instance negotiates exactly one protocol. In particular, a sharing
/// request never offers the legacy protocol as an outbound fallback.
pub(crate) struct Inbound<P: InboundProtocol> {
    wire: Wire<P>,
    protocol: P,
    gate: Arc<dyn MeshIngress>,
    tasks: JoinSet<ReplyTask<P::Response>>,
    dispatch_timeout: Duration,
    pending_dispatch: HashMap<rpc::InboundRequestId, AbortHandle>,
}

impl<P: InboundProtocol> Inbound<P> {
    pub(crate) fn new(gate: Arc<dyn MeshIngress>, timeout: Duration) -> Self {
        let wire = Wire::<P>::with_codec(
            cbor::codec::Codec::default()
                .set_request_size_maximum(MESH_MESSAGE_SIZE_MAXIMUM)
                .set_response_size_maximum(MESH_MESSAGE_SIZE_MAXIMUM),
            [(P::PROTOCOL, P::SUPPORT)],
            rpc::Config::default().with_request_timeout(timeout),
        );
        Self {
            wire,
            protocol: P::default(),
            gate,
            tasks: JoinSet::new(),
            dispatch_timeout: timeout,
            pending_dispatch: HashMap::new(),
        }
    }
}

impl<P: InboundProtocol> NetworkBehaviour for Inbound<P> {
    type ConnectionHandler = THandler<Wire<P>>;
    type ToSwarm = ();

    fn handle_established_inbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        local: &Multiaddr,
        remote: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        self.wire
            .handle_established_inbound_connection(id, peer, local, remote)
    }

    fn handle_pending_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: Option<PeerId>,
        addresses: &[Multiaddr],
        role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        if matches!(P::SUPPORT, ProtocolSupport::Inbound) {
            // The legacy listener does not contribute addresses for outbound dials.
            return Ok(Vec::new());
        }
        self.wire
            .handle_pending_outbound_connection(id, peer, addresses, role)
    }

    fn handle_established_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        addr: &Multiaddr,
        role: Endpoint,
        port: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        self.wire
            .handle_established_outbound_connection(id, peer, addr, role, port)
    }

    fn on_swarm_event(&mut self, event: FromSwarm<'_>) {
        self.wire.on_swarm_event(event);
    }

    fn on_connection_handler_event(
        &mut self,
        peer: PeerId,
        id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        self.wire.on_connection_handler_event(peer, id, event);
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<ToSwarm<(), THandlerInEvent<Self>>> {
        while let Poll::Ready(Some(result)) = self.tasks.poll_join_next(cx) {
            match result {
                Ok((id, response)) => {
                    self.pending_dispatch.remove(&id);
                    if let Some((channel, reply)) = response {
                        let _ = self.wire.send_response(channel, reply);
                    }
                }
                Err(error) => {
                    self.pending_dispatch
                        .retain(|_, task| task.id() != error.id());
                    if !error.is_cancelled() {
                        tracing::error!(%error, "{}", P::TASK_FAILURE);
                    }
                }
            }
        }
        while let Poll::Ready(event) = self.wire.poll(cx) {
            match event {
                ToSwarm::GenerateEvent(rpc::Event::Message {
                    peer,
                    message:
                        rpc::Message::Request {
                            request_id,
                            request,
                            channel,
                        },
                    ..
                }) => match P::receive(request) {
                    InboundRequest::Reply(reply) => {
                        let _ = self.wire.send_response(channel, reply);
                    }
                    InboundRequest::Dispatch(request) if self.tasks.len() >= MAX_IN_FLIGHT => {
                        let _ = self.wire.send_response(channel, P::overloaded(request));
                    }
                    InboundRequest::Dispatch(request) => {
                        let gate = Arc::clone(&self.gate);
                        let deadline = Instant::now() + self.dispatch_timeout;
                        let task = self.tasks.spawn(async move {
                            if !channel.is_open() {
                                return (request_id, None);
                            }
                            let response = tokio::select! {
                                biased;
                                _ = sleep_until(deadline) => None,
                                reply = P::dispatch(&*gate, peer, request) => Some((channel, reply)),
                            };
                            (request_id, response)
                        });
                        self.pending_dispatch.insert(request_id, task);
                        // Register the waker even when the first task was spawned
                        // after polling an empty JoinSet.
                        cx.waker().wake_by_ref();
                    }
                },
                ToSwarm::GenerateEvent(rpc::Event::Message {
                    peer,
                    message:
                        rpc::Message::Response {
                            request_id,
                            response,
                        },
                    ..
                }) => self
                    .protocol
                    .outbound_result(peer, request_id, Ok(response)),
                ToSwarm::GenerateEvent(rpc::Event::OutboundFailure {
                    peer,
                    request_id,
                    error,
                    ..
                }) => self.protocol.outbound_result(peer, request_id, Err(error)),
                ToSwarm::GenerateEvent(rpc::Event::InboundFailure { request_id, .. }) => {
                    if let Some(task) = self.pending_dispatch.remove(&request_id) {
                        task.abort();
                    }
                }
                ToSwarm::GenerateEvent(_) => {}
                event => return Poll::Ready(event.map_out(|_| ())),
            }
        }
        Poll::Pending
    }
}

pub(crate) type LegacyWire = Inbound<Legacy>;

#[derive(Default)]
pub(crate) struct Legacy;

impl InboundProtocol for Legacy {
    type Request = SwarmRequest;
    type ActorRequest = SwarmRequest;
    type Response = SwarmResponse;

    const PROTOCOL: StreamProtocol = LEGACY_PROTOCOL;
    const SUPPORT: ProtocolSupport = ProtocolSupport::Inbound;
    const TASK_FAILURE: &'static str = "mesh ingress task failed";

    fn receive(request: SwarmRequest) -> InboundRequest<SwarmRequest, SwarmResponse> {
        InboundRequest::Dispatch(request)
    }

    async fn dispatch(
        gate: &dyn MeshIngress,
        peer: PeerId,
        request: SwarmRequest,
    ) -> SwarmResponse {
        gate.handle_legacy(peer, request).await
    }

    fn overloaded(request: SwarmRequest) -> SwarmResponse {
        match request {
            SwarmRequest::Ask { .. } => SwarmResponse::Ask(Err(RemoteSendError::MailboxFull)),
            SwarmRequest::Tell { .. } => SwarmResponse::Tell(Err(RemoteSendError::MailboxFull)),
            SwarmRequest::Link { .. } => SwarmResponse::Link(Err(RemoteSendError::MailboxFull)),
            SwarmRequest::Unlink { .. } => SwarmResponse::Unlink(Err(RemoteSendError::MailboxFull)),
            SwarmRequest::SignalLinkDied { .. } => {
                SwarmResponse::SignalLinkDied(Err(RemoteSendError::MailboxFull))
            }
        }
    }
}

/// Kameo with the original messaging listener disabled (but registry and
/// outbound protocols unchanged). This is paired with `Inbound` on the swarm.
pub(crate) struct Egress(pub(crate) remote::Behaviour);

type MessagingHandler = THandler<remote::messaging::Behaviour>;
type RegistryHandler = THandler<remote::registry::Behaviour>;

impl NetworkBehaviour for Egress {
    type ConnectionHandler =
        ConnectionHandlerSelect<OutboundOnly<MessagingHandler>, RegistryHandler>;
    type ToSwarm = remote::Event;

    fn handle_pending_inbound_connection(
        &mut self,
        id: ConnectionId,
        local: &Multiaddr,
        remote: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        self.0.handle_pending_inbound_connection(id, local, remote)
    }

    fn handle_pending_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: Option<PeerId>,
        addresses: &[Multiaddr],
        role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        self.0
            .handle_pending_outbound_connection(id, peer, addresses, role)
    }

    fn handle_established_inbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        local: &Multiaddr,
        remote: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        let (messaging, registry) = self
            .0
            .handle_established_inbound_connection(id, peer, local, remote)?
            .into_inner();
        Ok(OutboundOnly(messaging).select(registry))
    }

    fn handle_established_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        addr: &Multiaddr,
        role: Endpoint,
        port: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        let (messaging, registry) = self
            .0
            .handle_established_outbound_connection(id, peer, addr, role, port)?
            .into_inner();
        Ok(OutboundOnly(messaging).select(registry))
    }

    fn on_swarm_event(&mut self, event: FromSwarm<'_>) {
        self.0.on_swarm_event(event);
    }

    fn on_connection_handler_event(
        &mut self,
        peer: PeerId,
        id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        self.0.on_connection_handler_event(peer, id, event);
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        self.0.poll(cx)
    }
}

pub(crate) struct OutboundOnly<H>(H);

impl<H: ConnectionHandler> ConnectionHandler for OutboundOnly<H> {
    type FromBehaviour = H::FromBehaviour;
    type ToBehaviour = H::ToBehaviour;
    type InboundProtocol = DeniedUpgrade;
    type InboundOpenInfo = ();
    type OutboundProtocol = H::OutboundProtocol;
    type OutboundOpenInfo = H::OutboundOpenInfo;

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        SubstreamProtocol::new(DeniedUpgrade, ())
    }

    fn connection_keep_alive(&self) -> bool {
        self.0.connection_keep_alive()
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<
        ConnectionHandlerEvent<Self::OutboundProtocol, Self::OutboundOpenInfo, Self::ToBehaviour>,
    > {
        self.0.poll(cx)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<Option<Self::ToBehaviour>> {
        self.0.poll_close(cx)
    }

    fn on_behaviour_event(&mut self, event: Self::FromBehaviour) {
        self.0.on_behaviour_event(event);
    }

    fn on_connection_event(
        &mut self,
        event: ConnectionEvent<'_, DeniedUpgrade, H::OutboundProtocol, (), H::OutboundOpenInfo>,
    ) {
        let event = match event {
            ConnectionEvent::FullyNegotiatedOutbound(event) => {
                ConnectionEvent::FullyNegotiatedOutbound(event)
            }
            ConnectionEvent::DialUpgradeError(event) => ConnectionEvent::DialUpgradeError(event),
            ConnectionEvent::AddressChange(event) => ConnectionEvent::AddressChange(event),
            ConnectionEvent::LocalProtocolsChange(event) => {
                ConnectionEvent::LocalProtocolsChange(event)
            }
            ConnectionEvent::RemoteProtocolsChange(event) => {
                ConnectionEvent::RemoteProtocolsChange(event)
            }
            ConnectionEvent::FullyNegotiatedInbound(event) => match event.protocol {},
            ConnectionEvent::ListenUpgradeError(_) => return,
            _ => return,
        };
        self.0.on_connection_event(event);
    }
}

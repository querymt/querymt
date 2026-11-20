//! Grant-aware request/response protocol. It never negotiates the legacy protocol.

use std::collections::HashMap;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use kameo::remote::messaging::{SwarmRequest, SwarmResponse};
use libp2p::core::{Endpoint, transport::PortUse};
use libp2p::request_response::{self as rpc, ProtocolSupport, cbor};
use libp2p::swarm::{
    ConnectionDenied, ConnectionId, FromSwarm, NetworkBehaviour, THandler, THandlerInEvent,
    THandlerOutEvent, ToSwarm,
};
use libp2p::{Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use super::{MAX_IN_FLIGHT, MeshIngress, SHARING_PROTOCOL};
use crate::provider_transport::MESH_MESSAGE_SIZE_MAXIMUM;

/// Protected request rejection, independent of transport and actor reply encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingRejection {
    #[error("mesh access denied")]
    Denied,
    #[error("mesh sharing policy unavailable")]
    Unavailable,
    #[error("unsupported mesh operation")]
    UnsupportedOperation,
    #[error("mesh ingress busy")]
    Busy,
    #[error("mesh access changed before reply")]
    Changed,
}

/// Failure of an outbound grant-aware exchange.
#[derive(Debug, Error)]
pub enum SharingCallError {
    #[error("mesh sharing transport unavailable")]
    Unavailable,
    #[error("peer does not negotiate the grant-aware protocol")]
    UnsupportedProtocol,
    #[error("mesh sharing request failed")]
    Transport(#[source] rpc::OutboundFailure),
    #[error("invalid mesh sharing reply")]
    UnexpectedReply,
    #[error(transparent)]
    Rejected(#[from] SharingRejection),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum SharingRequest {
    Probe,
    Actor(Box<SwarmRequest>),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum SharingResponse {
    Supported,
    Actor(Result<SwarmResponse, SharingRejection>),
}

pub(crate) type ResponseSender = oneshot::Sender<Result<SharingResponse, SharingCallError>>;
type Wire = cbor::Behaviour<SharingRequest, SharingResponse>;

pub(crate) struct SharingWire {
    wire: Wire,
    gate: Arc<dyn MeshIngress>,
    pending: HashMap<rpc::OutboundRequestId, (PeerId, ResponseSender)>,
    tasks: JoinSet<(rpc::ResponseChannel<SharingResponse>, SharingResponse)>,
}

impl SharingWire {
    pub(crate) fn new(gate: Arc<dyn MeshIngress>, timeout: Duration) -> Self {
        Self {
            wire: Wire::with_codec(
                cbor::codec::Codec::default()
                    .set_request_size_maximum(MESH_MESSAGE_SIZE_MAXIMUM)
                    .set_response_size_maximum(MESH_MESSAGE_SIZE_MAXIMUM),
                [(SHARING_PROTOCOL, ProtocolSupport::Full)],
                rpc::Config::default().with_request_timeout(timeout),
            ),
            gate,
            pending: HashMap::new(),
            tasks: JoinSet::new(),
        }
    }

    pub(crate) fn send(&mut self, peer: PeerId, request: SharingRequest, tx: ResponseSender) {
        if tx.is_closed() {
            return;
        }
        if self.pending.len() >= MAX_IN_FLIGHT {
            let _ = tx.send(Err(SharingRejection::Busy.into()));
            return;
        }
        let id = self.wire.send_request(&peer, request);
        self.pending.insert(id, (peer, tx));
    }

    fn reply(
        &mut self,
        id: rpc::OutboundRequestId,
        peer: PeerId,
        reply: Result<SharingResponse, SharingCallError>,
    ) {
        if let Some((expected_peer, tx)) = self.pending.remove(&id) {
            let _ = tx.send(if peer == expected_peer {
                reply
            } else {
                Err(SharingCallError::UnexpectedReply)
            });
        }
    }
}

impl NetworkBehaviour for SharingWire {
    type ConnectionHandler = THandler<Wire>;
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
                Ok((channel, reply)) => {
                    let _ = self.wire.send_response(channel, reply);
                }
                Err(error) => tracing::error!(%error, "mesh sharing task failed"),
            }
        }
        while let Poll::Ready(event) = self.wire.poll(cx) {
            match event {
                ToSwarm::GenerateEvent(rpc::Event::Message { peer, message, .. }) => {
                    match message {
                        rpc::Message::Request {
                            request, channel, ..
                        } => match request {
                            SharingRequest::Probe => {
                                let _ =
                                    self.wire.send_response(channel, SharingResponse::Supported);
                            }
                            SharingRequest::Actor(request) => {
                                if self.tasks.len() >= MAX_IN_FLIGHT {
                                    let _ = self.wire.send_response(
                                        channel,
                                        SharingResponse::Actor(Err(SharingRejection::Busy)),
                                    );
                                } else {
                                    let gate = Arc::clone(&self.gate);
                                    self.tasks.spawn(async move {
                                        (
                                            channel,
                                            SharingResponse::Actor(
                                                gate.handle_v1(peer, *request).await,
                                            ),
                                        )
                                    });
                                    cx.waker().wake_by_ref();
                                }
                            }
                        },
                        rpc::Message::Response {
                            request_id,
                            response,
                        } => self.reply(request_id, peer, Ok(response)),
                    }
                }
                ToSwarm::GenerateEvent(rpc::Event::OutboundFailure {
                    peer,
                    request_id,
                    error,
                    ..
                }) => {
                    let error = match error {
                        rpc::OutboundFailure::UnsupportedProtocols => {
                            SharingCallError::UnsupportedProtocol
                        }
                        error => SharingCallError::Transport(error),
                    };
                    self.reply(request_id, peer, Err(error));
                }
                ToSwarm::GenerateEvent(_) => {}
                event => return Poll::Ready(event.map_out(|_| ())),
            }
        }
        Poll::Pending
    }
}

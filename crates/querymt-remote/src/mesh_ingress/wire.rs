//! Grant-aware request/response protocol. It never negotiates the legacy protocol.

use std::collections::HashMap;

use kameo::remote::messaging::{SwarmRequest, SwarmResponse};
use libp2p::request_response::{self as rpc, ProtocolSupport};
use libp2p::{PeerId, StreamProtocol};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::oneshot;

use super::{
    Inbound, InboundProtocol, InboundRequest, MAX_IN_FLIGHT, MeshIngress, SHARING_PROTOCOL,
};

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

/// Recognize the two access refusals encoded by guarded owners on the
/// legacy Kameo wire. Match only the exact variant and protocol text, not
/// arbitrary serialization errors or handler messages.
pub fn is_legacy_access_denial<E>(error: &kameo::error::RemoteSendError<E>) -> bool {
    use kameo::error::RemoteSendError;
    match error {
        RemoteSendError::DeserializeMessage(message) => message == "mesh access denied",
        RemoteSendError::SerializeReply(message) => message == "mesh access changed before reply",
        _ => false,
    }
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
pub(crate) type SharingWire = Inbound<Sharing>;

#[derive(Default)]
pub(crate) struct Sharing {
    pending: HashMap<rpc::OutboundRequestId, (PeerId, ResponseSender)>,
}

impl Inbound<Sharing> {
    pub(crate) fn send(&mut self, peer: PeerId, request: SharingRequest, tx: ResponseSender) {
        if tx.is_closed() {
            return;
        }
        if self.protocol.pending.len() >= MAX_IN_FLIGHT {
            let _ = tx.send(Err(SharingRejection::Busy.into()));
            return;
        }
        let id = self.wire.send_request(&peer, request);
        self.protocol.pending.insert(id, (peer, tx));
    }
}

impl InboundProtocol for Sharing {
    type Request = SharingRequest;
    type ActorRequest = Box<SwarmRequest>;
    type Response = SharingResponse;

    const PROTOCOL: StreamProtocol = SHARING_PROTOCOL;
    const SUPPORT: ProtocolSupport = ProtocolSupport::Full;
    const TASK_FAILURE: &'static str = "mesh sharing task failed";

    fn receive(request: SharingRequest) -> InboundRequest<Self::ActorRequest, SharingResponse> {
        match request {
            SharingRequest::Probe => InboundRequest::Reply(SharingResponse::Supported),
            SharingRequest::Actor(request) => InboundRequest::Dispatch(request),
        }
    }

    async fn dispatch(
        gate: &dyn MeshIngress,
        peer: PeerId,
        request: Self::ActorRequest,
    ) -> SharingResponse {
        SharingResponse::Actor(gate.handle_v1(peer, *request).await)
    }

    fn overloaded(_: Self::ActorRequest) -> SharingResponse {
        SharingResponse::Actor(Err(SharingRejection::Busy))
    }

    fn outbound_result(
        &mut self,
        peer: PeerId,
        id: rpc::OutboundRequestId,
        result: Result<SharingResponse, rpc::OutboundFailure>,
    ) {
        if let Some((expected_peer, tx)) = self.pending.remove(&id) {
            let result = if peer != expected_peer {
                Err(SharingCallError::UnexpectedReply)
            } else {
                result.map_err(|error| match error {
                    rpc::OutboundFailure::UnsupportedProtocols => {
                        SharingCallError::UnsupportedProtocol
                    }
                    error => SharingCallError::Transport(error),
                })
            };
            let _ = tx.send(result);
        }
    }
}

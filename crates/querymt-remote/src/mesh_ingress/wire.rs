//! Grant-aware request/response protocol. It never negotiates the legacy protocol.

use std::collections::HashMap;
use std::time::Duration;

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
    /// Trust-scope tokens for the sender's current connection.
    Trust(Vec<String>),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum SharingResponse {
    Supported,
    Actor(Result<SwarmResponse, SharingRejection>),
    /// The trust scopes the presented tokens admitted the sender to.
    Trust(Vec<String>),
    /// The host was too busy to verify the presented tokens; retry later.
    TrustBusy,
}

/// Work that needs the ingress gate.
pub(crate) enum SharingDispatch {
    Actor(Box<SwarmRequest>),
    Trust(Vec<String>),
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
    type ActorRequest = SharingDispatch;
    type Response = SharingResponse;

    const PROTOCOL: StreamProtocol = SHARING_PROTOCOL;
    const SUPPORT: ProtocolSupport = ProtocolSupport::Full;
    const TASK_FAILURE: &'static str = "mesh sharing task failed";

    fn receive(request: SharingRequest) -> InboundRequest<Self::ActorRequest, SharingResponse> {
        match request {
            SharingRequest::Probe => InboundRequest::Reply(SharingResponse::Supported),
            SharingRequest::Actor(request) => {
                InboundRequest::Dispatch(SharingDispatch::Actor(request))
            }
            SharingRequest::Trust(tokens) => {
                InboundRequest::Dispatch(SharingDispatch::Trust(tokens))
            }
        }
    }

    async fn dispatch(
        gate: &dyn MeshIngress,
        peer: PeerId,
        request: Self::ActorRequest,
    ) -> SharingResponse {
        match request {
            SharingDispatch::Actor(request) => {
                SharingResponse::Actor(gate.handle_v1(peer, *request).await)
            }
            SharingDispatch::Trust(tokens) => {
                SharingResponse::Trust(gate.present_trust(peer, tokens).await)
            }
        }
    }

    fn overloaded(request: Self::ActorRequest) -> SharingResponse {
        match request {
            SharingDispatch::Actor(_) => SharingResponse::Actor(Err(SharingRejection::Busy)),
            SharingDispatch::Trust(_) => SharingResponse::TrustBusy,
        }
    }

    fn dispatch_budget(request: &Self::ActorRequest, wire_budget: Duration) -> Duration {
        // V1 callers advertise their whole exchange budget as the reply timeout.
        // A peer can shorten, but never extend, the host's wire budget.
        match request {
            SharingDispatch::Actor(request) => match request.as_ref() {
                SwarmRequest::Ask {
                    reply_timeout: Some(timeout),
                    ..
                } => wire_budget.min(*timeout),
                _ => wire_budget,
            },
            SharingDispatch::Trust(_) => wire_budget,
        }
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

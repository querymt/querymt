use kameo::actor::RemoteActorRef;
use kameo::remote::messaging::{SwarmRequest, SwarmResponse};
use kameo::remote::{RemoteActor, RemoteMessage};

use crate::MeshHandle;
use crate::mesh_ingress::SharingCallError;
use crate::peer_access::SharingProtocol;
use crate::{DeliveryCertainty, RemoteTransportFailure, RemoteTransportFailureKind};
use kameo::error::RemoteSendError;
use kameo::message::Message;
use kameo::reply::Reply;
use std::time::Duration;

/// Send a remote ask request with matching mailbox and reply timeouts.
pub async fn ask_remote_with_timeout<A, M>(
    actor_ref: &RemoteActorRef<A>,
    msg: &M,
    timeout: Duration,
) -> Result<<A::Reply as Reply>::Ok, RemoteSendError<<A::Reply as Reply>::Error>>
where
    A: kameo::Actor + Message<M> + kameo::remote::RemoteActor + kameo::remote::RemoteMessage<M>,
    M: serde::Serialize + Send + Sync + 'static,
    <A::Reply as Reply>::Ok: serde::de::DeserializeOwned,
    <A::Reply as Reply>::Error: serde::de::DeserializeOwned,
{
    actor_ref
        .ask(msg)
        .mailbox_timeout(timeout)
        .reply_timeout(timeout)
        .send()
        .await
}

/// A protected ask can be refused by the ingress gate before reaching an actor.
#[derive(Debug, thiserror::Error)]
pub enum SharingAskError<E: std::fmt::Display> {
    #[error(transparent)]
    Sharing(#[from] SharingCallError),
    #[error(transparent)]
    Actor(#[from] RemoteSendError<E>),
}

/// Send an ask over v1. No legacy fallback, including for unknown methods.
pub async fn ask_sharing_v1<A, M, R, E>(
    mesh: &MeshHandle,
    actor_ref: &RemoteActorRef<A>,
    msg: &M,
    timeout: Duration,
) -> Result<R, SharingAskError<E>>
where
    A: kameo::Actor + RemoteActor + RemoteMessage<M>,
    M: serde::Serialize + Send + Sync + 'static,
    R: serde::de::DeserializeOwned,
    E: serde::de::DeserializeOwned + std::fmt::Debug + std::fmt::Display,
{
    ask_sharing_v1_with_timeouts(mesh, actor_ref, msg, timeout, timeout).await
}

/// Send a v1 ask while retaining distinct mailbox and reply timeouts.
pub async fn ask_sharing_v1_with_timeouts<A, M, R, E>(
    mesh: &MeshHandle,
    actor_ref: &RemoteActorRef<A>,
    msg: &M,
    mailbox_timeout: Duration,
    reply_timeout: Duration,
) -> Result<R, SharingAskError<E>>
where
    A: kameo::Actor + RemoteActor + RemoteMessage<M>,
    M: serde::Serialize + Send + Sync + 'static,
    R: serde::de::DeserializeOwned,
    E: serde::de::DeserializeOwned + std::fmt::Debug + std::fmt::Display,
{
    let actor_id = actor_ref.id();
    let peer = actor_id
        .peer_id()
        .copied()
        .ok_or(SharingCallError::Unavailable)?;
    let payload = rmp_serde::to_vec_named(msg)
        .map_err(|error| RemoteSendError::SerializeMessage(error.to_string()))?;
    let exchange = mesh.sharing_call(
        peer,
        SwarmRequest::Ask {
            actor_id,
            actor_remote_id: <A as RemoteActor>::REMOTE_ID.into(),
            message_remote_id: <A as RemoteMessage<M>>::REMOTE_ID.into(),
            payload,
            mailbox_timeout: Some(mailbox_timeout),
            reply_timeout: Some(reply_timeout),
            immediate: false,
        },
    );
    let response = tokio::time::timeout(reply_timeout, exchange)
        .await
        .map_err(|_| {
            SharingCallError::Transport(libp2p::request_response::OutboundFailure::Timeout)
        })??;
    match response {
        SwarmResponse::Ask(Ok(bytes)) => {
            rmp_serde::from_slice(&bytes).map_err(|_| SharingCallError::UnexpectedReply.into())
        }
        SwarmResponse::Ask(Err(RemoteSendError::HandlerError(bytes))) => {
            let error = rmp_serde::from_slice(&bytes)
                .map_err(|error| RemoteSendError::DeserializeHandlerError(error.to_string()))?;
            Err(RemoteSendError::HandlerError(error).into())
        }
        SwarmResponse::Ask(Err(error)) => Err(error
            .map_err(|_| unreachable!("handler error handled above"))
            .into()),
        _ => Err(SharingCallError::UnexpectedReply.into()),
    }
}

/// Negotiate before the operation and use the selected protocol once.
pub async fn ask_with_sharing<A, M>(
    mesh: &MeshHandle,
    actor_ref: &RemoteActorRef<A>,
    msg: &M,
    timeout: Duration,
) -> Result<<A::Reply as Reply>::Ok, SharingAskError<<A::Reply as Reply>::Error>>
where
    A: kameo::Actor + Message<M> + RemoteActor + RemoteMessage<M>,
    M: serde::Serialize + Send + Sync + 'static,
    <A::Reply as Reply>::Ok: serde::de::DeserializeOwned,
    <A::Reply as Reply>::Error: serde::de::DeserializeOwned + std::fmt::Debug + std::fmt::Display,
{
    ask_with_sharing_timeouts(mesh, actor_ref, msg, timeout, timeout).await
}

/// [`ask_with_sharing`] with distinct mailbox and reply timeouts.
///
/// The protocols measure `reply_timeout` differently. On the legacy protocol
/// it starts once the remote mailbox admits the message, so an exchange can
/// last up to `mailbox_timeout + reply_timeout`. On v1 it bounds the whole
/// exchange, mailbox wait included, and the owner shortens its dispatch
/// budget to it. Choose `reply_timeout >= mailbox_timeout` so both protocols
/// leave the handler time to run.
pub async fn ask_with_sharing_timeouts<A, M>(
    mesh: &MeshHandle,
    actor_ref: &RemoteActorRef<A>,
    msg: &M,
    mailbox_timeout: Duration,
    reply_timeout: Duration,
) -> Result<<A::Reply as Reply>::Ok, SharingAskError<<A::Reply as Reply>::Error>>
where
    A: kameo::Actor + Message<M> + RemoteActor + RemoteMessage<M>,
    M: serde::Serialize + Send + Sync + 'static,
    <A::Reply as Reply>::Ok: serde::de::DeserializeOwned,
    <A::Reply as Reply>::Error: serde::de::DeserializeOwned + std::fmt::Debug + std::fmt::Display,
{
    with_sharing_protocol(
        mesh,
        actor_ref.id().peer_id().copied(),
        reply_timeout,
        ask_sharing_v1_with_timeouts(mesh, actor_ref, msg, mailbox_timeout, reply_timeout),
        async {
            Ok(actor_ref
                .ask(msg)
                .mailbox_timeout(mailbox_timeout)
                .reply_timeout(reply_timeout)
                .send()
                .await?)
        },
    )
    .await?
}

/// Protocol-independent outcome of a failed remote ask.
#[derive(Debug)]
pub enum RemoteAskFailure<E> {
    /// The remote handler ran and returned this error.
    Handler(E),
    /// The owner's access policy refused the request or changed before the reply.
    AccessDenied,
    /// The request failed in transport; see the failure for delivery certainty.
    Transport(RemoteTransportFailure),
}

impl<E: std::fmt::Display> From<RemoteSendError<E>> for RemoteAskFailure<E> {
    fn from(error: RemoteSendError<E>) -> Self {
        if crate::mesh_ingress::is_legacy_access_denial(&error) {
            return Self::AccessDenied;
        }
        match crate::classify_remote_send_error(error) {
            Ok(failure) => Self::Transport(failure),
            Err(error) => Self::Handler(error),
        }
    }
}

impl<E: std::fmt::Display> From<SharingAskError<E>> for RemoteAskFailure<E> {
    fn from(error: SharingAskError<E>) -> Self {
        use crate::mesh_ingress::SharingRejection as Rejection;
        use DeliveryCertainty::{NotDelivered, Unknown};
        use RemoteTransportFailureKind as Kind;

        let (error, kind, delivery) = match error {
            SharingAskError::Actor(error) => return error.into(),
            SharingAskError::Sharing(SharingCallError::Transport(error)) => {
                return RemoteSendError::<E>::from(error).into();
            }
            SharingAskError::Sharing(SharingCallError::Rejected(
                Rejection::Denied | Rejection::Changed,
            )) => return Self::AccessDenied,
            SharingAskError::Sharing(error @ SharingCallError::Rejected(Rejection::Busy)) => {
                (error, Kind::MailboxFull, NotDelivered)
            }
            SharingAskError::Sharing(
                error @ SharingCallError::Rejected(Rejection::Unavailable),
            ) => (error, Kind::ActorUnavailable, NotDelivered),
            // Raised only before submission: no peer id, no guarded transport
            // or a stopped swarm. A reply dropped in flight is a transport failure.
            SharingAskError::Sharing(error @ SharingCallError::Unavailable) => {
                (error, Kind::DialFailure, NotDelivered)
            }
            SharingAskError::Sharing(
                error @ (SharingCallError::UnsupportedProtocol
                | SharingCallError::Rejected(Rejection::UnsupportedOperation)),
            ) => (error, Kind::ProtocolMismatch, NotDelivered),
            SharingAskError::Sharing(error @ SharingCallError::UnexpectedReply) => {
                (error, Kind::Serialization, Unknown)
            }
        };
        Self::Transport(RemoteTransportFailure::new(
            kind,
            delivery,
            error.to_string(),
        ))
    }
}

/// Select a protocol before polling either operation. Only the selected operation
/// runs, so a failure after selection can never trigger a legacy retry.
/// Use `None` for an actor reference without a peer ID; this fails closed.
pub async fn with_sharing_protocol<T>(
    mesh: &MeshHandle,
    peer: Option<libp2p::PeerId>,
    timeout: Duration,
    guarded: impl Future<Output = T>,
    legacy: impl Future<Output = T>,
) -> Result<T, SharingCallError> {
    let peer = peer.ok_or(SharingCallError::Unavailable)?;
    Ok(match mesh.select_sharing_protocol(peer, timeout).await? {
        SharingProtocol::V1 => guarded.await,
        SharingProtocol::Legacy => legacy.await,
    })
}

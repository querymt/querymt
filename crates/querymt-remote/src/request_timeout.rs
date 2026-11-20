use kameo::actor::RemoteActorRef;
use kameo::remote::messaging::{SwarmRequest, SwarmResponse};
use kameo::remote::{RemoteActor, RemoteMessage};

use crate::MeshHandle;
use crate::mesh_ingress::SharingCallError;
use crate::peer_access::SharingProtocol;
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
            mailbox_timeout: Some(timeout),
            reply_timeout: Some(timeout),
            immediate: false,
        },
    );
    let response = tokio::time::timeout(timeout, exchange)
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
    let peer = actor_ref
        .id()
        .peer_id()
        .copied()
        .ok_or(SharingCallError::Unavailable)?;
    let protocol = mesh.select_sharing_protocol(peer, timeout).await?;
    match protocol {
        SharingProtocol::Legacy => Ok(ask_remote_with_timeout(actor_ref, msg, timeout).await?),
        SharingProtocol::V1 => ask_sharing_v1(mesh, actor_ref, msg, timeout).await,
    }
}

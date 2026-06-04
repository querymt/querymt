use kameo::actor::RemoteActorRef;
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

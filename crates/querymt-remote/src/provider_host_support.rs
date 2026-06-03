use crate::{ProviderBuildRequest, RemoteProviderBackend, StreamChunkRelay, StreamRelayMessage, relay_message_is_terminal};
use kameo::message::{Context, Message};
use kameo::remote::_internal;
use querymt::LLMProvider;
use tokio::sync::mpsc;

/// Ephemeral actor spawned on the requesting node for each streaming call.
///
/// Receives `StreamChunkRelay` messages via a direct `RemoteActorRef` and
/// forwards them into an `mpsc` channel. The actor stops itself after a
/// terminal relay so the receiver side sees EOF.
pub struct StreamReceiverActor {
    tx: mpsc::Sender<StreamRelayMessage>,
}

impl kameo::Actor for StreamReceiverActor {
    type Args = Self;
    type Error = kameo::error::Infallible;

    async fn on_start(
        args: Self::Args,
        _actor_ref: kameo::actor::ActorRef<Self>,
    ) -> Result<Self, Self::Error> {
        Ok(args)
    }

    async fn on_stop(
        &mut self,
        _actor_ref: kameo::actor::WeakActorRef<Self>,
        _reason: kameo::error::ActorStopReason,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl StreamReceiverActor {
    pub fn new(tx: mpsc::Sender<StreamRelayMessage>) -> Self {
        Self { tx }
    }
}

impl Message<StreamChunkRelay> for StreamReceiverActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamChunkRelay,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let is_terminal = relay_message_is_terminal(&msg.message);
        let _ = self.tx.send(msg.message).await;
        if is_terminal {
            ctx.actor_ref().kill();
        }
    }
}

/// Serialize `LLMParams` for forwarding to a remote provider, excluding
/// credentials and fields that are already passed separately.
pub fn params_for_remote_provider(params: &querymt::LLMParams) -> Option<serde_json::Value> {
    serde_json::to_value(params).ok().and_then(|v| match v {
        serde_json::Value::Object(mut obj) => {
            obj.remove("api_key");
            obj.remove("provider");
            obj.remove("model");
            obj.remove("name");
            if obj.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(obj))
            }
        }
        _ => None,
    })
}

fn sanitized_request_params(request: &serde_json::Value) -> Option<serde_json::Value> {
    let mut sanitized = request.clone();
    if let Some(obj) = sanitized.as_object_mut() {
        obj.remove("api_key");
        obj.remove("_remote_session_id");
    }

    if sanitized.as_object().is_some_and(|o| o.is_empty()) {
        None
    } else {
        Some(sanitized)
    }
}

/// Merge per-request params with host defaults while stripping transport-only
/// metadata and credentials from the request overlay.
pub fn merge_remote_provider_params(
    request_params: Option<&serde_json::Value>,
    host_defaults: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let request_params = request_params.and_then(sanitized_request_params);

    match (host_defaults, request_params.as_ref()) {
        (None, None) => None,
        (Some(defaults), None) => Some(defaults.clone()),
        (None, Some(request)) => Some(request.clone()),
        (Some(defaults), Some(request)) => {
            let mut merged = defaults.clone();
            if let (Some(base), Some(overlay)) = (merged.as_object_mut(), request.as_object()) {
                for (key, value) in overlay {
                    base.insert(key.clone(), value.clone());
                }
            }
            if merged.as_object().is_some_and(|o| o.is_empty()) {
                None
            } else {
                Some(merged)
            }
        }
    }
}

/// Build a provider for a single remote request, merging request params with
/// backend-provided host defaults.
#[tracing::instrument(
    name = "remote.provider_host.build_provider_for_request",
    skip(backend, request_params),
    fields(provider = %provider_name, model = %model)
)]
pub async fn build_provider_for_request<E>(
    backend: &dyn RemoteProviderBackend<Error = E>,
    provider_name: &str,
    model: &str,
    request_params: Option<&serde_json::Value>,
) -> Result<std::sync::Arc<dyn LLMProvider>, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let host_defaults = backend.host_default_params(provider_name, model);
    let merged = merge_remote_provider_params(request_params, host_defaults.as_ref());
    backend
        .build_provider(ProviderBuildRequest::new(provider_name, model).with_params(merged))
        .await
}

impl kameo::remote::RemoteActor for StreamReceiverActor {
    const REMOTE_ID: &'static str = "querymt::StreamReceiverActor";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_ACTORS)]
#[linkme(crate = _internal::linkme)]
static STREAM_RECEIVER_ACTOR_REG: (&'static str, _internal::RemoteActorFns) = (
    <StreamReceiverActor as kameo::remote::RemoteActor>::REMOTE_ID,
    _internal::RemoteActorFns {
        link: (|actor_id, sibling_id, sibling_remote_id| {
            Box::pin(_internal::link::<StreamReceiverActor>(
                actor_id,
                sibling_id,
                sibling_remote_id,
            ))
        }) as _internal::RemoteLinkFn,
        unlink: (|actor_id, sibling_id| {
            Box::pin(_internal::unlink::<StreamReceiverActor>(actor_id, sibling_id))
        }) as _internal::RemoteUnlinkFn,
        signal_link_died: (|dead_actor_id, notified_actor_id, stop_reason| {
            Box::pin(_internal::signal_link_died::<StreamReceiverActor>(
                dead_actor_id,
                notified_actor_id,
                stop_reason,
            ))
        }) as _internal::RemoteSignalLinkDiedFn,
    },
);

impl kameo::remote::RemoteMessage<StreamChunkRelay> for StreamReceiverActor {
    const REMOTE_ID: &'static str = "querymt::StreamChunkRelay";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_MESSAGES)]
#[linkme(crate = _internal::linkme)]
static REG_STREAM_CHUNK_RELAY: (
    _internal::RemoteMessageRegistrationID<'static>,
    _internal::RemoteMessageFns,
) = (
    _internal::RemoteMessageRegistrationID {
        actor_remote_id: <StreamReceiverActor as kameo::remote::RemoteActor>::REMOTE_ID,
        message_remote_id: <StreamReceiverActor as kameo::remote::RemoteMessage<StreamChunkRelay>>::REMOTE_ID,
    },
    _internal::RemoteMessageFns {
        ask: (|actor_id, msg, mailbox_timeout, reply_timeout| {
            Box::pin(_internal::ask::<StreamReceiverActor, StreamChunkRelay>(
                actor_id,
                msg,
                mailbox_timeout,
                reply_timeout,
            ))
        }) as _internal::RemoteAskFn,
        try_ask: (|actor_id, msg, reply_timeout| {
            Box::pin(_internal::try_ask::<StreamReceiverActor, StreamChunkRelay>(
                actor_id,
                msg,
                reply_timeout,
            ))
        }) as _internal::RemoteTryAskFn,
        tell: (|actor_id, msg, mailbox_timeout| {
            Box::pin(_internal::tell::<StreamReceiverActor, StreamChunkRelay>(
                actor_id,
                msg,
                mailbox_timeout,
            ))
        }) as _internal::RemoteTellFn,
        try_tell: (|actor_id, msg| {
            Box::pin(_internal::try_tell::<StreamReceiverActor, StreamChunkRelay>(actor_id, msg))
        }) as _internal::RemoteTryTellFn,
    },
);

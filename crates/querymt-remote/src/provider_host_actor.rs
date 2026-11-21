use crate::provider_host_error::RemoteProviderHostError;
use crate::provider_protocol::{
    CancelProviderStreamRequest, GetProviderContractInfo, GetProviderStreamStatus,
    ITEM_AWARE_CHAT_CONTRACT_VERSION, ProviderChatRequest, ProviderChatResponse,
    ProviderContractInfo, ProviderStreamRequest, ProviderStreamStatus, RenewProviderStreamLease,
    StreamRelayMessage, keep_stream_message_buffered, messages_require_item_aware_contract,
};
use crate::{RemoteProviderBackend, build_provider_for_request};
use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::remote::_internal;
use querymt::chat::ChatCall;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

mod streams;
use streams::{LiveStreams, Owner, Relay, StreamRequest, matching};

#[cfg(feature = "kameo-mesh")]
use crate::{
    MeshHandle,
    mesh_ingress::PeerRequest,
    provider_share::guarded::{GuardedProvider, failure, policy_error, quota_error},
};

pub(crate) fn validate_item_aware_request(
    messages: &[querymt::chat::ChatMessage],
    advertised_version: Option<u32>,
) -> Result<(), RemoteProviderHostError> {
    if !messages_require_item_aware_contract(messages) {
        return Ok(());
    }
    if advertised_version != Some(ITEM_AWARE_CHAT_CONTRACT_VERSION) {
        return Err(RemoteProviderHostError::provider_chat_message(
            "contract_check",
            format!(
                "item-aware history requires contract version {}; request advertised {}",
                ITEM_AWARE_CHAT_CONTRACT_VERSION,
                advertised_version.map_or_else(|| "none".to_string(), |value| value.to_string())
            ),
        ));
    }
    Ok(())
}

#[derive(Actor)]
pub struct ProviderHostActor {
    backend: Arc<dyn RemoteProviderBackend<Error = RemoteProviderHostError>>,
    active_streams: LiveStreams,
    shutdown: CancellationToken,
}

impl ProviderHostActor {
    pub fn new(backend: Arc<dyn RemoteProviderBackend<Error = RemoteProviderHostError>>) -> Self {
        Self {
            backend,
            active_streams: Default::default(),
            shutdown: CancellationToken::new(),
        }
    }
}

impl Drop for ProviderHostActor {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl Message<GetProviderContractInfo> for ProviderHostActor {
    type Reply = Result<ProviderContractInfo, RemoteProviderHostError>;

    #[tracing::instrument(
        name = "remote.provider_host.contract_info",
        // `msg` is skipped: its params may carry an inline API key.
        skip(self, msg, _ctx),
        // `auth.source` is recorded by the backend's credential resolution.
        fields(provider = %msg.provider, model = %msg.model, auth.source = tracing::field::Empty)
    )]
    async fn handle(
        &mut self,
        msg: GetProviderContractInfo,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let host_defaults = self.backend.host_default_params(&msg.provider, &msg.model);
        let params =
            crate::merge_remote_provider_params(msg.params.as_ref(), host_defaults.as_ref());
        self.backend
            .contract_info(
                crate::ProviderBuildRequest::new(&msg.provider, &msg.model).with_params(params),
            )
            .await
    }
}

impl Message<ProviderChatRequest> for ProviderHostActor {
    type Reply =
        kameo::reply::DelegatedReply<Result<ProviderChatResponse, RemoteProviderHostError>>;

    #[tracing::instrument(
        name = "remote.provider_host.chat",
        // `msg` is skipped: its params may carry an inline API key.
        skip(self, msg, ctx),
        fields(
            provider = %msg.provider,
            model = %msg.model,
            message_count = msg.messages.len(),
            has_tools = msg.tools.is_some(),
            has_params = msg.params.is_some(),
            tool_calls_returned = tracing::field::Empty,
            finish_reason = tracing::field::Empty,
        )
    )]
    async fn handle(
        &mut self,
        msg: ProviderChatRequest,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let backend = Arc::clone(&self.backend);

        ctx.spawn(async move {
            validate_item_aware_request(&msg.messages, msg.item_aware_contract_version)?;
            let provider = build_provider_for_request(
                backend.as_ref(),
                &msg.provider,
                &msg.model,
                msg.params.as_ref(),
            )
            .await?;

            let call = ChatCall::new(&msg.messages)
                .maybe_tools(msg.tools.as_deref())
                .options(msg.options.unwrap_or_default());
            let response = provider
                .chat(&call)
                .await
                .map_err(|e| RemoteProviderHostError::provider_chat("chat", &e))?;

            let tool_calls = response.tool_calls().unwrap_or_default();
            let finish_reason = response.finish_reason.map(|r| format!("{:?}", r));
            tracing::Span::current()
                .record("tool_calls_returned", tool_calls.len())
                .record("finish_reason", finish_reason.as_deref().unwrap_or("none"));

            Ok(ProviderChatResponse::from(response))
        })
    }
}

impl Message<ProviderStreamRequest<kameo::actor::RemoteActorRef<crate::ProviderStreamRouterActor>>>
    for ProviderHostActor
{
    type Reply = kameo::reply::DelegatedReply<Result<(), RemoteProviderHostError>>;

    #[tracing::instrument(
        name = "remote.provider_host.stream",
        // `msg` is skipped: its params may carry an inline API key.
        skip(self, msg, ctx),
        fields(
            provider = %msg.provider,
            model = %msg.model,
            session_id = %msg.session_id,
            request_id = %msg.request_id,
            message_count = msg.messages.len(),
            has_tools = msg.tools.is_some(),
            has_params = msg.params.is_some(),
            receiver_found = tracing::field::Empty,
        )
    )]
    async fn handle(
        &mut self,
        msg: StreamRequest,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let backend = self.backend.clone();
        let streams = self.active_streams.clone();
        let cancel = self.shutdown.child_token();
        ctx.spawn(async move {
            let relay = Relay::new(
                streams,
                &msg,
                Owner::Local,
                cancel,
                #[cfg(feature = "kameo-mesh")]
                None,
            )?;
            match relay.open_local(backend.as_ref(), msg).await {
                Ok(stream) => {
                    tokio::spawn(relay.run(std::future::ready(Ok(stream)), None));
                }
                Err(error) => relay.local_open_failed(error),
            }
            Ok(())
        })
    }
}

/// Outgoing relay queue for one stream. Data, terminal and failure messages
/// get contiguous per-request sequence numbers starting at 0 when queued;
/// heartbeats and connection notices stay unnumbered. A message keeps its
/// number until it is popped, so resends after a send error reuse it.
#[derive(Debug, Default)]
pub(crate) struct RelayQueue {
    queue: VecDeque<(Option<u64>, StreamRelayMessage)>,
    next_seq: u64,
}

impl RelayQueue {
    pub(crate) fn push_sequenced(&mut self, message: StreamRelayMessage) {
        self.queue.push_back((Some(self.next_seq), message));
        self.next_seq += 1;
    }

    pub(crate) fn push_unsequenced(&mut self, message: StreamRelayMessage) {
        self.queue.push_back((None, message));
    }

    pub(crate) fn front_cloned(&self) -> Option<(Option<u64>, StreamRelayMessage)> {
        self.queue.front().cloned()
    }

    pub(crate) fn pop_front(&mut self) {
        self.queue.pop_front();
    }

    fn clear(&mut self) {
        self.queue.clear();
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn replayable_count(&self) -> usize {
        self.queue
            .iter()
            .filter(|(_, msg)| keep_stream_message_buffered(msg))
            .count()
    }
}

impl Message<CancelProviderStreamRequest> for ProviderHostActor {
    type Reply = usize;

    async fn handle(
        &mut self,
        msg: CancelProviderStreamRequest,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let streams = matching(
            &self.active_streams,
            &Owner::Local,
            &msg.session_id,
            msg.request_id.as_deref(),
        );
        for stream in &streams {
            stream.cancel(msg.reason.as_deref());
        }
        streams.len()
    }
}

impl Message<RenewProviderStreamLease> for ProviderHostActor {
    type Reply = bool;

    async fn handle(
        &mut self,
        msg: RenewProviderStreamLease,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        matching(
            &self.active_streams,
            &Owner::Local,
            &msg.session_id,
            Some(&msg.request_id),
        )
        .first()
        .is_some_and(|stream| stream.renew(msg.lease_ttl_secs))
    }
}

impl Message<GetProviderStreamStatus> for ProviderHostActor {
    type Reply = Option<ProviderStreamStatus>;

    async fn handle(
        &mut self,
        msg: GetProviderStreamStatus,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        matching(
            &self.active_streams,
            &Owner::Local,
            &msg.session_id,
            msg.request_id.as_deref(),
        )
        .first()
        .map(|stream| stream.status())
    }
}

#[cfg(feature = "kameo-mesh")]
pub(crate) struct GuardedStreamStart(
    pub Arc<GuardedProvider>,
    pub MeshHandle,
    pub PeerRequest,
    pub StreamRequest,
    pub CancellationToken,
);

#[cfg(feature = "kameo-mesh")]
impl Message<GuardedStreamStart> for ProviderHostActor {
    type Reply = kameo::reply::DelegatedReply<Result<(), RemoteProviderHostError>>;

    async fn handle(
        &mut self,
        GuardedStreamStart(policy, mesh, peer, mut msg, admission): GuardedStreamStart,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use crate::peer_access::quota::QuotaError;
        use querymt::error::ProviderErrorKind;

        let streams = self.active_streams.clone();
        let cancel = self.shutdown.child_token();
        ctx.spawn(async move {
            if msg.stream_router_ref.id().peer_id() != Some(peer.key().peer.as_peer_id()) {
                return Err(quota_error(QuotaError::Denied));
            }
            let id = uuid::Uuid::parse_str(&msg.request_id).map_err(|_| {
                failure(ProviderErrorKind::InvalidRequest, "mesh_stream_id", "invalid stream request ID")
            })?;
            validate_item_aware_request(&msg.messages, msg.item_aware_contract_version)?;
            let request = policy.build_request(&msg.provider, &msg.model, msg.params.take())?;
            let access = policy.access(&peer, &msg.provider, &msg.model)?;
            let owner = Owner::peer(&peer)?;
            let (provider, mut reservation) = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(RemoteProviderHostError::Internal("provider host stopped".into())),
                _ = admission.cancelled() => return Err(RemoteProviderHostError::Internal("stream admission cancelled".into())),
                result = async {
                    let call = streams::call(&msg);
                    let provider = policy.prepare(&peer, request, &call, id, true).await?;
                    let reservation = provider.reserve(&call, true).await.map_err(policy_error)?;
                    Ok::<_, RemoteProviderHostError>((provider, reservation))
                } => result?,
            };
            if admission.is_cancelled() {
                reservation.release().await.map_err(quota_error)?;
                return Err(RemoteProviderHostError::Internal("stream admission cancelled".into()));
            }
            let relay = Relay::new(streams, &msg, owner, cancel, Some((mesh, access)))?;
            let settled = reservation.completion();
            tokio::spawn(relay.run(async move {
                provider.stream_reserved(&streams::call(&msg), reservation).await
                    .map_err(|error| RemoteProviderHostError::provider_chat("chat_stream", &error))
            }, Some(settled)));
            Ok(())
        })
    }
}

#[cfg(feature = "kameo-mesh")]
pub(crate) struct GuardedStreamControl<M>(pub PeerRequest, pub M);

#[cfg(feature = "kameo-mesh")]
macro_rules! guarded_control {
    ($msg:ty, $result:ty, $request_id:expr, |$streams:ident, $request:ident| $operation:block) => {
        impl Message<GuardedStreamControl<$msg>> for ProviderHostActor {
            type Reply = kameo::reply::DelegatedReply<Result<$result, RemoteProviderHostError>>;

            async fn handle(
                &mut self,
                GuardedStreamControl(peer, $request): GuardedStreamControl<$msg>,
                ctx: &mut Context<Self, Self::Reply>,
            ) -> Self::Reply {
                let active = self.active_streams.clone();
                ctx.spawn(async move {
                    let owner = Owner::peer(&peer)?;
                    let $streams = matching(&active, &owner, &$request.session_id, $request_id);
                    $operation
                })
            }
        }
    };
}

#[cfg(feature = "kameo-mesh")]
guarded_control!(
    GetProviderStreamStatus,
    Option<ProviderStreamStatus>,
    msg.request_id.as_deref(),
    |streams, msg| {
        if let Some(stream) = streams.first() {
            stream.authorize().await?;
            Ok(Some(stream.status()))
        } else {
            Ok(None)
        }
    }
);

#[cfg(feature = "kameo-mesh")]
guarded_control!(
    RenewProviderStreamLease,
    bool,
    Some(&msg.request_id),
    |streams, msg| {
        if let Some(stream) = streams.first() {
            stream.authorize().await?;
            Ok(stream.renew(msg.lease_ttl_secs))
        } else {
            Ok(false)
        }
    }
);

#[cfg(feature = "kameo-mesh")]
guarded_control!(
    CancelProviderStreamRequest,
    usize,
    msg.request_id.as_deref(),
    |streams, msg| {
        for stream in &streams {
            stream.authorize().await?;
            stream.cancel(msg.reason.as_deref());
        }
        Ok(streams.len())
    }
);

impl kameo::remote::RemoteActor for ProviderHostActor {
    const REMOTE_ID: &'static str = "querymt::ProviderHostActor";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_ACTORS)]
#[linkme(crate = _internal::linkme)]
static PROVIDER_HOST_ACTOR_REG: (&'static str, _internal::RemoteActorFns) = (
    <ProviderHostActor as kameo::remote::RemoteActor>::REMOTE_ID,
    _internal::RemoteActorFns {
        link: (|actor_id, sibling_id, sibling_remote_id| {
            Box::pin(_internal::link::<ProviderHostActor>(
                actor_id,
                sibling_id,
                sibling_remote_id,
            ))
        }) as _internal::RemoteLinkFn,
        unlink: (|actor_id, sibling_id| {
            Box::pin(_internal::unlink::<ProviderHostActor>(actor_id, sibling_id))
        }) as _internal::RemoteUnlinkFn,
        signal_link_died: (|dead_actor_id, notified_actor_id, stop_reason| {
            Box::pin(_internal::signal_link_died::<ProviderHostActor>(
                dead_actor_id,
                notified_actor_id,
                stop_reason,
            ))
        }) as _internal::RemoteSignalLinkDiedFn,
    },
);

macro_rules! remote_provider_msg_impl {
    ($actor:ty, $msg_ty:ty, $remote_id:expr, $static_name:ident) => {
        impl kameo::remote::RemoteMessage<$msg_ty> for $actor {
            const REMOTE_ID: &'static str = $remote_id;
        }

        #[_internal::linkme::distributed_slice(_internal::REMOTE_MESSAGES)]
        #[linkme(crate = _internal::linkme)]
        static $static_name: (
            _internal::RemoteMessageRegistrationID<'static>,
            _internal::RemoteMessageFns,
        ) = (
            _internal::RemoteMessageRegistrationID {
                actor_remote_id: <$actor as kameo::remote::RemoteActor>::REMOTE_ID,
                message_remote_id: <$actor as kameo::remote::RemoteMessage<$msg_ty>>::REMOTE_ID,
            },
            _internal::RemoteMessageFns {
                ask: (|actor_id, msg, mailbox_timeout, reply_timeout| {
                    Box::pin(_internal::ask::<$actor, $msg_ty>(
                        actor_id,
                        msg,
                        mailbox_timeout,
                        reply_timeout,
                    ))
                }) as _internal::RemoteAskFn,
                try_ask: (|actor_id, msg, reply_timeout| {
                    Box::pin(_internal::try_ask::<$actor, $msg_ty>(
                        actor_id,
                        msg,
                        reply_timeout,
                    ))
                }) as _internal::RemoteTryAskFn,
                tell: (|actor_id, msg, mailbox_timeout| {
                    Box::pin(_internal::tell::<$actor, $msg_ty>(
                        actor_id,
                        msg,
                        mailbox_timeout,
                    ))
                }) as _internal::RemoteTellFn,
                try_tell: (|actor_id, msg| {
                    Box::pin(_internal::try_tell::<$actor, $msg_ty>(actor_id, msg))
                }) as _internal::RemoteTryTellFn,
            },
        );
    };
}

remote_provider_msg_impl!(
    ProviderHostActor,
    GetProviderContractInfo,
    "querymt::GetProviderContractInfo.v1",
    REG_GET_PROVIDER_CONTRACT_INFO
);
remote_provider_msg_impl!(
    ProviderHostActor,
    ProviderChatRequest,
    "querymt::ProviderChatRequest",
    REG_PROVIDER_CHAT_REQUEST
);
remote_provider_msg_impl!(
    ProviderHostActor,
    ProviderStreamRequest<kameo::actor::RemoteActorRef<crate::ProviderStreamRouterActor>>,
    "querymt::ProviderStreamRequest",
    REG_PROVIDER_STREAM_REQUEST
);
remote_provider_msg_impl!(
    ProviderHostActor,
    CancelProviderStreamRequest,
    "querymt::CancelProviderStreamRequest",
    REG_CANCEL_PROVIDER_STREAM_REQUEST
);
remote_provider_msg_impl!(
    ProviderHostActor,
    RenewProviderStreamLease,
    "querymt::RenewProviderStreamLease",
    REG_RENEW_PROVIDER_STREAM_LEASE
);
remote_provider_msg_impl!(
    ProviderHostActor,
    GetProviderStreamStatus,
    "querymt::GetProviderStreamStatus",
    REG_GET_PROVIDER_STREAM_STATUS
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderStreamPhase;
    use async_trait::async_trait;
    use kameo::actor::Spawn;
    use parking_lot::Mutex;
    use querymt::LLMProvider;
    use querymt::chat::StreamChunk;
    use querymt::chat::{
        ChatCall, ChatCapabilities, ChatOutput, ChatProvider, GenerationOptionSupport,
        GenerationOptions,
    };
    use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
    use querymt::embedding::EmbeddingProvider;
    use querymt::error::LLMError;

    use crate::{ProviderBuildRequest, RemoteProviderBackend};

    /// Provider that records the per-request options it receives.
    #[derive(Default)]
    struct RecordingProvider {
        seen: Arc<Mutex<Vec<GenerationOptions>>>,
    }

    #[async_trait]
    impl ChatProvider for RecordingProvider {
        async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
            self.seen.lock().push(call.options);
            Ok(ChatOutput::default())
        }

        fn capabilities(&self) -> ChatCapabilities {
            ChatCapabilities::NONE.with_options(GenerationOptionSupport::ALL)
        }
    }

    #[async_trait]
    impl CompletionProvider for RecordingProvider {
        async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
            Err(LLMError::NotImplemented("unused".into()))
        }
    }

    #[async_trait]
    impl EmbeddingProvider for RecordingProvider {
        async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            Err(LLMError::NotImplemented("unused".into()))
        }
    }

    impl LLMProvider for RecordingProvider {}

    struct RecordingBackend {
        seen: Arc<Mutex<Vec<GenerationOptions>>>,
    }

    #[async_trait]
    impl RemoteProviderBackend for RecordingBackend {
        type Error = RemoteProviderHostError;

        async fn build_provider(
            &self,
            _request: ProviderBuildRequest,
        ) -> Result<Arc<dyn LLMProvider>, Self::Error> {
            Ok(Arc::new(RecordingProvider {
                seen: Arc::clone(&self.seen),
            }))
        }
    }

    fn chat_request(options: Option<GenerationOptions>) -> ProviderChatRequest {
        ProviderChatRequest {
            provider: "demo".into(),
            model: "m1".into(),
            messages: Vec::new(),
            tools: None,
            params: None,
            item_aware_contract_version: None,
            options,
        }
    }

    #[tokio::test]
    async fn chat_request_options_reach_the_backend_provider() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let actor = ProviderHostActor::spawn(ProviderHostActor::new(Arc::new(RecordingBackend {
            seen: Arc::clone(&seen),
        })));
        let options = GenerationOptions::default().with_max_tokens(64_000);

        actor
            .ask(chat_request(Some(options)))
            .await
            .expect("chat with options");

        assert_eq!(*seen.lock(), [options]);
    }

    #[tokio::test]
    async fn chat_request_without_options_keeps_configured_values() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let actor = ProviderHostActor::spawn(ProviderHostActor::new(Arc::new(RecordingBackend {
            seen: Arc::clone(&seen),
        })));

        actor
            .ask(chat_request(None))
            .await
            .expect("chat without options");

        assert_eq!(*seen.lock(), [GenerationOptions::NONE]);
    }

    #[tokio::test]
    async fn actor_requires_the_item_aware_contract_before_replaying_native_history() {
        use querymt::chat::{ChatMessage, ChatOutputItem, ChatReasoningItem};

        let seen = Arc::new(Mutex::new(Vec::new()));
        let actor = ProviderHostActor::spawn(ProviderHostActor::new(Arc::new(RecordingBackend {
            seen: seen.clone(),
        })));
        let mut request = chat_request(None);
        request
            .messages
            .push(ChatMessage::from_assistant_output(ChatOutput {
                items: vec![ChatOutputItem::Reasoning(ChatReasoningItem {
                    encrypted_content: Some("opaque continuation".into()),
                    id: None,
                    summary: vec![],
                    content: vec![],
                    signature: None,
                    status: None,
                    extensions: Default::default(),
                })],
                ..Default::default()
            }));

        let result = actor.ask(request.clone()).await;
        assert!(matches!(result, Err(kameo::error::SendError::HandlerError(
            RemoteProviderHostError::ProviderChat { operation, .. }
        )) if operation == "contract_check"));
        assert!(seen.lock().is_empty());

        request.item_aware_contract_version = Some(ITEM_AWARE_CHAT_CONTRACT_VERSION);
        actor.ask(request).await.expect("compatible contract");
        assert_eq!(*seen.lock(), [GenerationOptions::NONE]);
    }

    #[tokio::test]
    async fn contract_info_reports_the_built_providers_capabilities() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let actor =
            ProviderHostActor::spawn(ProviderHostActor::new(Arc::new(RecordingBackend { seen })));

        let info = actor
            .ask(GetProviderContractInfo {
                provider: "demo".into(),
                model: "m1".into(),
                params: None,
            })
            .await
            .expect("contract info");

        assert_eq!(info.capabilities.options, GenerationOptionSupport::ALL);
    }

    fn heartbeat() -> StreamRelayMessage {
        StreamRelayMessage::Heartbeat {
            phase: ProviderStreamPhase::Streaming,
            elapsed_ms: 0,
            idle_ms: 0,
            chunk_count: 0,
        }
    }

    fn text(s: &str) -> StreamRelayMessage {
        StreamRelayMessage::Chunk(StreamChunk::Text(s.into()))
    }

    #[test]
    fn relay_queue_numbers_data_and_terminals_contiguously_and_skips_heartbeats() {
        let mut queue = RelayQueue::default();
        queue.push_sequenced(text("a"));
        queue.push_unsequenced(heartbeat());
        queue.push_sequenced(text("b"));
        queue.push_unsequenced(StreamRelayMessage::TransportDisconnected { reason: "x".into() });
        queue.push_sequenced(StreamRelayMessage::TransportFailed {
            error: querymt::error::LLMError::Transport {
                kind: querymt::error::TransportErrorKind::Timeout,
                message: "grace".into(),
            }
            .to_payload(),
        });

        let mut seqs = Vec::new();
        while let Some((seq, _)) = queue.front_cloned() {
            seqs.push(seq);
            queue.pop_front();
        }
        assert_eq!(seqs, vec![Some(0), None, Some(1), None, Some(2)]);
        assert!(queue.is_empty());
    }

    #[test]
    fn relay_queue_resend_after_send_error_reuses_the_same_number() {
        let mut queue = RelayQueue::default();
        queue.push_sequenced(text("a"));
        queue.push_sequenced(text("b"));

        // A failed send does not pop; the host re-reads the front.
        let first = queue.front_cloned().expect("front");
        let resent = queue.front_cloned().expect("front again");
        assert_eq!(first.0, Some(0));
        assert_eq!(resent.0, Some(0));
        // Messages queued meanwhile keep counting up.
        queue.push_sequenced(text("c"));
        queue.pop_front();
        assert_eq!(queue.front_cloned().expect("front").0, Some(1));
    }

    /// A resent message (same `seq`) reaches the consumer once.
    #[tokio::test]
    async fn router_delivers_resent_message_once() {
        use crate::provider_stream_router::ProviderStreamRouterActor;
        use crate::stream_router_protocol::RoutedStreamRelayMessage;

        let router_ref =
            ProviderStreamRouterActor::spawn(ProviderStreamRouterActor::new(None, None));
        router_ref
            .ask(crate::provider_stream_router::RegisterRequest {
                request_id: "r".into(),
            })
            .await
            .expect("register");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        router_ref
            .ask(crate::provider_stream_router::AttachStreamConsumer {
                request_id: "r".into(),
                consumer_tx: tx,
            })
            .await
            .expect("attach");

        let mut queue = RelayQueue::default();
        queue.push_sequenced(text("a"));
        let (seq, message) = queue.front_cloned().expect("front");
        for _ in 0..2 {
            router_ref
                .ask(RoutedStreamRelayMessage {
                    request_id: "r".into(),
                    message: message.clone(),
                    seq,
                })
                .await
                .expect("route");
        }
        assert!(matches!(rx.try_recv(), Ok(StreamRelayMessage::Chunk(_))));
        assert!(rx.try_recv().is_err(), "duplicate must not be delivered");
    }
}

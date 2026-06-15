use crate::{RemoteProviderClientCore, RemoteProviderClientTransport, RemoteProviderStreamState};
use futures_util::StreamExt;
use querymt::LLMProvider;
use querymt::chat::{ChatMessage, ChatProvider, StreamChunk, Tool};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::LLMError;
use std::pin::Pin;
use std::time::Instant;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Clone)]
pub struct RemoteChatProvider<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    core: RemoteProviderClientCore<TTransport>,
}

impl<TTransport> RemoteChatProvider<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    pub fn new(core: RemoteProviderClientCore<TTransport>) -> Self {
        Self { core }
    }

    pub fn core(&self) -> &RemoteProviderClientCore<TTransport> {
        &self.core
    }

    fn remote_session_id(&self) -> Option<&str> {
        self.core.config().remote_session_id()
    }

    pub async fn cancel_remote_stream(
        &self,
        session_id: &str,
        request_id: Option<&str>,
        reason: Option<&str>,
    ) {
        let _ = self
            .core
            .cancel_stream(session_id, request_id, reason)
            .await;
    }

    pub async fn get_remote_stream_status(
        &self,
        session_id: &str,
        request_id: Option<&str>,
    ) -> Option<crate::ProviderStreamStatus> {
        self.core.get_stream_status(session_id, request_id).await
    }
}

#[async_trait::async_trait]
impl<TTransport> ChatProvider for RemoteChatProvider<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    fn supports_streaming(&self) -> bool {
        true
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn querymt::chat::ChatResponse>, LLMError> {
        let chat_response = self
            .core
            .chat_with_tools(messages, tools, |error| {
                matches!(
                    error,
                    LLMError::Transport {
                        kind: querymt::error::TransportErrorKind::ConnectionClosed
                            | querymt::error::TransportErrorKind::ConnectionRefused,
                        ..
                    }
                )
            })
            .await?;

        Ok(Box::new(chat_response))
    }

    async fn chat_stream_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<
        Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
        LLMError,
    > {
        let request_id = Uuid::now_v7().to_string();
        let session_id = self
            .remote_session_id()
            .unwrap_or("unknown-session")
            .to_string();
        let provider_name = self.core.config().provider_name.clone();
        let model_name = self.core.config().model.clone();
        let target_locator = self.core.config().target_locator.clone();
        let setup_span = tracing::info_span!(
            "remote.provider_client.stream.setup",
            session_id = %session_id,
            request_id = %request_id,
            provider = %provider_name,
            model = %model_name,
            target_locator = %target_locator,
            message_count = messages.len(),
            tool_count = tools.map(|t| t.len()).unwrap_or(0),
            first_chunk_ms = tracing::field::Empty,
        );
        let _setup_guard = setup_span.enter();

        tracing::info!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            provider = %provider_name,
            model = %model_name,
            target_locator = %target_locator,
            "starting remote provider stream setup"
        );

        tracing::debug!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            "looking up remote provider host"
        );
        let host_ref = self.core.lookup_host().await?;
        tracing::debug!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            "remote provider host lookup completed"
        );

        let (tx, rx) = mpsc::channel::<crate::StreamRelayMessage>(64);
        tracing::debug!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            channel_capacity = 64,
            "registering and attaching stream router consumer"
        );
        let (_router_ref, remote_router_ref) = self
            .core
            .prepare_stream_router(&session_id, &request_id, tx)
            .await?;
        tracing::debug!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            "stream router consumer attached"
        );

        let stream_request = self.core.build_stream_request(
            messages,
            tools,
            session_id.clone(),
            request_id.clone(),
            remote_router_ref,
            self.core.transport().stream_reconnect_grace().as_secs(),
        );

        tracing::debug!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            "sending remote provider stream request"
        );
        self.core
            .send_stream_request_with_retry(&host_ref, stream_request, |error| {
                matches!(
                    error,
                    LLMError::Transport {
                        kind: querymt::error::TransportErrorKind::ConnectionClosed
                            | querymt::error::TransportErrorKind::ConnectionRefused,
                        ..
                    }
                )
            })
            .await?;
        tracing::info!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            provider = %provider_name,
            model = %model_name,
            target_locator = %target_locator,
            "remote provider stream request acknowledged"
        );

        let raw_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let session_id_for_stream = session_id.clone();
        let request_id_for_stream = request_id.clone();
        let provider_for_stream = provider_name.clone();
        let model_for_stream = model_name.clone();
        let target_for_stream = target_locator.clone();
        let reconnect_grace = self.core.transport().stream_reconnect_grace();
        let stream_start = Instant::now();
        let local_peer_id = self.core.transport().local_peer_id_display().await;
        let target_peer_id_display = self
            .core
            .transport()
            .target_peer_id_display(self.core.config().target_locator())
            .await;
        let lease_renew_every = self.core.config().lease_renew_every();
        let renew_lease_factory =
            self.core
                .make_renew_lease_fn(host_ref.clone(), session_id.clone(), request_id.clone());
        let peer_alive_fn = self.core.make_target_peer_alive_fn();
        let setup_span = setup_span.clone();
        tracing::info!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            local_peer_id = %local_peer_id,
            target_peer_id = %target_peer_id_display,
            lease_renew_every_ms = lease_renew_every.as_millis(),
            reconnect_grace_ms = reconnect_grace.as_millis(),
            "remote provider stream consumer ready"
        );

        let stream = futures_util::stream::unfold(
            (
                raw_stream,
                RemoteProviderStreamState::new(),
                tokio::time::Instant::now() + lease_renew_every,
            ),
            move |(mut raw_stream, mut stream_state, mut renew_due)| {
                let session_id_for_stream = session_id_for_stream.clone();
                let request_id_for_stream = request_id_for_stream.clone();
                let target_peer_id_display = target_peer_id_display.clone();
                let local_peer_id = local_peer_id.clone();
                let provider_for_stream = provider_for_stream.clone();
                let model_for_stream = model_for_stream.clone();
                let target_for_stream = target_for_stream.clone();
                let setup_span = setup_span.clone();
                let renew_lease = renew_lease_factory.clone();
                let peer_alive_fn = peer_alive_fn.clone();
                async move {
                    if stream_state.is_finished() {
                        tracing::debug!(
                            target: "querymt_remote::provider::stream",
                            session_id = %session_id_for_stream,
                            request_id = %request_id_for_stream,
                            chunk_index = stream_state.chunk_index(),
                            "remote provider stream finished after terminal chunk"
                        );
                        return None;
                    }

                    if let Some((chunk, chunk_index, first_chunk_ms)) =
                        stream_state.take_pending(stream_start)
                    {
                        if let Some(first_chunk_ms) = first_chunk_ms {
                            setup_span.record("first_chunk_ms", first_chunk_ms);
                        }
                        if let StreamChunk::Done { finish_reason } = &chunk {
                            tracing::info!(
                                target: "querymt_remote::provider::stream",
                                session_id = %session_id_for_stream,
                                request_id = %request_id_for_stream,
                                local_peer_id = %local_peer_id,
                                target_peer_id = %target_peer_id_display,
                                provider = %provider_for_stream,
                                model = %model_for_stream,
                                target_node = %target_for_stream,
                                chunk_index,
                                elapsed_ms = stream_start.elapsed().as_millis(),
                                finish_reason = ?finish_reason,
                                pending_chunks = stream_state.pending_chunks_len(),
                                "stream done received from remote provider pending batch"
                            );
                        }
                        return Some((Ok(chunk), (raw_stream, stream_state, renew_due)));
                    }

                    loop {
                        let now = tokio::time::Instant::now();
                        let sleep = if now >= renew_due {
                            let _ = renew_lease().await;
                            renew_due = tokio::time::Instant::now() + lease_renew_every;
                            tokio::time::sleep(std::time::Duration::ZERO)
                        } else {
                            tokio::time::sleep_until(renew_due)
                        };

                        let next = if let Some(remaining) =
                            stream_state.reconnect_remaining(reconnect_grace)
                        {
                            if remaining.is_zero() {
                                return Some((
                                    Err(RemoteProviderClientCore::<TTransport>::reconnect_timeout_error(reconnect_grace)),
                                    (raw_stream, stream_state, renew_due),
                                ));
                            }
                            tokio::select! {
                                item = raw_stream.next() => item,
                                _ = sleep => {
                                    let _ = renew_lease().await;
                                    renew_due = tokio::time::Instant::now() + lease_renew_every;
                                    continue;
                                }
                                _ = tokio::time::sleep(remaining) => {
                                    return Some((
                                        Err(RemoteProviderClientCore::<TTransport>::reconnect_timeout_error(reconnect_grace)),
                                        (raw_stream, stream_state, renew_due),
                                    ));
                                }
                            }
                        } else {
                            tokio::select! {
                                item = raw_stream.next() => item,
                                _ = sleep => {
                                    let _ = renew_lease().await;
                                    renew_due = tokio::time::Instant::now() + lease_renew_every;
                                    continue;
                                }
                            }
                        };

                        let peer_alive = peer_alive_fn().await;
                        match RemoteProviderClientCore::<TTransport>::poll_stream_message(
                            crate::PollStreamContext {
                                setup_span: setup_span.clone(),
                                stream_start,
                                session_id: &session_id_for_stream,
                                request_id: &request_id_for_stream,
                                local_peer_id: &local_peer_id,
                                target_peer_id: &target_peer_id_display,
                                provider_name: &provider_for_stream,
                                model: &model_for_stream,
                                target_name: &target_for_stream,
                            },
                            next,
                            &mut stream_state,
                            peer_alive,
                        ) {
                            Ok(Some(chunk)) => {
                                break Some((Ok(chunk), (raw_stream, stream_state, renew_due)));
                            }
                            Ok(None) => {
                                continue;
                            }
                            Err(error) => {
                                break Some((Err(error), (raw_stream, stream_state, renew_due)));
                            }
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}

#[async_trait::async_trait]
impl<TTransport> CompletionProvider for RemoteChatProvider<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented(
            "RemoteChatProvider: completion not supported (use chat instead)".into(),
        ))
    }
}

#[async_trait::async_trait]
impl<TTransport> EmbeddingProvider for RemoteChatProvider<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented(
            "RemoteChatProvider: embedding not supported".into(),
        ))
    }
}

impl<TTransport> LLMProvider for RemoteChatProvider<TTransport> where
    TTransport: RemoteProviderClientTransport + 'static
{
}

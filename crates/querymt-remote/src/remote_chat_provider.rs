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
        let host_ref = self.core.lookup_host().await?;
        let request_id = Uuid::now_v7().to_string();
        let session_id = self
            .remote_session_id()
            .unwrap_or("unknown-session")
            .to_string();

        let (tx, rx) = mpsc::channel::<crate::StreamRelayMessage>(64);
        let (_router_ref, remote_router_ref) = self
            .core
            .prepare_stream_router(&session_id, &request_id, tx)
            .await?;

        let stream_request = self.core.build_stream_request(
            messages,
            tools,
            session_id.clone(),
            request_id.clone(),
            remote_router_ref,
            self.core.transport().stream_reconnect_grace().as_secs(),
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

        let raw_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let session_id_for_stream = session_id.clone();
        let request_id_for_stream = request_id.clone();
        let provider_for_stream = self.core.config().provider_name.clone();
        let model_for_stream = self.core.config().model.clone();
        let target_for_stream = self.core.config().target_locator.clone();
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
        let setup_span = tracing::Span::current();

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
                    if let Some((chunk, _chunk_index, first_chunk_ms)) =
                        stream_state.take_pending(stream_start)
                    {
                        if let Some(first_chunk_ms) = first_chunk_ms {
                            setup_span.record("first_chunk_ms", first_chunk_ms);
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

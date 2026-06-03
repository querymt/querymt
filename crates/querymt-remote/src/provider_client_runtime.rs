use crate::{
    CancelProviderStreamRequest, GetProviderStreamStatus, ProviderChatRequest, ProviderChatResponse,
    ProviderStreamRequest, ProviderStreamStatus, RemoteProviderClientConfig, StreamRelayMessage,
};
use async_trait::async_trait;
use querymt::error::{LLMError, TransportErrorKind};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

#[async_trait]
pub trait RemoteProviderClientTransport: Send + Sync {
    type HostRef: Clone + Send + Sync + 'static;
    type RouterRef: Clone + Send + Sync + 'static;
    type RemoteRouterRef: Clone + Send + Sync + 'static;

    async fn local_peer_id_display(&self) -> String;
    async fn target_peer_id_display(&self, target_locator: &str) -> String;
    async fn invalidate_cached_host(&self, target_locator: &str);
    async fn lookup_host(&self, target_locator: &str) -> Result<Self::HostRef, LLMError>;
    async fn prepare_stream_router(
        &self,
        session_id: &str,
        request_id: &str,
        consumer_tx: tokio::sync::mpsc::Sender<StreamRelayMessage>,
    ) -> Result<(Self::RouterRef, Self::RemoteRouterRef), LLMError>;
    async fn send_chat_request(
        &self,
        host: &Self::HostRef,
        request: &ProviderChatRequest,
    ) -> Result<ProviderChatResponse, LLMError>;
    async fn send_stream_request(
        &self,
        host: &Self::HostRef,
        request: ProviderStreamRequest<Self::RemoteRouterRef>,
    ) -> Result<(), LLMError>;
    async fn cancel_stream(
        &self,
        host: &Self::HostRef,
        request: CancelProviderStreamRequest,
    ) -> Result<(), LLMError>;
    async fn renew_stream_lease(
        &self,
        host: &Self::HostRef,
        session_id: &str,
        request_id: &str,
        lease_ttl_secs: u64,
    ) -> Result<bool, LLMError>;
    async fn get_stream_status(
        &self,
        host: &Self::HostRef,
        request: GetProviderStreamStatus,
    ) -> Result<Option<ProviderStreamStatus>, LLMError>;
    async fn is_target_peer_alive(&self, target_locator: &str) -> bool;
    fn stream_reconnect_grace(&self) -> std::time::Duration;
}

pub type RenewLeaseFuture = Pin<Box<dyn Future<Output = bool> + Send + 'static>>;
pub type PeerAliveFuture = Pin<Box<dyn Future<Output = bool> + Send + 'static>>;
pub type StreamRenewFn = Arc<dyn Fn() -> RenewLeaseFuture + Send + Sync>;
pub type StreamPeerAliveFn = Arc<dyn Fn() -> PeerAliveFuture + Send + Sync>;

#[derive(Clone)]
pub struct RemoteProviderClientCore<TTransport>
where
    TTransport: RemoteProviderClientTransport,
{
    transport: Arc<TTransport>,
    config: RemoteProviderClientConfig,
}

impl<TTransport> RemoteProviderClientCore<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    pub fn new(transport: Arc<TTransport>, config: RemoteProviderClientConfig) -> Self {
        Self { transport, config }
    }

    pub fn config(&self) -> &RemoteProviderClientConfig {
        &self.config
    }

    pub fn transport(&self) -> &Arc<TTransport> {
        &self.transport
    }

    pub async fn lookup_host(&self) -> Result<TTransport::HostRef, LLMError> {
        self.transport.lookup_host(self.config.target_locator()).await
    }

    pub async fn invalidate_cached_host(&self) {
        self.transport
            .invalidate_cached_host(self.config.target_locator())
            .await;
    }

    pub fn build_chat_request(
        &self,
        messages: &[querymt::chat::ChatMessage],
        tools: Option<&[querymt::chat::Tool]>,
    ) -> ProviderChatRequest {
        self.config.build_chat_request(messages, tools)
    }

    pub async fn chat_with_tools(
        &self,
        messages: &[querymt::chat::ChatMessage],
        tools: Option<&[querymt::chat::Tool]>,
        should_retry: impl Fn(&LLMError) -> bool,
    ) -> Result<ProviderChatResponse, LLMError> {
        let request = self.build_chat_request(messages, tools);
        self.send_chat_request_with_retry(&request, should_retry).await
    }

    pub fn build_stream_request(
        &self,
        messages: &[querymt::chat::ChatMessage],
        tools: Option<&[querymt::chat::Tool]>,
        session_id: String,
        request_id: String,
        remote_router_ref: TTransport::RemoteRouterRef,
        reconnect_grace_secs: u64,
    ) -> ProviderStreamRequest<TTransport::RemoteRouterRef> {
        self.config.build_stream_request(
            messages,
            tools,
            session_id,
            request_id,
            remote_router_ref,
            reconnect_grace_secs,
        )
    }

    pub async fn cancel_stream(
        &self,
        session_id: &str,
        request_id: Option<&str>,
        reason: Option<&str>,
    ) -> Result<(), LLMError> {
        let host = self.lookup_host().await?;
        self.transport
            .cancel_stream(
                &host,
                CancelProviderStreamRequest {
                    session_id: session_id.to_string(),
                    request_id: request_id.map(str::to_string),
                    reason: reason.map(str::to_string),
                },
            )
            .await
    }

    pub async fn renew_stream_lease(&self, session_id: &str, request_id: &str) -> bool {
        let Ok(host) = self.lookup_host().await else {
            return false;
        };
        self.transport
            .renew_stream_lease(&host, session_id, request_id, self.config.lease_ttl_secs)
            .await
            .unwrap_or(false)
    }

    pub async fn get_stream_status(
        &self,
        session_id: &str,
        request_id: Option<&str>,
    ) -> Option<ProviderStreamStatus> {
        let Ok(host) = self.lookup_host().await else {
            return None;
        };
        self.transport
            .get_stream_status(
                &host,
                GetProviderStreamStatus {
                    session_id: session_id.to_string(),
                    request_id: request_id.map(str::to_string),
                },
            )
            .await
            .ok()
            .flatten()
    }

    pub async fn send_chat_request_with_retry(
        &self,
        request: &ProviderChatRequest,
        should_retry: impl Fn(&LLMError) -> bool,
    ) -> Result<ProviderChatResponse, LLMError> {
        let host = self.lookup_host().await?;
        match self.transport.send_chat_request(&host, request).await {
            Ok(response) => Ok(response),
            Err(error) if should_retry(&error) => {
                self.invalidate_cached_host().await;
                let host = self.lookup_host().await?;
                self.transport.send_chat_request(&host, request).await
            }
            Err(error) => Err(error),
        }
    }

    pub async fn prepare_stream_router(
        &self,
        session_id: &str,
        request_id: &str,
        consumer_tx: tokio::sync::mpsc::Sender<StreamRelayMessage>,
    ) -> Result<(TTransport::RouterRef, TTransport::RemoteRouterRef), LLMError> {
        self.transport
            .prepare_stream_router(session_id, request_id, consumer_tx)
            .await
    }

    pub async fn send_stream_request_with_retry(
        &self,
        host: &TTransport::HostRef,
        request: ProviderStreamRequest<TTransport::RemoteRouterRef>,
        should_retry: impl Fn(&LLMError) -> bool,
    ) -> Result<(), LLMError> {
        match self.transport.send_stream_request(host, request.clone()).await {
            Ok(()) => Ok(()),
            Err(error) if should_retry(&error) => {
                self.invalidate_cached_host().await;
                let host = self.lookup_host().await?;
                self.transport.send_stream_request(&host, request).await
            }
            Err(error) => Err(error),
        }
    }

    pub fn renew_lease_future(
        &self,
        host: TTransport::HostRef,
        session_id: String,
        request_id: String,
    ) -> RenewLeaseFuture {
        let transport = Arc::clone(&self.transport);
        let lease_ttl_secs = self.config.lease_ttl_secs;
        Box::pin(async move {
            transport
                .renew_stream_lease(&host, &session_id, &request_id, lease_ttl_secs)
                .await
                .unwrap_or(false)
        })
    }

    pub fn target_peer_alive_future(&self) -> PeerAliveFuture {
        let transport = Arc::clone(&self.transport);
        let target_locator = self.config.target_locator.clone();
        Box::pin(async move { transport.is_target_peer_alive(&target_locator).await })
    }

    pub fn make_renew_lease_fn(
        &self,
        host: TTransport::HostRef,
        session_id: String,
        request_id: String,
    ) -> StreamRenewFn {
        let transport = Arc::clone(&self.transport);
        let lease_ttl_secs = self.config.lease_ttl_secs;
        Arc::new(move || {
            let transport = Arc::clone(&transport);
            let session_id = session_id.clone();
            let request_id = request_id.clone();
            let host = host.clone();
            Box::pin(async move {
                transport
                    .renew_stream_lease(&host, &session_id, &request_id, lease_ttl_secs)
                    .await
                    .unwrap_or(false)
            })
        })
    }

    pub fn make_target_peer_alive_fn(&self) -> StreamPeerAliveFn {
        let transport = Arc::clone(&self.transport);
        let target_locator = self.config.target_locator.clone();
        Arc::new(move || {
            let transport = Arc::clone(&transport);
            let target_locator = target_locator.clone();
            Box::pin(async move { transport.is_target_peer_alive(&target_locator).await })
        })
    }

    pub fn poll_stream_message(
        setup_span: tracing::Span,
        stream_start: std::time::Instant,
        _reconnect_grace: std::time::Duration,
        session_id: &str,
        request_id: &str,
        local_peer_id: &str,
        target_peer_id: &str,
        provider_name: &str,
        model: &str,
        target_name: &str,
        next: Option<StreamRelayMessage>,
        stream_state: &mut crate::RemoteProviderStreamState,
        peer_alive: bool,
    ) -> Result<Option<querymt::chat::StreamChunk>, LLMError> {
        match next {
            Some(StreamRelayMessage::Chunk(chunk)) => {
                let elapsed_ms = stream_start.elapsed().as_millis();
                let (chunk_index, first_chunk_ms) = stream_state.note_chunk(stream_start);
                if let Some(first_chunk_ms) = first_chunk_ms {
                    setup_span.record("first_chunk_ms", first_chunk_ms);
                }
                if let querymt::chat::StreamChunk::Done { finish_reason } = &chunk {
                    tracing::debug!(
                        target: "querymt_remote::provider::stream",
                        session_id = %session_id,
                        request_id = %request_id,
                        local_peer_id = %local_peer_id,
                        target_peer_id = %target_peer_id,
                        provider = %provider_name,
                        model = %model,
                        target_node = %target_name,
                        chunk_index,
                        elapsed_ms,
                        finish_reason = ?finish_reason,
                        "stream done received from remote provider"
                    );
                } else {
                    tracing::trace!(
                        target: "querymt_remote::provider::stream",
                        session_id = %session_id,
                        request_id = %request_id,
                        local_peer_id = %local_peer_id,
                        target_peer_id = %target_peer_id,
                        provider = %provider_name,
                        model = %model,
                        target_node = %target_name,
                        chunk_index,
                        elapsed_ms,
                        "stream chunk received"
                    );
                }
                Ok(Some(chunk))
            }
            Some(StreamRelayMessage::ChunkBatch(chunks)) => {
                let elapsed_ms = stream_start.elapsed().as_millis();
                let Some((chunk, _chunk_index, first_chunk_ms, batch_len)) =
                    stream_state.push_batch_and_take_first(stream_start, chunks)
                else {
                    tracing::warn!(
                        target: "querymt_remote::provider::stream",
                        session_id = %session_id,
                        request_id = %request_id,
                        "empty chunk batch after extend; continuing"
                    );
                    return Ok(None);
                };
                tracing::trace!(
                    target: "remote::mesh_provider::stream",
                    session_id = %session_id,
                    request_id = %request_id,
                    local_peer_id = %local_peer_id,
                    target_peer_id = %target_peer_id,
                    provider = %provider_name,
                    model = %model,
                    target_node = %target_name,
                    batch_len,
                    elapsed_ms,
                    "stream batch received"
                );
                if let Some(first_chunk_ms) = first_chunk_ms {
                    setup_span.record("first_chunk_ms", first_chunk_ms);
                }
                Ok(Some(chunk))
            }
            Some(StreamRelayMessage::Heartbeat {
                phase,
                elapsed_ms,
                idle_ms,
                chunk_count,
            }) => {
                tracing::info!(
                    target: "querymt_remote::provider::heartbeat",
                    session_id = %session_id,
                    request_id = %request_id,
                    local_peer_id = %local_peer_id,
                    target_peer_id = %target_peer_id,
                    provider = %provider_name,
                    model = %model,
                    target_node = %target_name,
                    phase = ?phase,
                    elapsed_ms,
                    idle_ms,
                    chunk_count,
                    "remote provider heartbeat"
                );
                Ok(None)
            }
            Some(StreamRelayMessage::ProviderError { error }) => Err(LLMError::from_payload(error)),
            Some(StreamRelayMessage::TransportDisconnected { reason }) => {
                tracing::warn!(
                    target: "remote::mesh_provider::stream",
                    session_id = %session_id,
                    request_id = %request_id,
                    local_peer_id = %local_peer_id,
                    target_peer_id = %target_peer_id,
                    provider = %provider_name,
                    model = %model,
                    target_node = %target_name,
                    reason,
                    "stream transport disconnected (internal state, continuing)"
                );
                stream_state.note_disconnect();
                Ok(None)
            }
            Some(StreamRelayMessage::TransportReconnected { buffered_chunks }) => {
                tracing::info!(
                    target: "remote::mesh_provider::stream",
                    session_id = %session_id,
                    request_id = %request_id,
                    local_peer_id = %local_peer_id,
                    target_peer_id = %target_peer_id,
                    provider = %provider_name,
                    model = %model,
                    target_node = %target_name,
                    buffered_chunks,
                    "stream transport reconnected (internal state update)"
                );
                stream_state.note_reconnect();
                Ok(None)
            }
            Some(StreamRelayMessage::TransportFailed { error }) => Err(LLMError::from_payload(error)),
            None => {
                if let Some(error) = stream_state.closed_error(peer_alive) {
                    Err(error)
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub fn reconnect_timeout_error(reconnect_grace: std::time::Duration) -> LLMError {
        LLMError::Transport {
            kind: TransportErrorKind::Timeout,
            message: format!("reconnect grace expired after {:?}", reconnect_grace),
        }
    }
}

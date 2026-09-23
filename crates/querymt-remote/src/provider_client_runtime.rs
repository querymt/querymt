use crate::{
    CancelProviderStreamRequest, GetProviderContractInfo, GetProviderStreamStatus,
    ProviderChatRequest, ProviderChatResponse, ProviderContractInfo, ProviderStreamRequest,
    ProviderStreamStatus, RemoteProviderClientConfig, StreamRelayMessage,
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
    async fn get_contract_info(
        &self,
        host: &Self::HostRef,
        request: GetProviderContractInfo,
    ) -> Result<ProviderContractInfo, LLMError>;
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

pub struct PollStreamContext<'a> {
    pub setup_span: tracing::Span,
    pub stream_start: std::time::Instant,
    pub session_id: &'a str,
    pub request_id: &'a str,
    pub local_peer_id: &'a str,
    pub target_peer_id: &'a str,
    pub provider_name: &'a str,
    pub model: &'a str,
    pub target_name: &'a str,
}

#[derive(Clone)]
pub struct RemoteProviderClientCore<TTransport>
where
    TTransport: RemoteProviderClientTransport,
{
    transport: Arc<TTransport>,
    config: RemoteProviderClientConfig,
    contract_info: tokio::sync::OnceCell<ProviderContractInfo>,
}

impl<TTransport> RemoteProviderClientCore<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    pub fn new(transport: Arc<TTransport>, config: RemoteProviderClientConfig) -> Self {
        Self {
            transport,
            config,
            contract_info: tokio::sync::OnceCell::new(),
        }
    }

    pub fn config(&self) -> &RemoteProviderClientConfig {
        &self.config
    }

    pub fn transport(&self) -> &Arc<TTransport> {
        &self.transport
    }

    pub async fn lookup_host(&self) -> Result<TTransport::HostRef, LLMError> {
        self.transport
            .lookup_host(self.config.target_locator())
            .await
    }

    pub async fn invalidate_cached_host(&self) {
        self.transport
            .invalidate_cached_host(self.config.target_locator())
            .await;
    }

    pub async fn validate_contract(
        &self,
        host: &TTransport::HostRef,
        required_version: Option<u32>,
    ) -> Result<(), LLMError> {
        // The handshake is unconditional: it doubles as the mesh wire-protocol
        // version check, so wire-incompatible peers fail fast with a clear
        // message instead of opaque MessagePack decoding errors later.
        let info = self
            .contract_info
            .get_or_try_init(|| async {
                self.transport
                    .get_contract_info(host, GetProviderContractInfo)
                    .await
                    .map_err(|error| match error {
                        // Transport failures keep their classification so upstream
                        // retry policies still treat them as retryable connection
                        // errors rather than contract problems.
                        err @ LLMError::Transport { .. } => err,
                        error => LLMError::InvalidRequest(format!(
                            "remote provider peer failed the mesh protocol handshake: {error}"
                        )),
                    })
            })
            .await?;
        match info.protocol_version {
            Some(version) if version == crate::provider_protocol::MESH_PROTOCOL_VERSION => {}
            other => {
                return Err(LLMError::InvalidRequest(format!(
                    "remote provider peer speaks mesh protocol version {}, but this build requires {}; upgrade both peers to the same querymt build",
                    other.map_or_else(
                        || "unknown (peer predates protocol versioning)".to_string(),
                        |version| version.to_string()
                    ),
                    crate::provider_protocol::MESH_PROTOCOL_VERSION
                )));
            }
        }
        let Some(required_version) = required_version else {
            return Ok(());
        };
        if info.item_aware_chat_version != Some(required_version) {
            return Err(LLMError::InvalidRequest(format!(
                "remote provider peer cannot advertise item-aware chat contract version {required_version}: advertised version: {}",
                info.item_aware_chat_version
                    .map_or_else(|| "none".to_string(), |value| value.to_string())
            )));
        }
        Ok(())
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
        let host = self.lookup_host().await?;
        self.validate_contract(&host, request.item_aware_contract_version)
            .await?;
        self.send_chat_request_with_retry(&host, &request, should_retry)
            .await
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
        host: &TTransport::HostRef,
        request: &ProviderChatRequest,
        should_retry: impl Fn(&LLMError) -> bool,
    ) -> Result<ProviderChatResponse, LLMError> {
        match self.transport.send_chat_request(host, request).await {
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
        match self
            .transport
            .send_stream_request(host, request.clone())
            .await
        {
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
        context: PollStreamContext<'_>,
        next: Option<StreamRelayMessage>,
        stream_state: &mut crate::RemoteProviderStreamState,
        peer_alive: bool,
    ) -> Result<Option<querymt::chat::StreamChunk>, LLMError> {
        let PollStreamContext {
            setup_span,
            stream_start,
            session_id,
            request_id,
            local_peer_id,
            target_peer_id,
            provider_name,
            model,
            target_name,
        } = context;
        match next {
            Some(StreamRelayMessage::Chunk(chunk)) => {
                let elapsed_ms = stream_start.elapsed().as_millis();
                let (chunk_index, first_chunk_ms) =
                    stream_state.note_chunk_received(&chunk, stream_start);
                if let Some(first_chunk_ms) = first_chunk_ms {
                    setup_span.record("first_chunk_ms", first_chunk_ms);
                }
                if querymt::chat::chunk_is_terminal(&chunk) {
                    tracing::info!(
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
                        chunk = ?chunk,
                        pending_chunks = stream_state.pending_chunks_len(),
                        "stream terminal received from remote provider"
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
                tracing::debug!(
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
                    pending_chunks = stream_state.pending_chunks_len(),
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
            Some(StreamRelayMessage::TransportFailed { error }) => {
                Err(LLMError::from_payload(error))
            }
            None => {
                tracing::warn!(
                    target: "querymt_remote::provider::stream",
                    session_id = %session_id,
                    request_id = %request_id,
                    local_peer_id = %local_peer_id,
                    target_peer_id = %target_peer_id,
                    provider = %provider_name,
                    model = %model,
                    target_node = %target_name,
                    peer_alive,
                    disconnected = stream_state.is_disconnected(),
                    pending_chunks = stream_state.pending_chunks_len(),
                    chunk_index = stream_state.chunk_index(),
                    elapsed_ms = stream_start.elapsed().as_millis(),
                    terminal_seen = stream_state.terminal_seen(),
                    "remote provider stream channel closed before terminal"
                );
                Err(stream_state.closed_error(peer_alive))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_protocol::MESH_PROTOCOL_VERSION;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestTransport {
        contract_info: ProviderContractInfo,
        contract_calls: AtomicUsize,
    }

    #[async_trait]
    impl RemoteProviderClientTransport for TestTransport {
        type HostRef = ();
        type RouterRef = ();
        type RemoteRouterRef = ();

        async fn local_peer_id_display(&self) -> String {
            "local".into()
        }

        async fn target_peer_id_display(&self, _target_locator: &str) -> String {
            "remote".into()
        }

        async fn invalidate_cached_host(&self, _target_locator: &str) {}

        async fn lookup_host(&self, _target_locator: &str) -> Result<Self::HostRef, LLMError> {
            Ok(())
        }

        async fn get_contract_info(
            &self,
            _host: &Self::HostRef,
            _request: GetProviderContractInfo,
        ) -> Result<ProviderContractInfo, LLMError> {
            self.contract_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.contract_info.clone())
        }

        async fn prepare_stream_router(
            &self,
            _session_id: &str,
            _request_id: &str,
            _consumer_tx: tokio::sync::mpsc::Sender<StreamRelayMessage>,
        ) -> Result<(Self::RouterRef, Self::RemoteRouterRef), LLMError> {
            unreachable!("not used by contract tests")
        }

        async fn send_chat_request(
            &self,
            _host: &Self::HostRef,
            _request: &ProviderChatRequest,
        ) -> Result<ProviderChatResponse, LLMError> {
            unreachable!("not used by contract tests")
        }

        async fn send_stream_request(
            &self,
            _host: &Self::HostRef,
            _request: ProviderStreamRequest<Self::RemoteRouterRef>,
        ) -> Result<(), LLMError> {
            unreachable!("not used by contract tests")
        }

        async fn cancel_stream(
            &self,
            _host: &Self::HostRef,
            _request: CancelProviderStreamRequest,
        ) -> Result<(), LLMError> {
            Ok(())
        }

        async fn renew_stream_lease(
            &self,
            _host: &Self::HostRef,
            _session_id: &str,
            _request_id: &str,
            _lease_ttl_secs: u64,
        ) -> Result<bool, LLMError> {
            Ok(true)
        }

        async fn get_stream_status(
            &self,
            _host: &Self::HostRef,
            _request: GetProviderStreamStatus,
        ) -> Result<Option<ProviderStreamStatus>, LLMError> {
            Ok(None)
        }

        async fn is_target_peer_alive(&self, _target_locator: &str) -> bool {
            true
        }

        fn stream_reconnect_grace(&self) -> std::time::Duration {
            std::time::Duration::from_secs(1)
        }
    }

    fn core_with_contract(
        contract_info: ProviderContractInfo,
    ) -> (Arc<TestTransport>, RemoteProviderClientCore<TestTransport>) {
        let transport = Arc::new(TestTransport {
            contract_info,
            contract_calls: AtomicUsize::new(0),
        });
        let core = RemoteProviderClientCore::new(
            Arc::clone(&transport),
            RemoteProviderClientConfig::new("peer", "llama_cpp", "model"),
        );
        (transport, core)
    }

    #[tokio::test]
    async fn contract_handshake_is_cached_after_success() {
        let (transport, core) = core_with_contract(ProviderContractInfo {
            item_aware_chat_version: None,
            protocol_version: Some(MESH_PROTOCOL_VERSION),
        });

        core.validate_contract(&(), None).await.unwrap();
        core.validate_contract(&(), None).await.unwrap();

        assert_eq!(transport.contract_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_protocol_version_fails_with_upgrade_guidance() {
        let (_transport, core) = core_with_contract(ProviderContractInfo {
            item_aware_chat_version: None,
            protocol_version: None,
        });

        let error = core.validate_contract(&(), None).await.unwrap_err();
        assert!(matches!(error, LLMError::InvalidRequest(_)));
        assert!(error.to_string().contains("predates protocol versioning"));
        assert!(error.to_string().contains("upgrade both peers"));
    }
}

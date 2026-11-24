use crate::{
    CancelProviderStreamRequest, GetProviderContractInfo, GetProviderStreamStatus,
    ProviderChatRequest, ProviderChatResponse, ProviderContractInfo, ProviderStreamRequest,
    ProviderStreamStatus, RemoteProviderClientConfig, StreamRelayMessage,
};
use async_trait::async_trait;
use querymt::chat::{ChatCall, ChatCapabilities};
use querymt::error::{LLMError, TransportErrorKind};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Stable label for the failing error class, used in handshake diagnostics.
///
/// Keeps transport failures distinguishable from semantic rejections in logs so
/// an operator can tell a mesh outage apart from a protocol mismatch.
fn error_kind_label(error: &LLMError) -> &'static str {
    match error {
        LLMError::Transport { kind, .. } => match kind {
            TransportErrorKind::ConnectionRefused => "transport_connection_refused",
            TransportErrorKind::ConnectionReset => "transport_connection_reset",
            TransportErrorKind::Timeout => "transport_timeout",
            TransportErrorKind::ConnectionClosed => "transport_connection_closed",
            TransportErrorKind::Dns => "transport_dns",
            TransportErrorKind::Tls => "transport_tls",
            TransportErrorKind::ProtocolMismatch => "transport_protocol_mismatch",
            TransportErrorKind::Other => "transport_other",
        },
        LLMError::IoError { .. } => "io_error",
        LLMError::InvalidRequest(_) => "invalid_request",
        LLMError::ProviderError(_) => "provider_error",
        LLMError::Cancelled => "cancelled",
        _ => "other",
    }
}

#[async_trait]
pub trait RemoteProviderClientTransport: Send + Sync {
    type HostRef: Clone + Send + Sync + 'static;
    type RouterRef: Clone + Send + Sync + 'static;
    type RemoteRouterRef: Clone + Send + Sync + 'static;

    async fn local_peer_id_display(&self) -> String;
    async fn target_peer_id_display(&self, target_locator: &str) -> String;
    async fn invalidate_cached_host(&self, target_locator: &str);
    fn is_stale_host(&self, _error: &LLMError) -> bool {
        false
    }
    fn retry_dispatch(&self, _error: &LLMError, fallback: bool) -> bool {
        fallback
    }
    async fn lookup_host(&self, target_locator: &str) -> Result<Self::HostRef, LLMError>;
    async fn get_contract_info(
        &self,
        host: &Self::HostRef,
        request: GetProviderContractInfo,
    ) -> Result<ProviderContractInfo, LLMError>;
    async fn prepare_stream_router(
        &self,
        host: &Self::HostRef,
        session_id: &str,
        request_id: &str,
        consumer_tx: tokio::sync::mpsc::UnboundedSender<StreamRelayMessage>,
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
pub type CancelStreamFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

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
    contract_info: Arc<parking_lot::RwLock<Arc<tokio::sync::OnceCell<ProviderContractInfo>>>>,
}

impl<TTransport> RemoteProviderClientCore<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    pub fn new(transport: Arc<TTransport>, config: RemoteProviderClientConfig) -> Self {
        Self {
            transport,
            config,
            contract_info: Arc::default(),
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
        *self.contract_info.write() = Arc::default();
    }

    async fn refresh_host(
        &self,
        required_version: Option<u32>,
    ) -> Result<TTransport::HostRef, LLMError> {
        self.invalidate_cached_host().await;
        let host = self.lookup_host().await?;
        self.validate_contract(&host, required_version).await?;
        Ok(host)
    }

    pub(crate) async fn lookup_validated_host(
        &self,
        required_version: Option<u32>,
    ) -> Result<TTransport::HostRef, LLMError> {
        let host = self.lookup_host().await?;
        self.validate_or_refresh_host(host, required_version).await
    }

    pub(crate) async fn validate_or_refresh_host(
        &self,
        host: TTransport::HostRef,
        required_version: Option<u32>,
    ) -> Result<TTransport::HostRef, LLMError> {
        match self.validate_contract(&host, required_version).await {
            Ok(()) => Ok(host),
            Err(error) if self.transport.is_stale_host(&error) => {
                self.refresh_host(required_version).await
            }
            Err(error) => {
                self.invalidate_cached_host().await;
                Err(error)
            }
        }
    }

    /// Perform the mesh contract handshake and verify protocol compatibility.
    ///
    /// The handshake is unconditional (cached after the first success) so
    /// wire-incompatible peers fail fast with an actionable message instead of
    /// opaque MessagePack decoding errors later. Connectivity failures are
    /// preserved as transport errors — a mesh timeout is not a provider
    /// rejection and must stay retryable.
    pub async fn validate_contract(
        &self,
        host: &TTransport::HostRef,
        required_version: Option<u32>,
    ) -> Result<(), LLMError> {
        self.validated_contract_info(host, required_version)
            .await
            .map(|_| ())
    }

    async fn validated_contract_info(
        &self,
        host: &TTransport::HostRef,
        required_version: Option<u32>,
    ) -> Result<ProviderContractInfo, LLMError> {
        let started = std::time::Instant::now();
        let cache = self.contract_info.read().clone();
        let info = match cache.get_or_try_init(|| async {
                let info = self
                    .transport
                    .get_contract_info(
                        host,
                        GetProviderContractInfo {
                            provider: self.config.provider_name.clone(),
                            model: self.config.model.clone(),
                            params: self.config.params.clone(),
                            pass: self.config.pass.clone(),
                        },
                    )
                    .await?;
                // Validate inside the init closure so a peer with a missing or
                // incompatible protocol version is never cached: a later call
                // re-fetches and can succeed once the peer is upgraded.
                match info.protocol_version {
                    Some(version) if version == crate::provider_protocol::MESH_PROTOCOL_VERSION => {
                        // A node that predates model passes ignores the field and
                        // would serve the request under this node's own access.
                        match (&self.config.pass, &info.charged_peer) {
                            (Some(pass), charged) if charged.as_deref() != Some(pass.issuer.as_str()) => {
                                Err(LLMError::InvalidRequest(
                                    "the model node does not confirm that it charges the model pass issuer".to_string(),
                                ))
                            }
                            _ => Ok(info),
                        }
                    }
                    other => {
                        let error = LLMError::InvalidRequest(format!(
                            "remote provider peer speaks mesh protocol version {}, but this build requires {}; upgrade both peers to the same querymt build",
                            other.map_or_else(
                                || "unknown (peer predates protocol versioning)".to_string(),
                                |version| version.to_string()
                            ),
                            crate::provider_protocol::MESH_PROTOCOL_VERSION
                        ));
                        tracing::warn!(
                            target: "querymt_remote::provider_client_runtime",
                            peer_protocol_version = ?info.protocol_version,
                            required_protocol_version = crate::provider_protocol::MESH_PROTOCOL_VERSION,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "remote provider mesh protocol mismatch"
                        );
                        Err(error)
                    }
                }
            })
            .await
        {
            Ok(info) => info,
            Err(error) => {
                // Only successful handshakes are cached, so a later attempt can
                // still negotiate once connectivity returns.
                tracing::warn!(
                    target: "querymt_remote::provider_client_runtime",
                    error = %error,
                    error_kind = error_kind_label(&error),
                    retryable = error.is_retryable(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "remote provider contract handshake failed"
                );
                return Err(error);
            }
        };

        let Some(required_version) = required_version else {
            return Ok(info.clone());
        };
        if info.item_aware_chat_version != Some(required_version) {
            let error = LLMError::InvalidRequest(format!(
                "remote provider peer cannot advertise item-aware chat contract version {required_version}: advertised version: {}",
                info.item_aware_chat_version
                    .map_or_else(|| "none".to_string(), |value| value.to_string())
            ));
            tracing::warn!(
                target: "querymt_remote::provider_client_runtime",
                advertised = ?info.item_aware_chat_version,
                required = required_version,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "remote provider item-aware contract mismatch"
            );
            return Err(error);
        }
        Ok(info.clone())
    }

    /// Negotiate provider facts before a caller snapshots its model profile.
    pub async fn negotiate_contract(&self) -> Result<ProviderContractInfo, LLMError> {
        let host = self.lookup_host().await?;
        match self.validated_contract_info(&host, None).await {
            Err(error) if self.transport.is_stale_host(&error) => {
                self.invalidate_cached_host().await;
                let host = self.lookup_host().await?;
                self.validated_contract_info(&host, None).await
            }
            result => result,
        }
    }

    pub fn build_chat_request(&self, call: &ChatCall<'_>) -> ProviderChatRequest {
        self.config.build_chat_request(call)
    }

    /// Chat capabilities the remote peer reported for its provider.
    ///
    /// Known only after the contract handshake. Until then no per-request
    /// option is assumed to be honoured, so callers never rely on an override
    /// an older or less capable peer would drop.
    pub fn capabilities(&self) -> ChatCapabilities {
        self.contract_info
            .read()
            .get()
            .map_or(ChatCapabilities::NONE.with_streaming(true), |info| {
                info.capabilities
            })
    }

    pub async fn chat(
        &self,
        call: &ChatCall<'_>,
        should_retry: impl Fn(&LLMError) -> bool,
    ) -> Result<ProviderChatResponse, LLMError> {
        let request = self.config.build_chat_request(call);
        let host = self
            .lookup_validated_host(request.item_aware_contract_version)
            .await?;
        self.send_chat_request_with_retry(&host, &request, should_retry)
            .await
    }

    pub fn build_stream_request(
        &self,
        call: &ChatCall<'_>,
        session_id: String,
        request_id: String,
        remote_router_ref: TTransport::RemoteRouterRef,
        reconnect_grace_secs: u64,
    ) -> ProviderStreamRequest<TTransport::RemoteRouterRef> {
        self.config.build_stream_request(
            call,
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
            Err(error) if self.transport.retry_dispatch(&error, should_retry(&error)) => {
                let host = self
                    .refresh_host(request.item_aware_contract_version)
                    .await?;
                self.transport.send_chat_request(&host, request).await
            }
            Err(error) => Err(error),
        }
    }

    pub async fn prepare_stream_router(
        &self,
        host: &TTransport::HostRef,
        session_id: &str,
        request_id: &str,
        consumer_tx: tokio::sync::mpsc::UnboundedSender<StreamRelayMessage>,
    ) -> Result<(TTransport::RouterRef, TTransport::RemoteRouterRef), LLMError> {
        self.transport
            .prepare_stream_router(host, session_id, request_id, consumer_tx)
            .await
    }

    pub async fn send_stream_request_with_retry(
        &self,
        host: &TTransport::HostRef,
        request: ProviderStreamRequest<TTransport::RemoteRouterRef>,
        should_retry: impl Fn(&LLMError) -> bool,
    ) -> Result<TTransport::HostRef, LLMError> {
        match self
            .transport
            .send_stream_request(host, request.clone())
            .await
        {
            Ok(()) => Ok(host.clone()),
            Err(error) if self.transport.retry_dispatch(&error, should_retry(&error)) => {
                let host = self
                    .refresh_host(request.item_aware_contract_version)
                    .await?;
                self.transport.send_stream_request(&host, request).await?;
                Ok(host)
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

    /// A lazy host-side cancellation of one stream; nothing is sent until it is polled.
    pub fn cancel_stream_future(
        &self,
        host: TTransport::HostRef,
        session_id: String,
        request_id: String,
        reason: &'static str,
    ) -> CancelStreamFuture {
        let transport = Arc::clone(&self.transport);
        Box::pin(async move {
            let request = CancelProviderStreamRequest {
                session_id,
                request_id: Some(request_id),
                reason: Some(reason.to_owned()),
            };
            if let Err(error) = transport.cancel_stream(&host, request).await {
                tracing::debug!(
                    target: "querymt_remote::provider::stream",
                    %error,
                    "remote provider stream cancellation failed"
                );
            }
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
        contract_info: std::sync::Mutex<ProviderContractInfo>,
        contract_calls: AtomicUsize,
        host_invalidations: AtomicUsize,
        contract_error: Option<TransportErrorKind>,
        consumer: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<StreamRelayMessage>>>,
        terminal_on_renewal: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl RemoteProviderClientTransport for TestTransport {
        type HostRef = usize;
        type RouterRef = ();
        type RemoteRouterRef = ();

        async fn local_peer_id_display(&self) -> String {
            "local".into()
        }

        async fn target_peer_id_display(&self, _target_locator: &str) -> String {
            "remote".into()
        }

        async fn invalidate_cached_host(&self, _target_locator: &str) {
            self.host_invalidations.fetch_add(1, Ordering::SeqCst);
        }

        async fn lookup_host(&self, _target_locator: &str) -> Result<Self::HostRef, LLMError> {
            Ok(self.host_invalidations.load(Ordering::SeqCst))
        }

        async fn get_contract_info(
            &self,
            _host: &Self::HostRef,
            _request: GetProviderContractInfo,
        ) -> Result<ProviderContractInfo, LLMError> {
            self.contract_calls.fetch_add(1, Ordering::SeqCst);
            match self.contract_error {
                Some(kind) => Err(LLMError::Transport {
                    kind,
                    message: "handshake transport failure".to_string(),
                }),
                None => Ok(self.contract_info.lock().unwrap().clone()),
            }
        }
        async fn prepare_stream_router(
            &self,
            _host: &Self::HostRef,
            _session_id: &str,
            _request_id: &str,
            consumer_tx: tokio::sync::mpsc::UnboundedSender<StreamRelayMessage>,
        ) -> Result<(Self::RouterRef, Self::RemoteRouterRef), LLMError> {
            *self.consumer.lock().unwrap() = Some(consumer_tx);
            Ok(((), ()))
        }

        async fn send_chat_request(
            &self,
            host: &Self::HostRef,
            _request: &ProviderChatRequest,
        ) -> Result<ProviderChatResponse, LLMError> {
            if *host == 0 {
                return Err(LLMError::ProviderError("fixture host withdrawn".into()));
            }
            Ok(ProviderChatResponse::from(
                querymt::chat::ChatOutput::default(),
            ))
        }

        async fn send_stream_request(
            &self,
            _host: &Self::HostRef,
            _request: ProviderStreamRequest<Self::RemoteRouterRef>,
        ) -> Result<(), LLMError> {
            Ok(())
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
            if self.terminal_on_renewal.swap(false, Ordering::Relaxed) {
                self.consumer
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .send(StreamRelayMessage::Chunk(
                        querymt::chat::StreamChunk::Done {
                            finish_reason: querymt::chat::FinishReason::Stop,
                        },
                    ))
                    .unwrap();
            }
            Ok(false)
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
        core_with_contract_and_error(contract_info, None)
    }

    #[tokio::test]
    async fn cached_chat_refreshes_and_renegotiates_after_a_host_is_withdrawn() {
        let (transport, core) = core_with_contract(ProviderContractInfo {
            protocol_version: Some(MESH_PROTOCOL_VERSION),
            capabilities: ChatCapabilities::NONE,
            item_aware_chat_version: None,
            generation: Default::default(),
            charged_peer: None,
        });
        core.negotiate_contract().await.unwrap();
        core.chat(&ChatCall::new(&[]), |error| {
            matches!(error, LLMError::ProviderError(message) if message == "fixture host withdrawn")
        })
        .await
        .unwrap();
        assert_eq!(transport.host_invalidations.load(Ordering::SeqCst), 1);
        assert_eq!(transport.contract_calls.load(Ordering::SeqCst), 2);
    }

    fn core_with_contract_and_error(
        contract_info: ProviderContractInfo,
        contract_error: Option<TransportErrorKind>,
    ) -> (Arc<TestTransport>, RemoteProviderClientCore<TestTransport>) {
        let transport = Arc::new(TestTransport {
            contract_info: std::sync::Mutex::new(contract_info),
            contract_calls: AtomicUsize::new(0),
            host_invalidations: AtomicUsize::new(0),
            contract_error,
            consumer: Default::default(),
            terminal_on_renewal: Default::default(),
        });
        let core = RemoteProviderClientCore::new(
            Arc::clone(&transport),
            RemoteProviderClientConfig::new("peer", "llama_cpp", "model"),
        );
        (transport, core)
    }

    #[tokio::test]
    async fn a_model_node_without_pass_support_is_not_used() {
        // The client does not verify the signature: the model node does.
        let pass = crate::ModelPass {
            version: 1,
            issuer: "issuer".into(),
            audience: "host".into(),
            model_node: "node".into(),
            provider: "llama_cpp".into(),
            model: "model".into(),
            session_id: "session".into(),
            issued_at: 1,
            expires_at: 2,
            signature: String::new(),
        };
        let contract = |charged_peer: Option<&str>| ProviderContractInfo {
            protocol_version: Some(MESH_PROTOCOL_VERSION),
            capabilities: ChatCapabilities::NONE,
            item_aware_chat_version: None,
            generation: Default::default(),
            charged_peer: charged_peer.map(str::to_owned),
        };
        let with_pass = |info| {
            let (transport, core) = core_with_contract(info);
            let config = core.config().clone().with_pass(Some(pass.clone()));
            (
                transport.clone(),
                RemoteProviderClientCore::new(transport, config),
            )
        };

        // A node that predates passes omits the charged peer, and one that
        // charges someone else is not the issuer's node either.
        for charged in [None, Some("host")] {
            let (transport, core) = with_pass(contract(charged));
            let error = core.negotiate_contract().await.unwrap_err();
            assert!(
                error.to_string().contains("charges the model pass issuer"),
                "{error}"
            );
            // The refusal is not cached, so an upgraded node can be used later.
            assert!(core.negotiate_contract().await.is_err());
            assert_eq!(transport.contract_calls.load(Ordering::SeqCst), 2);
        }

        // A node that confirms the issuer is used, and every request carries the pass.
        let (_, core) = with_pass(contract(Some("issuer")));
        core.negotiate_contract().await.unwrap();
        let call = ChatCall::new(&[]);
        assert_eq!(core.build_chat_request(&call).pass, Some(pass.clone()));
        assert_eq!(
            core.config()
                .build_stream_request(&call, "s".into(), "r".into(), (), 1)
                .pass,
            Some(pass)
        );

        // Without a pass, the node is not asked to name anyone.
        let (_, core) = core_with_contract(contract(None));
        core.negotiate_contract().await.unwrap();
        assert!(core.build_chat_request(&call).pass.is_none());
    }

    #[test]
    fn relay_sequence_gap_failure_maps_to_retryable_transport_error() {
        let mut stream_state = crate::RemoteProviderStreamState::new();
        let gap = LLMError::Transport {
            kind: TransportErrorKind::Other,
            message: "relay sequence gap: missing seq 3 (held 2)".to_string(),
        };
        let context = PollStreamContext {
            setup_span: tracing::Span::none(),
            stream_start: std::time::Instant::now(),
            session_id: "s",
            request_id: "r",
            local_peer_id: "local",
            target_peer_id: "remote",
            provider_name: "p",
            model: "m",
            target_name: "t",
        };
        let result = RemoteProviderClientCore::<TestTransport>::poll_stream_message(
            context,
            Some(StreamRelayMessage::TransportFailed {
                error: gap.to_payload(),
            }),
            &mut stream_state,
            true,
        );
        let error = result.expect_err("gap failure must be an error");
        assert!(matches!(error, LLMError::Transport { .. }), "{error:?}");
        assert!(error.is_retryable());
        assert!(error.to_string().contains("missing seq 3"), "{error}");
    }

    #[tokio::test]
    async fn contract_handshake_is_cached_after_success() {
        let (transport, core) = core_with_contract(ProviderContractInfo {
            item_aware_chat_version: None,
            protocol_version: Some(MESH_PROTOCOL_VERSION),
            capabilities: Default::default(),
            generation: Default::default(),
            charged_peer: None,
        });

        core.validate_contract(&0, None).await.unwrap();
        core.validate_contract(&0, None).await.unwrap();

        assert_eq!(transport.contract_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn negotiation_resolves_recovery_profile_before_any_chat() {
        let expected = ProviderContractInfo {
            item_aware_chat_version: Some(querymt::chat::ITEM_AWARE_CHAT_CONTRACT_VERSION),
            protocol_version: Some(MESH_PROTOCOL_VERSION),
            capabilities: ChatCapabilities::NONE
                .with_streaming(true)
                .with_options(querymt::chat::GenerationOptionSupport::ALL),
            generation: crate::provider_protocol::ProviderGenerationProfile {
                initial_max_tokens: Some(32_000),
                output_ceiling: Some(128_000),
                reasoning_effort: Some(querymt::chat::ReasoningEffort::High),
            },
            charged_peer: None,
        };
        let (transport, core) = core_with_contract(expected.clone());
        assert!(!core.capabilities().options.max_tokens);
        assert_eq!(core.negotiate_contract().await.unwrap(), expected);
        assert_eq!(core.capabilities(), expected.capabilities);
        assert_eq!(core.negotiate_contract().await.unwrap(), expected);
        assert_eq!(transport.contract_calls.load(Ordering::SeqCst), 1);
        // TestTransport rejects chat calls: negotiation must not invoke the model.
    }

    #[tokio::test]
    async fn remote_chat_provider_reports_no_output_provenance() {
        use querymt::chat::ChatProvider as _;
        let (_transport, core) = core_with_contract(ProviderContractInfo {
            item_aware_chat_version: Some(querymt::chat::ITEM_AWARE_CHAT_CONTRACT_VERSION),
            protocol_version: Some(MESH_PROTOCOL_VERSION),
            capabilities: ChatCapabilities::NONE.with_streaming(true),
            generation: Default::default(),
            charged_peer: None,
        });
        let provider = crate::RemoteChatProvider::new(core);
        // Before and after the handshake: the contract carries capabilities,
        // not provenance, so the peer's output identity is not known here.
        assert_eq!(provider.output_provenance(), None);
        provider.core().negotiate_contract().await.unwrap();
        assert!(provider.capabilities().streaming);
        assert_eq!(provider.output_provenance(), None);
    }

    #[tokio::test]
    async fn remote_stream_waits_for_an_in_flight_terminal_but_ends_if_it_is_lost() {
        use futures_util::StreamExt as _;
        use querymt::chat::ChatProvider as _;
        for terminal in [true, false] {
            let (transport, _) = core_with_contract(ProviderContractInfo {
                protocol_version: Some(MESH_PROTOCOL_VERSION),
                capabilities: ChatCapabilities::NONE.with_streaming(true),
                item_aware_chat_version: None,
                generation: Default::default(),
                charged_peer: None,
            });
            transport
                .terminal_on_renewal
                .store(terminal, Ordering::Relaxed);
            let core = RemoteProviderClientCore::new(
                transport,
                RemoteProviderClientConfig::new("peer", "fixture", "model")
                    .with_stream_controls(10, 3),
            );
            let provider = crate::RemoteChatProvider::new(core);
            let mut stream = provider.chat_stream(&ChatCall::new(&[])).await.unwrap();
            let result = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
                .await
                .unwrap()
                .unwrap();
            if terminal {
                assert!(querymt::chat::chunk_is_terminal(&result.unwrap()));
            } else {
                assert!(matches!(
                    result,
                    Err(LLMError::Transport {
                        kind: TransportErrorKind::ConnectionClosed,
                        ..
                    })
                ));
            }
            assert!(stream.next().await.is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn queued_terminal_wins_when_consumer_polls_after_lease_loss_deadline() {
        use futures_util::StreamExt as _;
        use querymt::chat::{ChatProvider as _, FinishReason, StreamChunk};
        use std::time::Duration;

        let (transport, _) = core_with_contract(ProviderContractInfo {
            protocol_version: Some(MESH_PROTOCOL_VERSION),
            capabilities: ChatCapabilities::NONE.with_streaming(true),
            item_aware_chat_version: None,
            generation: Default::default(),
            charged_peer: None,
        });
        let core = RemoteProviderClientCore::new(
            transport.clone(),
            RemoteProviderClientConfig::new("peer", "fixture", "model").with_stream_controls(10, 3),
        );
        let provider = crate::RemoteChatProvider::new(core);
        let mut stream = provider.chat_stream(&ChatCall::new(&[])).await.unwrap();
        let sender = transport.consumer.lock().unwrap().as_ref().unwrap().clone();
        // Poll once after the first renewal is due. The failed renewal arms the
        // deadline before the ready text is yielded back to this consumer.
        tokio::time::advance(Duration::from_secs(1)).await;
        sender
            .send(StreamRelayMessage::Chunk(StreamChunk::Text("first".into())))
            .unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamChunk::Text(_)))
        ));
        sender
            .send(StreamRelayMessage::Chunk(StreamChunk::Done {
                finish_reason: FinishReason::Stop,
            }))
            .unwrap();
        tokio::time::advance(
            Duration::from_secs(1) + crate::provider_host_actor::STREAM_RELAY_TIMEOUT * 2,
        )
        .await;
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamChunk::Done { .. }))
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn missing_protocol_version_fails_with_upgrade_guidance() {
        let (_transport, core) = core_with_contract(ProviderContractInfo {
            item_aware_chat_version: None,
            protocol_version: None,
            capabilities: Default::default(),
            generation: Default::default(),
            charged_peer: None,
        });

        let error = core.validate_contract(&0, None).await.unwrap_err();
        assert!(matches!(error, LLMError::InvalidRequest(_)));
        assert!(error.to_string().contains("predates protocol versioning"));
        assert!(error.to_string().contains("upgrade both peers"));
    }

    #[tokio::test]
    async fn chat_handshake_transport_failure_evicts_host_and_preserves_error() {
        // Regression: a mesh I/O timeout used to be wrapped as
        // `InvalidRequest`, which made the client report
        // "Provider rejected the request" for a connectivity problem.
        let transport_error = TransportErrorKind::ConnectionClosed;
        let (transport, core) = core_with_contract_and_error(
            ProviderContractInfo {
                item_aware_chat_version: None,
                protocol_version: Some(MESH_PROTOCOL_VERSION),
                capabilities: Default::default(),
                generation: Default::default(),
                charged_peer: None,
            },
            Some(transport_error),
        );

        let error = core.chat(&ChatCall::new(&[]), |_| true).await.unwrap_err();
        assert!(matches!(
            error,
            LLMError::Transport {
                kind: TransportErrorKind::ConnectionClosed,
                ..
            }
        ));
        assert!(error.is_retryable());
        assert_eq!(transport.contract_calls.load(Ordering::SeqCst), 1);
        assert_eq!(transport.host_invalidations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_handshake_is_not_cached_so_a_later_attempt_can_negotiate() {
        let (transport, core) = core_with_contract_and_error(
            ProviderContractInfo {
                item_aware_chat_version: None,
                protocol_version: Some(MESH_PROTOCOL_VERSION),
                capabilities: Default::default(),
                generation: Default::default(),
                charged_peer: None,
            },
            Some(TransportErrorKind::Timeout),
        );

        assert!(core.validate_contract(&0, None).await.is_err());
        assert!(core.validate_contract(&0, None).await.is_err());
        assert_eq!(
            transport.contract_calls.load(Ordering::SeqCst),
            2,
            "each failed handshake must retry rather than cache the failure"
        );
    }

    #[tokio::test]
    async fn mismatched_protocol_version_is_not_cached_so_a_later_attempt_can_negotiate() {
        let (transport, core) = core_with_contract(ProviderContractInfo {
            item_aware_chat_version: None,
            protocol_version: Some(MESH_PROTOCOL_VERSION + 1),
            capabilities: Default::default(),
            generation: Default::default(),
            charged_peer: None,
        });

        let error = core.validate_contract(&0, None).await.unwrap_err();
        assert!(matches!(error, LLMError::InvalidRequest(_)));
        assert!(error.to_string().contains("upgrade both peers"));
        assert_eq!(
            transport.contract_calls.load(Ordering::SeqCst),
            1,
            "a protocol mismatch must not be cached"
        );

        // The peer upgrades and now speaks the current protocol version; the
        // next handshake must re-fetch instead of replaying the stale error.
        *transport.contract_info.lock().unwrap() = ProviderContractInfo {
            item_aware_chat_version: None,
            protocol_version: Some(MESH_PROTOCOL_VERSION),
            capabilities: Default::default(),
            generation: Default::default(),
            charged_peer: None,
        };
        core.validate_contract(&0, None).await.unwrap();
        assert_eq!(transport.contract_calls.load(Ordering::SeqCst), 2);
    }
}

//! `MeshChatProvider` — client-side proxy that executes LLM calls on a remote mesh node.
//!
//! When a session's `LLMConfig` targets a provider owned by a different node,
//! `SessionProvider::build_provider` wraps the request in a `MeshChatProvider` instead
//! of constructing a local provider. API keys and OAuth tokens never leave the
//! owning node — only `ChatMessage`s flow out and `StreamChunk`s / `ProviderChatResponse`s
//! flow back.
//!
//! ## Non-streaming flow
//!
//! `chat_with_tools` → `ask(ProviderChatRequest)` → `ProviderHostActor` on target node
//!                   → `ProviderChatResponse` (serializable concrete type)
//!                   → wrapped in `Box<dyn ChatResponse>`
//!
//! ## Streaming flow
//!
//! `chat_stream_with_tools` → spawn `StreamReceiverActor` + register in DHT
//!                          → `tell(ProviderStreamRequest)` to `ProviderHostActor`
//!                          → `StreamChunkRelay` messages arrive at `StreamReceiverActor`
//!                          → forwarded via `mpsc` channel → `Stream<StreamChunk>`

use crate::agent::remote::NodeId;
use crate::agent::remote::mesh::MeshHandle;
use crate::agent::remote::provider_host::{
    CancelProviderStreamRequest, GetProviderStreamStatus, ProviderChatRequest, ProviderHostActor,
    ProviderStreamRequest, ProviderStreamStatus, RenewProviderStreamLease, StreamReceiverActor,
    StreamRelayMessage,
};
use futures_util::StreamExt;
use kameo::actor::Spawn;
use libp2p::PeerId;
use querymt::LLMProvider;
use querymt::chat::{ChatMessage, ChatProvider, StreamChunk, Tool};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::{LLMError, LLMErrorPayload, TransportErrorKind};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Instant;
use tokio::sync::{RwLock, mpsc};
use tracing::Instrument;
use uuid::Uuid;

// ── MeshChatProvider ──────────────────────────────────────────────────────────

/// A `ChatProvider` (and `LLMProvider`) implementation that proxies all LLM calls
/// to a `ProviderHostActor` running on `target_node` in the kameo mesh.
///
/// Constructed by `SessionProvider::build_provider` when:
/// 1. The session's `LLMConfig.provider_node_id` names a specific remote node, or
/// 2. The provider is not available locally but is found on a mesh peer.
///
/// API keys never leave the owning node. Only `ChatMessage`s flow outbound,
/// and `ProviderChatResponse` / `StreamChunkRelay` flow back.
static PROVIDER_HOST_CACHE: OnceLock<
    RwLock<HashMap<String, kameo::actor::RemoteActorRef<ProviderHostActor>>>,
> = OnceLock::new();

#[derive(Clone)]
pub struct MeshChatProvider {
    /// Provider name, e.g. `"anthropic"`.
    provider_name: String,
    /// Model name, e.g. `"claude-sonnet-4-20250514"`.
    model: String,
    /// Mesh handle used for DHT lookups and actor registration.
    mesh: MeshHandle,
    /// DHT name of the target `ProviderHostActor`, e.g. `"provider_host::peer::<peer_id>"`.
    target_dht_name: String,
    /// Per-session LLM parameters (system prompt, temperature, etc.) to forward
    /// to the remote `ProviderHostActor`.
    params: Option<serde_json::Value>,
    /// Heartbeat interval for remote provider stream liveness tracking.
    heartbeat_interval_secs: u64,
    /// Lease TTL for orphaned remote provider stream detection.
    lease_ttl_secs: u64,
}

impl MeshChatProvider {
    fn target_peer_id(&self) -> Option<PeerId> {
        self.target_dht_name
            .strip_prefix("provider_host::peer::")
            .and_then(|s| s.parse::<PeerId>().ok())
    }

    fn remote_session_id(&self) -> Option<&str> {
        self.params
            .as_ref()
            .and_then(|v| v.get("_remote_session_id"))
            .and_then(|v| v.as_str())
    }

    fn provider_host_cache()
    -> &'static RwLock<HashMap<String, kameo::actor::RemoteActorRef<ProviderHostActor>>> {
        PROVIDER_HOST_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
    }

    async fn invalidate_cached_provider_host(&self) {
        Self::provider_host_cache()
            .write()
            .await
            .remove(&self.target_dht_name);
    }

    /// Create a new `MeshChatProvider`.
    ///
    /// # Arguments
    /// * `mesh`          — live mesh handle (for DHT operations).
    /// * `target_node_id` — stable mesh node id (`PeerId`) string of the node
    ///   whose `ProviderHostActor` to call.
    /// * `provider_name`  — provider plugin name (e.g. `"anthropic"`).
    /// * `model`          — model name (e.g. `"claude-sonnet-4-20250514"`).
    pub fn new(mesh: &MeshHandle, target_node_id: &str, provider_name: &str, model: &str) -> Self {
        Self {
            provider_name: provider_name.to_string(),
            model: model.to_string(),
            mesh: mesh.clone(),
            target_dht_name: super::dht_name::provider_host(&target_node_id),
            params: None,
            heartbeat_interval_secs: 10,
            lease_ttl_secs: 60,
        }
    }

    /// Typed constructor for call sites that already validated/parsed a node id.
    pub fn from_node_id(
        mesh: &MeshHandle,
        target_node_id: &NodeId,
        provider_name: &str,
        model: &str,
    ) -> Self {
        Self::new(mesh, &target_node_id.to_string(), provider_name, model)
    }

    /// Attach per-session LLM parameters to forward to the remote provider.
    ///
    /// These override the host's defaults for system prompt, temperature,
    /// top_p, and other per-session config.
    pub fn with_params(mut self, params: Option<serde_json::Value>) -> Self {
        self.params = params;
        self
    }

    pub fn with_stream_controls(
        mut self,
        heartbeat_interval_secs: u64,
        lease_ttl_secs: u64,
    ) -> Self {
        self.heartbeat_interval_secs = heartbeat_interval_secs.max(1);
        self.lease_ttl_secs = lease_ttl_secs.max(1);
        self
    }

    pub async fn cancel_remote_stream(
        &self,
        session_id: &str,
        request_id: Option<&str>,
        reason: Option<&str>,
    ) {
        let Ok(host_ref) = self.lookup_provider_host().await else {
            return;
        };
        let _ = host_ref
            .ask(&CancelProviderStreamRequest {
                session_id: session_id.to_string(),
                request_id: request_id.map(str::to_string),
                reason: reason.map(str::to_string),
            })
            .await;
    }

    async fn renew_remote_stream_lease(&self, session_id: &str, request_id: &str) -> bool {
        let Ok(host_ref) = self.lookup_provider_host().await else {
            return false;
        };
        host_ref
            .ask(&RenewProviderStreamLease {
                session_id: session_id.to_string(),
                request_id: request_id.to_string(),
                lease_ttl_secs: self.lease_ttl_secs,
            })
            .await
            .unwrap_or(false)
    }

    pub async fn get_remote_stream_status(
        &self,
        session_id: &str,
        request_id: Option<&str>,
    ) -> Option<ProviderStreamStatus> {
        let Ok(host_ref) = self.lookup_provider_host().await else {
            return None;
        };
        host_ref
            .ask(&GetProviderStreamStatus {
                session_id: session_id.to_string(),
                request_id: request_id.map(str::to_string),
            })
            .await
            .ok()
            .flatten()
    }

    /// Resolve the remote `ProviderHostActor` ref from the DHT.
    #[tracing::instrument(
        name = "remote.mesh_provider.dht_lookup",
        skip(self),
        fields(
            dht_name = %self.target_dht_name,
            provider = %self.provider_name,
            model = %self.model,
            session_id = self.remote_session_id().unwrap_or("unknown-session"),
            local_peer_id = %self.mesh.peer_id(),
            target_peer_id = tracing::field::display(self.target_peer_id().as_ref().map_or("unknown-peer".to_string(), ToString::to_string)),
            found = tracing::field::Empty,
            cache_hit = tracing::field::Empty,
            lookup_ms = tracing::field::Empty,
        )
    )]
    async fn lookup_provider_host(
        &self,
    ) -> Result<kameo::actor::RemoteActorRef<ProviderHostActor>, LLMError> {
        if let Some(peer_id) = self.target_peer_id()
            && !self.mesh.is_peer_alive(&peer_id)
        {
            self.invalidate_cached_provider_host().await;
        }

        if let Some(cached) = Self::provider_host_cache()
            .read()
            .await
            .get(&self.target_dht_name)
            .cloned()
        {
            tracing::Span::current().record("cache_hit", true);
            tracing::Span::current().record("found", true);
            tracing::Span::current().record("lookup_ms", 0_u64);
            return Ok(cached);
        }

        tracing::Span::current().record("cache_hit", false);
        let lookup_start = Instant::now();
        let result = self
            .mesh
            .lookup_actor::<ProviderHostActor>(&self.target_dht_name)
            .await
            .map_err(|e| {
                LLMError::ProviderError(format!(
                    "MeshChatProvider: DHT lookup for '{}' failed: {}",
                    self.target_dht_name, e
                ))
            })?
            .ok_or_else(|| {
                LLMError::ProviderError(format!(
                    "MeshChatProvider: provider host '{}' not found in DHT (is the node online?)",
                    self.target_dht_name
                ))
            });
        tracing::Span::current().record("lookup_ms", lookup_start.elapsed().as_millis() as u64);
        tracing::Span::current().record("found", result.is_ok());
        if let Ok(actor_ref) = &result {
            Self::provider_host_cache()
                .write()
                .await
                .insert(self.target_dht_name.clone(), actor_ref.clone());
        }
        result
    }
}

// ── ChatProvider impl ─────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl ChatProvider for MeshChatProvider {
    fn supports_streaming(&self) -> bool {
        true
    }

    #[tracing::instrument(
        name = "remote.mesh_provider.chat",
        skip(self, messages, tools),
        fields(
            provider = %self.provider_name,
            model = %self.model,
            target_node = %self.target_dht_name,
            session_id = self.remote_session_id().unwrap_or("unknown-session"),
            local_peer_id = %self.mesh.peer_id(),
            target_peer_id = tracing::field::display(self.target_peer_id().as_ref().map_or("unknown-peer".to_string(), ToString::to_string)),
            message_count = messages.len(),
            has_tools = tools.is_some(),
        )
    )]
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn querymt::chat::ChatResponse>, LLMError> {
        let mut host_ref = self.lookup_provider_host().await?;

        let request = ProviderChatRequest {
            provider: self.provider_name.clone(),
            model: self.model.clone(),
            messages: messages.to_vec(),
            tools: tools.map(|t| t.to_vec()),
            params: self.params.clone(),
        };

        // ask() flattens Result<Result<T,E>, RemoteSendError> into Result<T, RemoteSendError<E>>
        let chat_response = match host_ref.ask(&request).await {
            Ok(response) => response,
            Err(error) if should_retry_remote_send(&error) => {
                self.invalidate_cached_provider_host().await;
                host_ref = self.lookup_provider_host().await?;
                host_ref
                    .ask(&request)
                    .await
                    .map_err(remote_send_error_to_llm_error_with_handler)?
            }
            Err(error) => return Err(remote_send_error_to_llm_error_with_handler(error)),
        };

        log::debug!(
            "MeshChatProvider: non-streaming response from {} ({}/{})",
            self.target_dht_name,
            self.provider_name,
            self.model
        );

        Ok(Box::new(chat_response))
    }

    #[tracing::instrument(
        name = "remote.mesh_provider.chat_stream.setup",
        skip(self, messages, tools),
        fields(
            provider = %self.provider_name,
            model = %self.model,
            target_node = %self.target_dht_name,
            session_id = self.remote_session_id().unwrap_or("unknown-session"),
            local_peer_id = %self.mesh.peer_id(),
            target_peer_id = tracing::field::display(self.target_peer_id().as_ref().map_or("unknown-peer".to_string(), ToString::to_string)),
            message_count = messages.len(),
            has_tools = tools.is_some(),
            request_id = tracing::field::Empty,
            provider_lookup_ms = tracing::field::Empty,
            register_receiver_ms = tracing::field::Empty,
            send_request_ms = tracing::field::Empty,
            first_chunk_ms = tracing::field::Empty,
        )
    )]
    async fn chat_stream_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<
        Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
        LLMError,
    > {
        let setup_span = tracing::Span::current();
        let lookup_started = Instant::now();
        let mut host_ref = self.lookup_provider_host().await?;
        setup_span.record(
            "provider_lookup_ms",
            lookup_started.elapsed().as_millis() as u64,
        );

        // ── 1. Create the mpsc channel ────────────────────────────────────────
        let (tx, rx) = mpsc::channel::<StreamRelayMessage>(64);

        // ── 2. Spawn the ephemeral StreamReceiverActor on the local node ──────
        let request_id = Uuid::now_v7().to_string();
        let session_id = self
            .remote_session_id()
            .unwrap_or("unknown-session")
            .to_string();
        let stream_rx_name = super::dht_name::stream_receiver(&session_id, &request_id);
        setup_span.record("request_id", &request_id);

        {
            let register_started = Instant::now();
            let reg_span = tracing::info_span!(
                "remote.mesh_provider.dht_register_receiver",
                session_id = %session_id,
                request_id = %request_id,
                stream_rx_name = %stream_rx_name,
                local_peer_id = %self.mesh.peer_id(),
                target_peer_id = tracing::field::display(self.target_peer_id().as_ref().map_or("unknown-peer".to_string(), ToString::to_string)),
            );
            let receiver_actor =
                StreamReceiverActor::new(tx, stream_rx_name.clone(), Some(self.mesh.clone()));
            let receiver_ref = StreamReceiverActor::spawn(receiver_actor);

            // Register in REMOTE_REGISTRY + DHT so the remote ProviderHostActor can
            // send StreamChunkRelay messages back to us.
            self.mesh
                .register_actor(receiver_ref, stream_rx_name.clone())
                .instrument(reg_span)
                .await;
            setup_span.record(
                "register_receiver_ms",
                register_started.elapsed().as_millis() as u64,
            );
        }

        log::debug!(
            "MeshChatProvider: registered StreamReceiverActor as '{}' for {}/{}",
            stream_rx_name,
            self.provider_name,
            self.model
        );

        // ── 3. Tell the ProviderHostActor to start streaming ──────────────────
        let stream_request = ProviderStreamRequest {
            provider: self.provider_name.clone(),
            model: self.model.clone(),
            messages: messages.to_vec(),
            tools: tools.map(|t| t.to_vec()),
            session_id: session_id.clone(),
            request_id: request_id.clone(),
            stream_receiver_name: stream_rx_name.clone(),
            reconnect_grace_secs: self.mesh.stream_reconnect_grace().as_secs(),
            heartbeat_interval_secs: self.heartbeat_interval_secs,
            lease_ttl_secs: self.lease_ttl_secs,
            params: self.params.clone(),
        };

        let send_started = Instant::now();
        match host_ref.tell(&stream_request).send_ack().await {
            Ok(()) => {}
            Err(error) if should_retry_remote_send(&error) => {
                self.invalidate_cached_provider_host().await;
                host_ref = self.lookup_provider_host().await?;
                host_ref
                    .tell(&stream_request)
                    .send_ack()
                    .await
                    .map_err(remote_send_error_to_llm_error_no_handler)?;
            }
            Err(error) => return Err(remote_send_error_to_llm_error_no_handler(error)),
        }
        setup_span.record("send_request_ms", send_started.elapsed().as_millis() as u64);

        log::debug!(
            "MeshChatProvider: streaming request sent to {} ({}/{})",
            self.target_dht_name,
            self.provider_name,
            self.model
        );

        // ── 4. Wrap mpsc::Receiver as Stream<Item = Result<StreamChunk, LLMError>> ──
        //
        // Transport disconnects are handled explicitly via relay control messages.
        // We still keep a large reconnect grace window as a last-resort failure
        // boundary when the remote node disappears and does not come back.
        let raw_stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        let session_id_for_stream = session_id.clone();
        let request_id_for_stream = request_id.clone();
        let stream_rx_name_for_log = stream_rx_name.clone();
        let provider_for_stream = self.provider_name.clone();
        let model_for_stream = self.model.clone();
        let target_for_stream = self.target_dht_name.clone();
        let reconnect_grace = self.mesh.stream_reconnect_grace();
        let stream_start = Instant::now();
        let local_peer_id = self.mesh.peer_id().to_string();
        let target_peer_id = self.target_peer_id();
        let mesh = self.mesh.clone();
        let lease_renew_every = std::time::Duration::from_secs((self.lease_ttl_secs / 3).max(1));
        let provider_handle = self.clone();

        let stream = futures_util::stream::unfold(
            (
                raw_stream,
                None::<tokio::time::Instant>,
                0_u64,
                VecDeque::<StreamChunk>::new(),
                false,
                tokio::time::Instant::now() + lease_renew_every,
            ),
            move |(
                mut raw_stream,
                disconnected_since,
                mut chunk_index,
                mut pending_chunks,
                mut first_chunk_recorded,
                mut renew_due,
            )| {
                let mesh = mesh.clone();
                let session_id_for_stream = session_id_for_stream.clone();
                let request_id_for_stream = request_id_for_stream.clone();
                let target_peer_id = target_peer_id;
                let local_peer_id = local_peer_id.clone();
                let stream_rx_name_for_log = stream_rx_name_for_log.clone();
                let provider_for_stream = provider_for_stream.clone();
                let model_for_stream = model_for_stream.clone();
                let target_for_stream = target_for_stream.clone();
                let setup_span = setup_span.clone();
                let provider_handle = provider_handle.clone();
                async move {
                    if tokio::time::Instant::now() >= renew_due {
                        let _ = provider_handle
                            .renew_remote_stream_lease(
                                &session_id_for_stream,
                                &request_id_for_stream,
                            )
                            .await;
                        renew_due = tokio::time::Instant::now() + lease_renew_every;
                    }

                    if let Some(chunk) = pending_chunks.pop_front() {
                        chunk_index += 1;
                        let elapsed_ms = stream_start.elapsed().as_millis();
                        if !first_chunk_recorded {
                            setup_span.record("first_chunk_ms", elapsed_ms as u64);
                            first_chunk_recorded = true;
                        }
                        return Some((
                            Ok(chunk),
                            (
                                raw_stream,
                                disconnected_since,
                                chunk_index,
                                pending_chunks,
                                first_chunk_recorded,
                                renew_due,
                            ),
                        ));
                    }

                    loop {
                        let next = if let Some(since) = disconnected_since {
                            let elapsed = since.elapsed();
                            let remaining = reconnect_grace.saturating_sub(elapsed);
                            if remaining.is_zero() {
                                return Some((
                                    Err(LLMError::Transport {
                                        kind: TransportErrorKind::Timeout,
                                        message: format!(
                                            "reconnect grace expired after {:?}",
                                            reconnect_grace,
                                        ),
                                    }),
                                    (
                                        raw_stream,
                                        disconnected_since,
                                        chunk_index,
                                        pending_chunks,
                                        first_chunk_recorded,
                                        renew_due,
                                    ),
                                ));
                            }
                            match tokio::time::timeout(remaining, raw_stream.next()).await {
                                Ok(item) => item,
                                Err(_) => {
                                    return Some((
                                        Err(LLMError::Transport {
                                            kind: TransportErrorKind::Timeout,
                                            message: format!(
                                                "reconnect grace expired after {:?}",
                                                reconnect_grace,
                                            ),
                                        }),
                                        (
                                            raw_stream,
                                            disconnected_since,
                                            chunk_index,
                                            pending_chunks,
                                            first_chunk_recorded,
                                            renew_due,
                                        ),
                                    ));
                                }
                            }
                        } else {
                            raw_stream.next().await
                        };

                        match next {
                            Some(StreamRelayMessage::Chunk(chunk)) => {
                                let elapsed_ms = stream_start.elapsed().as_millis();
                                if !first_chunk_recorded {
                                    setup_span.record("first_chunk_ms", elapsed_ms as u64);
                                    first_chunk_recorded = true;
                                }
                                if let StreamChunk::Done { finish_reason } = &chunk {
                                    tracing::debug!(
                                        target: "remote::mesh_provider::stream",
                                        session_id = %session_id_for_stream,
                                        request_id = %request_id_for_stream,
                                        local_peer_id = %local_peer_id,
                                        target_peer_id = tracing::field::display(target_peer_id.as_ref().map_or("unknown-peer".to_string(), ToString::to_string)),
                                        provider = %provider_for_stream,
                                        model = %model_for_stream,
                                        target_node = %target_for_stream,
                                        stream_rx = %stream_rx_name_for_log,
                                        chunk_index = chunk_index + 1,
                                        elapsed_ms,
                                        finish_reason = ?finish_reason,
                                        "stream done received from remote provider"
                                    );
                                } else {
                                    tracing::trace!(
                                        target: "remote::mesh_provider::stream",
                                        session_id = %session_id_for_stream,
                                        request_id = %request_id_for_stream,
                                        local_peer_id = %local_peer_id,
                                        target_peer_id = tracing::field::display(target_peer_id.as_ref().map_or("unknown-peer".to_string(), ToString::to_string)),
                                        provider = %provider_for_stream,
                                        model = %model_for_stream,
                                        target_node = %target_for_stream,
                                        stream_rx = %stream_rx_name_for_log,
                                        chunk_index = chunk_index + 1,
                                        elapsed_ms,
                                        "stream chunk received"
                                    );
                                }
                                chunk_index += 1;
                                break Some((
                                    Ok(chunk),
                                    (
                                        raw_stream,
                                        None,
                                        chunk_index,
                                        pending_chunks,
                                        first_chunk_recorded,
                                        renew_due,
                                    ),
                                ));
                            }
                            Some(StreamRelayMessage::ChunkBatch(chunks)) => {
                                let elapsed_ms = stream_start.elapsed().as_millis();
                                if !first_chunk_recorded && !chunks.is_empty() {
                                    setup_span.record("first_chunk_ms", elapsed_ms as u64);
                                    first_chunk_recorded = true;
                                }
                                let batch_len = chunks.len();
                                pending_chunks.extend(chunks);
                                tracing::trace!(
                                    target: "remote::mesh_provider::stream",
                                    session_id = %session_id_for_stream,
                                    request_id = %request_id_for_stream,
                                    local_peer_id = %local_peer_id,
                                    target_peer_id = tracing::field::display(target_peer_id.as_ref().map_or("unknown-peer".to_string(), ToString::to_string)),
                                    provider = %provider_for_stream,
                                    model = %model_for_stream,
                                    target_node = %target_for_stream,
                                    stream_rx = %stream_rx_name_for_log,
                                    batch_len,
                                    elapsed_ms,
                                    "stream batch received"
                                );
                                let chunk = pending_chunks.pop_front()?;
                                chunk_index += 1;
                                break Some((
                                    Ok(chunk),
                                    (
                                        raw_stream,
                                        None,
                                        chunk_index,
                                        pending_chunks,
                                        first_chunk_recorded,
                                        renew_due,
                                    ),
                                ));
                            }
                            Some(StreamRelayMessage::Heartbeat {
                                phase,
                                elapsed_ms,
                                idle_ms,
                                chunk_count,
                            }) => {
                                tracing::info!(
                                    target: "remote::mesh_provider::heartbeat",
                                    session_id = %session_id_for_stream,
                                    request_id = %request_id_for_stream,
                                    local_peer_id = %local_peer_id,
                                    target_peer_id = tracing::field::display(target_peer_id.as_ref().map_or("unknown-peer".to_string(), ToString::to_string)),
                                    provider = %provider_for_stream,
                                    model = %model_for_stream,
                                    target_node = %target_for_stream,
                                    stream_rx = %stream_rx_name_for_log,
                                    phase = ?phase,
                                    elapsed_ms,
                                    idle_ms,
                                    chunk_count,
                                    "remote provider heartbeat"
                                );
                                continue;
                            }
                            Some(StreamRelayMessage::ProviderError { error }) => {
                                break Some((
                                    Err(LLMError::from_payload(error)),
                                    (
                                        raw_stream,
                                        disconnected_since,
                                        chunk_index,
                                        pending_chunks,
                                        first_chunk_recorded,
                                        renew_due,
                                    ),
                                ));
                            }
                            Some(StreamRelayMessage::TransportDisconnected { reason }) => {
                                tracing::warn!(
                                    target: "remote::mesh_provider::stream",
                                    session_id = %session_id_for_stream,
                                    request_id = %request_id_for_stream,
                                    local_peer_id = %local_peer_id,
                                    target_peer_id = tracing::field::display(target_peer_id.as_ref().map_or("unknown-peer".to_string(), ToString::to_string)),
                                    provider = %provider_for_stream,
                                    model = %model_for_stream,
                                    target_node = %target_for_stream,
                                    stream_rx = %stream_rx_name_for_log,
                                    reason,
                                    "stream transport disconnected"
                                );
                                break Some((
                                    Err(LLMError::RemoteStreamDisconnected { message: reason }),
                                    (
                                        raw_stream,
                                        None,
                                        chunk_index,
                                        pending_chunks,
                                        first_chunk_recorded,
                                        renew_due,
                                    ),
                                ));
                            }
                            Some(StreamRelayMessage::TransportReconnected { buffered_chunks }) => {
                                tracing::info!(
                                    target: "remote::mesh_provider::stream",
                                    session_id = %session_id_for_stream,
                                    request_id = %request_id_for_stream,
                                    local_peer_id = %local_peer_id,
                                    target_peer_id = tracing::field::display(target_peer_id.as_ref().map_or("unknown-peer".to_string(), ToString::to_string)),
                                    provider = %provider_for_stream,
                                    model = %model_for_stream,
                                    target_node = %target_for_stream,
                                    stream_rx = %stream_rx_name_for_log,
                                    buffered_chunks,
                                    "stream transport reconnected"
                                );
                                break Some((
                                    Err(LLMError::RemoteStreamReconnected {
                                        message: format!("buffered_chunks={}", buffered_chunks),
                                    }),
                                    (
                                        raw_stream,
                                        None,
                                        chunk_index,
                                        pending_chunks,
                                        first_chunk_recorded,
                                        renew_due,
                                    ),
                                ));
                            }
                            Some(StreamRelayMessage::TransportFailed { error }) => {
                                break Some((
                                    Err(LLMError::from_payload(error)),
                                    (
                                        raw_stream,
                                        disconnected_since,
                                        chunk_index,
                                        pending_chunks,
                                        first_chunk_recorded,
                                        renew_due,
                                    ),
                                ));
                            }
                            None => {
                                let peer_alive = target_peer_id
                                    .as_ref()
                                    .is_some_and(|peer_id| mesh.is_peer_alive(peer_id));
                                if disconnected_since.is_some() || !peer_alive {
                                    break Some((
                                        Err(LLMError::Transport {
                                            kind: TransportErrorKind::ConnectionClosed,
                                            message: format!(
                                                "stream receiver closed (peer_alive={})",
                                                peer_alive,
                                            ),
                                        }),
                                        (
                                            raw_stream,
                                            disconnected_since,
                                            chunk_index,
                                            pending_chunks,
                                            first_chunk_recorded,
                                            renew_due,
                                        ),
                                    ));
                                } else {
                                    break None;
                                }
                            }
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}

// ── CompletionProvider stub ───────────────────────────────────────────────────

#[async_trait::async_trait]
impl CompletionProvider for MeshChatProvider {
    async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented(
            "MeshChatProvider: completion not supported (use chat instead)".into(),
        ))
    }
}

// ── EmbeddingProvider stub ────────────────────────────────────────────────────

#[async_trait::async_trait]
impl EmbeddingProvider for MeshChatProvider {
    async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented(
            "MeshChatProvider: embedding not supported".into(),
        ))
    }
}

// ── LLMProvider ───────────────────────────────────────────────────────────────

impl LLMProvider for MeshChatProvider {}

fn remote_send_error_base<E>(error: kameo::error::RemoteSendError<E>) -> Result<LLMError, E> {
    use kameo::error::RemoteSendError;

    match error {
        RemoteSendError::ActorNotRunning | RemoteSendError::ActorStopped => {
            Ok(LLMError::Transport {
                kind: TransportErrorKind::ConnectionClosed,
                message: "remote actor not running".to_string(),
            })
        }
        RemoteSendError::UnknownActor { .. } | RemoteSendError::UnknownMessage { .. } => {
            Ok(LLMError::Transport {
                kind: TransportErrorKind::ConnectionClosed,
                message: "remote actor unavailable".to_string(),
            })
        }
        RemoteSendError::BadActorType => {
            Ok(LLMError::ProviderError("bad remote actor type".to_string()))
        }
        RemoteSendError::MailboxFull => Ok(LLMError::Transport {
            kind: TransportErrorKind::Other,
            message: "remote mailbox full".to_string(),
        }),
        RemoteSendError::ReplyTimeout | RemoteSendError::NetworkTimeout => {
            Ok(LLMError::Transport {
                kind: TransportErrorKind::Timeout,
                message: "network timeout".to_string(),
            })
        }
        RemoteSendError::DialFailure => Ok(LLMError::Transport {
            kind: TransportErrorKind::ConnectionRefused,
            message: "dial failure".to_string(),
        }),
        RemoteSendError::ConnectionClosed => Ok(LLMError::Transport {
            kind: TransportErrorKind::ConnectionClosed,
            message: "connection closed".to_string(),
        }),
        RemoteSendError::UnsupportedProtocols => Ok(LLMError::ProviderError(
            "remote protocol unsupported".to_string(),
        )),
        RemoteSendError::SerializeMessage(err)
        | RemoteSendError::DeserializeMessage(err)
        | RemoteSendError::SerializeReply(err)
        | RemoteSendError::SerializeHandlerError(err)
        | RemoteSendError::DeserializeHandlerError(err) => Ok(LLMError::ProviderError(err)),
        RemoteSendError::SwarmNotBootstrapped => Ok(LLMError::Transport {
            kind: TransportErrorKind::Other,
            message: "swarm not bootstrapped".to_string(),
        }),
        RemoteSendError::Io(Some(err)) => Ok(LLMError::from(err)),
        RemoteSendError::Io(None) => Ok(LLMError::Transport {
            kind: TransportErrorKind::Other,
            message: "remote IO failure".to_string(),
        }),
        RemoteSendError::HandlerError(err) => Err(err),
    }
}

fn remote_send_error_to_llm_error_with_handler(
    error: kameo::error::RemoteSendError<crate::error::AgentError>,
) -> LLMError {
    match remote_send_error_base(error) {
        Ok(err) => err,
        Err(agent_error) => decode_remote_handler_error(agent_error),
    }
}

fn remote_send_error_to_llm_error_no_handler(
    error: kameo::error::RemoteSendError<kameo::error::Infallible>,
) -> LLMError {
    match remote_send_error_base(error) {
        Ok(err) => err,
        Err(never) => match never {},
    }
}

fn decode_remote_handler_error(agent_error: crate::error::AgentError) -> LLMError {
    match agent_error {
        crate::error::AgentError::ProviderChat { reason, .. } => {
            serde_json::from_str::<LLMErrorPayload>(&reason)
                .map(LLMError::from_payload)
                .unwrap_or_else(|_| LLMError::ProviderError(reason))
        }
        other => LLMError::ProviderError(other.to_string()),
    }
}

pub(super) fn should_retry_remote_send<E>(error: &kameo::error::RemoteSendError<E>) -> bool {
    use kameo::error::RemoteSendError;

    matches!(
        error,
        RemoteSendError::ActorNotRunning
            | RemoteSendError::ActorStopped
            | RemoteSendError::UnknownActor { .. }
            | RemoteSendError::UnknownMessage { .. }
            | RemoteSendError::DialFailure
            | RemoteSendError::ConnectionClosed
    )
}

// ── find_provider_on_mesh ─────────────────────────────────────────────────────

/// Scan the mesh for any node advertising `provider_name` in its available models.
///
/// Returns the stable node id of the first node that has valid credentials for
/// the provider, or `None` if no peer is advertising it.
///
/// This is used by `SessionProvider::build_provider` as a mesh-fallback (Case 3) when
/// the provider is unavailable locally.
///
/// # Implementation note
///
/// This function queries each `RemoteNodeManager` via `ListAvailableModels` +
/// `GetNodeInfo` and uses the reported `node_id` directly.
/// The explicit `provider_node_id` path (Case 1) is the primary flow; this
/// best-effort scan only runs when `provider_node_id` is `None`.
#[tracing::instrument(
    name = "remote.mesh_provider.find_on_mesh",
    skip(mesh),
    fields(
        provider_name,
        peers_checked = tracing::field::Empty,
        found = tracing::field::Empty,
    )
)]
pub(crate) async fn find_provider_on_mesh(
    mesh: &MeshHandle,
    provider_name: &str,
) -> Option<NodeId> {
    use crate::agent::remote::node_manager::{GetNodeInfo, RemoteNodeManager};
    use crate::agent::remote::{ListAvailableModels, NodeInfo};

    let mut stream = mesh.lookup_all_actors::<RemoteNodeManager>(super::dht_name::NODE_MANAGER);
    let mut peers_checked: u32 = 0;

    while let Some(node_ref_result) = stream.next().await {
        let node_ref = match node_ref_result {
            Ok(r) => r,
            Err(e) => {
                log::debug!("find_provider_on_mesh: DHT stream error: {}", e);
                continue;
            }
        };

        peers_checked += 1;
        tracing::Span::current().record("peers_checked", peers_checked);

        // Ask for available models first (cheaper filter).
        let models = match node_ref
            .ask::<ListAvailableModels>(&ListAvailableModels)
            .await
        {
            Ok(m) => m,
            Err(e) => {
                log::debug!("find_provider_on_mesh: ListAvailableModels failed: {}", e);
                continue;
            }
        };

        if !models.iter().any(|m| m.provider == provider_name) {
            continue;
        }

        // This peer has the provider — ask for its stable node identity.
        let node_info: NodeInfo = match node_ref.ask::<GetNodeInfo>(&GetNodeInfo).await {
            Ok(info) => info,
            Err(e) => {
                log::debug!(
                    "find_provider_on_mesh: GetNodeInfo failed for peer with provider '{}': {}",
                    provider_name,
                    e
                );
                continue;
            }
        };

        log::info!(
            "find_provider_on_mesh: provider '{}' found on mesh peer '{}' ({}) (mesh fallback)",
            provider_name,
            node_info.hostname,
            node_info.node_id
        );
        tracing::Span::current().record("found", true);
        return Some(node_info.node_id);
    }

    tracing::Span::current().record("found", false);
    None
}

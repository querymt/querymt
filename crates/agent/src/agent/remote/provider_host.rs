//! `ProviderHostActor` — exposes this node's LLM providers as callable mesh services.
//!
//! Each mesh node runs one `ProviderHostActor`. Remote nodes send it
//! `ProviderChatRequest` (non-streaming) or `ProviderStreamRequest` (streaming)
//! messages to execute LLM calls using the local node's providers and API keys.
//!
//! API keys never leave the owning node. Only `ChatMessage`s flow in and
//! `ProviderChatResponse` / `StreamChunkRelay` flow out.
//!
//! Registered in the Kademlia DHT as `"provider_host::peer::{peer_id}"`.

use crate::agent::agent_config::AgentConfig;
use crate::error::AgentError;
use crate::session::provider::ProviderRequest;
use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::remote::_internal;
use parking_lot::Mutex;
use querymt::LLMProvider;
use querymt::ToolCall;
use querymt::Usage;
use querymt::chat::{ChatMessage, ChatResponse, FinishReason, StreamChunk, Tool};
use querymt::error::LLMErrorPayload;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

// ── Wire types ────────────────────────────────────────────────────────────────

/// The concrete, serializable representation of an LLM response.
///
/// `Box<dyn ChatResponse>` cannot be sent across the mesh — this type maps
/// 1:1 to the `ChatResponse` trait methods and is what `ProviderHostActor`
/// returns for non-streaming calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderChatResponse {
    pub text: Option<String>,
    pub thinking: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<querymt::Usage>,
    pub finish_reason: Option<String>,
}

impl fmt::Display for ProviderChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.text {
            Some(t) => write!(f, "{}", t),
            None => write!(f, "[no text]"),
        }
    }
}

impl ChatResponse for ProviderChatResponse {
    fn text(&self) -> Option<String> {
        self.text.clone()
    }

    fn thinking(&self) -> Option<String> {
        self.thinking.clone()
    }

    fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        if self.tool_calls.is_empty() {
            None
        } else {
            Some(self.tool_calls.clone())
        }
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        self.finish_reason.as_deref().map(|r| match r {
            "Stop" => FinishReason::Stop,
            "Length" => FinishReason::Length,
            "ContentFilter" => FinishReason::ContentFilter,
            "ToolCalls" => FinishReason::ToolCalls,
            "Error" => FinishReason::Error,
            "Other" => FinishReason::Other,
            _ => FinishReason::Unknown,
        })
    }

    fn usage(&self) -> Option<Usage> {
        self.usage.clone()
    }
}

/// Stream relay/control payload sent from `ProviderHostActor` to the
/// `StreamReceiverActor` on the requesting node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum StreamRelayMessage {
    /// Normal streamed chunk from the upstream provider.
    Chunk(StreamChunk),
    /// Batched streamed chunks from the upstream provider.
    ChunkBatch(Vec<StreamChunk>),
    /// Liveness pulse from the provider host while waiting on an upstream model.
    Heartbeat {
        phase: ProviderStreamPhase,
        elapsed_ms: u64,
        idle_ms: u64,
        chunk_count: u64,
    },
    /// Provider/model error produced by the upstream stream.
    ProviderError { error: LLMErrorPayload },
    /// The transport path to the requesting node disappeared but may recover.
    TransportDisconnected { reason: String },
    /// Delivery has resumed after a temporary transport disconnect.
    TransportReconnected { buffered_chunks: usize },
    /// The stream failed permanently because reconnect grace expired.
    TransportFailed { error: LLMErrorPayload },
}

/// Thin wrapper around a stream relay/control payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunkRelay {
    pub message: StreamRelayMessage,
}

// ── Message types ─────────────────────────────────────────────────────────────

/// Non-streaming provider call message (use `ask()`).
///
/// The requesting node sends this to the `ProviderHostActor` and waits for a
/// `ProviderChatResponse`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderChatRequest {
    /// Provider name (e.g. `"anthropic"`, `"openai"`).
    pub provider: String,
    /// Model name (e.g. `"claude-sonnet-4-20250514"`).
    pub model: String,
    /// The conversation history to send to the model.
    pub messages: Vec<ChatMessage>,
    /// Tool definitions, if any.
    pub tools: Option<Vec<Tool>>,
    /// Per-session LLM parameters (system prompt, temperature, top_p, etc.)
    /// forwarded from the requesting node's delegate config.
    ///
    /// `None` when the requesting node is an old version that doesn't send
    /// params — the host falls back to its own `initial_params`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// Streaming provider call message (use `tell()`).
///
/// The `ProviderHostActor` streams chunks back to the requesting node by
/// looking up the `StreamReceiverActor` registered under `stream_receiver_name`
/// in the DHT and sending `StreamChunkRelay` messages to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStreamRequest {
    /// Provider name.
    pub provider: String,
    /// Model name.
    pub model: String,
    /// The conversation history.
    pub messages: Vec<ChatMessage>,
    /// Tool definitions, if any.
    pub tools: Option<Vec<Tool>>,
    /// Stable session id that owns this request.
    pub session_id: String,
    /// Unique request id within the session.
    pub request_id: String,
    /// DHT name of the `StreamReceiverActor` on the requesting node.
    /// Format: `"stream_rx::{session_id}::{request_id}"`.
    pub stream_receiver_name: String,
    /// Grace period in seconds to wait for stream reconnection before failing.
    pub reconnect_grace_secs: u64,
    /// How often the provider host should emit liveness heartbeats while waiting.
    #[serde(default = "default_stream_heartbeat_secs")]
    pub heartbeat_interval_secs: u64,
    /// Lease TTL in seconds. The provider host cancels orphaned requests when renewals stop.
    #[serde(default = "default_stream_lease_ttl_secs")]
    pub lease_ttl_secs: u64,
    /// Per-session LLM parameters forwarded from the requesting node.
    /// See [`ProviderChatRequest::params`] for details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

fn default_stream_heartbeat_secs() -> u64 {
    10
}

fn default_stream_lease_ttl_secs() -> u64 {
    60
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamPhase {
    OpeningUpstream,
    WaitingFirstChunk,
    Streaming,
    ReceiverDisconnected,
    Cancelling,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStreamStatus {
    pub session_id: String,
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub phase: ProviderStreamPhase,
    pub elapsed_ms: u64,
    pub idle_ms: u64,
    pub chunk_count: u64,
    pub receiver_connected: bool,
    pub lease_expires_in_ms: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelProviderStreamRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewProviderStreamLease {
    pub session_id: String,
    pub request_id: String,
    pub lease_ttl_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetProviderStreamStatus {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

fn keep_stream_message_buffered(message: &StreamRelayMessage) -> bool {
    !matches!(
        message,
        StreamRelayMessage::Heartbeat { .. }
            | StreamRelayMessage::TransportDisconnected { .. }
            | StreamRelayMessage::TransportReconnected { .. }
    )
}

fn relay_message_is_terminal(message: &StreamRelayMessage) -> bool {
    matches!(
        message,
        StreamRelayMessage::Chunk(StreamChunk::Done { .. })
            | StreamRelayMessage::ProviderError { .. }
            | StreamRelayMessage::TransportFailed { .. }
    ) || matches!(
        message,
        StreamRelayMessage::ChunkBatch(chunks)
            if chunks.iter().any(|chunk| matches!(chunk, StreamChunk::Done { .. }))
    )
}

fn update_active_stream(
    active_streams: &Arc<Mutex<HashMap<String, ActiveProviderStream>>>,
    request_id: &str,
    f: impl FnOnce(&mut ActiveProviderStream),
) {
    if let Some(stream) = active_streams.lock().get_mut(request_id) {
        f(stream);
    }
}

fn remove_active_stream(
    active_streams: &Arc<Mutex<HashMap<String, ActiveProviderStream>>>,
    request_id: &str,
) {
    active_streams.lock().remove(request_id);
}

// ── StreamReceiverActor ───────────────────────────────────────────────────────

/// Ephemeral actor spawned on the **requesting** node for each streaming call.
///
/// Registered in the DHT as `"stream_rx::{request_id}"`. Receives
/// `StreamChunkRelay` messages from the `ProviderHostActor` on the owning node
/// and feeds them into an `mpsc` channel. The consumer wraps the channel
/// receiver as a `Stream<Item = Result<StreamChunk, LLMError>>`.
///
/// Self-destructs when it receives `StreamChunk::Done` or an error relay.
/// On stop, it deregisters itself from the Kademlia DHT so the name entry
/// does not linger after the stream is complete.
pub struct StreamReceiverActor {
    tx: mpsc::Sender<StreamRelayMessage>,
    /// The name this actor is registered under in the Kademlia DHT.
    /// Used to deregister on stop so the entry doesn't linger.
    dht_name: String,
    /// Optional mesh handle for cleaning up re-registration closures on stop.
    mesh: Option<super::mesh::MeshHandle>,
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
        // Remove the re-registration closure so dead actors don't accumulate
        // in the MeshHandle's re_register_fns map (Bug 1 fix).
        if let Some(ref mesh) = self.mesh {
            mesh.deregister_actor(&self.dht_name);
        }

        // Deregister from the Kademlia DHT so the ephemeral name entry is cleaned up.
        if let Err(e) = kameo::remote::unregister(self.dht_name.clone()).await {
            log::debug!(
                "StreamReceiverActor: failed to unregister '{}' from DHT: {}",
                self.dht_name,
                e
            );
        } else {
            log::debug!(
                "StreamReceiverActor: deregistered '{}' from DHT",
                self.dht_name
            );
        }
        Ok(())
    }
}

impl StreamReceiverActor {
    pub fn new(
        tx: mpsc::Sender<StreamRelayMessage>,
        dht_name: String,
        mesh: Option<super::mesh::MeshHandle>,
    ) -> Self {
        Self { tx, dht_name, mesh }
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

        // Forward to channel (ignore send errors — receiver may have dropped).
        let _ = self.tx.send(msg.message).await;

        if is_terminal {
            ctx.actor_ref().kill();
        }
    }
}

// ── ProviderHostActor ─────────────────────────────────────────────────────────

/// Per-node actor that serves LLM provider calls to the mesh.
///
/// Spawned once during mesh bootstrap alongside `RemoteNodeManager`.
/// Registered in the DHT as `"provider_host::peer::{peer_id}"`.
///
/// # Provider construction
///
/// Each request carries optional per-session `params` (system prompt,
/// temperature, etc.) from the requesting node.  The host merges these
/// with its own `initial_params` (hardware config like `n_ctx`,
/// `flash_attention`, model path) and builds a fresh provider per
/// request.  This is cheap because expensive model loading is cached
/// at the factory level (e.g. `LlamaCppFactory` caches the loaded
/// `Arc<LlamaModel>`).
#[derive(Debug)]
struct ActiveProviderStream {
    session_id: String,
    request_id: String,
    provider: String,
    model: String,
    phase: ProviderStreamPhase,
    started_at: Instant,
    last_progress_at: Instant,
    last_heartbeat_at: Instant,
    chunk_count: u64,
    receiver_connected: bool,
    lease_expires_at: Instant,
    last_error: Option<String>,
    cancel_token: CancellationToken,
}

impl ActiveProviderStream {
    fn status(&self) -> ProviderStreamStatus {
        let now = Instant::now();
        ProviderStreamStatus {
            session_id: self.session_id.clone(),
            request_id: self.request_id.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            phase: self.phase,
            elapsed_ms: now.duration_since(self.started_at).as_millis() as u64,
            idle_ms: now.duration_since(self.last_progress_at).as_millis() as u64,
            chunk_count: self.chunk_count,
            receiver_connected: self.receiver_connected,
            lease_expires_in_ms: self
                .lease_expires_at
                .checked_duration_since(now)
                .unwrap_or_default()
                .as_millis() as u64,
            last_error: self.last_error.clone(),
        }
    }
}

#[derive(Actor)]
pub struct ProviderHostActor {
    config: Arc<AgentConfig>,
    active_streams: Arc<Mutex<HashMap<String, ActiveProviderStream>>>,
}

impl ProviderHostActor {
    pub fn new(config: Arc<AgentConfig>) -> Self {
        Self {
            config,
            active_streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn provider_stream_status(
        &self,
        session_id: &str,
        request_id: Option<&str>,
    ) -> Option<ProviderStreamStatus> {
        let streams = self.active_streams.lock();
        if let Some(request_id) = request_id {
            return streams
                .get(request_id)
                .filter(|stream| stream.session_id == session_id)
                .map(ActiveProviderStream::status);
        }

        streams
            .values()
            .find(|stream| stream.session_id == session_id)
            .map(ActiveProviderStream::status)
    }

    fn cancel_streams(
        &self,
        session_id: &str,
        request_id: Option<&str>,
        reason: Option<&str>,
    ) -> usize {
        let reason = reason.unwrap_or("cancel requested").to_string();
        let mut streams = self.active_streams.lock();
        let mut cancelled = 0usize;
        for stream in streams.values_mut() {
            if stream.session_id != session_id {
                continue;
            }
            if let Some(request_id) = request_id
                && stream.request_id != request_id
            {
                continue;
            }
            stream.phase = ProviderStreamPhase::Cancelling;
            stream.last_error = Some(reason.clone());
            stream.cancel_token.cancel();
            cancelled += 1;
        }
        cancelled
    }

    fn renew_stream_lease(&self, session_id: &str, request_id: &str, lease_ttl_secs: u64) -> bool {
        let mut streams = self.active_streams.lock();
        let Some(stream) = streams.get_mut(request_id) else {
            return false;
        };
        if stream.session_id != session_id {
            return false;
        }
        stream.lease_expires_at = Instant::now() + Duration::from_secs(lease_ttl_secs.max(1));
        true
    }

    /// Build a provider for a single request, merging request params with
    /// host defaults.
    ///
    /// Request params (system, temperature, top_p, etc.) override host
    /// defaults.  `api_key` is always stripped from request params —
    /// keys never leave the owning node.
    #[tracing::instrument(
        name = "remote.provider_host.build_provider_for_request",
        skip(self, request_params),
        fields(provider = %provider_name, model = %model)
    )]
    async fn build_provider_for_request(
        &self,
        provider_name: &str,
        model: &str,
        request_params: Option<&serde_json::Value>,
    ) -> Result<Arc<dyn LLMProvider>, AgentError> {
        let host_defaults = params_for_remote_provider(self.config.provider.initial_params());

        // Request params override host defaults
        let merged = merge_params(request_params, host_defaults.as_ref());

        self.config
            .provider
            .build_provider(ProviderRequest::new(provider_name, model).with_params(merged.as_ref()))
            .await
            .map_err(|e| {
                AgentError::Internal(format!(
                    "ProviderHostActor: failed to build provider '{}' model '{}': {}",
                    provider_name, model, e
                ))
            })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Serialize [`LLMParams`] for forwarding to a remote provider, excluding
/// fields that are passed as separate arguments to
/// [`SessionProvider::build_provider`] (`provider`, `model`, `name`) and
/// sensitive credentials (`api_key`) that must never leave the owning node.
///
/// This ensures *all* configuration — system prompt, temperature, top_p,
/// custom provider keys, etc. — reaches the remote provider factory.
pub(crate) fn params_for_remote_provider(params: &querymt::LLMParams) -> Option<serde_json::Value> {
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
        // Credentials and transport metadata must not reach provider factories.
        obj.remove("api_key");
        obj.remove("_remote_session_id");
    }

    if sanitized.as_object().is_some_and(|o| o.is_empty()) {
        None
    } else {
        Some(sanitized)
    }
}

/// Merge per-request params (from the requesting node's delegate config)
/// with host defaults (from this node's `initial_params`).
///
/// - Start with host defaults (hardware params like `n_ctx`, `flash_attention`,
///   `kv_cache_type_k/v`, model path, etc.).
/// - Overlay request params on top (overrides `system`, `temperature`, `top_p`,
///   etc. with the delegate's per-session values).
/// - `api_key` and transport-only metadata are always stripped from request
///   params so they never reach provider factories on the host node.
///
/// Returns `None` if neither host defaults nor request params have any fields.
pub(crate) fn merge_params(
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

// ── Non-streaming handler ─────────────────────────────────────────────────────

impl Message<ProviderChatRequest> for ProviderHostActor {
    type Reply = Result<ProviderChatResponse, AgentError>;

    #[tracing::instrument(
        name = "remote.provider_host.chat",
        skip_all,
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
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let provider = self
            .build_provider_for_request(&msg.provider, &msg.model, msg.params.as_ref())
            .await?;

        let tools_slice = msg.tools.as_deref();

        let response = provider
            .chat_with_tools(&msg.messages, tools_slice)
            .await
            .map_err(|e| AgentError::ProviderChat {
                operation: "chat_with_tools".to_string(),
                reason: serde_json::to_string(&e.to_payload()).unwrap_or_else(|_| e.to_string()),
            })?;

        let tool_calls = response.tool_calls().unwrap_or_default();
        let finish_reason = response.finish_reason().map(|r| format!("{:?}", r));

        tracing::Span::current()
            .record("tool_calls_returned", tool_calls.len())
            .record("finish_reason", finish_reason.as_deref().unwrap_or("none"));

        log::trace!(
            "ProviderHostActor: non-streaming call to {}/{} complete (tool_calls={}, finish={:?})",
            msg.provider,
            msg.model,
            tool_calls.len(),
            finish_reason,
        );

        Ok(ProviderChatResponse {
            text: response.text(),
            thinking: response.thinking(),
            tool_calls,
            usage: response.usage(),
            finish_reason,
        })
    }
}

// ── Streaming handler ─────────────────────────────────────────────────────────

impl Message<ProviderStreamRequest> for ProviderHostActor {
    type Reply = Result<(), AgentError>;

    #[tracing::instrument(
        name = "remote.provider_host.stream",
        skip_all,
        fields(
            provider = %msg.provider,
            model = %msg.model,
            session_id = %msg.session_id,
            request_id = %msg.request_id,
            message_count = msg.messages.len(),
            has_tools = msg.tools.is_some(),
            has_params = msg.params.is_some(),
            receiver_name = %msg.stream_receiver_name,
            receiver_found = tracing::field::Empty,
        )
    )]
    async fn handle(
        &mut self,
        msg: ProviderStreamRequest,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use futures_util::StreamExt;

        const MAX_BATCH_SIZE: usize = 16;
        const BATCH_FLUSH_INTERVAL: Duration = Duration::from_millis(25);

        let provider = self
            .build_provider_for_request(&msg.provider, &msg.model, msg.params.as_ref())
            .await?;

        let tools_slice = msg.tools.as_deref();

        let mut stream = provider
            .chat_stream_with_tools(&msg.messages, tools_slice)
            .await
            .map_err(|e| AgentError::ProviderChat {
                operation: "chat_stream_with_tools".to_string(),
                reason: serde_json::to_string(&e.to_payload()).unwrap_or_else(|_| e.to_string()),
            })?;

        // Look up the StreamReceiverActor on the requesting node.
        let receiver_ref = {
            let lookup_span = tracing::info_span!(
                "remote.provider_host.stream.lookup_receiver",
                session_id = %msg.session_id,
                request_id = %msg.request_id,
                receiver_name = %msg.stream_receiver_name,
                found = tracing::field::Empty,
            );
            let result = kameo::actor::RemoteActorRef::<StreamReceiverActor>::lookup(
                msg.stream_receiver_name.clone(),
            )
            .instrument(lookup_span.clone())
            .await
            .map_err(|e| AgentError::SwarmLookupFailed {
                key: msg.stream_receiver_name.clone(),
                reason: e.to_string(),
            })
            .and_then(|opt| {
                opt.ok_or_else(|| AgentError::RemoteSessionNotFound {
                    details: format!(
                        "ProviderHostActor: stream receiver '{}' not found in DHT",
                        msg.stream_receiver_name
                    ),
                })
            });
            lookup_span.record("found", result.is_ok());
            result
        }?;

        tracing::Span::current().record("receiver_found", true);

        let provider_name = msg.provider.clone();
        let model = msg.model.clone();
        let session_id = msg.session_id.clone();
        let receiver_name = msg.stream_receiver_name.clone();
        let request_id = msg.request_id.clone();
        let reconnect_grace = Duration::from_secs(msg.reconnect_grace_secs.max(1));
        let heartbeat_interval = Duration::from_secs(msg.heartbeat_interval_secs.max(1));
        let lease_ttl_secs = msg.lease_ttl_secs.max(1);
        let lease_ttl = Duration::from_secs(lease_ttl_secs);
        let active_streams = Arc::clone(&self.active_streams);
        let cancel_token = CancellationToken::new();
        let started_at = Instant::now();

        {
            let mut streams = active_streams.lock();
            streams.insert(
                request_id.clone(),
                ActiveProviderStream {
                    session_id: session_id.clone(),
                    request_id: request_id.clone(),
                    provider: provider_name.clone(),
                    model: model.clone(),
                    phase: ProviderStreamPhase::OpeningUpstream,
                    started_at,
                    last_progress_at: started_at,
                    last_heartbeat_at: started_at,
                    chunk_count: 0,
                    receiver_connected: true,
                    lease_expires_at: started_at + lease_ttl,
                    last_error: None,
                    cancel_token: cancel_token.clone(),
                },
            );
        }

        // Relay chunks asynchronously so this handler returns promptly.
        let relay_span = tracing::info_span!(
            "remote.provider_host.stream.relay",
            provider = %provider_name,
            model = %model,
            session_id = %session_id,
            receiver_name = %receiver_name,
            request_id = %request_id,
            chunk_count = tracing::field::Empty,
            batch_count = tracing::field::Empty,
            max_batch_size = tracing::field::Empty,
        );
        tokio::spawn(
            async move {
                let mut receiver_ref = receiver_ref;
                let mut chunk_count = 0usize;
                let mut batch_count = 0usize;
                let mut max_batch_size = 0usize;
                let relay_start = started_at;
                let mut buffered: VecDeque<StreamRelayMessage> = VecDeque::new();
                let mut pending_batch: Vec<StreamChunk> = Vec::new();
                let mut disconnected_since: Option<tokio::time::Instant> = None;
                let mut upstream_done = false;
                let mut terminal_message: Option<StreamRelayMessage> = None;
                let mut heartbeat_tick = tokio::time::interval(heartbeat_interval);
                heartbeat_tick.tick().await;

                update_active_stream(&active_streams, &request_id, |stream| {
                    stream.phase = ProviderStreamPhase::WaitingFirstChunk;
                });

                let flush_batch = |buffered: &mut VecDeque<StreamRelayMessage>,
                                   pending_batch: &mut Vec<StreamChunk>,
                                   batch_count: &mut usize,
                                   max_batch_size: &mut usize| {
                    if pending_batch.is_empty() {
                        return;
                    }
                    *batch_count += 1;
                    *max_batch_size = (*max_batch_size).max(pending_batch.len());
                    let message = if pending_batch.len() == 1 {
                        StreamRelayMessage::Chunk(pending_batch.pop().expect("batch has one chunk"))
                    } else {
                        StreamRelayMessage::ChunkBatch(std::mem::take(pending_batch))
                    };
                    buffered.push_back(message);
                };

                'outer: loop {
                    tokio::select! {
                        _ = cancel_token.cancelled(), if !upstream_done => {
                            upstream_done = true;
                            flush_batch(&mut buffered, &mut pending_batch, &mut batch_count, &mut max_batch_size);
                            update_active_stream(&active_streams, &request_id, |stream| {
                                stream.phase = ProviderStreamPhase::Cancelling;
                                stream.last_error.get_or_insert_with(|| "cancelled".to_string());
                            });
                            terminal_message = Some(StreamRelayMessage::TransportFailed {
                                error: querymt::error::LLMError::Transport {
                                    kind: querymt::error::TransportErrorKind::ConnectionClosed,
                                    message: "provider stream cancelled".to_string(),
                                }.to_payload(),
                            });
                        }
                        _ = heartbeat_tick.tick() => {
                            let now = Instant::now();
                            let mut lease_expired = false;
                            let mut status = None;
                            update_active_stream(&active_streams, &request_id, |stream| {
                                if now >= stream.lease_expires_at {
                                    lease_expired = true;
                                    stream.phase = ProviderStreamPhase::Cancelling;
                                    stream.last_error = Some("stream lease expired".to_string());
                                    stream.cancel_token.cancel();
                                    return;
                                }
                                stream.last_heartbeat_at = now;
                                status = Some(stream.status());
                            });
                            if lease_expired {
                                continue;
                            }
                            if let Some(status) = status {
                                buffered.push_back(StreamRelayMessage::Heartbeat {
                                    phase: status.phase,
                                    elapsed_ms: status.elapsed_ms,
                                    idle_ms: status.idle_ms,
                                    chunk_count: status.chunk_count,
                                });
                            }
                        }
                        next_chunk = async {
                            if pending_batch.is_empty() {
                                stream.next().await
                            } else {
                                tokio::time::timeout(BATCH_FLUSH_INTERVAL, stream.next()).await.unwrap_or_default()
                            }
                        }, if !upstream_done => {
                            match next_chunk {
                                Some(chunk_result) => {
                                    let mut finish_reason = None;
                                    match chunk_result {
                                        Ok(chunk) => {
                                            if let StreamChunk::Done { finish_reason: reason } = &chunk {
                                                finish_reason = Some(*reason);
                                                upstream_done = true;
                                            }
                                            pending_batch.push(chunk);
                                            if upstream_done || pending_batch.len() >= MAX_BATCH_SIZE {
                                                flush_batch(&mut buffered, &mut pending_batch, &mut batch_count, &mut max_batch_size);
                                            }
                                            let chunk_delta = if matches!(pending_batch.last(), Some(StreamChunk::Done { .. })) { 0 } else { 1 };
                                            update_active_stream(&active_streams, &request_id, |stream| {
                                                stream.phase = ProviderStreamPhase::Streaming;
                                                stream.last_progress_at = Instant::now();
                                                stream.receiver_connected = disconnected_since.is_none();
                                                stream.chunk_count = stream.chunk_count.saturating_add(chunk_delta as u64);
                                            });
                                        }
                                        Err(e) => {
                                            flush_batch(&mut buffered, &mut pending_batch, &mut batch_count, &mut max_batch_size);
                                            upstream_done = true;
                                            let payload = e.to_payload();
                                            let message = e.to_string();
                                            update_active_stream(&active_streams, &request_id, |stream| {
                                                stream.phase = ProviderStreamPhase::Failed;
                                                stream.last_error = Some(message);
                                                stream.last_progress_at = Instant::now();
                                            });
                                            terminal_message = Some(StreamRelayMessage::ProviderError { error: payload });
                                        }
                                    }

                                    if let Some(reason) = finish_reason {
                                        tracing::debug!(
                                            target: "remote::provider_host::stream",
                                            session_id = %session_id,
                                            request_id = %request_id,
                                            provider = %provider_name,
                                            model = %model,
                                            receiver_name = %receiver_name,
                                            finish_reason = ?reason,
                                            chunk_index = chunk_count + pending_batch.len(),
                                            "stream done from upstream provider"
                                        );
                                        update_active_stream(&active_streams, &request_id, |stream| {
                                            stream.phase = ProviderStreamPhase::Completed;
                                            stream.last_progress_at = Instant::now();
                                        });
                                    }
                                }
                                None => {
                                    if pending_batch.is_empty() {
                                        upstream_done = true;
                                        update_active_stream(&active_streams, &request_id, |stream| {
                                            stream.phase = ProviderStreamPhase::Completed;
                                            stream.last_progress_at = Instant::now();
                                        });
                                    } else {
                                        flush_batch(&mut buffered, &mut pending_batch, &mut batch_count, &mut max_batch_size);
                                    }
                                }
                            }
                        }
                    }

                    if let Some(message) = terminal_message.take() {
                        buffered.push_back(message);
                    }

                    loop {
                        if disconnected_since.is_some() {
                            match kameo::actor::RemoteActorRef::<StreamReceiverActor>::lookup(receiver_name.clone()).await {
                                Ok(Some(found)) => {
                                    receiver_ref = found;
                                    let replay_count = buffered.iter().filter(|msg| keep_stream_message_buffered(msg)).count();
                                    let _ = receiver_ref.tell(&StreamChunkRelay {
                                        message: StreamRelayMessage::TransportReconnected {
                                            buffered_chunks: replay_count,
                                        },
                                    }).send();
                                    disconnected_since = None;
                                    update_active_stream(&active_streams, &request_id, |stream| {
                                        stream.phase = if stream.chunk_count == 0 {
                                            ProviderStreamPhase::WaitingFirstChunk
                                        } else {
                                            ProviderStreamPhase::Streaming
                                        };
                                        stream.receiver_connected = true;
                                    });
                                }
                                Ok(None) | Err(_) => {
                                    let since = disconnected_since.get_or_insert_with(tokio::time::Instant::now);
                                    update_active_stream(&active_streams, &request_id, |stream| {
                                        stream.phase = ProviderStreamPhase::ReceiverDisconnected;
                                        stream.receiver_connected = false;
                                    });
                                    if since.elapsed() >= reconnect_grace {
                                        buffered.push_back(StreamRelayMessage::TransportFailed {
                                            error: querymt::error::LLMError::Transport {
                                                kind: querymt::error::TransportErrorKind::Timeout,
                                                message: format!("reconnect grace expired after {:?}", reconnect_grace),
                                            }.to_payload(),
                                        });
                                        update_active_stream(&active_streams, &request_id, |stream| {
                                            stream.phase = ProviderStreamPhase::Failed;
                                            stream.last_error = Some("reconnect grace expired".to_string());
                                        });
                                        upstream_done = true;
                                    }
                                    tokio::time::sleep(Duration::from_millis(250)).await;
                                    if upstream_done {
                                        break 'outer;
                                    }
                                    continue;
                                }
                            }
                        }

                        let Some(front) = buffered.front().cloned() else {
                            break;
                        };
                        let relay = StreamChunkRelay { message: front };
                        if let Err(e) = receiver_ref.tell(&relay).send() {
                            tracing::warn!(
                                target: "remote::provider_host::stream",
                                session_id = %session_id,
                                request_id = %request_id,
                                provider = %provider_name,
                                model = %model,
                                receiver_name = %receiver_name,
                                error = %e,
                                "failed to relay chunk to receiver"
                            );
                            if disconnected_since.is_none() {
                                disconnected_since = Some(tokio::time::Instant::now());
                                let _ = receiver_ref.tell(&StreamChunkRelay {
                                    message: StreamRelayMessage::TransportDisconnected {
                                        reason: e.to_string(),
                                    },
                                }).send();
                                update_active_stream(&active_streams, &request_id, |stream| {
                                    stream.phase = ProviderStreamPhase::ReceiverDisconnected;
                                    stream.receiver_connected = false;
                                    stream.last_error = Some(e.to_string());
                                });
                            }
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            continue;
                        }

                        buffered.pop_front();
                        let relayed_chunks = match &relay.message {
                            StreamRelayMessage::Chunk(_) => 1,
                            StreamRelayMessage::ChunkBatch(chunks) => chunks.len(),
                            StreamRelayMessage::Heartbeat { .. }
                            | StreamRelayMessage::ProviderError { .. }
                            | StreamRelayMessage::TransportDisconnected { .. }
                            | StreamRelayMessage::TransportReconnected { .. }
                            | StreamRelayMessage::TransportFailed { .. } => 0,
                        };
                        chunk_count += relayed_chunks;
                        if relayed_chunks > 0 {
                            update_active_stream(&active_streams, &request_id, |stream| {
                                stream.chunk_count = chunk_count as u64;
                                stream.last_progress_at = Instant::now();
                            });
                        }
                        let elapsed_ms = relay_start.elapsed().as_millis();
                        let is_terminal = relay_message_is_terminal(&relay.message);
                        tracing::trace!(
                            target: "remote::provider_host::stream",
                            session_id = %session_id,
                            request_id = %request_id,
                            provider = %provider_name,
                            model = %model,
                            receiver_name = %receiver_name,
                            chunk_index = chunk_count,
                            relayed_chunks,
                            elapsed_ms,
                            is_terminal,
                            "chunk relayed"
                        );
                        if !keep_stream_message_buffered(&relay.message) {
                            continue;
                        }
                        if is_terminal {
                            break 'outer;
                        }
                    }

                    if upstream_done && buffered.is_empty() && pending_batch.is_empty() {
                        break;
                    }
                }

                remove_active_stream(&active_streams, &request_id);
                tracing::Span::current().record("chunk_count", chunk_count);
                tracing::Span::current().record("batch_count", batch_count);
                tracing::Span::current().record("max_batch_size", max_batch_size);
                tracing::trace!(
                    target: "remote::provider_host::stream",
                    session_id = %session_id,
                    request_id = %request_id,
                    provider = %provider_name,
                    model = %model,
                    receiver_name = %receiver_name,
                    chunk_count,
                    batch_count,
                    max_batch_size,
                    "stream relay complete"
                );
            }
            .instrument(relay_span),
        );

        Ok(())
    }
}

impl Message<CancelProviderStreamRequest> for ProviderHostActor {
    type Reply = usize;

    async fn handle(
        &mut self,
        msg: CancelProviderStreamRequest,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.cancel_streams(
            &msg.session_id,
            msg.request_id.as_deref(),
            msg.reason.as_deref(),
        )
    }
}

impl Message<RenewProviderStreamLease> for ProviderHostActor {
    type Reply = bool;

    async fn handle(
        &mut self,
        msg: RenewProviderStreamLease,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.renew_stream_lease(&msg.session_id, &msg.request_id, msg.lease_ttl_secs)
    }
}

impl Message<GetProviderStreamStatus> for ProviderHostActor {
    type Reply = Option<ProviderStreamStatus>;

    async fn handle(
        &mut self,
        msg: GetProviderStreamStatus,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.provider_stream_status(&msg.session_id, msg.request_id.as_deref())
    }
}

// ── RemoteActor + RemoteMessage registrations ─────────────────────────────────

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
            Box::pin(_internal::unlink::<StreamReceiverActor>(
                actor_id, sibling_id,
            ))
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

// ProviderHostActor messages
remote_provider_msg_impl!(
    ProviderHostActor,
    ProviderChatRequest,
    "querymt::ProviderChatRequest",
    REG_PROVIDER_CHAT_REQUEST
);
remote_provider_msg_impl!(
    ProviderHostActor,
    ProviderStreamRequest,
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

// StreamReceiverActor message
remote_provider_msg_impl!(
    StreamReceiverActor,
    StreamChunkRelay,
    "querymt::StreamChunkRelay",
    REG_STREAM_CHUNK_RELAY
);

//! `ProviderHostActor` — exposes this node's LLM providers as callable mesh services.
//!
//! Each mesh node runs one `ProviderHostActor`. Remote nodes send it
//! `ProviderChatRequest` (non-streaming) or `ProviderStreamRequest` (streaming)
//! messages to execute LLM calls using the local node's providers and API keys.
//!
//! API keys never leave the owning node. Only `ChatMessage`s flow in and
//! `ProviderChatResponse` / `StreamChunkRelay` flow out.
//!
//! Registered in the Kademlia DHT as `"provider_host::{hostname}"`.

use crate::agent::agent_config::AgentConfig;
use crate::error::AgentError;
use crate::session::provider::build_provider_from_config;
use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::remote::_internal;
use querymt::LLMProvider;
use querymt::ToolCall;
use querymt::Usage;
use querymt::chat::{ChatMessage, ChatResponse, FinishReason, StreamChunk, Tool};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use tokio::sync::mpsc;
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

/// Thin wrapper around `StreamChunk` used for the streaming relay path.
///
/// Each chunk produced by `chat_stream_with_tools` on the owning node is
/// sent to the `StreamReceiverActor` on the requesting node as a
/// `StreamChunkRelay`. Errors are serialized as `String` since `LLMError`
/// is not `Serialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunkRelay {
    /// `Ok(chunk)` for normal chunks, `Err(message)` for errors.
    pub chunk: Result<StreamChunk, String>,
}

/// How long `StreamReceiverActor` waits for the next chunk before closing
/// the stream with a timeout error.
///
/// This prevents stalled streams from keeping the ephemeral actor alive
/// indefinitely when a remote node goes down mid-stream.
pub const STREAM_CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
    /// DHT name of the `StreamReceiverActor` on the requesting node.
    /// Format: `"stream_rx::{request_id}"`.
    pub stream_receiver_name: String,
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
    tx: mpsc::Sender<Result<StreamChunk, String>>,
    /// The name this actor is registered under in the Kademlia DHT.
    /// Used to deregister on stop so the entry doesn't linger.
    dht_name: String,
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
    pub fn new(tx: mpsc::Sender<Result<StreamChunk, String>>, dht_name: String) -> Self {
        Self { tx, dht_name }
    }
}

impl Message<StreamChunkRelay> for StreamReceiverActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamChunkRelay,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let is_terminal = matches!(&msg.chunk, Ok(StreamChunk::Done { .. }) | Err(_));

        // Forward to channel (ignore send errors — receiver may have dropped).
        let _ = self.tx.send(msg.chunk).await;

        if is_terminal {
            ctx.actor_ref().kill();
        }
    }
}

// ── ProviderHostActor ─────────────────────────────────────────────────────────

/// Per-node actor that serves LLM provider calls to the mesh.
///
/// Spawned once during mesh bootstrap alongside `RemoteNodeManager`.
/// Registered in the DHT as `"provider_host::{hostname}"`.
#[derive(Actor)]
pub struct ProviderHostActor {
    config: Arc<AgentConfig>,
    /// LRU-style provider cache keyed by `(provider_name, model)`.
    /// Re-uses already-constructed providers to avoid redundant plugin loads.
    provider_cache: HashMap<(String, String), Arc<dyn LLMProvider>>,
}

impl ProviderHostActor {
    pub fn new(config: Arc<AgentConfig>) -> Self {
        Self {
            config,
            provider_cache: HashMap::new(),
        }
    }

    /// Resolve or build the provider for the given name + model.
    #[tracing::instrument(
        name = "remote.provider_host.get_or_build_provider",
        skip(self),
        fields(provider = %provider_name, model = %model, cache_hit = tracing::field::Empty)
    )]
    async fn get_or_build_provider(
        &mut self,
        provider_name: &str,
        model: &str,
    ) -> Result<Arc<dyn LLMProvider>, AgentError> {
        let key = (provider_name.to_string(), model.to_string());

        if let Some(cached) = self.provider_cache.get(&key) {
            tracing::Span::current().record("cache_hit", true);
            return Ok(Arc::clone(cached));
        }

        tracing::Span::current().record("cache_hit", false);

        let plugin_registry = self.config.provider.plugin_registry();
        let provider = build_provider_from_config(
            &plugin_registry,
            provider_name,
            model,
            None, // no extra params
            None, // no API key override — use local keys
            None, // provider_node: always local on the owning node
            None, // mesh_handle: not needed — owning node builds directly
        )
        .await
        .map_err(|e| {
            AgentError::Internal(format!(
                "ProviderHostActor: failed to build provider '{}' model '{}': {}",
                provider_name, model, e
            ))
        })?;

        self.provider_cache.insert(key, Arc::clone(&provider));
        Ok(provider)
    }
}

// ── Non-streaming handler ─────────────────────────────────────────────────────

impl Message<ProviderChatRequest> for ProviderHostActor {
    type Reply = Result<ProviderChatResponse, AgentError>;

    #[tracing::instrument(
        name = "remote.provider_host.chat",
        skip(self, _ctx),
        fields(
            provider = %msg.provider,
            model = %msg.model,
            message_count = msg.messages.len(),
            has_tools = msg.tools.is_some(),
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
            .get_or_build_provider(&msg.provider, &msg.model)
            .await?;

        let tools_slice = msg.tools.as_deref();

        let response = provider
            .chat_with_tools(&msg.messages, tools_slice)
            .await
            .map_err(|e| AgentError::ProviderChat {
                operation: "chat_with_tools".to_string(),
                reason: e.to_string(),
            })?;

        let tool_calls = response.tool_calls().unwrap_or_default();
        let finish_reason = response.finish_reason().map(|r| format!("{:?}", r));

        tracing::Span::current()
            .record("tool_calls_returned", tool_calls.len())
            .record("finish_reason", finish_reason.as_deref().unwrap_or("none"));

        log::debug!(
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
        skip(self, _ctx),
        fields(
            provider = %msg.provider,
            model = %msg.model,
            message_count = msg.messages.len(),
            has_tools = msg.tools.is_some(),
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

        let provider = self
            .get_or_build_provider(&msg.provider, &msg.model)
            .await?;

        let tools_slice = msg.tools.as_deref();

        let mut stream = provider
            .chat_stream_with_tools(&msg.messages, tools_slice)
            .await
            .map_err(|e| AgentError::ProviderChat {
                operation: "chat_stream_with_tools".to_string(),
                reason: e.to_string(),
            })?;

        // Look up the StreamReceiverActor on the requesting node.
        let receiver_ref = {
            let lookup_span = tracing::info_span!(
                "remote.provider_host.stream.lookup_receiver",
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
        let receiver_name = msg.stream_receiver_name.clone();

        // Relay chunks asynchronously so this handler returns promptly.
        // Propagate the current span into the spawned task so chunk-level
        // trace events appear as children of this handler span.
        let relay_span = tracing::info_span!(
            "remote.provider_host.stream.relay",
            provider = %provider_name,
            model = %model,
            receiver_name = %receiver_name,
            chunk_count = tracing::field::Empty,
        );
        tokio::spawn(
            async move {
                let mut chunk_count = 0usize;
                let relay_start = std::time::Instant::now();

                while let Some(chunk_result) = stream.next().await {
                    let relay = StreamChunkRelay {
                        chunk: chunk_result.map_err(|e| e.to_string()),
                    };

                    let is_done =
                        matches!(relay.chunk, Ok(StreamChunk::Done { .. })) || relay.chunk.is_err();

                    if let Err(e) = receiver_ref.tell(&relay).send() {
                        log::warn!(
                            "ProviderHostActor: failed to relay chunk to '{}': {}",
                            receiver_name,
                            e
                        );
                        break;
                    }

                    chunk_count += 1;
                    let elapsed_ms = relay_start.elapsed().as_millis();
                    tracing::trace!(
                        target: "remote::provider_host::stream",
                        chunk_index = chunk_count,
                        elapsed_ms,
                        is_done,
                        "chunk relayed"
                    );

                    if is_done {
                        break;
                    }
                }

                tracing::Span::current().record("chunk_count", chunk_count);
                log::debug!(
                    "ProviderHostActor: streaming call to {}/{} complete ({} chunks relayed to '{}')",
                    provider_name,
                    model,
                    chunk_count,
                    receiver_name,
                );
            }
            .instrument(relay_span),
        );

        Ok(())
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

// StreamReceiverActor message
remote_provider_msg_impl!(
    StreamReceiverActor,
    StreamChunkRelay,
    "querymt::StreamChunkRelay",
    REG_STREAM_CHUNK_RELAY
);

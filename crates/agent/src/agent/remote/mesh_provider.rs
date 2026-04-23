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
    ProviderChatRequest, ProviderHostActor, ProviderStreamRequest, StreamReceiverActor,
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
use std::pin::Pin;
use std::time::Instant;
use tokio::sync::mpsc;
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
}

impl MeshChatProvider {
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

    /// Resolve the remote `ProviderHostActor` ref from the DHT.
    #[tracing::instrument(
        name = "remote.mesh_provider.dht_lookup",
        skip(self),
        fields(dht_name = %self.target_dht_name, found = tracing::field::Empty)
    )]
    async fn lookup_provider_host(
        &self,
    ) -> Result<kameo::actor::RemoteActorRef<ProviderHostActor>, LLMError> {
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
        tracing::Span::current().record("found", result.is_ok());
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
            message_count = messages.len(),
            has_tools = tools.is_some(),
        )
    )]
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn querymt::chat::ChatResponse>, LLMError> {
        let host_ref = self.lookup_provider_host().await?;

        let request = ProviderChatRequest {
            provider: self.provider_name.clone(),
            model: self.model.clone(),
            messages: messages.to_vec(),
            tools: tools.map(|t| t.to_vec()),
            params: self.params.clone(),
        };

        // ask() flattens Result<Result<T,E>, RemoteSendError> into Result<T, RemoteSendError<E>>
        let chat_response = host_ref
            .ask(&request)
            .await
            .map_err(remote_send_error_to_llm_error_with_handler)?;

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
            message_count = messages.len(),
            has_tools = tools.is_some(),
            request_id = tracing::field::Empty,
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
        let host_ref = self.lookup_provider_host().await?;

        // ── 1. Create the mpsc channel ────────────────────────────────────────
        let (tx, rx) = mpsc::channel::<StreamRelayMessage>(64);

        // ── 2. Spawn the ephemeral StreamReceiverActor on the local node ──────
        let request_id = Uuid::now_v7().to_string();
        let session_id = self
            .params
            .as_ref()
            .and_then(|v| v.get("_remote_session_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown-session")
            .to_string();
        let stream_rx_name = super::dht_name::stream_receiver(&session_id, &request_id);
        tracing::Span::current().record("request_id", &request_id);

        {
            let reg_span = tracing::info_span!(
                "remote.mesh_provider.dht_register_receiver",
                stream_rx_name = %stream_rx_name,
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
            session_id,
            request_id: request_id.clone(),
            stream_receiver_name: stream_rx_name.clone(),
            reconnect_grace_secs: self.mesh.stream_reconnect_grace().as_secs(),
            params: self.params.clone(),
        };

        host_ref
            .tell(&stream_request)
            .send()
            .map_err(remote_send_error_to_llm_error_no_handler)?;

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

        let stream_rx_name_for_log = stream_rx_name.clone();
        let provider_for_stream = self.provider_name.clone();
        let model_for_stream = self.model.clone();
        let target_for_stream = self.target_dht_name.clone();
        let reconnect_grace = self.mesh.stream_reconnect_grace();
        let stream_start = Instant::now();
        let target_peer_id = self
            .target_dht_name
            .strip_prefix("provider_host::peer::")
            .and_then(|s| s.parse::<PeerId>().ok());
        let mesh = self.mesh.clone();

        let stream = futures_util::stream::unfold(
            (raw_stream, None::<tokio::time::Instant>, 0_u64),
            move |(mut raw_stream, disconnected_since, mut chunk_index)| {
                let mesh = mesh.clone();
                let target_peer_id = target_peer_id;
                let stream_rx_name_for_log = stream_rx_name_for_log.clone();
                let provider_for_stream = provider_for_stream.clone();
                let model_for_stream = model_for_stream.clone();
                let target_for_stream = target_for_stream.clone();
                async move {
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
                                (raw_stream, disconnected_since, chunk_index),
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
                                    (raw_stream, disconnected_since, chunk_index),
                                ));
                            }
                        }
                    } else {
                        raw_stream.next().await
                    };

                    match next {
                        Some(StreamRelayMessage::Chunk(chunk)) => {
                            chunk_index += 1;
                            let elapsed_ms = stream_start.elapsed().as_millis();
                            let is_done = matches!(&chunk, StreamChunk::Done { .. });
                            tracing::trace!(
                                target: "remote::mesh_provider::stream",
                                provider = %provider_for_stream,
                                model = %model_for_stream,
                                target_node = %target_for_stream,
                                stream_rx = %stream_rx_name_for_log,
                                chunk_index,
                                elapsed_ms,
                                is_done,
                                "stream chunk received"
                            );
                            Some((Ok(chunk), (raw_stream, None, chunk_index)))
                        }
                        Some(StreamRelayMessage::ProviderError { error }) => Some((
                            Err(LLMError::from_payload(error)),
                            (raw_stream, disconnected_since, chunk_index),
                        )),
                        Some(StreamRelayMessage::TransportDisconnected { reason }) => {
                            log::warn!(
                                "MeshChatProvider: stream '{}' transport disconnected: {}",
                                stream_rx_name_for_log,
                                reason,
                            );
                            Some((
                                Err(LLMError::RemoteStreamDisconnected { message: reason }),
                                (raw_stream, Some(tokio::time::Instant::now()), chunk_index),
                            ))
                        }
                        Some(StreamRelayMessage::TransportReconnected { buffered_chunks }) => {
                            log::info!(
                                "MeshChatProvider: stream '{}' transport reconnected (buffered_chunks={})",
                                stream_rx_name_for_log,
                                buffered_chunks,
                            );
                            Some((
                                Err(LLMError::RemoteStreamReconnected {
                                    message: format!("buffered_chunks={}", buffered_chunks),
                                }),
                                (raw_stream, None, chunk_index),
                            ))
                        }
                        Some(StreamRelayMessage::TransportFailed { error }) => Some((
                            Err(LLMError::from_payload(error)),
                            (raw_stream, disconnected_since, chunk_index),
                        )),
                        None => {
                            let peer_alive = target_peer_id
                                .as_ref()
                                .is_some_and(|peer_id| mesh.is_peer_alive(peer_id));
                            if disconnected_since.is_some() || !peer_alive {
                                Some((
                                    Err(LLMError::Transport {
                                        kind: TransportErrorKind::ConnectionClosed,
                                        message: format!(
                                            "stream receiver closed (peer_alive={})",
                                            peer_alive,
                                        ),
                                    }),
                                    (raw_stream, disconnected_since, chunk_index),
                                ))
                            } else {
                                None
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

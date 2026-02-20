//! `MeshChatProvider` — client-side proxy that executes LLM calls on a remote mesh node.
//!
//! When a session's `LLMConfig` targets a provider owned by a different node,
//! `build_provider_from_config` wraps the request in a `MeshChatProvider` instead
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

use crate::agent::remote::mesh::MeshHandle;
use crate::agent::remote::provider_host::{
    ProviderChatRequest, ProviderHostActor, ProviderStreamRequest, STREAM_CHUNK_TIMEOUT,
    StreamReceiverActor,
};
use futures_util::StreamExt;
use kameo::actor::Spawn;
use querymt::LLMProvider;
use querymt::chat::{ChatMessage, ChatProvider, StreamChunk, Tool};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::LLMError;
use std::pin::Pin;
use tokio::sync::mpsc;
use tracing::Instrument;
use uuid::Uuid;

// ── MeshChatProvider ──────────────────────────────────────────────────────────

/// A `ChatProvider` (and `LLMProvider`) implementation that proxies all LLM calls
/// to a `ProviderHostActor` running on `target_node` in the kameo mesh.
///
/// Constructed by `build_provider_from_config` when:
/// 1. The session's `LLMConfig.provider_node` names a specific remote node, or
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
    /// DHT name of the target `ProviderHostActor`, e.g. `"provider_host::gpu-server"`.
    target_dht_name: String,
}

impl MeshChatProvider {
    /// Create a new `MeshChatProvider`.
    ///
    /// # Arguments
    /// * `mesh`          — live mesh handle (for DHT operations).
    /// * `target_node`   — hostname of the node whose `ProviderHostActor` to call.
    ///   Format: plain hostname (e.g. `"gpu-server"`).
    /// * `provider_name` — provider plugin name (e.g. `"anthropic"`).
    /// * `model`         — model name (e.g. `"claude-sonnet-4-20250514"`).
    pub fn new(mesh: &MeshHandle, target_node: &str, provider_name: &str, model: &str) -> Self {
        Self {
            provider_name: provider_name.to_string(),
            model: model.to_string(),
            mesh: mesh.clone(),
            target_dht_name: format!("provider_host::{}", target_node),
        }
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
        };

        // ask() flattens Result<Result<T,E>, RemoteSendError> into Result<T, RemoteSendError<E>>
        let chat_response = host_ref.ask(&request).await.map_err(|e| {
            LLMError::ProviderError(format!(
                "MeshChatProvider: remote call to '{}' failed: {}",
                self.target_dht_name, e
            ))
        })?;

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
        let (tx, rx) = mpsc::channel::<Result<StreamChunk, String>>(64);

        // ── 2. Spawn the ephemeral StreamReceiverActor on the local node ──────
        let request_id = Uuid::now_v7().to_string();
        let stream_rx_name = format!("stream_rx::{}", request_id);
        tracing::Span::current().record("request_id", &request_id);

        {
            let reg_span = tracing::info_span!(
                "remote.mesh_provider.dht_register_receiver",
                stream_rx_name = %stream_rx_name,
            );
            let receiver_actor = StreamReceiverActor::new(tx, stream_rx_name.clone());
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
            stream_receiver_name: stream_rx_name.clone(),
        };

        host_ref.tell(&stream_request).send().map_err(|e| {
            LLMError::ProviderError(format!(
                "MeshChatProvider: failed to send stream request to '{}': {}",
                self.target_dht_name, e
            ))
        })?;

        log::debug!(
            "MeshChatProvider: streaming request sent to {} ({}/{})",
            self.target_dht_name,
            self.provider_name,
            self.model
        );

        // ── 4. Wrap mpsc::Receiver as Stream<Item = Result<StreamChunk, LLMError>> ──
        //
        // Apply a per-chunk timeout so stalled streams (e.g. provider node went
        // down mid-stream) terminate with a clear error rather than hanging
        // forever.
        //
        // DHT cleanup (unregister the ephemeral StreamReceiverActor) is handled
        // by the actor's `on_stop` hook — no extra bookkeeping needed here.
        let raw_stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        // `tokio_stream::StreamExt::timeout` wraps each item as `Result<T, Elapsed>`.
        let timed = tokio_stream::StreamExt::timeout(raw_stream, STREAM_CHUNK_TIMEOUT);

        let stream_rx_name_for_log = stream_rx_name.clone();
        let provider_for_stream = self.provider_name.clone();
        let model_for_stream = self.model.clone();
        let target_for_stream = self.target_dht_name.clone();
        let mut chunk_index: u64 = 0;
        let stream_start = std::time::Instant::now();

        // Map the timeout wrapper to `Result<StreamChunk, LLMError>`.
        let stream = StreamExt::map(
            timed,
            move |timeout_result| -> Result<StreamChunk, LLMError> {
                match timeout_result {
                    // Chunk arrived in time — map the inner string-error to LLMError.
                    Ok(relay_result) => {
                        chunk_index += 1;
                        let elapsed_ms = stream_start.elapsed().as_millis();
                        let is_done =
                            matches!(&relay_result, Ok(StreamChunk::Done { .. }) | Err(_));
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
                        relay_result.map_err(|e| {
                            LLMError::ProviderError(format!("MeshChatProvider: {}", e))
                        })
                    }
                    // No chunk arrived within the deadline — remote node may be down.
                    Err(_elapsed) => {
                        log::warn!(
                            "MeshChatProvider: stream '{}' timed out after {:?} — remote node may be down",
                            stream_rx_name_for_log,
                            STREAM_CHUNK_TIMEOUT,
                        );
                        Err(LLMError::ProviderError(format!(
                            "MeshChatProvider: stream timed out after {:?} — remote node may be unreachable",
                            STREAM_CHUNK_TIMEOUT,
                        )))
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

// ── find_provider_on_mesh ─────────────────────────────────────────────────────

/// Scan the mesh for any node advertising `provider_name` in its available models.
///
/// Returns the hostname suffix (as used in DHT name `"provider_host::{hostname}"`)
/// of the first node that has valid credentials for the provider, or `None` if no
/// peer is advertising it.
///
/// This is used by `build_provider_from_config` as a mesh-fallback (Case 3) when
/// the provider is unavailable locally.
///
/// # Implementation note
///
/// This function queries each `RemoteNodeManager` via `ListAvailableModels` +
/// `GetNodeInfo`. Both calls are made to the same peer so we obtain the real
/// hostname without any DHT name-reverse-lookup.  The explicit `provider_node`
/// path (Case 1) is the primary flow; this best-effort scan only runs when
/// `provider_node` is `None`.
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
) -> Option<String> {
    use crate::agent::remote::node_manager::{GetNodeInfo, RemoteNodeManager};
    use crate::agent::remote::{ListAvailableModels, NodeInfo};

    let mut stream = mesh.lookup_all_actors::<RemoteNodeManager>("node_manager");
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

        // This peer has the provider — ask for its hostname.
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
            "find_provider_on_mesh: provider '{}' found on mesh peer '{}' (mesh fallback)",
            provider_name,
            node_info.hostname
        );
        tracing::Span::current().record("found", true);
        return Some(node_info.hostname);
    }

    tracing::Span::current().record("found", false);
    None
}

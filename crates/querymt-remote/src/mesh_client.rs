use crate::{
    AttachStreamConsumer, CancelProviderStreamRequest, GenericProviderStreamRequest,
    GetProviderCatalog, GetProviderStreamStatus, MeshHandle, MeshScopeId, NodeId,
    ProviderCatalogActor, ProviderHostActor, ProviderStreamRouterActor, ProviderStreamStatus,
    RegisterRequest, RemoteChatProvider, RemoteProviderClientConfig, RemoteProviderClientCore,
    RemoteProviderClientTransport, StreamRelayMessage, remote_send_error_to_llm_error_no_handler,
    scoped_provider_catalog, scoped_provider_host,
};
use async_trait::async_trait;
use kameo::actor::Spawn;
use libp2p::PeerId;
use querymt::LLMProvider;
use querymt::chat::{ChatMessage, ChatProvider, StreamChunk, Tool};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::LLMError;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};

static PROVIDER_HOST_CACHE: OnceLock<RwLock<HashMap<String, kameo::actor::RemoteActorRef<ProviderHostActor>>>> =
    OnceLock::new();
static STREAM_ROUTER_CACHE: OnceLock<RwLock<HashMap<String, kameo::actor::ActorRef<ProviderStreamRouterActor>>>> =
    OnceLock::new();

#[derive(Clone)]
pub struct KameoMeshClientTransport {
    mesh: MeshHandle,
}

impl KameoMeshClientTransport {
    pub fn new(mesh: MeshHandle) -> Self {
        Self { mesh }
    }

    fn provider_host_cache()
    -> &'static RwLock<HashMap<String, kameo::actor::RemoteActorRef<ProviderHostActor>>> {
        PROVIDER_HOST_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
    }

    fn stream_router_cache()
    -> &'static RwLock<HashMap<String, kameo::actor::ActorRef<ProviderStreamRouterActor>>> {
        STREAM_ROUTER_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
    }

    pub fn peer_id(&self) -> PeerId {
        *self.mesh.peer_id()
    }

    pub fn stream_reconnect_grace(&self) -> Duration {
        self.mesh.stream_reconnect_grace()
    }

    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.mesh.is_peer_alive(peer_id)
    }

    pub fn best_scope_for_peer(&self, peer_id: &PeerId) -> Option<MeshScopeId> {
        self.mesh.best_route_for_peer(peer_id).map(|route| route.scope)
    }

    async fn invalidate_cached_provider_host(&self, target_locator: &str) {
        Self::provider_host_cache()
            .write()
            .await
            .remove(target_locator);
    }

    async fn lookup_provider_host(
        &self,
        target_locator: &str,
    ) -> Result<kameo::actor::RemoteActorRef<ProviderHostActor>, LLMError> {
        if let Some(cached) = Self::provider_host_cache()
            .read()
            .await
            .get(target_locator)
            .cloned()
        {
            tracing::Span::current().record("cache_hit", true);
            tracing::Span::current().record("found", true);
            tracing::Span::current().record("lookup_ms", 0_u64);
            return Ok(cached);
        }

        tracing::Span::current().record("cache_hit", false);
        let lookup_start = std::time::Instant::now();
        let result = self
            .mesh
            .lookup_actor::<ProviderHostActor>(target_locator)
            .await
            .map_err(|e| {
                LLMError::ProviderError(format!(
                    "MeshChatProvider: actor lookup for '{}' failed: {}",
                    target_locator, e
                ))
            })?
            .ok_or_else(|| {
                LLMError::ProviderError(format!(
                    "MeshChatProvider: provider host '{}' not found (is the node online?)",
                    target_locator
                ))
            });
        tracing::Span::current().record("lookup_ms", lookup_start.elapsed().as_millis() as u64);
        tracing::Span::current().record("found", result.is_ok());
        if let Ok(actor_ref) = &result {
            Self::provider_host_cache()
                .write()
                .await
                .insert(target_locator.to_string(), actor_ref.clone());
        }
        result
    }

    async fn get_or_create_stream_router(
        &self,
        session_id: &str,
    ) -> kameo::actor::ActorRef<ProviderStreamRouterActor> {
        if let Some(cached) = Self::stream_router_cache()
            .read()
            .await
            .get(session_id)
            .cloned()
        {
            return cached;
        }

        let router = ProviderStreamRouterActor::new(None, None);
        let router_ref = ProviderStreamRouterActor::spawn(router);

        Self::stream_router_cache()
            .write()
            .await
            .insert(session_id.to_string(), router_ref.clone());

        router_ref
    }
}

#[async_trait]
impl RemoteProviderClientTransport for KameoMeshClientTransport {
    type HostRef = kameo::actor::RemoteActorRef<ProviderHostActor>;
    type RouterRef = kameo::actor::ActorRef<ProviderStreamRouterActor>;
    type RemoteRouterRef = kameo::actor::RemoteActorRef<ProviderStreamRouterActor>;

    async fn local_peer_id_display(&self) -> String {
        self.peer_id().to_string()
    }

    async fn target_peer_id_display(&self, target_locator: &str) -> String {
        target_locator
            .rsplit("::peer::")
            .next()
            .unwrap_or("unknown-peer")
            .to_string()
    }

    async fn invalidate_cached_host(&self, target_locator: &str) {
        self.invalidate_cached_provider_host(target_locator).await;
    }

    async fn lookup_host(&self, target_locator: &str) -> Result<Self::HostRef, LLMError> {
        self.lookup_provider_host(target_locator).await
    }

    async fn prepare_stream_router(
        &self,
        session_id: &str,
        request_id: &str,
        consumer_tx: mpsc::Sender<StreamRelayMessage>,
    ) -> Result<(Self::RouterRef, Self::RemoteRouterRef), LLMError> {
        let router = self.get_or_create_stream_router(session_id).await;
        router
            .ask(RegisterRequest {
                request_id: request_id.to_string(),
            })
            .await
            .map_err(|e| LLMError::ProviderError(format!("failed to register stream request: {}", e)))?;
        router
            .ask(AttachStreamConsumer {
                request_id: request_id.to_string(),
                consumer_tx,
            })
            .await
            .map_err(|e| LLMError::ProviderError(format!("failed to attach stream consumer: {}", e)))?;
        let remote = router.clone().into_remote_ref().await;
        Ok((router, remote))
    }

    async fn send_chat_request(
        &self,
        host: &Self::HostRef,
        request: &crate::ProviderChatRequest,
    ) -> Result<crate::ProviderChatResponse, LLMError> {
        host.ask(request)
            .await
            .map_err(|e| match crate::remote_send_error_base(e) {
                Ok(err) => err,
                Err(handler) => crate::decode_payload_handler_error(
                    &serde_json::to_string(&handler.to_payload())
                        .unwrap_or_else(|_| handler.to_string()),
                ),
            })
    }

    async fn send_stream_request(
        &self,
        host: &Self::HostRef,
        request: GenericProviderStreamRequest<Self::RemoteRouterRef>,
    ) -> Result<(), LLMError> {
        host.tell(&request)
            .send_ack()
            .await
            .map_err(remote_send_error_to_llm_error_no_handler)
    }

    async fn cancel_stream(
        &self,
        host: &Self::HostRef,
        request: CancelProviderStreamRequest,
    ) -> Result<(), LLMError> {
        host.ask(&request)
            .await
            .map(|_| ())
            .map_err(remote_send_error_to_llm_error_no_handler)
    }

    async fn renew_stream_lease(
        &self,
        host: &Self::HostRef,
        session_id: &str,
        request_id: &str,
        lease_ttl_secs: u64,
    ) -> Result<bool, LLMError> {
        host.ask(&crate::RenewProviderStreamLease {
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
            lease_ttl_secs,
        })
        .await
        .map_err(remote_send_error_to_llm_error_no_handler)
    }

    async fn get_stream_status(
        &self,
        host: &Self::HostRef,
        request: GetProviderStreamStatus,
    ) -> Result<Option<ProviderStreamStatus>, LLMError> {
        host.ask(&request)
            .await
            .map_err(remote_send_error_to_llm_error_no_handler)
    }

    async fn is_target_peer_alive(&self, target_locator: &str) -> bool {
        target_locator
            .rsplit("::peer::")
            .next()
            .and_then(|s| s.parse::<PeerId>().ok())
            .is_some_and(|peer_id| self.is_peer_alive(&peer_id))
    }

    fn stream_reconnect_grace(&self) -> Duration {
        self.stream_reconnect_grace()
    }
}

#[derive(Clone)]
pub struct MeshChatProvider {
    inner: RemoteChatProvider<KameoMeshClientTransport>,
}

impl MeshChatProvider {
    fn rebuild_with_config(&mut self, config: RemoteProviderClientConfig) {
        let core = RemoteProviderClientCore::new(self.inner.core().transport().clone(), config);
        self.inner = RemoteChatProvider::new(core);
    }

    pub fn new(mesh: &MeshHandle, target_node_id: &str, provider_name: &str, model: &str) -> Self {
        let peer_id = target_node_id.parse::<PeerId>().ok();
        let adapter = KameoMeshClientTransport::new(mesh.clone());
        let target_scope = peer_id
            .as_ref()
            .and_then(|pid| adapter.best_scope_for_peer(pid))
            .unwrap_or(MeshScopeId::lan_default());

        let client = RemoteProviderClientConfig::new(
            scoped_provider_host(&target_scope, &target_node_id),
            provider_name,
            model,
        );
        let core = RemoteProviderClientCore::new(Arc::new(adapter), client);

        Self {
            inner: RemoteChatProvider::new(core),
        }
    }

    pub fn from_node_id(
        mesh: &MeshHandle,
        target_node_id: &NodeId,
        provider_name: &str,
        model: &str,
    ) -> Self {
        Self::new(mesh, &target_node_id.to_string(), provider_name, model)
    }

    pub fn with_params(mut self, params: Option<serde_json::Value>) -> Self {
        let config = self.inner.core().config().clone().with_params(params);
        self.rebuild_with_config(config);
        self
    }

    pub fn with_stream_controls(
        mut self,
        heartbeat_interval_secs: u64,
        lease_ttl_secs: u64,
    ) -> Self {
        let config = self
            .inner
            .core()
            .config()
            .clone()
            .with_stream_controls(heartbeat_interval_secs, lease_ttl_secs);
        self.rebuild_with_config(config);
        self
    }

    pub async fn cancel_remote_stream(
        &self,
        session_id: &str,
        request_id: Option<&str>,
        reason: Option<&str>,
    ) {
        self.inner
            .cancel_remote_stream(session_id, request_id, reason)
            .await;
    }

    pub async fn get_remote_stream_status(
        &self,
        session_id: &str,
        request_id: Option<&str>,
    ) -> Option<ProviderStreamStatus> {
        self.inner
            .get_remote_stream_status(session_id, request_id)
            .await
    }
}

#[async_trait]
impl ChatProvider for MeshChatProvider {
    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn querymt::chat::ChatResponse>, LLMError> {
        self.inner.chat_with_tools(messages, tools).await
    }

    async fn chat_stream_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, LLMError>> + Send>>, LLMError>
    {
        self.inner.chat_stream_with_tools(messages, tools).await
    }
}

#[async_trait]
impl CompletionProvider for MeshChatProvider {
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        self.inner.complete(req).await
    }
}

#[async_trait]
impl EmbeddingProvider for MeshChatProvider {
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        self.inner.embed(input).await
    }
}

impl LLMProvider for MeshChatProvider {}

#[tracing::instrument(
    name = "remote.mesh_provider.find_on_mesh",
    skip(mesh),
    fields(
        provider_name,
        peers_checked = tracing::field::Empty,
        found = tracing::field::Empty,
    )
)]
pub async fn find_provider_on_mesh(mesh: &MeshHandle, provider_name: &str) -> Option<NodeId> {
    let mut peers_checked: u32 = 0;
    let mut candidates: Vec<(PeerId, NodeId)> = Vec::new();

    for scope in mesh.active_scopes() {
        let mut seen_catalog_peers = std::collections::HashSet::new();
        for peer_id in mesh.route_peer_ids() {
            let catalog_name = scoped_provider_catalog(&scope, &peer_id);
            let Ok(Some(catalog_ref)) = mesh.lookup_actor::<ProviderCatalogActor>(catalog_name).await else {
                continue;
            };
            if !seen_catalog_peers.insert(peer_id) {
                continue;
            }

            peers_checked += 1;
            tracing::Span::current().record("peers_checked", peers_checked);

            let snapshot = match catalog_ref.ask::<GetProviderCatalog>(&GetProviderCatalog).await {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    log::debug!("find_provider_on_mesh: GetProviderCatalog failed: {}", e);
                    continue;
                }
            };

            if !snapshot.providers.iter().any(|entry| entry.provider == provider_name) {
                continue;
            }

            if let Ok(node_id) = NodeId::parse(&snapshot.node.node_id) {
                candidates.push((peer_id, node_id));
            }
        }
    }

    candidates.sort_by_key(|(peer_id, node_id)| {
        let priority = mesh
            .best_route_for_peer(peer_id)
            .map(|route| route.priority)
            .unwrap_or(0);
        (std::cmp::Reverse(priority), node_id.to_string())
    });
    candidates.dedup_by(|a, b| a.0 == b.0);

    if let Some((peer_id, node_id)) = candidates.first() {
        log::info!(
            "find_provider_on_mesh: provider '{}' selected mesh peer '{}' ({}) (mesh fallback)",
            provider_name,
            peer_id,
            node_id
        );
        tracing::Span::current().record("found", true);
        return Some(node_id.clone());
    }

    tracing::Span::current().record("found", false);
    None
}

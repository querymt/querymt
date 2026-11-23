use crate::mesh_ingress::{SharingCallError, SharingRejection};
use crate::provider_share::guarded::MeteredChat;
use crate::request_timeout::{
    SharingAskError, ask_sharing_v1, ask_with_sharing, with_sharing_protocol,
};
use crate::{
    AttachStreamConsumer, CancelProviderStreamRequest, GenericProviderStreamRequest,
    GetProviderCatalog, GetProviderContractInfo, GetProviderStreamStatus, MeshHandle, MeshScopeId,
    NodeId, ProviderCatalogActor, ProviderContractInfo, ProviderHostActor,
    ProviderStreamRouterActor, ProviderStreamStatus, RegisterRequest, RemoteChatProvider,
    RemoteProviderClientConfig, RemoteProviderClientCore, RemoteProviderClientTransport,
    StreamRelayMessage, remote_send_error_to_llm_error_no_handler, scoped_provider_catalog,
    scoped_provider_host,
};
use async_trait::async_trait;
use kameo::actor::Spawn;
use libp2p::PeerId;
use querymt::LLMProvider;
use querymt::chat::{ChatCall, ChatCapabilities, ChatOutput, ChatProvider, ChatStream};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::LLMError;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};

static PROVIDER_HOST_CACHE: OnceLock<
    RwLock<HashMap<String, kameo::actor::RemoteActorRef<ProviderHostActor>>>,
> = OnceLock::new();
type StreamRouterKey = (PeerId, Option<PeerId>, String);
type StreamRouters =
    RwLock<HashMap<StreamRouterKey, kameo::actor::ActorRef<ProviderStreamRouterActor>>>;
static STREAM_ROUTER_CACHE: OnceLock<StreamRouters> = OnceLock::new();

fn provider_host_peer(target_locator: &str) -> Result<PeerId, LLMError> {
    target_locator
        .rsplit_once("::provider_host::peer::")
        .and_then(|(_, peer)| peer.parse().ok())
        .ok_or_else(|| {
            LLMError::ProviderError(format!("Invalid provider host locator '{target_locator}'"))
        })
}

fn require_provider_host_peer(expected: PeerId, resolved: Option<&PeerId>) -> Result<(), LLMError> {
    if resolved == Some(&expected) {
        Ok(())
    } else {
        Err(LLMError::ProviderError(format!(
            "Provider host lookup did not resolve to expected peer '{expected}'"
        )))
    }
}

fn provider_ask_error(error: SharingAskError<crate::RemoteProviderHostError>) -> LLMError {
    match error {
        SharingAskError::Sharing(error) => sharing_error(error),
        SharingAskError::Actor(error) => match crate::remote_send_error_base(error) {
            Ok(error) => error,
            Err(handler) => LLMError::from_payload(handler.to_payload()),
        },
    }
}

fn sharing_error(error: SharingCallError) -> LLMError {
    use querymt::error::{ProviderErrorKind, ProviderFailure};
    let (kind, code) = match error {
        SharingCallError::Rejected(SharingRejection::Denied | SharingRejection::Changed) => {
            (ProviderErrorKind::Authentication, "mesh_access_denied")
        }
        SharingCallError::UnsupportedProtocol
        | SharingCallError::Rejected(SharingRejection::UnsupportedOperation) => (
            ProviderErrorKind::UnsupportedOperation,
            "mesh_access_incompatible",
        ),
        _ => (
            ProviderErrorKind::UnknownPermanent,
            "mesh_access_unavailable",
        ),
    };
    // An ambiguous v1 failure must not re-dispatch with a fresh attempt ID.
    ProviderFailure::new(kind, error.to_string())
        .with_code(Some(code.into()))
        .into()
}

#[derive(Clone)]
pub struct KameoMeshClientTransport {
    mesh: MeshHandle,
}

impl KameoMeshClientTransport {
    const STREAM_ROUTER_TIMEOUT: Duration = Duration::from_secs(10);
    const PROVIDER_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
    const PROVIDER_CHAT_REPLY_TIMEOUT: Duration = Duration::from_secs(600);

    pub fn new(mesh: MeshHandle) -> Self {
        Self { mesh }
    }

    fn provider_host_cache()
    -> &'static RwLock<HashMap<String, kameo::actor::RemoteActorRef<ProviderHostActor>>> {
        PROVIDER_HOST_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
    }

    fn stream_router_cache() -> &'static StreamRouters {
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
        self.mesh
            .best_route_for_peer(peer_id)
            .map(|route| route.scope)
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
        let expected_peer = provider_host_peer(target_locator)?;
        if let Some(cached) = Self::provider_host_cache()
            .read()
            .await
            .get(target_locator)
            .cloned()
        {
            require_provider_host_peer(expected_peer, cached.id().peer_id())?;
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
            })
            .and_then(|actor| {
                require_provider_host_peer(expected_peer, actor.id().peer_id())?;
                Ok(actor)
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

    async fn stream_control<M, R>(
        &self,
        host: &kameo::actor::RemoteActorRef<ProviderHostActor>,
        message: &M,
    ) -> Result<R, LLMError>
    where
        ProviderHostActor: kameo::remote::RemoteMessage<M> + kameo::message::Message<M, Reply = R>,
        M: serde::Serialize + Send + Sync + 'static,
        R: kameo::reply::Reply<Ok = R, Error = kameo::error::Infallible>
            + serde::de::DeserializeOwned,
    {
        let timeout = Self::PROVIDER_CONTROL_TIMEOUT;
        with_sharing_protocol(
            &self.mesh,
            host.id().peer_id().copied(),
            timeout,
            async {
                ask_sharing_v1(&self.mesh, host, message, timeout)
                    .await
                    .map_err(provider_ask_error)
            },
            async {
                crate::ask_remote_with_timeout(host, message, timeout)
                    .await
                    .map_err(remote_send_error_to_llm_error_no_handler)
            },
        )
        .await
        .map_err(sharing_error)?
    }

    async fn get_or_create_stream_router(
        &self,
        session_id: &str,
        host: PeerId,
    ) -> kameo::actor::ActorRef<ProviderStreamRouterActor> {
        let ingress = self.mesh.inbound_router();
        let key = (self.peer_id(), ingress.map(|_| host), session_id.to_owned());
        let mut routers = Self::stream_router_cache().write().await;
        routers
            .entry(key)
            .or_insert_with(|| {
                let router = ProviderStreamRouterActor::new(None, None);
                let prepared =
                    ProviderStreamRouterActor::prepare().reply_timeout(Self::STREAM_ROUTER_TIMEOUT);
                let actor = prepared.actor_ref().clone();
                if let Some(ingress) = ingress {
                    crate::provider_stream_router::register_peer_relay(
                        ingress,
                        actor.id(),
                        &actor,
                        host,
                    );
                }
                prepared.spawn(router);
                actor
            })
            .clone()
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

    async fn get_contract_info(
        &self,
        host: &Self::HostRef,
        request: GetProviderContractInfo,
    ) -> Result<ProviderContractInfo, LLMError> {
        ask_with_sharing(&self.mesh, host, &request, Self::PROVIDER_CONTROL_TIMEOUT)
            .await
            .map_err(provider_ask_error)
    }

    async fn prepare_stream_router(
        &self,
        host: &Self::HostRef,
        session_id: &str,
        request_id: &str,
        consumer_tx: mpsc::UnboundedSender<StreamRelayMessage>,
    ) -> Result<(Self::RouterRef, Self::RemoteRouterRef), LLMError> {
        let peer = host
            .id()
            .peer_id()
            .copied()
            .ok_or_else(|| sharing_error(SharingCallError::Unavailable))?;
        let router = self.get_or_create_stream_router(session_id, peer).await;
        router
            .ask(RegisterRequest {
                request_id: request_id.to_string(),
            })
            .mailbox_timeout(Self::STREAM_ROUTER_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                LLMError::ProviderError(format!("failed to register stream request: {}", e))
            })?;
        router
            .ask(AttachStreamConsumer {
                request_id: request_id.to_string(),
                consumer_tx,
            })
            .mailbox_timeout(Self::STREAM_ROUTER_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                LLMError::ProviderError(format!("failed to attach stream consumer: {}", e))
            })?;
        let remote = router.clone().into_remote_ref().await;
        Ok((router, remote))
    }

    async fn send_chat_request(
        &self,
        host: &Self::HostRef,
        request: &crate::ProviderChatRequest,
    ) -> Result<crate::ProviderChatResponse, LLMError> {
        let control_timeout = Self::PROVIDER_CONTROL_TIMEOUT;
        let reply_timeout = Self::PROVIDER_CHAT_REPLY_TIMEOUT;
        with_sharing_protocol(
            &self.mesh,
            host.id().peer_id().copied(),
            control_timeout,
            async {
                ask_sharing_v1(
                    &self.mesh,
                    host,
                    &MeteredChat {
                        request_id: uuid::Uuid::now_v7(),
                        request: request.clone(),
                    },
                    reply_timeout,
                )
                .await
                .map_err(provider_ask_error)
            },
            async {
                let encoded_bytes = crate::provider_transport::remote_ask_request_size(
                    host,
                    request,
                    Some(control_timeout),
                    Some(reply_timeout),
                )?;
                crate::provider_transport::ensure_mesh_request_fits(encoded_bytes)?;
                host.ask(request)
                    .mailbox_timeout(control_timeout)
                    .reply_timeout(reply_timeout)
                    .send()
                    .await
                    .map_err(|e| match crate::remote_send_error_base(e) {
                        Ok(err) => {
                            crate::provider_transport::remap_legacy_oversize_eof(err, encoded_bytes)
                        }
                        Err(handler) => LLMError::from_payload(handler.to_payload()),
                    })
            },
        )
        .await
        .map_err(sharing_error)?
    }

    async fn send_stream_request(
        &self,
        host: &Self::HostRef,
        request: GenericProviderStreamRequest<Self::RemoteRouterRef>,
    ) -> Result<(), LLMError> {
        let timeout = Self::PROVIDER_CONTROL_TIMEOUT;
        with_sharing_protocol(
            &self.mesh,
            host.id().peer_id().copied(),
            timeout,
            async {
                ask_sharing_v1(&self.mesh, host, &request, timeout)
                    .await
                    .map_err(provider_ask_error)
            },
            async {
                let encoded_bytes =
                    crate::provider_transport::remote_tell_request_size(host, &request, None)?;
                crate::provider_transport::ensure_mesh_request_fits(encoded_bytes)?;
                host.tell(&request).send_ack().await.map_err(|error| {
                    crate::provider_transport::remap_legacy_oversize_eof(
                        remote_send_error_to_llm_error_no_handler(error),
                        encoded_bytes,
                    )
                })
            },
        )
        .await
        .map_err(sharing_error)?
    }

    async fn cancel_stream(
        &self,
        host: &Self::HostRef,
        request: CancelProviderStreamRequest,
    ) -> Result<(), LLMError> {
        self.stream_control(host, &request).await.map(|_| ())
    }

    async fn renew_stream_lease(
        &self,
        host: &Self::HostRef,
        session_id: &str,
        request_id: &str,
        lease_ttl_secs: u64,
    ) -> Result<bool, LLMError> {
        let request = crate::RenewProviderStreamLease {
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
            lease_ttl_secs,
        };
        self.stream_control(host, &request).await
    }

    async fn get_stream_status(
        &self,
        host: &Self::HostRef,
        request: GetProviderStreamStatus,
    ) -> Result<Option<ProviderStreamStatus>, LLMError> {
        self.stream_control(host, &request).await
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

    /// Resolve the remote provider's capabilities and generation defaults before use.
    pub async fn negotiate_contract(&self) -> Result<crate::ProviderContractInfo, LLMError> {
        self.inner.core().negotiate_contract().await
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
    fn capabilities(&self) -> ChatCapabilities {
        self.inner.capabilities()
    }

    fn output_provenance(&self) -> Option<querymt::chat::ChatOutputProvenance> {
        self.inner.output_provenance()
    }

    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        self.inner.chat(call).await
    }

    async fn chat_stream(&self, call: &ChatCall<'_>) -> Result<ChatStream, LLMError> {
        self.inner.chat_stream(call).await
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
            let Ok(Some(catalog_ref)) = mesh
                .lookup_actor::<ProviderCatalogActor>(catalog_name)
                .await
            else {
                continue;
            };
            if !seen_catalog_peers.insert(peer_id) {
                continue;
            }

            peers_checked += 1;
            tracing::Span::current().record("peers_checked", peers_checked);

            let snapshot = match ask_with_sharing(
                mesh,
                &catalog_ref,
                &GetProviderCatalog,
                Duration::from_secs(10),
            )
            .await
            {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    log::debug!("find_provider_on_mesh: GetProviderCatalog failed: {}", e);
                    continue;
                }
            };

            if !snapshot
                .providers
                .iter()
                .any(|entry| entry.provider == provider_name)
            {
                continue;
            }

            if NodeId::parse(&snapshot.node.node_id).is_ok_and(|id| id.as_peer_id() == &peer_id) {
                candidates.push((peer_id, NodeId::from_peer_id(peer_id)));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_host_actor::tests::{RecordingBackend, init_local_actor_swarm};
    use crate::{MeshTransportKind, MeshTransportMode, RouteTable, StaticCatalogBackend};
    use querymt::chat::GenerationOptions;
    use tokio::sync::broadcast;

    fn mesh_fixture() -> (MeshHandle, Arc<RouteTable>) {
        init_local_actor_swarm();
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let routes = Arc::new(RouteTable::new(Duration::from_secs(60)));
        let mesh = MeshHandle::new(
            keypair.public().to_peer_id(),
            broadcast::channel(8).0,
            routes.clone(),
            "routing-fixture".into(),
            Default::default(),
            keypair,
            None,
            None,
            MeshTransportMode::Lan,
            mpsc::unbounded_channel().0,
            Duration::from_secs(5),
        );
        (mesh, routes)
    }

    #[tokio::test]
    async fn catalog_discovery_prefers_the_best_matching_route_without_a_node_manager() {
        let (mesh, routes) = mesh_fixture();
        let scope = MeshScopeId::lan_default();
        let preferred = PeerId::random();
        let fallback = PeerId::random();
        let mismatched = PeerId::random();
        let mut catalogs = Vec::new();
        for (peer, claimed_peer, priority) in [
            (preferred, preferred, 20),
            (fallback, fallback, 10),
            (mismatched, preferred, 30),
        ] {
            routes.upsert_addrs(
                peer,
                MeshTransportKind::Lan,
                scope.clone(),
                [libp2p::Multiaddr::empty()],
                priority,
            );
            let catalog = ProviderCatalogActor::spawn(ProviderCatalogActor::new(Arc::new(
                StaticCatalogBackend::provider_model(
                    claimed_peer.to_string(),
                    None,
                    "fixture",
                    "demo",
                ),
            )));
            mesh.register_actor(catalog.clone(), scoped_provider_catalog(&scope, &peer))
                .await;
            catalogs.push(catalog);
        }

        assert_eq!(
            find_provider_on_mesh(&mesh, "fixture").await,
            Some(NodeId::from_peer_id(preferred))
        );
        assert!(
            find_provider_on_mesh(&mesh, "never-advertised")
                .await
                .is_none()
        );
        drop(catalogs);
    }

    #[test]
    fn provider_host_selection_requires_the_locators_peer() {
        let selected = PeerId::random();
        let locator = scoped_provider_host(&MeshScopeId::lan_default(), &selected);
        let expected = provider_host_peer(&locator).unwrap();
        assert!(require_provider_host_peer(expected, Some(&selected)).is_ok());
        assert!(require_provider_host_peer(expected, Some(&PeerId::random())).is_err());
        assert!(require_provider_host_peer(expected, None).is_err());
    }

    #[tokio::test]
    async fn mesh_chat_uses_the_targets_scope_and_forwards_generation_options() {
        let (mesh, routes) = mesh_fixture();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let host = ProviderHostActor::spawn(ProviderHostActor::new(Arc::new(RecordingBackend {
            seen: seen.clone(),
        })));
        let target = NodeId::from_peer_id(*host.id().peer_id().unwrap());
        let scope = MeshScopeId::Iroh {
            mesh_id: "fixture-mesh".into(),
        };
        routes.upsert_addrs(
            *target.as_peer_id(),
            MeshTransportKind::Iroh,
            scope.clone(),
            [libp2p::Multiaddr::empty()],
            20,
        );
        let locator = scoped_provider_host(&scope, &target.to_string());
        mesh.register_actor(host.clone(), &locator).await;

        let provider = MeshChatProvider::from_node_id(&mesh, &target, "fixture", "demo")
            .with_stream_controls(7, 33);
        assert!(provider.capabilities().streaming);
        let options = GenerationOptions::NONE.with_max_tokens(123);
        provider
            .chat(&ChatCall::new(&[]).options(options))
            .await
            .expect("chat routed to the scoped fixture host");
        assert_eq!(seen.lock().as_slice(), &[options]);
        let core = provider.inner.core();
        assert_eq!(core.config().heartbeat_interval_secs, 7);
        assert_eq!(core.config().lease_ttl_secs, 33);
        assert!(
            !provider.capabilities().streaming,
            "the negotiated host only supports chat"
        );
        assert_eq!(provider.output_provenance(), None);
        assert!(
            provider
                .get_remote_stream_status("missing-session", None)
                .await
                .is_none()
        );
        assert_eq!(
            core.transport()
                .lookup_provider_host(&locator)
                .await
                .unwrap()
                .id(),
            host.id()
        );
        core.transport()
            .invalidate_cached_provider_host(&locator)
            .await;
        assert!(
            !KameoMeshClientTransport::provider_host_cache()
                .read()
                .await
                .contains_key(&locator)
        );
    }

    #[tokio::test]
    async fn unregistered_provider_host_error_names_the_lan_locator() {
        let (mesh, _) = mesh_fixture();
        let provider = MeshChatProvider::new(&mesh, "unregistered-fixture-host", "fixture", "demo");
        let error = provider.chat(&ChatCall::new(&[])).await.unwrap_err();
        assert!(
            matches!(error, LLMError::ProviderError(message) if message.contains("scope::lan::default::provider_host::peer::unregistered-fixture-host"))
        );
    }
}

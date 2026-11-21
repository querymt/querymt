use super::*;
use crate::mesh_ingress::{
    MeshIngress, ResponseSender, SharingCallError, SharingRejection, SharingRequest,
    SharingResponse,
};
use crate::mesh_runtime_support::SwarmCommand;
use crate::peer_access::quota::{
    AttemptUsage, QuotaAttempt, QuotaEdit, QuotaScope, QuotaSettlement, QuotaStatus, ReservationId,
};
use crate::peer_access::{
    GrantStore, GrantStoreError, ModelSelection, PeerGrantKey, ProviderGrant,
};
use crate::{
    CancelProviderStreamRequest, GetProviderStreamStatus, MeshHandle, MeshTransportMode, NodeId,
    ProviderStreamPhase, ProviderStreamRequest, ProviderStreamStatus, RenewProviderStreamLease,
    RoutedStreamRelayMessage, StaticCatalogBackend, StreamRelayMessage,
};
use async_trait::async_trait;
use kameo::actor::{ActorRef, Spawn};
use kameo::error::RemoteSendError;
use kameo::remote::{
    RemoteActor, RemoteMessage,
    messaging::{SwarmRequest, SwarmResponse},
};
use libp2p::PeerId;
use parking_lot::Mutex;
use querymt::chat::{ChatOutput, ChatProvider, ChatStream, FinishReason, StreamChunk};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::TransportErrorKind;
use querymt::{LLMProvider, Usage};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;

struct Store {
    grants: Mutex<HashMap<PeerGrantKey, PeerGrantState>>,
    attempts: Mutex<HashMap<(PeerGrantKey, Uuid), ReservationId>>,
    settled: Mutex<Vec<(ReservationId, AttemptUsage)>>,
    changes: broadcast::Sender<PeerGrantKey>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            grants: Default::default(),
            attempts: Default::default(),
            settled: Default::default(),
            changes: broadcast::channel(16).0,
        }
    }
}

#[async_trait]
impl GrantStore for Store {
    async fn grant(&self, key: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
        Ok(self.grants.lock().get(key).cloned().unwrap_or_default())
    }
    async fn replace_grant(
        &self,
        key: &PeerGrantKey,
        expected: Uuid,
        grant: PeerGrant,
    ) -> Result<PeerGrantState, GrantStoreError> {
        let mut grants = self.grants.lock();
        let state = grants.entry(key.clone()).or_default();
        if state.revision != expected {
            return Err(GrantStoreError::Conflict {
                expected,
                actual: state.revision,
            });
        }
        *state = PeerGrantState {
            grant,
            revision: Uuid::now_v7(),
        };
        let state = state.clone();
        drop(grants);
        let _ = self.changes.send(key.clone());
        Ok(state)
    }
    fn subscribe_changes(&self) -> Option<broadcast::Receiver<PeerGrantKey>> {
        Some(self.changes.subscribe())
    }
    async fn managed_peers(&self, _: &NodeId) -> Result<Vec<NodeId>, GrantStoreError> {
        unreachable!("not needed for provider requests")
    }
}

#[async_trait]
impl QuotaStore for Store {
    async fn quota(
        &self,
        _: &PeerGrantKey,
        _: &QuotaScope,
        _: i64,
    ) -> Result<QuotaStatus, QuotaError> {
        unreachable!("dispatch uses atomic admission, not a status poll")
    }
    async fn edit_quota(
        &self,
        _: &PeerGrantKey,
        _: Uuid,
        _: &QuotaScope,
        _: QuotaEdit,
        _: i64,
    ) -> Result<Uuid, QuotaError> {
        unreachable!("fixture has immutable caps")
    }
    async fn reserve(&self, attempt: QuotaAttempt) -> Result<ReservationId, QuotaError> {
        assert_eq!(
            self.grants.lock()[&attempt.peer].revision,
            attempt.grant_revision
        );
        assert_eq!(attempt.provider_id, "fixture");
        assert_eq!(attempt.model_id, "allowed");
        assert_eq!(attempt.bound_tokens, Some(25));
        use std::collections::hash_map::Entry;
        match self
            .attempts
            .lock()
            .entry((attempt.peer, attempt.request_id))
        {
            Entry::Occupied(_) => Err(QuotaError::DuplicateAttempt),
            Entry::Vacant(entry) => Ok(*entry.insert(ReservationId(Uuid::now_v7()))),
        }
    }
    async fn mark_dispatched(&self, id: ReservationId) -> Result<(), QuotaError> {
        assert!(self.attempts.lock().values().any(|attempt| *attempt == id));
        Ok(())
    }
    async fn release_unused(&self, _: ReservationId) -> Result<(), QuotaError> {
        unreachable!("preparation succeeds in this fixture")
    }
    async fn settle(
        &self,
        id: ReservationId,
        usage: AttemptUsage,
    ) -> Result<QuotaSettlement, QuotaError> {
        assert!(self.attempts.lock().values().any(|attempt| *attempt == id));
        self.settled.lock().push((id, usage));
        Ok(QuotaSettlement::Recorded)
    }
    async fn reconcile_startup(&self, _: &NodeId) -> Result<(), QuotaError> {
        unreachable!("no persisted state")
    }
}

#[derive(Clone, Copy, Default)]
enum StreamEnding {
    #[default]
    Done,
    Eof,
    Error,
}

struct BlockPreparation {
    started: tokio::sync::oneshot::Sender<()>,
    dropped: tokio::sync::oneshot::Sender<()>,
}

#[derive(Default)]
struct Backend {
    builds: Mutex<Vec<ProviderBuildRequest>>,
    block_preparation: Mutex<Option<BlockPreparation>>,
    calls: Arc<AtomicUsize>,
    finish_stream: Arc<tokio::sync::Notify>,
    stream_ending: StreamEnding,
}

#[async_trait]
impl RemoteProviderBackend for Backend {
    type Error = RemoteProviderHostError;

    fn host_default_params(&self, _: &str, _: &str) -> Option<Value> {
        Some(serde_json::json!({"base_url": "https://example.test/provider", "max_tokens": 20}))
    }

    fn quota_bound(&self, _: &ProviderBuildRequest, _: &ChatCall<'_>) -> Option<i64> {
        Some(25)
    }

    async fn build_provider(
        &self,
        request: ProviderBuildRequest,
    ) -> Result<Arc<dyn LLMProvider>, Self::Error> {
        self.builds.lock().push(request);
        let block = self.block_preparation.lock().take();
        if let Some(BlockPreparation { started, dropped }) = block {
            let _ = started.send(());
            std::future::pending::<()>().await;
            drop(dropped);
        }
        Ok(Arc::new(Provider {
            calls: self.calls.clone(),
            finish_stream: self.finish_stream.clone(),
            stream_ending: self.stream_ending,
        }))
    }
}

struct Provider {
    calls: Arc<AtomicUsize>,
    finish_stream: Arc<tokio::sync::Notify>,
    stream_ending: StreamEnding,
}

#[async_trait]
impl ChatProvider for Provider {
    async fn chat(&self, _: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatOutput {
            usage: Some(Usage {
                input_tokens: 8,
                output_tokens: 4,
                reasoning_tokens: 2,
                cache_read: 3,
                cache_write: 1,
                ..Usage::default()
            }),
            ..ChatOutput::default()
        })
    }

    async fn chat_stream(&self, _: &ChatCall<'_>) -> Result<ChatStream, LLMError> {
        use futures_util::{StreamExt, stream};
        self.calls.fetch_add(1, Ordering::Relaxed);
        let finish = self.finish_stream.clone();
        let initial = stream::iter([
            Ok(StreamChunk::Text("started".into())),
            Ok(StreamChunk::Usage(Usage {
                input_tokens: 8,
                ..Usage::default()
            })),
        ]);
        let ending = self.stream_ending;
        let complete = stream::once(async move {
            finish.notified().await;
            stream::iter(match ending {
                StreamEnding::Done => vec![
                    Ok(StreamChunk::Usage(Usage {
                        input_tokens: 8,
                        output_tokens: 4,
                        reasoning_tokens: 2,
                        cache_read: 3,
                        cache_write: 1,
                        ..Usage::default()
                    })),
                    Ok(StreamChunk::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ],
                StreamEnding::Eof => vec![],
                StreamEnding::Error => vec![Err(LLMError::Transport {
                    kind: TransportErrorKind::ConnectionClosed,
                    message: "fixture upstream error".into(),
                })],
            })
        })
        .flatten();
        Ok(Box::pin(initial.chain(complete)))
    }
}

#[async_trait]
impl CompletionProvider for Provider {
    async fn complete(&self, _: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        unreachable!("chat only")
    }
}

#[async_trait]
impl EmbeddingProvider for Provider {
    async fn embed(&self, _: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        unreachable!("chat only")
    }
}
impl LLMProvider for Provider {}

fn ask_request<A: RemoteActor + RemoteMessage<M>, M: Serialize>(
    actor_id: ActorId,
    request: &M,
) -> SwarmRequest {
    SwarmRequest::Ask {
        actor_id,
        actor_remote_id: <A as RemoteActor>::REMOTE_ID.into(),
        message_remote_id: <A as RemoteMessage<M>>::REMOTE_ID.into(),
        payload: rmp_serde::to_vec_named(request).unwrap(),
        mailbox_timeout: None,
        reply_timeout: None,
        immediate: false,
    }
}

async fn ask<
    A: RemoteActor + RemoteMessage<M>,
    M: Serialize,
    R: serde::de::DeserializeOwned,
    E: serde::de::DeserializeOwned,
>(
    router: &InboundRouter,
    caller: PeerId,
    actor_id: ActorId,
    request: &M,
) -> Result<R, E> {
    decode_ask(
        router
            .handle_v1(caller, ask_request::<A, _>(actor_id, request))
            .await
            .unwrap(),
    )
}

fn decode_ask<R: serde::de::DeserializeOwned, E: serde::de::DeserializeOwned>(
    response: SwarmResponse,
) -> Result<R, E> {
    match response {
        SwarmResponse::Ask(Ok(data)) => Ok(rmp_serde::from_slice(&data).unwrap()),
        SwarmResponse::Ask(Err(RemoteSendError::HandlerError(data))) => {
            Err(rmp_serde::from_slice(&data).unwrap())
        }
        other => panic!("unexpected route result: {other:?}"),
    }
}

fn code(error: RemoteProviderHostError) -> String {
    let LLMError::ProviderResponseError(failure) = LLMError::from_payload(error.to_payload())
    else {
        panic!("expected structured provider failure");
    };
    failure.code().unwrap().to_owned()
}

fn key(host: PeerId, peer: PeerId) -> PeerGrantKey {
    PeerGrantKey {
        host: NodeId::from_peer_id(host),
        peer: NodeId::from_peer_id(peer),
    }
}

fn allowed() -> PeerGrant {
    PeerGrant::Custom {
        sessions: false,
        filesystem: false,
        providers: vec![ProviderGrant {
            provider_id: "fixture".into(),
            models: ModelSelection::Selected {
                model_ids: vec!["allowed".into()],
            },
        }],
    }
}

#[tokio::test]
async fn registered_provider_share_gates_catalog_independently_of_discovery() {
    use crate::provider_share::ProviderShare;
    use crate::{
        LanDiscovery, LanMeshConfig, MeshRuntimeConfig, bootstrap_mesh_runtime_with_policy,
    };

    let cache = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::default());
    let runtime = bootstrap_mesh_runtime_with_policy(
        &MeshRuntimeConfig {
            enabled: true,
            lan: Some(LanMeshConfig {
                listen: Some("/ip4/127.0.0.1/tcp/0".into()),
                discovery: LanDiscovery::None,
                directory: Default::default(),
            }),
            identity_file: Some(cache.path().join("identity")),
            iroh_enabled: false,
            iroh_gso: false,
            iroh_dns: None,
            iroh_scopes: vec![],
            request_timeout: Duration::from_secs(3),
            stream_reconnect_grace: Duration::from_secs(3),
            node_name: Some("catalog-fixture".into()),
            peers: vec![],
            auto_fallback: false,
        },
        store.clone(),
    )
    .await
    .unwrap();
    let host = *runtime.peer_id();
    let peer = PeerId::random();
    let backend = Arc::new(Backend::default());
    let share = ProviderShare::new(
        backend,
        Arc::new(StaticCatalogBackend::provider_models(
            host.to_string(),
            None,
            "fixture",
            ["allowed".into(), "denied".into()],
        )),
    );
    let mesh = runtime.as_mesh_handle();
    share.register_on_mesh(&runtime).await;
    let router = mesh.inbound_router().unwrap();
    let catalog = share.provider_catalog().id();
    let read = || ask_request::<ProviderCatalogActor, _>(catalog, &GetProviderCatalog);
    assert!(matches!(
        router.handle_v1(peer, read()).await,
        Err(crate::mesh_ingress::SharingRejection::Denied)
    ));

    let binding = key(host, peer);
    let grant = store
        .replace_grant(&binding, Uuid::nil(), allowed())
        .await
        .unwrap();
    // Publishing the same share again must preserve its guarded routes and
    // must not make the legacy actor available to the Custom recipient.
    share.register_on_mesh(&runtime).await;
    let response = router.handle_v1(peer, read()).await.unwrap();
    let result = decode_ask::<ProviderCatalogSnapshot, String>(response).unwrap();
    assert_eq!(
        result
            .providers
            .iter()
            .filter_map(|entry| entry.model.as_deref())
            .collect::<Vec<_>>(),
        ["allowed"]
    );
    assert!(matches!(
        router.handle_legacy(peer, read()).await,
        SwarmResponse::Ask(Err(_))
    ));

    let mut chat = MeteredChat {
        request_id: Uuid::now_v7(),
        request: ProviderChatRequest {
            provider: "fixture".into(),
            model: "denied".into(),
            messages: vec![],
            tools: None,
            params: None,
            item_aware_contract_version: None,
            options: None,
        },
    };
    let provider = share.provider_host().id();
    let forbidden = ask::<ProviderHostActor, _, ProviderChatResponse, RemoteProviderHostError>(
        router, peer, provider, &chat,
    )
    .await
    .unwrap_err();
    assert_eq!(code(forbidden), "mesh_access_denied");
    assert!(store.attempts.lock().is_empty());
    chat.request.model = "allowed".into();
    let served = ask::<ProviderHostActor, _, ProviderChatResponse, RemoteProviderHostError>(
        router, peer, provider, &chat,
    )
    .await
    .unwrap();
    assert_eq!(served.output.usage.unwrap().input_tokens, 8);
    assert_eq!(store.attempts.lock().len(), 1);

    store
        .replace_grant(&binding, grant.revision, PeerGrant::None)
        .await
        .unwrap();
    for request in [read(), ask_request::<ProviderHostActor, _>(provider, &chat)] {
        assert!(matches!(
            router.handle_v1(peer, request).await,
            Err(crate::mesh_ingress::SharingRejection::Denied)
        ));
    }
    assert_eq!(store.attempts.lock().len(), 1);
    mesh.shutdown().await;
}

#[tokio::test]
async fn guarded_catalog_and_chat_enforce_grants_host_binding_and_attempt_identity() {
    let host = PeerId::random();
    let peer = PeerId::random();
    let catalog_id = ActorId::new_with_peer_id(1, host);
    let provider_id = ActorId::new_with_peer_id(2, host);
    let store = Arc::new(Store::default());
    store
        .replace_grant(&key(host, peer), Uuid::nil(), allowed())
        .await
        .unwrap();
    let backend = Arc::new(Backend::default());
    let catalog = Arc::new(StaticCatalogBackend::provider_models(
        host.to_string(),
        None,
        "fixture",
        ["allowed".into(), "denied".into()],
    ));
    let router = InboundRouter::new(host, store.clone());
    Arc::new(GuardedProvider {
        backend: backend.clone(),
        catalog,
        store: store.clone(),
    })
    .install(&router, catalog_id, provider_id);
    let snapshot = ask::<ProviderCatalogActor, _, ProviderCatalogSnapshot, String>(
        &router,
        peer,
        catalog_id,
        &GetProviderCatalog,
    )
    .await
    .unwrap();
    assert_eq!(
        snapshot
            .providers
            .iter()
            .filter_map(|entry| entry.model.as_deref())
            .collect::<Vec<_>>(),
        ["allowed"]
    );
    let mut request = MeteredChat {
        request_id: Uuid::now_v7(),
        request: ProviderChatRequest {
            provider: "fixture".into(),
            model: "denied".into(),
            messages: vec![],
            tools: None,
            params: Some(
                serde_json::json!({"base_url": "https://example.test/ignored", "temperature": 0.5}),
            ),
            item_aware_contract_version: None,
            options: None,
        },
    };
    let denied = ask::<ProviderHostActor, _, ProviderChatResponse, RemoteProviderHostError>(
        &router,
        peer,
        provider_id,
        &request,
    )
    .await
    .unwrap_err();
    assert_eq!(code(denied), "mesh_access_denied");
    assert!(backend.builds.lock().is_empty());
    request.request.model = "allowed".into();
    ask::<ProviderHostActor, _, ProviderChatResponse, RemoteProviderHostError>(
        &router,
        peer,
        provider_id,
        &request,
    )
    .await
    .unwrap();
    let duplicate = ask::<ProviderHostActor, _, ProviderChatResponse, RemoteProviderHostError>(
        &router,
        peer,
        provider_id,
        &request,
    )
    .await
    .unwrap_err();
    assert_eq!(code(duplicate), "mesh_quota_duplicate_attempt");
    assert_eq!(backend.calls.load(Ordering::Relaxed), 1);
    let id = store.attempts.lock()[&(key(host, peer), request.request_id)];
    assert_eq!(*store.settled.lock(), [(id, AttemptUsage::Observed(18))]);
    assert_eq!(
        backend.builds.lock()[0].params,
        Some(
            serde_json::json!({"base_url": "https://example.test/provider", "max_tokens": 20, "temperature": 0.5})
        )
    );
}

#[tokio::test]
async fn guarded_provider_streams_isolate_peer_controls_and_settle_revoked_work() {
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let host = keypair.public().to_peer_id();
    let peer = PeerId::random();
    let other = PeerId::random();
    let store = Arc::new(Store::default());
    let revision = store
        .replace_grant(&key(host, peer), Uuid::nil(), allowed())
        .await
        .unwrap()
        .revision;
    store
        .replace_grant(&key(host, other), Uuid::nil(), allowed())
        .await
        .unwrap();
    let backend = Arc::new(Backend::default());
    let ingress = InboundRouter::new(host, store.clone());
    let (command_tx, mut commands) = mpsc::unbounded_channel();
    let mesh = MeshHandle::new(
        host,
        broadcast::channel(8).0,
        Arc::new(crate::RouteTable::new(Duration::from_secs(60))),
        "test-host".into(),
        Default::default(),
        keypair,
        None,
        None,
        MeshTransportMode::Lan,
        command_tx,
        Duration::from_secs(10),
    );
    let provider_id = ActorId::new_with_peer_id(1, host);
    let host_actor = ProviderHostActor::spawn(ProviderHostActor::new(backend.clone()));
    let catalog = Arc::new(StaticCatalogBackend::provider_models(
        host.to_string(),
        None,
        "fixture",
        ["allowed".into()],
    ));
    streams::install(
        Arc::new(GuardedProvider {
            backend: backend.clone(),
            catalog,
            store: store.clone(),
        }),
        &mesh,
        &ingress,
        provider_id,
        &host_actor,
    );
    // Kameo deserializes the legacy callback envelope against its process-global
    // registry. V1 delivery itself goes through the normal sharing command path.
    init_callback_registry(host);
    let (relay_tx, mut relays) = mpsc::unbounded_channel();
    let driver = tokio::spawn(async move {
        while let Some(command) = commands.recv().await {
            let SwarmCommand::Sharing { peer, request, tx } = command else {
                panic!("unexpected command")
            };
            let SharingRequest::Actor(request) = request else {
                panic!("expected callback")
            };
            let SwarmRequest::Ask {
                actor_id, payload, ..
            } = *request
            else {
                panic!("expected ask")
            };
            assert_eq!(actor_id.peer_id(), Some(&peer));
            let message: RoutedStreamRelayMessage = rmp_serde::from_slice(&payload).unwrap();
            relay_tx.send((peer, message)).unwrap();
            let _ = tx.send(Ok(SharingResponse::Actor(Ok(SwarmResponse::Ask(Ok(
                rmp_serde::to_vec(&()).unwrap(),
            ))))));
        }
    });
    timeout(Duration::from_secs(5), async {
        let request_id = Uuid::now_v7();
        let mut request = ProviderStreamRequest {
            provider: "fixture".into(), model: "allowed".into(), messages: vec![], tools: None,
            session_id: "shared".into(), request_id: request_id.to_string(),
            stream_router_ref: Callback { id: ActorId::new_with_peer_id(2, other) },
            reconnect_grace_secs: 5, heartbeat_interval_secs: 60, lease_ttl_secs: 60,
            params: None, item_aware_contract_version: None, options: None,
        };
        let rejected = ask::<ProviderHostActor, _, (), RemoteProviderHostError>(&ingress, peer, provider_id, &request).await.unwrap_err();
        assert_eq!(code(rejected), "mesh_access_denied");
        assert!(store.attempts.lock().is_empty());
        request.stream_router_ref.id = ActorId::new_with_peer_id(2, peer);
        ask::<ProviderHostActor, _, (), RemoteProviderHostError>(&ingress, peer, provider_id, &request).await.unwrap();
        let status = GetProviderStreamStatus { session_id: request.session_id.clone(), request_id: Some(request.request_id.clone()) };
        assert!(ask::<ProviderHostActor, _, Option<ProviderStreamStatus>, RemoteProviderHostError>(&ingress, other, provider_id, &status).await.unwrap().is_none());
        let renew = RenewProviderStreamLease { session_id: request.session_id.clone(), request_id: request.request_id.clone(), lease_ttl_secs: 60 };
        assert!(!ask::<ProviderHostActor, _, bool, RemoteProviderHostError>(&ingress, other, provider_id, &renew).await.unwrap());
        let cancel = CancelProviderStreamRequest { session_id: request.session_id.clone(), request_id: None, reason: None };
        assert_eq!(ask::<ProviderHostActor, _, usize, RemoteProviderHostError>(&ingress, other, provider_id, &cancel).await.unwrap(), 0);
        assert!(ask::<ProviderHostActor, _, bool, RemoteProviderHostError>(&ingress, peer, provider_id, &renew).await.unwrap());
        assert_eq!(relays.recv().await.unwrap().0, peer);

        // Identical claimed session/request IDs are independent for different peers.
        request.stream_router_ref.id = ActorId::new_with_peer_id(2, other);
        ask::<ProviderHostActor, _, (), RemoteProviderHostError>(&ingress, other, provider_id, &request).await.unwrap();
        assert_eq!(relays.recv().await.unwrap().0, other);
        store.replace_grant(&key(host, peer), revision, PeerGrant::None).await.unwrap();
        let (recipient, rejected) = relays.recv().await.unwrap();
        assert_eq!(recipient, peer);
        let StreamRelayMessage::ProviderError { error } = rejected.message else {
            panic!("expected revocation error");
        };
        assert!(matches!(LLMError::from_payload(error), LLMError::ProviderResponseError(failure) if failure.code() == Some("mesh_access_denied")));
        assert!(rejected.seq.is_none());
        assert!(ask::<ProviderHostActor, _, Option<ProviderStreamStatus>, RemoteProviderHostError>(&ingress, other, provider_id, &status).await.unwrap().is_some());
        backend.finish_stream.notify_one();
        let (recipient, terminal) = relays.recv().await.unwrap();
        assert_eq!(recipient, other);
        assert!(crate::relay_message_is_terminal(&terminal.message));
        assert_eq!(backend.calls.load(Ordering::Relaxed), 2);
        let attempts = store.attempts.lock();
        assert_eq!(*store.settled.lock(), [
            (attempts[&(key(host, peer), request_id)], AttemptUsage::Unknown),
            (attempts[&(key(host, other), request_id)], AttemptUsage::Observed(18)),
        ]);
    }).await.expect("guarded stream test timed out");
    driver.abort();
    assert!(driver.await.unwrap_err().is_cancelled());
}

#[derive(Clone, Serialize)]
struct Callback {
    id: ActorId,
}

impl RemoteMessage<ProviderStreamRequest<Callback>> for ProviderHostActor {
    const REMOTE_ID: &'static str = <Self as RemoteMessage<
        ProviderStreamRequest<kameo::actor::RemoteActorRef<crate::ProviderStreamRouterActor>>,
    >>::REMOTE_ID;
}

struct StreamFixture {
    host: PeerId,
    peer: PeerId,
    actor: ActorId,
    host_actor: ActorRef<ProviderHostActor>,
    router: InboundRouter,
    store: Arc<Store>,
    backend: Arc<Backend>,
    commands: mpsc::UnboundedReceiver<SwarmCommand>,
}

impl StreamFixture {
    async fn new(ending: StreamEnding) -> Self {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let host = keypair.public().to_peer_id();
        let peer = PeerId::random();
        let store = Arc::new(Store::default());
        store
            .replace_grant(&key(host, peer), Uuid::nil(), PeerGrant::Full)
            .await
            .unwrap();
        let backend = Arc::new(Backend {
            stream_ending: ending,
            ..Backend::default()
        });
        let router = InboundRouter::new(host, store.clone());
        let (command_tx, commands) = mpsc::unbounded_channel();
        let mesh = MeshHandle::new(
            host,
            broadcast::channel(8).0,
            Arc::new(crate::RouteTable::new(Duration::from_secs(60))),
            "test-host".into(),
            Default::default(),
            keypair,
            None,
            None,
            MeshTransportMode::Lan,
            command_tx,
            Duration::from_secs(10),
        );
        let actor = ActorId::new_with_peer_id(1, host);
        let host_actor = ProviderHostActor::spawn(ProviderHostActor::new(backend.clone()));
        streams::install(
            Arc::new(GuardedProvider {
                backend: backend.clone(),
                catalog: Arc::new(StaticCatalogBackend::provider_model(
                    host.to_string(),
                    None,
                    "fixture",
                    "allowed",
                )),
                store: store.clone(),
            }),
            &mesh,
            &router,
            actor,
            &host_actor,
        );
        init_callback_registry(host);
        Self {
            host,
            peer,
            actor,
            host_actor,
            router,
            store,
            backend,
            commands,
        }
    }

    fn request(&self) -> ProviderStreamRequest<Callback> {
        ProviderStreamRequest {
            provider: "fixture".into(),
            model: "allowed".into(),
            messages: vec![],
            tools: None,
            session_id: "fixture-session".into(),
            request_id: Uuid::now_v7().to_string(),
            stream_router_ref: Callback {
                id: ActorId::new_with_peer_id(2, self.peer),
            },
            reconnect_grace_secs: 1,
            heartbeat_interval_secs: 60,
            lease_ttl_secs: 60,
            params: None,
            item_aware_contract_version: None,
            options: None,
        }
    }

    async fn call<M: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        request: &M,
    ) -> Result<R, RemoteProviderHostError>
    where
        ProviderHostActor: RemoteMessage<M>,
    {
        ask::<ProviderHostActor, _, _, _>(&self.router, self.peer, self.actor, request).await
    }

    async fn legacy<M: Serialize, R: serde::de::DeserializeOwned>(
        &self,
        request: &M,
    ) -> Result<R, RemoteProviderHostError>
    where
        ProviderHostActor: RemoteMessage<M>,
    {
        decode_ask(
            self.router
                .handle_legacy(
                    self.peer,
                    ask_request::<ProviderHostActor, _>(self.actor, request),
                )
                .await,
        )
    }

    async fn relay(
        &mut self,
        request: &ProviderStreamRequest<Callback>,
    ) -> (RoutedStreamRelayMessage, ResponseSender) {
        let command = timeout(Duration::from_secs(4), self.commands.recv())
            .await
            .expect("timed out waiting for callback")
            .expect("egress closed");
        let SwarmCommand::Sharing {
            peer,
            request: SharingRequest::Actor(envelope),
            tx,
        } = command
        else {
            panic!("expected a sharing callback");
        };
        let SwarmRequest::Ask {
            actor_id, payload, ..
        } = *envelope
        else {
            panic!("expected callback ask");
        };
        assert_eq!(peer, self.peer);
        assert_eq!(actor_id, request.stream_router_ref.id);
        let relay: RoutedStreamRelayMessage = rmp_serde::from_slice(&payload).unwrap();
        assert_eq!(relay.request_id, request.request_id);
        (relay, tx)
    }

    fn assert_settled(&self, request: &ProviderStreamRequest<Callback>, usage: AttemptUsage) {
        assert_eq!(self.backend.calls.load(Ordering::Relaxed), 1);
        let id = self.store.attempts.lock()[&(
            key(self.host, self.peer),
            request.request_id.parse().unwrap(),
        )];
        assert_eq!(*self.store.settled.lock(), [(id, usage)]);
    }
}

fn init_callback_registry(host: PeerId) {
    // The callback envelope needs Kameo's process-wide registry, but V1 uses
    // mesh commands for egress. An already initialized registry works too.
    let _ = kameo::remote::Behaviour::new(
        host,
        crate::mesh_runtime::messaging_config(Duration::from_secs(3)),
    )
    .try_init_global();
}

fn ack(reply: ResponseSender) {
    reply
        .send(Ok(SharingResponse::Actor(Ok(SwarmResponse::Ask(Ok(
            rmp_serde::to_vec(&()).unwrap(),
        ))))))
        .unwrap();
}

fn disconnect(reply: ResponseSender) {
    reply.send(Err(SharingCallError::Unavailable)).unwrap();
}

fn assert_transport(
    relay: RoutedStreamRelayMessage,
    expected_kind: TransportErrorKind,
    expected_message: &str,
) {
    assert_eq!(relay.seq, None);
    let StreamRelayMessage::ProviderError { error } = relay.message else {
        panic!("expected terminal provider error, got {:?}", relay.message);
    };
    let LLMError::Transport { kind, message } = LLMError::from_payload(error) else {
        panic!("expected transport failure");
    };
    assert_eq!(kind, expected_kind);
    assert!(message.contains(expected_message), "{message}");
}

fn stream_status(request: &ProviderStreamRequest<Callback>) -> GetProviderStreamStatus {
    GetProviderStreamStatus {
        session_id: request.session_id.clone(),
        request_id: Some(request.request_id.clone()),
    }
}

#[tokio::test]
async fn guarded_stream_lifecycle_operations_are_inert_except_for_the_owning_control() {
    let mut fixture = StreamFixture::new(StreamEnding::Done).await;
    let request = fixture.request();
    fixture.call::<_, ()>(&request).await.unwrap();
    let (initial, reply) = fixture.relay(&request).await;
    assert_eq!(initial.seq, Some(0));
    ack(reply);

    let link = || SwarmRequest::Link {
        actor_id: fixture.actor,
        actor_remote_id: <ProviderHostActor as RemoteActor>::REMOTE_ID.into(),
        sibling_id: request.stream_router_ref.id,
        sibling_remote_id: <crate::ProviderStreamRouterActor as RemoteActor>::REMOTE_ID.into(),
    };
    assert!(matches!(
        fixture.router.handle_legacy(fixture.peer, link()).await,
        SwarmResponse::Link(Err(RemoteSendError::BadActorType))
    ));
    assert!(matches!(
        fixture.router.handle_v1(fixture.peer, link()).await,
        Err(SharingRejection::UnsupportedOperation)
    ));

    let status = stream_status(&request);
    let renew = RenewProviderStreamLease {
        session_id: request.session_id.clone(),
        request_id: request.request_id.clone(),
        lease_ttl_secs: 1,
    };
    let mut cancel = CancelProviderStreamRequest {
        session_id: request.session_id.clone(),
        request_id: Some(Uuid::now_v7().to_string()),
        reason: None,
    };
    assert_eq!(fixture.call::<_, usize>(&cancel).await.unwrap(), 0);
    cancel.request_id = Some(request.request_id.clone());
    // A full legacy grant still cannot control a V1 request of the same peer.
    assert!(
        fixture
            .legacy::<_, Option<ProviderStreamStatus>>(&status)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!fixture.legacy::<_, bool>(&renew).await.unwrap());
    assert_eq!(fixture.legacy::<_, usize>(&cancel).await.unwrap(), 0);
    // The local actor API also cannot see or mutate peer-owned streams.
    assert!(
        fixture
            .host_actor
            .ask(status.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!fixture.host_actor.ask(renew.clone()).await.unwrap());
    assert_eq!(fixture.host_actor.ask(cancel.clone()).await.unwrap(), 0);
    let live = fixture
        .call::<_, Option<ProviderStreamStatus>>(&status)
        .await
        .unwrap()
        .unwrap();
    assert!(live.receiver_connected);
    assert!(live.lease_expires_in_ms > 1_000);
    assert!(fixture.store.settled.lock().is_empty());

    assert_eq!(fixture.call::<_, usize>(&cancel).await.unwrap(), 1);
    assert!(!fixture.call::<_, bool>(&renew).await.unwrap());
    let (terminal, reply) = fixture.relay(&request).await;
    assert_transport(
        terminal,
        TransportErrorKind::ConnectionClosed,
        "provider stream cancelled",
    );
    fixture.assert_settled(&request, AttemptUsage::Unknown);
    assert!(
        fixture
            .call::<_, Option<ProviderStreamStatus>>(&status)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.call::<_, usize>(&cancel).await.unwrap(), 0);
    ack(reply);
}

#[tokio::test]
async fn dropping_guarded_admission_cancels_an_in_progress_prepare() {
    let fixture = StreamFixture::new(StreamEnding::Done).await;
    let request = fixture.request();
    let (started, prepare_started) = tokio::sync::oneshot::channel();
    let (dropped, prepare_dropped) = tokio::sync::oneshot::channel();
    *fixture.backend.block_preparation.lock() = Some(BlockPreparation { started, dropped });
    let mut call = Box::pin(fixture.call::<_, ()>(&request));
    timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut call => panic!("preparation unexpectedly returned: {result:?}"),
            result = prepare_started => result.unwrap(),
        }
        drop(call);
        prepare_dropped.await.unwrap_err();
    })
    .await
    .expect("dropping ingress must cancel preparation");
    assert!(fixture.store.attempts.lock().is_empty());
    assert_eq!(fixture.backend.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn stopping_host_cancels_and_settles_a_guarded_stream_with_egress_blocked() {
    let mut fixture = StreamFixture::new(StreamEnding::Done).await;
    let request = fixture.request();
    fixture.call::<_, ()>(&request).await.unwrap();
    let (initial, held_reply) = fixture.relay(&request).await;
    assert_eq!(initial.seq, Some(0));

    fixture.host_actor.stop_gracefully().await.unwrap();
    let (terminal, reply) = fixture.relay(&request).await;
    assert_transport(
        terminal,
        TransportErrorKind::ConnectionClosed,
        "provider stream cancelled",
    );
    fixture.assert_settled(&request, AttemptUsage::Unknown);
    ack(reply);
    drop(held_reply);
}

#[tokio::test]
async fn guarded_stream_reconnect_retries_data_and_terminal_without_restarting_inference() {
    let mut fixture = StreamFixture::new(StreamEnding::Done).await;
    let request = fixture.request();
    fixture.call::<_, ()>(&request).await.unwrap();
    let (first, reply) = fixture.relay(&request).await;
    assert_eq!(first.seq, Some(0));
    disconnect(reply);
    let (notice, reply) = fixture.relay(&request).await;
    assert!(matches!(
        notice.message,
        StreamRelayMessage::TransportReconnected { buffered_chunks: 1 }
    ));
    assert_eq!(notice.seq, None);
    let live = fixture
        .call::<_, Option<ProviderStreamStatus>>(&stream_status(&request))
        .await
        .unwrap()
        .unwrap();
    assert!(!live.receiver_connected);
    assert_eq!(live.chunk_count, 0);
    ack(reply);
    let (retry, reply) = fixture.relay(&request).await;
    assert_eq!(
        rmp_serde::to_vec_named(&retry).unwrap(),
        rmp_serde::to_vec_named(&first).unwrap()
    );
    ack(reply);

    fixture.backend.finish_stream.notify_one();
    let (terminal, reply) = fixture.relay(&request).await;
    assert_eq!(terminal.seq, Some(1));
    assert!(crate::relay_message_is_terminal(&terminal.message));
    fixture.assert_settled(&request, AttemptUsage::Observed(18));
    disconnect(reply);
    let (notice, reply) = fixture.relay(&request).await;
    assert!(matches!(
        notice.message,
        StreamRelayMessage::TransportReconnected { buffered_chunks: 1 }
    ));
    assert_eq!(notice.seq, None);
    assert!(
        fixture
            .call::<_, Option<ProviderStreamStatus>>(&stream_status(&request))
            .await
            .unwrap()
            .is_none()
    );
    ack(reply);
    let (retry, reply) = fixture.relay(&request).await;
    assert_eq!(
        rmp_serde::to_vec_named(&retry).unwrap(),
        rmp_serde::to_vec_named(&terminal).unwrap()
    );
    fixture.assert_settled(&request, AttemptUsage::Observed(18));
    ack(reply);
}

#[tokio::test]
async fn guarded_stream_grace_expiration_settles_before_delivering_scoped_timeout() {
    let mut fixture = StreamFixture::new(StreamEnding::Done).await;
    let request = fixture.request();
    fixture.call::<_, ()>(&request).await.unwrap();
    let (initial, reply) = fixture.relay(&request).await;
    assert_eq!(initial.seq, Some(0));
    disconnect(reply);

    timeout(Duration::from_secs(5), async {
        loop {
            let (notice, reply) = fixture.relay(&request).await;
            let settled = !fixture.store.settled.lock().is_empty();
            assert_eq!(notice.seq, None);
            assert!(matches!(notice.message, StreamRelayMessage::TransportReconnected { buffered_chunks }
                if buffered_chunks == usize::from(!settled)));
            if settled {
                ack(reply);
                break;
            }
            disconnect(reply);
        }
    }).await.expect("reconnect grace did not expire");
    let (terminal, reply) = fixture.relay(&request).await;
    assert_transport(
        terminal,
        TransportErrorKind::Timeout,
        "reconnect grace expired",
    );
    fixture.assert_settled(&request, AttemptUsage::Unknown);
    assert!(
        fixture
            .call::<_, Option<ProviderStreamStatus>>(&stream_status(&request))
            .await
            .unwrap()
            .is_none()
    );
    ack(reply);
}

#[tokio::test]
async fn guarded_stream_heartbeat_does_not_renew_the_lease() {
    let mut fixture = StreamFixture::new(StreamEnding::Done).await;
    let mut request = fixture.request();
    request.heartbeat_interval_secs = 1;
    request.lease_ttl_secs = 2;
    fixture.call::<_, ()>(&request).await.unwrap();
    let (initial, reply) = fixture.relay(&request).await;
    assert_eq!(initial.seq, Some(0));
    ack(reply);
    let (heartbeat, reply) = fixture.relay(&request).await;
    assert_eq!(heartbeat.seq, None);
    assert!(matches!(
        heartbeat.message,
        StreamRelayMessage::Heartbeat {
            phase: ProviderStreamPhase::Streaming,
            chunk_count: 2,
            ..
        }
    ));
    ack(reply);
    timeout(Duration::from_secs(5), async {
        loop {
            let (terminal, reply) = fixture.relay(&request).await;
            if matches!(terminal.message, StreamRelayMessage::Heartbeat { .. }) {
                ack(reply);
                continue;
            }
            assert_transport(
                terminal,
                TransportErrorKind::Timeout,
                "lease or deadline expired",
            );
            fixture.assert_settled(&request, AttemptUsage::Unknown);
            ack(reply);
            break;
        }
    })
    .await
    .expect("stream lease did not expire");
}

#[tokio::test]
async fn guarded_stream_reports_upstream_error_and_missing_terminal() {
    for (ending, message) in [
        (StreamEnding::Eof, "provider ended without a terminal chunk"),
        (StreamEnding::Error, "fixture upstream error"),
    ] {
        let mut fixture = StreamFixture::new(ending).await;
        let request = fixture.request();
        fixture.call::<_, ()>(&request).await.unwrap();
        let (initial, reply) = fixture.relay(&request).await;
        assert_eq!(initial.seq, Some(0));
        assert!(!crate::relay_message_is_terminal(&initial.message));
        ack(reply);
        fixture.backend.finish_stream.notify_one();
        let (terminal, reply) = fixture.relay(&request).await;
        assert_transport(terminal, TransportErrorKind::ConnectionClosed, message);
        fixture.assert_settled(&request, AttemptUsage::Unknown);
        ack(reply);
    }
}

#[tokio::test]
async fn guarded_stream_rejects_invalid_id_and_unlisted_model_before_reserving() {
    let fixture = StreamFixture::new(StreamEnding::Done).await;
    let mut request = fixture.request();
    let valid_id = std::mem::replace(&mut request.request_id, "not-a-uuid".into());
    assert_eq!(
        code(fixture.call::<_, ()>(&request).await.unwrap_err()),
        "mesh_stream_id"
    );
    request.request_id = valid_id;
    request.model = "unlisted".into();
    assert_eq!(
        code(fixture.call::<_, ()>(&request).await.unwrap_err()),
        "mesh_access_denied"
    );
    assert!(fixture.backend.builds.lock().is_empty());
    assert!(fixture.store.attempts.lock().is_empty());
    assert_eq!(fixture.backend.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn guarded_provider_contract_requires_eligibility_and_valid_inference_options() {
    let host = PeerId::random();
    let custom = PeerId::random();
    let full = PeerId::random();
    let catalog_id = ActorId::new_with_peer_id(1, host);
    let actor = ActorId::new_with_peer_id(2, host);
    let store = Arc::new(Store::default());
    for (peer, grant) in [(custom, allowed()), (full, PeerGrant::Full)] {
        store
            .replace_grant(&key(host, peer), Uuid::nil(), grant)
            .await
            .unwrap();
    }
    let backend = Arc::new(Backend::default());
    let router = InboundRouter::new(host, store.clone());
    Arc::new(GuardedProvider {
        backend: backend.clone(),
        catalog: Arc::new(StaticCatalogBackend::provider(
            host.to_string(),
            None,
            "fixture",
        )),
        store: store.clone(),
    })
    .install(&router, catalog_id, actor);

    let browse_catalog = |peer| {
        ask::<ProviderCatalogActor, _, ProviderCatalogSnapshot, String>(
            &router,
            peer,
            catalog_id,
            &GetProviderCatalog,
        )
    };
    assert!(browse_catalog(custom).await.unwrap().providers.is_empty());
    let snapshot = browse_catalog(full).await.unwrap();
    assert!(snapshot.node.capabilities.is_empty());
    assert_eq!(snapshot.providers.len(), 1);
    assert_eq!(snapshot.providers[0].provider, "fixture");
    assert_eq!(snapshot.providers[0].model, None);

    let mut request = GetProviderContractInfo {
        provider: "fixture".into(),
        model: "allowed".into(),
        params: None,
    };
    assert_eq!(
        code(
            ask::<ProviderHostActor, _, ProviderContractInfo, RemoteProviderHostError>(
                &router, custom, actor, &request,
            )
            .await
            .unwrap_err()
        ),
        "mesh_access_denied"
    );
    request.params = Some(serde_json::json!({"temperature": "not a number"}));
    assert_eq!(
        code(
            ask::<ProviderHostActor, _, ProviderContractInfo, RemoteProviderHostError>(
                &router, full, actor, &request,
            )
            .await
            .unwrap_err()
        ),
        "mesh_access_invalid_params"
    );
    assert!(backend.builds.lock().is_empty());

    request.params = Some(serde_json::json!({"temperature": 0.25}));
    let contract = ask::<ProviderHostActor, _, ProviderContractInfo, RemoteProviderHostError>(
        &router, full, actor, &request,
    )
    .await
    .unwrap();
    assert_eq!(
        contract.protocol_version,
        Some(crate::provider_protocol::MESH_PROTOCOL_VERSION)
    );
    assert_eq!(
        contract.item_aware_chat_version,
        Some(crate::ITEM_AWARE_CHAT_CONTRACT_VERSION)
    );
    assert_eq!(
        backend.builds.lock()[0].params,
        Some(
            serde_json::json!({"base_url": "https://example.test/provider", "max_tokens": 20, "temperature": 0.25})
        )
    );
    assert_eq!(backend.calls.load(Ordering::Relaxed), 0);
    assert!(store.attempts.lock().is_empty());

    let chat = ProviderChatRequest {
        provider: request.provider,
        model: request.model,
        messages: vec![],
        tools: None,
        params: None,
        item_aware_contract_version: None,
        options: None,
    };
    let response = router
        .handle_legacy(full, ask_request::<ProviderHostActor, _>(actor, &chat))
        .await;
    let response = decode_ask::<ProviderChatResponse, RemoteProviderHostError>(response).unwrap();
    assert_eq!(response.output.usage.unwrap().input_tokens, 8);
    assert_eq!(backend.calls.load(Ordering::Relaxed), 1);
    let attempts = store.attempts.lock();
    assert_eq!(attempts.len(), 1);
    let ((peer, request_id), reservation) = attempts.iter().next().unwrap();
    assert_eq!(*peer, key(host, full));
    assert!(!request_id.is_nil());
    assert_eq!(
        *store.settled.lock(),
        [(*reservation, AttemptUsage::Observed(18))]
    );
}

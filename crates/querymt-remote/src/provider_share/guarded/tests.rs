use super::*;
use crate::mesh_ingress::MeshIngress;
use crate::peer_access::quota::{QuotaEdit, QuotaScope, QuotaStatus, ReservationId};
use crate::peer_access::{
    GrantStore, GrantStoreError, ModelSelection, PeerGrantKey, ProviderGrant,
};
use crate::{NodeId, StaticCatalogBackend};
use async_trait::async_trait;
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
use querymt::{LLMProvider, Usage};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Store {
    grants: Mutex<HashMap<PeerGrantKey, PeerGrantState>>,
    attempts: Mutex<HashMap<(PeerGrantKey, Uuid), ReservationId>>,
    settled: Mutex<Vec<(ReservationId, AttemptUsage)>>,
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
        Ok(state.clone())
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
        assert!(self.grants.lock().contains_key(&attempt.peer));
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

#[derive(Default)]
struct Backend {
    builds: Mutex<Vec<ProviderBuildRequest>>,
    calls: Arc<AtomicUsize>,
    finish_stream: Arc<tokio::sync::Notify>,
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
        Ok(Arc::new(Provider {
            calls: self.calls.clone(),
            finish_stream: self.finish_stream.clone(),
        }))
    }
}

struct Provider {
    calls: Arc<AtomicUsize>,
    finish_stream: Arc<tokio::sync::Notify>,
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
        let complete = stream::once(async move {
            finish.notified().await;
            Ok(StreamChunk::Usage(Usage {
                input_tokens: 8,
                output_tokens: 4,
                reasoning_tokens: 2,
                cache_read: 3,
                cache_write: 1,
                ..Usage::default()
            }))
        })
        .chain(stream::iter([Ok(StreamChunk::Done {
            finish_reason: FinishReason::Stop,
        })]));
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
    let result = router
        .handle_v1(
            caller,
            SwarmRequest::Ask {
                actor_id,
                actor_remote_id: <A as RemoteActor>::REMOTE_ID.into(),
                message_remote_id: <A as RemoteMessage<M>>::REMOTE_ID.into(),
                payload: rmp_serde::to_vec_named(request).unwrap(),
                mailbox_timeout: None,
                reply_timeout: None,
                immediate: false,
            },
        )
        .await
        .unwrap();
    match result {
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
    use crate::mesh_ingress::{SharingRequest, SharingResponse};
    use crate::mesh_runtime_support::SwarmCommand;
    use crate::{
        CancelProviderStreamRequest, GetProviderStreamStatus, MeshHandle, MeshTransportMode,
        ProviderStreamRequest, ProviderStreamStatus, RenewProviderStreamLease,
        RoutedStreamRelayMessage, StreamRelayMessage,
    };
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc};
    use tokio::time::timeout;

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
    );
    // Kameo deserializes the legacy callback envelope against its process-global
    // registry. V1 delivery itself goes through the normal sharing command path.
    kameo::remote::Behaviour::new(
        host,
        crate::mesh_runtime::messaging_config(Duration::from_secs(3)),
    )
    .init_global();
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
        #[derive(Clone, Serialize)]
        struct Callback { id: ActorId }
        impl RemoteMessage<ProviderStreamRequest<Callback>> for ProviderHostActor {
            const REMOTE_ID: &'static str = <Self as RemoteMessage<ProviderStreamRequest<kameo::actor::RemoteActorRef<crate::ProviderStreamRouterActor>>>>::REMOTE_ID;
        }
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

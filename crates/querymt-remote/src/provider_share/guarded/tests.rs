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
use querymt::chat::{ChatOutput, ChatProvider};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::{LLMProvider, Usage};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Store {
    key: PeerGrantKey,
    grant: PeerGrantState,
    attempts: Mutex<HashSet<Uuid>>,
    settled: Mutex<Vec<AttemptUsage>>,
}

#[async_trait]
impl GrantStore for Store {
    async fn grant(&self, key: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
        Ok(if self.key == *key {
            self.grant.clone()
        } else {
            PeerGrantState::default()
        })
    }
    async fn replace_grant(
        &self,
        _: &PeerGrantKey,
        _: Uuid,
        _: PeerGrant,
    ) -> Result<PeerGrantState, GrantStoreError> {
        unreachable!("fixture has immutable grants")
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
        assert_eq!(attempt.peer, self.key);
        assert_eq!(attempt.provider_id, "fixture");
        assert_eq!(attempt.model_id, "allowed");
        assert_eq!(attempt.bound_tokens, Some(25));
        if !self.attempts.lock().insert(attempt.request_id) {
            return Err(QuotaError::DuplicateAttempt);
        }
        Ok(ReservationId(attempt.request_id))
    }
    async fn mark_dispatched(&self, id: ReservationId) -> Result<(), QuotaError> {
        assert!(self.attempts.lock().contains(&id.0));
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
        assert!(self.attempts.lock().contains(&id.0));
        self.settled.lock().push(usage);
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
        Ok(Arc::new(Provider(self.calls.clone())))
    }
}

struct Provider(Arc<AtomicUsize>);

#[async_trait]
impl ChatProvider for Provider {
    async fn chat(&self, _: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        self.0.fetch_add(1, Ordering::Relaxed);
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

#[tokio::test]
async fn guarded_catalog_and_chat_enforce_grants_host_binding_and_attempt_identity() {
    let host = PeerId::random();
    let peer = PeerId::random();
    let catalog_id = ActorId::new_with_peer_id(1, host);
    let provider_id = ActorId::new_with_peer_id(2, host);
    let store = Arc::new(Store {
        key: PeerGrantKey {
            host: NodeId::from_peer_id(host),
            peer: NodeId::from_peer_id(peer),
        },
        grant: PeerGrantState {
            revision: Uuid::now_v7(),
            grant: PeerGrant::Custom {
                sessions: false,
                filesystem: false,
                providers: vec![ProviderGrant {
                    provider_id: "fixture".into(),
                    models: ModelSelection::Selected {
                        model_ids: vec!["allowed".into()],
                    },
                }],
            },
        },
        attempts: Mutex::default(),
        settled: Mutex::default(),
    });
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
    assert_eq!(*store.settled.lock(), [AttemptUsage::Observed(18)]);
    assert_eq!(
        backend.builds.lock()[0].params,
        Some(
            serde_json::json!({"base_url": "https://example.test/provider", "max_tokens": 20, "temperature": 0.5})
        )
    );
}

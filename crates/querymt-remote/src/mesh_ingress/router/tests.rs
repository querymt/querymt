use super::*;
use crate::peer_access::{GrantStoreError, PeerGrantState, PolicyChange, PolicyEffect};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tokio::sync::broadcast;
use uuid::Uuid;

struct TestStore {
    entries: RwLock<HashMap<PeerGrantKey, PeerGrantState>>,
    unavailable: AtomicBool,
    changes: broadcast::Sender<PolicyChange>,
}

impl Default for TestStore {
    fn default() -> Self {
        Self {
            entries: Default::default(),
            unavailable: Default::default(),
            changes: broadcast::channel(8).0,
        }
    }
}

#[async_trait]
impl GrantStore for TestStore {
    async fn grant(&self, key: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
        if self.unavailable.load(Ordering::Relaxed) {
            return Err(GrantStoreError::Backend(Box::new(std::io::Error::other(
                "offline fixture",
            ))));
        }
        Ok(self.entries.read().get(key).cloned().unwrap_or_default())
    }

    async fn replace_grant(
        &self,
        key: &PeerGrantKey,
        expected: Uuid,
        grant: PeerGrant,
    ) -> Result<PeerGrantState, GrantStoreError> {
        let mut entries = self.entries.write();
        let actual = entries.get(key).map_or(Uuid::nil(), |state| state.revision);
        if actual != expected {
            return Err(GrantStoreError::Conflict { expected, actual });
        }
        let state = PeerGrantState {
            grant,
            revision: Uuid::now_v7(),
        };
        entries.insert(key.clone(), state.clone());
        let _ = self.changes.send(PolicyChange {
            key: key.clone(),
            effect: PolicyEffect::Grant(state.grant.clone()),
        });
        Ok(state)
    }

    fn subscribe_changes(&self) -> Option<broadcast::Receiver<PolicyChange>> {
        Some(self.changes.subscribe())
    }

    async fn managed_peers(&self, host: &NodeId) -> Result<Vec<NodeId>, GrantStoreError> {
        Ok(self
            .entries
            .read()
            .keys()
            .filter(|key| &key.host == host)
            .map(|key| key.peer.clone())
            .collect())
    }
}

#[derive(kameo::Actor)]
struct FixtureActor;

impl RemoteActor for FixtureActor {
    const REMOTE_ID: &'static str = "fixture::Actor";
}

#[derive(Serialize, Deserialize)]
struct Echo {
    claimed: String,
}

impl RemoteMessage<Echo> for FixtureActor {
    const REMOTE_ID: &'static str = "fixture::Echo";
}

fn request(to: ActorId, claimed: PeerId) -> SwarmRequest {
    SwarmRequest::Ask {
        actor_id: to,
        actor_remote_id: <FixtureActor as RemoteActor>::REMOTE_ID.into(),
        message_remote_id: <FixtureActor as RemoteMessage<Echo>>::REMOTE_ID.into(),
        payload: rmp_serde::to_vec_named(&Echo {
            claimed: claimed.to_string(),
        })
        .unwrap(),
        mailbox_timeout: None,
        reply_timeout: None,
        immediate: false,
    }
}

fn key(host: PeerId, peer: PeerId) -> PeerGrantKey {
    PeerGrantKey {
        host: NodeId::from_peer_id(host),
        peer: NodeId::from_peer_id(peer),
    }
}

#[tokio::test]
async fn session_handler_receives_policy_watch_started_at_admission() {
    let host = PeerId::random();
    let peer = PeerId::random();
    let actor = ActorId::new_with_peer_id(1, host);
    let store = Arc::new(TestStore::default());
    let key = key(host, peer);
    let revision = store
        .replace_grant(&key, Uuid::nil(), PeerGrant::Full)
        .await
        .unwrap()
        .revision;
    let router = InboundRouter::new(host, store.clone());
    router.register::<FixtureActor, Echo, _, (), _, _>(
        actor,
        InboundDelivery::Ask,
        InboundAccess::Sessions,
        move |origin, _| {
            let store = store.clone();
            let key = key.clone();
            async move {
                // An uninterrupted grant should stay authorized; the handler
                // still needs to observe changes before it took ownership.
                store
                    .replace_grant(&key, revision, PeerGrant::Full)
                    .await
                    .unwrap();
                let mut changes = origin.take_policy_changes().unwrap();
                assert!(matches!(changes.try_recv(), Ok(change) if change.key == key));
                Ok(())
            }
        },
    );
    assert!(matches!(
        router.handle_v1(peer, request(actor, peer)).await,
        Ok(SwarmResponse::Ask(Ok(_)))
    ));
}

#[tokio::test]
async fn legacy_router_requires_transport_bound_full_grant() {
    let host = PeerId::random();
    let allowed = PeerId::random();
    let other = PeerId::random();
    let actor = ActorId::new_with_peer_id(17, host);
    let store = Arc::new(TestStore::default());
    let router = InboundRouter::new(host, store.clone());
    // An actor that is not exported, such as one from before a restart, is gone.
    assert!(matches!(
        router.ask(allowed, request(actor, allowed)).await,
        Err(RemoteSendError::ActorNotRunning)
    ));
    router.register::<FixtureActor, Echo, _, String, _, _>(
        actor,
        InboundDelivery::LegacyAsk,
        InboundAccess::Full,
        |origin, _| async move { Ok(origin.key().peer.to_string()) },
    );
    let allowed_key = key(host, allowed);
    let custom = store
        .replace_grant(
            &allowed_key,
            Uuid::nil(),
            PeerGrant::Custom {
                sessions: true,
                filesystem: false,
                shell: false,
                providers: vec![],
            },
        )
        .await
        .unwrap();
    let denied = router
        .ask(allowed, request(actor, allowed))
        .await
        .unwrap_err();
    assert!(super::super::is_legacy_access_denial(&denied));
    let full = store
        .replace_grant(&allowed_key, custom.revision, PeerGrant::Full)
        .await
        .unwrap();
    assert!(matches!(
        router.ask(other, request(actor, allowed)).await,
        Err(RemoteSendError::DeserializeMessage(_))
    ));
    let reply = router.ask(allowed, request(actor, other)).await.unwrap();
    assert_eq!(
        rmp_serde::from_slice::<String>(&reply).unwrap(),
        allowed.to_string()
    );
    assert!(matches!(
        router
            .handle_legacy(
                allowed,
                SwarmRequest::Link {
                    actor_id: actor,
                    actor_remote_id: <FixtureActor as RemoteActor>::REMOTE_ID.into(),
                    sibling_id: ActorId::new_with_peer_id(9, allowed),
                    sibling_remote_id: <FixtureActor as RemoteActor>::REMOTE_ID.into(),
                }
            )
            .await,
        SwarmResponse::Link(Err(RemoteSendError::BadActorType))
    ));
    store.unavailable.store(true, Ordering::Relaxed);
    let unavailable = router
        .ask(allowed, request(actor, allowed))
        .await
        .unwrap_err();
    assert!(matches!(
        unavailable,
        RemoteSendError::DeserializeMessage(_)
    ));
    assert!(!super::super::is_legacy_access_denial(&unavailable));
    store.unavailable.store(false, Ordering::Relaxed);
    store
        .replace_grant(&allowed_key, full.revision, PeerGrant::None)
        .await
        .unwrap();
    assert!(matches!(
        router.ask(allowed, request(actor, allowed)).await,
        Err(RemoteSendError::DeserializeMessage(_))
    ));
}

#[tokio::test]
async fn grant_aware_route_authorizes_custom_sessions_only_on_v1() {
    let host = PeerId::random();
    let peer = PeerId::random();
    let actor = ActorId::new_with_peer_id(17, host);
    let store = Arc::new(TestStore::default());
    let router = InboundRouter::new(host, store.clone());
    router.register::<FixtureActor, Echo, _, String, _, _>(
        actor,
        InboundDelivery::V1Ask,
        InboundAccess::Sessions,
        |origin, _| async move {
            assert_eq!(origin.protocol(), SharingProtocol::V1);
            Ok(origin.key().peer.to_string())
        },
    );
    assert!(matches!(
        router.handle_v1(peer, request(actor, peer)).await,
        Err(SharingRejection::Denied)
    ));
    store
        .replace_grant(
            &key(host, peer),
            Uuid::nil(),
            PeerGrant::Custom {
                sessions: true,
                filesystem: false,
                shell: false,
                providers: vec![],
            },
        )
        .await
        .unwrap();
    let SwarmResponse::Ask(Ok(reply)) = router.handle_v1(peer, request(actor, peer)).await.unwrap()
    else {
        panic!("expected the granted session route to succeed");
    };
    assert_eq!(
        rmp_serde::from_slice::<String>(&reply).unwrap(),
        peer.to_string()
    );
    assert!(matches!(
        router.handle_legacy(peer, request(actor, peer)).await,
        SwarmResponse::Ask(Err(RemoteSendError::UnknownMessage { .. }))
    ));
}

#[tokio::test]
async fn provider_relay_requires_expected_host_and_active_request_without_reverse_grant() {
    use crate::StreamRelayMessage;
    use crate::provider_stream_router::{
        AttachStreamConsumer, ProviderStreamRouterActor, RegisterRequest, register_peer_relay,
    };
    use crate::stream_router_protocol::RoutedStreamRelayMessage;
    use kameo::actor::Spawn;
    use querymt::chat::StreamChunk;

    let client = PeerId::random();
    let host = PeerId::random();
    let ingress = InboundRouter::new(client, Arc::new(TestStore::default()));
    let actor = ProviderStreamRouterActor::spawn(ProviderStreamRouterActor::new(None, None));
    let actor_id = ActorId::new_with_peer_id(actor.id().sequence_id(), client);
    register_peer_relay(&ingress, actor_id, &actor, host);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    actor
        .ask(RegisterRequest {
            request_id: "active".into(),
        })
        .await
        .unwrap();
    actor
        .ask(AttachStreamConsumer {
            request_id: "active".into(),
            consumer_tx: tx,
        })
        .await
        .unwrap();

    let relay = |request_id: &str, text: &str| SwarmRequest::Tell {
        actor_id,
        actor_remote_id: <ProviderStreamRouterActor as RemoteActor>::REMOTE_ID.into(),
        message_remote_id:
            <ProviderStreamRouterActor as RemoteMessage<RoutedStreamRelayMessage>>::REMOTE_ID.into(),
        payload: rmp_serde::to_vec_named(&RoutedStreamRelayMessage {
            request_id: request_id.into(),
            message: StreamRelayMessage::Chunk(StreamChunk::Text(text.into())),
            seq: None,
        })
        .unwrap(),
        mailbox_timeout: None,
        immediate: false,
    };
    assert!(matches!(
        ingress
            .handle_legacy(PeerId::random(), relay("active", "wrong host"))
            .await,
        SwarmResponse::Tell(Err(_))
    ));
    assert!(matches!(
        ingress
            .handle_legacy(host, relay("missing", "wrong request"))
            .await,
        SwarmResponse::Tell(Err(_))
    ));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        ingress
            .handle_legacy(host, relay("active", "legacy data"))
            .await,
        SwarmResponse::Tell(Ok(()))
    ));
    assert!(
        matches!(rx.try_recv(), Ok(StreamRelayMessage::Chunk(StreamChunk::Text(text))) if text == "legacy data")
    );

    let ask = |request_id: &str| {
        let SwarmRequest::Tell {
            actor_id,
            actor_remote_id,
            message_remote_id,
            payload,
            ..
        } = relay(request_id, "V1 data")
        else {
            unreachable!()
        };
        SwarmRequest::Ask {
            actor_id,
            actor_remote_id,
            message_remote_id,
            payload,
            mailbox_timeout: None,
            reply_timeout: None,
            immediate: false,
        }
    };
    assert!(matches!(
        ingress.handle_v1(PeerId::random(), ask("active")).await,
        Ok(SwarmResponse::Ask(Err(RemoteSendError::HandlerError(_))))
    ));
    assert!(matches!(
        ingress.handle_v1(host, ask("missing")).await,
        Ok(SwarmResponse::Ask(Err(RemoteSendError::HandlerError(_))))
    ));
    assert!(rx.try_recv().is_err());
    assert!(matches!(
        ingress.handle_v1(host, ask("active")).await,
        Ok(SwarmResponse::Ask(Ok(_)))
    ));
    assert!(
        matches!(rx.try_recv(), Ok(StreamRelayMessage::Chunk(StreamChunk::Text(text))) if text == "V1 data")
    );
}

#[tokio::test]
async fn shared_ask_uses_the_negotiated_protocol_for_access() {
    let host = PeerId::random();
    let peer = PeerId::random();
    let actor = ActorId::new_with_peer_id(17, host);
    let store = Arc::new(TestStore::default());
    let router = InboundRouter::new(host, store.clone());
    let peer_key = key(host, peer);
    let custom = store
        .replace_grant(
            &peer_key,
            Uuid::nil(),
            PeerGrant::Custom {
                sessions: true,
                filesystem: false,
                shell: false,
                providers: vec![],
            },
        )
        .await
        .unwrap();
    router.register::<FixtureActor, Echo, _, String, _, _>(
        actor,
        InboundDelivery::Ask,
        InboundAccess::Sessions,
        |origin, _| async move { Ok(origin.protocol() == SharingProtocol::V1) },
    );
    assert!(matches!(
        router.handle_legacy(peer, request(actor, peer)).await,
        SwarmResponse::Ask(Err(RemoteSendError::DeserializeMessage(_)))
    ));
    let Ok(SwarmResponse::Ask(Ok(reply))) = router.handle_v1(peer, request(actor, peer)).await
    else {
        panic!("expected v1 to honor a custom session grant");
    };
    assert!(rmp_serde::from_slice::<bool>(&reply).unwrap());

    store
        .replace_grant(&peer_key, custom.revision, PeerGrant::Full)
        .await
        .unwrap();
    let SwarmResponse::Ask(Ok(reply)) = router.handle_legacy(peer, request(actor, peer)).await
    else {
        panic!("expected legacy to honor a full grant");
    };
    assert!(!rmp_serde::from_slice::<bool>(&reply).unwrap());
}

#[tokio::test]
async fn legacy_only_ask_and_mixed_delivery_preserve_their_envelopes() {
    let host = PeerId::random();
    let peer = PeerId::random();
    let legacy = ActorId::new_with_peer_id(17, host);
    let mixed = ActorId::new_with_peer_id(18, host);
    let router = InboundRouter::new(host, Arc::new(TestStore::default()));
    router.register::<FixtureActor, Echo, _, String, _, _>(
        legacy,
        InboundDelivery::LegacyAsk,
        InboundAccess::Identity,
        |_, _| async { Ok(()) },
    );
    router.register::<FixtureActor, Echo, _, String, _, _>(
        mixed,
        InboundDelivery::LegacyTellV1Ask,
        InboundAccess::Identity,
        |_, _| async { Ok(()) },
    );
    let tell = || SwarmRequest::Tell {
        actor_id: mixed,
        actor_remote_id: <FixtureActor as RemoteActor>::REMOTE_ID.into(),
        message_remote_id: <FixtureActor as RemoteMessage<Echo>>::REMOTE_ID.into(),
        payload: rmp_serde::to_vec_named(&Echo {
            claimed: String::new(),
        })
        .unwrap(),
        mailbox_timeout: None,
        immediate: false,
    };
    assert!(matches!(
        router.handle_v1(peer, request(legacy, peer)).await,
        Ok(SwarmResponse::Ask(Err(
            RemoteSendError::UnknownMessage { .. }
        )))
    ));
    assert!(matches!(
        router.handle_legacy(peer, request(mixed, peer)).await,
        SwarmResponse::Ask(Err(RemoteSendError::UnknownMessage { .. }))
    ));
    assert!(matches!(
        router.handle_v1(peer, tell()).await,
        Err(SharingRejection::UnsupportedOperation)
    ));
    assert!(matches!(
        router.handle_legacy(peer, tell()).await,
        SwarmResponse::Tell(Ok(()))
    ));
    let Ok(SwarmResponse::Ask(Ok(reply))) = router.handle_v1(peer, request(mixed, peer)).await
    else {
        panic!("expected v1 Ask to carry the callback acknowledgement");
    };
    rmp_serde::from_slice::<()>(&reply).unwrap();
}

#[tokio::test]
async fn router_refuses_reply_if_admission_policy_history_is_incomplete() {
    let host = PeerId::random();
    let peer = PeerId::random();
    let actor = ActorId::new_with_peer_id(17, host);
    let store = Arc::new(TestStore::default());
    store
        .replace_grant(&key(host, peer), Uuid::nil(), PeerGrant::Full)
        .await
        .unwrap();
    let router = InboundRouter::new(host, store.clone());
    router.register::<FixtureActor, Echo, _, String, _, _>(
        actor,
        InboundDelivery::V1Ask,
        InboundAccess::Full,
        move |origin, _| {
            let store = store.clone();
            async move {
                // Unrelated grants suffice to make the history incomplete;
                // this request itself remains allowed by the current snapshot.
                for _ in 0..9 {
                    store
                        .replace_grant(&key(host, PeerId::random()), Uuid::nil(), PeerGrant::Full)
                        .await
                        .unwrap();
                }
                let mut history = origin.take_policy_changes().unwrap();
                assert!(matches!(
                    history.try_recv(),
                    Err(broadcast::error::TryRecvError::Lagged(_))
                ));
                assert!(origin.take_policy_changes().is_none());
                Ok(())
            }
        },
    );
    assert!(matches!(
        router.handle_v1(peer, request(actor, peer)).await,
        Err(SharingRejection::Changed)
    ));
}

#[tokio::test]
async fn legacy_router_suppresses_reply_after_grant_changes() {
    let host = PeerId::random();
    let peer = PeerId::random();
    let actor = ActorId::new_with_peer_id(17, host);
    let store = Arc::new(TestStore::default());
    let router = Arc::new(InboundRouter::new(host, store.clone()));
    let peer_key = key(host, peer);
    let full = store
        .replace_grant(&peer_key, Uuid::nil(), PeerGrant::Full)
        .await
        .unwrap();
    let started = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    router.register::<FixtureActor, Echo, _, String, _, _>(
        actor,
        InboundDelivery::LegacyAsk,
        InboundAccess::Full,
        {
            let started = Arc::clone(&started);
            let finish = Arc::clone(&finish);
            move |_, _| {
                let started = Arc::clone(&started);
                let finish = Arc::clone(&finish);
                async move {
                    started.notify_one();
                    finish.notified().await;
                    Ok("protected".to_owned())
                }
            }
        },
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let task = tokio::spawn(async move { router.ask(peer, request(actor, peer)).await });
        started.notified().await;
        let revoked = store
            .replace_grant(&peer_key, full.revision, PeerGrant::None)
            .await
            .unwrap();
        store
            .replace_grant(&peer_key, revoked.revision, PeerGrant::Full)
            .await
            .unwrap();
        finish.notify_one();
        let changed = task.await.unwrap().unwrap_err();
        assert!(matches!(changed, RemoteSendError::SerializeReply(_)));
        assert!(super::super::is_legacy_access_denial(&changed));
    })
    .await
    .unwrap();
}

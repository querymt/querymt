use super::*;
use crate::peer_access::{GrantStoreError, PeerGrantState};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
struct TestStore {
    entries: RwLock<HashMap<PeerGrantKey, PeerGrantState>>,
    unavailable: AtomicBool,
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
        Ok(state)
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
async fn legacy_router_requires_transport_bound_full_grant() {
    let host = PeerId::random();
    let allowed = PeerId::random();
    let other = PeerId::random();
    let actor = ActorId::new_with_peer_id(17, host);
    let store = Arc::new(TestStore::default());
    let router = InboundRouter::new(host, store.clone());
    assert!(matches!(
        router.ask(allowed, request(actor, allowed)).await,
        Err(RemoteSendError::UnknownMessage { .. })
    ));
    router.register_ask::<FixtureActor, Echo, _, String, _, _>(
        actor,
        InboundAccess::LegacyFull,
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
                providers: vec![],
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        router.ask(allowed, request(actor, allowed)).await,
        Err(RemoteSendError::DeserializeMessage(_))
    ));
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
    assert!(matches!(
        router.ask(allowed, request(actor, allowed)).await,
        Err(RemoteSendError::DeserializeMessage(_))
    ));
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
    router.register_v1_ask::<FixtureActor, Echo, _, String, _, _>(
        actor,
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
    router.register_ask::<FixtureActor, Echo, _, String, _, _>(actor, InboundAccess::LegacyFull, {
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
    });
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
        assert!(matches!(
            task.await.unwrap(),
            Err(RemoteSendError::SerializeReply(_))
        ));
    })
    .await
    .unwrap();
}

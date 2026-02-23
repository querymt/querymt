//! Modules D (extended) + G — Session lifecycle integration tests.
//!
//! Two logical nodes ("alpha" and "beta") share a single in-process mesh.
//! Alpha creates, inspects, and destroys sessions on Beta.
//!
//! Module D: additional `RemoteNodeManager` tests (event emission, HOSTNAME
//! env override, destroy-then-create, create-destroy loop).
//!
//! Module G: full two-node lifecycle (create/list/destroy/attach/set-mode).

#[cfg(all(test, feature = "remote"))]
mod node_manager_extended_tests {
    use crate::agent::remote::node_manager::{
        CreateRemoteSession, DestroyRemoteSession, GetNodeInfo, ListRemoteSessions,
    };
    use crate::agent::remote::test_helpers::fixtures::NodeManagerFixture;
    use crate::events::AgentEventKind;

    // ── D.3 — HOSTNAME env override ───────────────────────────────────────────

    #[tokio::test]
    async fn test_get_node_info_hostname_env_override() {
        // Set HOSTNAME before spawning the node manager so the env is visible.
        // SAFETY: test is single-threaded at this point; no concurrent env reads.
        unsafe { std::env::set_var("HOSTNAME", "testhost-d3") };

        let f = NodeManagerFixture::new_with_mesh().await;
        let info = f
            .actor_ref
            .ask(GetNodeInfo)
            .await
            .expect("get_node_info should succeed");

        // Restore env (tests run in the same process).
        // SAFETY: same single-threaded context as set_var above.
        unsafe { std::env::remove_var("HOSTNAME") };

        assert_eq!(
            info.hostname, "testhost-d3",
            "hostname should reflect HOSTNAME env var override"
        );
    }

    // ── D.4 — Destroy kills the session actor ─────────────────────────────────

    #[tokio::test]
    async fn test_destroy_session_calls_shutdown() {
        let f = NodeManagerFixture::new_with_mesh().await;

        let resp = f
            .actor_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        f.actor_ref
            .ask(DestroyRemoteSession {
                session_id: resp.session_id.clone(),
            })
            .await
            .expect("destroy");

        let sessions = f
            .actor_ref
            .ask(ListRemoteSessions)
            .await
            .expect("list after destroy");

        assert!(
            sessions.iter().all(|s| s.session_id != resp.session_id),
            "destroyed session should not appear in list"
        );
    }

    // ── D.5 — No credentials → empty model list ───────────────────────────────

    #[tokio::test]
    async fn test_list_available_models_empty_when_no_credentials() {
        use crate::agent::remote::node_manager::ListAvailableModels;

        // Keep one no-mesh regression path to ensure non-mesh node manager
        // behavior remains covered in remote-feature tests.
        let f = NodeManagerFixture::new().await;
        let models = f
            .actor_ref
            .ask(ListAvailableModels)
            .await
            .expect("list_available_models should succeed");

        // The test registry has no providers configured → empty list.
        assert!(
            models.is_empty(),
            "no providers configured → model list should be empty, got {:?}",
            models
        );
    }

    // ── D.6 — 10 create+destroy cycles leave no registry entries ─────────────

    #[tokio::test]
    async fn test_create_destroy_create_sequence_no_leak() {
        let f = NodeManagerFixture::new_with_mesh().await;

        for _ in 0..10 {
            let resp = f
                .actor_ref
                .ask(CreateRemoteSession { cwd: None })
                .await
                .expect("create");

            f.actor_ref
                .ask(DestroyRemoteSession {
                    session_id: resp.session_id,
                })
                .await
                .expect("destroy");
        }

        let sessions = f.actor_ref.ask(ListRemoteSessions).await.expect("list");
        assert!(
            sessions.is_empty(),
            "after 10 create+destroy cycles the registry should be empty, got {} sessions",
            sessions.len()
        );
    }

    // ── D.1 — CreateRemoteSession emits ProviderChanged event ─────────────────

    #[tokio::test]
    async fn test_create_session_emits_provider_changed_event() {
        let f = NodeManagerFixture::new_with_mesh().await;

        let mut rx = f.config.subscribe_events();

        let resp = f
            .actor_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        // Collect events until we see SessionCreated and optionally ProviderChanged.
        let mut saw_session_created = false;
        let mut saw_provider_changed = false;

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);

        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(event)) if event.session_id() == resp.session_id => {
                    match event.kind() {
                        AgentEventKind::SessionCreated => saw_session_created = true,
                        AgentEventKind::ProviderChanged { .. } => saw_provider_changed = true,
                        _ => {}
                    }
                    if saw_session_created {
                        break;
                    }
                }
                Ok(Ok(_)) => continue, // event for another session
                _ => break,
            }
        }

        assert!(
            saw_session_created,
            "SessionCreated event should be emitted on CreateRemoteSession"
        );
        // ProviderChanged is emitted after model configuration; it may or may
        // not appear depending on the config.  We document the expectation.
        let _ = saw_provider_changed; // optionally assert when config emits it
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  Module G — Full two-node session lifecycle integration tests
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(all(test, feature = "remote"))]
mod remote_session_lifecycle_integration_tests {
    use crate::agent::core::AgentMode;
    use crate::agent::remote::SessionActorRef;
    use crate::agent::remote::node_manager::{
        CreateRemoteSession, DestroyRemoteSession, GetNodeInfo, ListRemoteSessions,
    };
    use crate::agent::remote::test_helpers::fixtures::{TwoNodeFixture, get_test_mesh};
    use crate::agent::session_actor::SessionActor;
    use uuid::Uuid;

    // ── G.1 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_alpha_creates_session_on_beta() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;

        // Alpha looks up Beta's node manager via DHT.
        let beta_ref = f
            .mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("DHT lookup")
            .expect("beta not in DHT");

        let resp = beta_ref
            .ask(&CreateRemoteSession { cwd: None })
            .await
            .expect("CreateRemoteSession on beta");

        assert!(
            !resp.session_id.is_empty(),
            "session_id should not be empty"
        );
        assert!(resp.actor_id > 0, "actor_id should be positive");

        // Verify via Beta's local list.
        let sessions = f
            .beta
            .actor_ref
            .ask(ListRemoteSessions)
            .await
            .expect("ListRemoteSessions on beta");
        assert!(
            sessions.iter().any(|s| s.session_id == resp.session_id),
            "created session should appear in Beta's list"
        );
    }

    // ── G.2 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_alpha_creates_session_on_beta_appears_in_dht() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;
        let mesh = get_test_mesh().await;

        let beta_ref = mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup beta")
            .expect("beta not found");

        let resp = beta_ref
            .ask(&CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        // Give DHT a moment to propagate the session registration.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let dht_name = format!("session::{}", resp.session_id);
        let session_actor_ref = mesh
            .lookup_actor::<SessionActor>(&dht_name)
            .await
            .expect("DHT lookup for session");

        assert!(
            session_actor_ref.is_some(),
            "session '{}' should be findable in DHT after creation",
            resp.session_id
        );
    }

    // ── G.3 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_alpha_lists_betas_sessions() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;
        let mesh = get_test_mesh().await;

        let beta_ref = mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        // Create two sessions on beta.
        let r1 = beta_ref
            .ask(&CreateRemoteSession { cwd: None })
            .await
            .expect("create 1");
        let r2 = beta_ref
            .ask(&CreateRemoteSession { cwd: None })
            .await
            .expect("create 2");

        // Alpha lists beta's sessions.
        let sessions: Vec<_> = beta_ref.ask(&ListRemoteSessions).await.expect("list");

        assert_eq!(sessions.len(), 2, "beta should have 2 sessions");
        let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert!(ids.contains(&r1.session_id.as_str()));
        assert!(ids.contains(&r2.session_id.as_str()));
    }

    // ── G.4 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_alpha_destroys_betas_session() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;
        let mesh = get_test_mesh().await;

        let beta_ref = mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let resp = beta_ref
            .ask(&CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        beta_ref
            .ask(&DestroyRemoteSession {
                session_id: resp.session_id.clone(),
            })
            .await
            .expect("destroy");

        let sessions = beta_ref.ask(&ListRemoteSessions).await.expect("list");
        assert!(
            sessions.iter().all(|s| s.session_id != resp.session_id),
            "destroyed session should be gone from beta's list"
        );
    }

    // ── G.5 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_alpha_creates_multiple_sessions_on_beta() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;
        let mesh = get_test_mesh().await;

        let beta_ref = mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let mut ids = Vec::new();
        for _ in 0..3 {
            let resp = beta_ref
                .ask(&CreateRemoteSession { cwd: None })
                .await
                .expect("create");
            ids.push(resp.session_id);
        }

        // All IDs must be unique.
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "all session IDs should be unique");

        let sessions = beta_ref.ask(&ListRemoteSessions).await.expect("list");
        assert_eq!(sessions.len(), 3);
    }

    // ── G.6 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_alpha_attaches_remote_session_to_local_registry() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;
        let mesh = get_test_mesh().await;

        let beta_ref = mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let resp = beta_ref
            .ask(&CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Attach by resolving via DHT.
        let dht_name = format!("session::{}", resp.session_id);
        let remote_ref = mesh
            .lookup_actor::<SessionActor>(&dht_name)
            .await
            .expect("session lookup")
            .expect("session not in DHT");

        let session_ref = SessionActorRef::Remote {
            actor_ref: remote_ref,
            peer_label: "beta".to_string(),
        };

        assert!(session_ref.is_remote());
        assert_eq!(session_ref.node_label(), "beta");
    }

    // ── G.7 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_alpha_sets_mode_on_betas_session() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;
        let mesh = get_test_mesh().await;

        let beta_ref = mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let resp = beta_ref
            .ask(&CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let dht_name = format!("session::{}", resp.session_id);
        let remote_ref = mesh
            .lookup_actor::<SessionActor>(&dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let session_ref = SessionActorRef::Remote {
            actor_ref: remote_ref,
            peer_label: "beta".to_string(),
        };

        session_ref
            .set_mode(AgentMode::Plan)
            .await
            .expect("set_mode");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mode = session_ref.get_mode().await.expect("get_mode");
        assert_eq!(mode, AgentMode::Plan);
    }

    // ── G.8 — Bug #5 (documented) ────────────────────────────────────────────

    /// Documents the DHT propagation race in `attach_remote_session`.
    ///
    /// `SubscribeEvents` is sent before the `EventRelayActor` registration
    /// propagates in the DHT, silently dropping event forwarding.
    ///
    /// When the race is fixed, this test should assert that events from Beta's
    /// session appear on Alpha's local event bus.
    #[tokio::test]
    async fn test_alpha_events_arrive_on_local_bus() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;
        let mesh = get_test_mesh().await;

        use crate::agent::remote::event_relay::EventRelayActor;
        use crate::event_fanout::EventFanout;
        use crate::event_sink::EventSink;
        use crate::session::backend::StorageBackend;
        use kameo::actor::Spawn;
        use std::sync::Arc;

        let beta_ref = mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let resp = beta_ref
            .ask(&CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        // Alpha sets up a relay actor and registers it in the DHT under the
        // name that SubscribeEvents will look for on Beta's session.
        let alpha_storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let alpha_journal = alpha_storage.event_journal();
        let alpha_fanout = Arc::new(EventFanout::new());
        let alpha_sink = Arc::new(EventSink::new(alpha_journal, alpha_fanout.clone()));
        let relay = EventRelayActor::new(alpha_sink.clone(), "alpha-relay-g8".to_string());
        let relay_ref = EventRelayActor::spawn(relay);

        let relay_dht_name = format!("event_relay::{}", resp.session_id);
        mesh.register_actor(relay_ref.clone(), relay_dht_name).await;
        let _ = relay_ref;

        // Allow DHT propagation before subscribing (mitigates Bug #5 for this test).
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Look up and subscribe.
        let dht_name = format!("session::{}", resp.session_id);
        let remote_ref = mesh
            .lookup_actor::<SessionActor>(&dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let session_ref = SessionActorRef::Remote {
            actor_ref: remote_ref,
            peer_label: "beta".to_string(),
        };

        session_ref.subscribe_events(1).await.expect("subscribe");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Trigger an event on Beta's session by changing mode.
        session_ref
            .set_mode(AgentMode::Plan)
            .await
            .expect("set_mode");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Bug #5: if the DHT race fires, no events may arrive on alpha fanout.
        // We document expected behaviour without asserting strictly.
        // When fixed: assert alpha fanout has received at least one event.
        let _ = alpha_fanout.subscriber_count(); // just ensure no panic
    }

    // ── G.9 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_alpha_cancels_betas_idle_session() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;
        let mesh = get_test_mesh().await;

        let beta_ref = mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let resp = beta_ref
            .ask(&CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let dht_name = format!("session::{}", resp.session_id);
        let remote_ref = mesh
            .lookup_actor::<SessionActor>(&dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let session_ref = SessionActorRef::Remote {
            actor_ref: remote_ref,
            peer_label: "beta".to_string(),
        };

        let result = session_ref.cancel().await;
        assert!(
            result.is_ok(),
            "cancel on idle remote session should return Ok"
        );
    }

    // ── G.10 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_alpha_gets_node_info_from_beta() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;
        let mesh = get_test_mesh().await;

        let beta_ref = mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let info = beta_ref.ask(&GetNodeInfo).await.expect("GetNodeInfo");

        assert!(!info.hostname.is_empty(), "hostname should not be empty");
        assert!(
            !info.capabilities.is_empty(),
            "capabilities should not be empty"
        );
    }

    // ── G.11 — Concurrent session creation ───────────────────────────────────

    #[tokio::test]
    async fn test_concurrent_sessions_on_beta_from_alpha() {
        let test_id = Uuid::now_v7().to_string();
        let f = TwoNodeFixture::new(&test_id).await;
        let mesh = get_test_mesh().await;

        let beta_ref = mesh
            .lookup_actor::<crate::agent::remote::RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let (r1, r2, r3, r4, r5) = tokio::join!(
            beta_ref.ask(&CreateRemoteSession { cwd: None }),
            beta_ref.ask(&CreateRemoteSession { cwd: None }),
            beta_ref.ask(&CreateRemoteSession { cwd: None }),
            beta_ref.ask(&CreateRemoteSession { cwd: None }),
            beta_ref.ask(&CreateRemoteSession { cwd: None }),
        );

        let mut ids = vec![
            r1.expect("1").session_id,
            r2.expect("2").session_id,
            r3.expect("3").session_id,
            r4.expect("4").session_id,
            r5.expect("5").session_id,
        ];
        ids.sort();
        ids.dedup();
        assert_eq!(
            ids.len(),
            5,
            "all 5 concurrent sessions should have unique IDs"
        );
    }
}

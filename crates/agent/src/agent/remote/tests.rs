//! Tests for remote session support: SessionActorRef, SessionRegistry,
//! RemoteNodeManager, EventRelayActor, and EventForwarder.
//!
//! These tests exercise the local-variant code paths end-to-end without
//! requiring a live kameo mesh / libp2p swarm.  Feature-gated tests that
//! exercise the `Remote` variant live in a separate `#[cfg(feature = "remote")]`
//! submodule at the bottom.

use crate::agent::agent_config::AgentConfig;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::{AgentMode, SessionRuntime};
use crate::agent::remote::SessionActorRef;
use crate::agent::session_actor::SessionActor;
use crate::agent::session_registry::SessionRegistry;
use crate::event_bus::EventBus;
use crate::events::{AgentEvent, AgentEventKind, EventOrigin};
use crate::session::backend::StorageBackend;
use crate::session::sqlite_storage::SqliteStorage;
use kameo::actor::Spawn;
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;

// ═══════════════════════════════════════════════════════════════════════════
//  Test helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Build a minimal `AgentConfig` suitable for unit tests that don't need LLM calls.
///
/// Returns `(Arc<AgentConfig>, TempDir)` — the `TempDir` must be kept alive for
/// the duration of the test.
async fn test_agent_config() -> (Arc<AgentConfig>, TempDir) {
    let temp_dir = TempDir::new().expect("create temp dir");
    let config_path = temp_dir.path().join("providers.toml");
    std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
    let registry = PluginRegistry::from_path(&config_path).expect("create plugin registry");
    let plugin_registry = Arc::new(registry);

    let storage = Arc::new(
        SqliteStorage::connect(":memory:".into())
            .await
            .expect("create sqlite storage"),
    );
    let llm = LLMParams::new().provider("mock").model("mock");

    let builder = AgentConfigBuilder::new(plugin_registry, storage.session_store(), llm);
    let config = Arc::new(builder.build());
    (config, temp_dir)
}

/// Spawn a `SessionActor` with default runtime and return a `SessionActorRef::Local`.
fn spawn_test_session(config: Arc<AgentConfig>, session_id: &str) -> SessionActorRef {
    let runtime = SessionRuntime::new(None, HashMap::new(), HashMap::new(), Vec::new());
    let actor = SessionActor::new(config, session_id.to_string(), runtime);
    let actor_ref = SessionActor::spawn(actor);
    SessionActorRef::Local(actor_ref)
}

// ═══════════════════════════════════════════════════════════════════════════
//  SessionActorRef — Local variant
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_local_ref_is_not_remote() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-local-1");
    assert!(!session_ref.is_remote());
    assert_eq!(session_ref.node_label(), "local");
}

#[tokio::test]
async fn test_local_ref_get_mode_default_is_build() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-mode-1");
    let mode = session_ref
        .get_mode()
        .await
        .expect("get_mode should succeed");
    assert_eq!(mode, AgentMode::Build);
}

#[tokio::test]
async fn test_local_ref_set_mode_roundtrip() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-mode-2");

    session_ref
        .set_mode(AgentMode::Plan)
        .await
        .expect("set_mode Plan");
    let mode = session_ref.get_mode().await.expect("get_mode");
    assert_eq!(mode, AgentMode::Plan);

    session_ref
        .set_mode(AgentMode::Build)
        .await
        .expect("set_mode Build");
    let mode = session_ref.get_mode().await.expect("get_mode");
    assert_eq!(mode, AgentMode::Build);
}

#[tokio::test]
async fn test_local_ref_cancel_idle_session() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-cancel-1");
    // Cancel on idle session should succeed (no-op)
    session_ref.cancel().await.expect("cancel on idle session");
}

#[tokio::test]
async fn test_local_ref_shutdown() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-shutdown-1");
    session_ref.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn test_local_ref_get_session_limits_none() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-limits-1");
    let limits = session_ref
        .get_session_limits()
        .await
        .expect("get_session_limits");
    assert!(limits.is_none(), "no limits configured → None");
}

#[tokio::test]
async fn test_local_ref_get_llm_config() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-llm-1");
    // Session doesn't exist in the DB → handler returns an error.
    // We just verify the message round-trips without panic.
    let result = session_ref.get_llm_config().await;
    assert!(result.is_err(), "no session in DB → should return error");
}

#[tokio::test]
async fn test_local_ref_subscribe_unsubscribe_events() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-events-1");

    // Subscribe with a dummy relay_actor_id
    session_ref
        .subscribe_events(42)
        .await
        .expect("subscribe_events");
    // Unsubscribe
    session_ref
        .unsubscribe_events(42)
        .await
        .expect("unsubscribe_events");
}

#[tokio::test]
async fn test_local_ref_get_history() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "test-history-1");
    // Session doesn't exist in the DB → handler returns an error.
    // We just verify the message round-trips without panic.
    let result = session_ref.get_history().await;
    assert!(result.is_err(), "no session in DB → should return error");
}

// ═══════════════════════════════════════════════════════════════════════════
//  SessionRegistry — CRUD operations
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_registry_initially_empty() {
    let (config, _td) = test_agent_config().await;
    let registry = SessionRegistry::new(config);
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert!(registry.session_ids().is_empty());
}

#[tokio::test]
async fn test_registry_insert_and_get() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config.clone(), "reg-1");

    let mut registry = SessionRegistry::new(config);
    registry.insert("reg-1".to_string(), session_ref);

    assert_eq!(registry.len(), 1);
    assert!(!registry.is_empty());
    assert!(registry.get("reg-1").is_some());
    assert!(registry.get("reg-nonexistent").is_none());
}

#[tokio::test]
async fn test_registry_insert_and_remove() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config.clone(), "reg-2");

    let mut registry = SessionRegistry::new(config);
    registry.insert("reg-2".to_string(), session_ref);
    assert_eq!(registry.len(), 1);

    let removed = registry.remove("reg-2");
    assert!(removed.is_some());
    assert_eq!(registry.len(), 0);
    assert!(registry.get("reg-2").is_none());
}

#[tokio::test]
async fn test_registry_remove_nonexistent() {
    let (config, _td) = test_agent_config().await;
    let mut registry = SessionRegistry::new(config);
    let removed = registry.remove("does-not-exist");
    assert!(removed.is_none());
}

#[tokio::test]
async fn test_registry_session_ids() {
    let (config, _td) = test_agent_config().await;
    let ref1 = spawn_test_session(config.clone(), "id-a");
    let ref2 = spawn_test_session(config.clone(), "id-b");
    let ref3 = spawn_test_session(config.clone(), "id-c");

    let mut registry = SessionRegistry::new(config);
    registry.insert("id-a".to_string(), ref1);
    registry.insert("id-b".to_string(), ref2);
    registry.insert("id-c".to_string(), ref3);

    let mut ids = registry.session_ids();
    ids.sort();
    assert_eq!(ids, vec!["id-a", "id-b", "id-c"]);
}

#[tokio::test]
async fn test_registry_overwrite() {
    let (config, _td) = test_agent_config().await;
    let ref1 = spawn_test_session(config.clone(), "ow-1");
    let ref2 = spawn_test_session(config.clone(), "ow-1-replacement");

    let mut registry = SessionRegistry::new(config);
    registry.insert("ow-1".to_string(), ref1);
    assert_eq!(registry.len(), 1);

    // Insert again with same key — should overwrite
    registry.insert("ow-1".to_string(), ref2);
    assert_eq!(registry.len(), 1);

    // The stored ref should still be gettable
    assert!(registry.get("ow-1").is_some());
}

// ═══════════════════════════════════════════════════════════════════════════
//  EventRelayActor — extended tests
// ═══════════════════════════════════════════════════════════════════════════

mod event_relay_extended {
    use super::*;
    use crate::agent::remote::event_relay::{EventRelayActor, RelayedEvent};

    #[tokio::test]
    async fn test_relay_multiple_events() {
        let local_bus = Arc::new(EventBus::new());
        let relay = EventRelayActor::new(local_bus.clone(), "multi-test".to_string());
        let relay_ref = <EventRelayActor as Spawn>::spawn(relay);

        let mut rx = local_bus.subscribe();

        let events = vec![
            AgentEvent {
                seq: 1,
                timestamp: 100,
                session_id: "s1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::SessionCreated,
            },
            AgentEvent {
                seq: 2,
                timestamp: 200,
                session_id: "s1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::PromptReceived {
                    content: "hello".to_string(),
                    message_id: None,
                },
            },
            AgentEvent {
                seq: 3,
                timestamp: 300,
                session_id: "s1".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::LlmRequestStart { message_count: 5 },
            },
        ];

        for event in &events {
            relay_ref
                .tell(RelayedEvent {
                    event: event.clone(),
                })
                .await
                .expect("tell should succeed");
        }

        // Receive all 3 events with original metadata intact.
        for expected_event in &events {
            let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
                .await
                .expect("should receive within timeout")
                .expect("recv should succeed");

            assert_eq!(received.seq, expected_event.seq);
            assert_eq!(received.timestamp, expected_event.timestamp);
            assert_eq!(received.session_id, expected_event.session_id);
        }
    }

    #[tokio::test]
    async fn test_relay_preserves_session_id() {
        let local_bus = Arc::new(EventBus::new());
        let relay = EventRelayActor::new(local_bus.clone(), "preserve-test".to_string());
        let relay_ref = <EventRelayActor as Spawn>::spawn(relay);

        let mut rx = local_bus.subscribe();

        let event = AgentEvent {
            seq: 99,
            timestamp: 555,
            session_id: "unique-session-id-xyz".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        };

        relay_ref
            .tell(RelayedEvent {
                event: event.clone(),
            })
            .await
            .expect("tell");

        let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        assert_eq!(received.session_id, "unique-session-id-xyz");
    }

    #[tokio::test]
    async fn test_relay_preserves_event_kind() {
        let local_bus = Arc::new(EventBus::new());
        let relay = EventRelayActor::new(local_bus.clone(), "kind-test".to_string());
        let relay_ref = <EventRelayActor as Spawn>::spawn(relay);

        let mut rx = local_bus.subscribe();

        let event = AgentEvent {
            seq: 7,
            timestamp: 777,
            session_id: "s1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::Error {
                message: "test error".to_string(),
            },
        };

        relay_ref
            .tell(RelayedEvent {
                event: event.clone(),
            })
            .await
            .expect("tell");

        let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        match received.kind {
            AgentEventKind::Error { message } => {
                assert_eq!(message, "test error");
            }
            other => panic!("expected Error event, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_relay_different_sessions() {
        let local_bus = Arc::new(EventBus::new());
        let relay = EventRelayActor::new(local_bus.clone(), "multi-session".to_string());
        let relay_ref = <EventRelayActor as Spawn>::spawn(relay);

        let mut rx = local_bus.subscribe();

        let event_a = AgentEvent {
            seq: 1,
            timestamp: 100,
            session_id: "session-a".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        };
        let event_b = AgentEvent {
            seq: 2,
            timestamp: 200,
            session_id: "session-b".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        };

        relay_ref
            .tell(RelayedEvent {
                event: event_a.clone(),
            })
            .await
            .expect("tell a");
        relay_ref
            .tell(RelayedEvent {
                event: event_b.clone(),
            })
            .await
            .expect("tell b");

        let r1 = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");
        let r2 = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        let mut ids = vec![r1.session_id, r2.session_id];
        ids.sort();
        assert_eq!(ids, vec!["session-a", "session-b"]);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  EventForwarder — stub tests (no remote feature)
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(not(feature = "remote"))]
mod event_forwarder_stub {
    use crate::agent::remote::EventForwarder;

    #[test]
    #[should_panic(expected = "requires the 'remote' feature")]
    fn test_stub_panics_on_construction() {
        let _ = EventForwarder::new((), "test".to_string());
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  RemoteNodeManager — lifecycle tests (requires `remote` feature)
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "remote")]
mod node_manager_tests {
    use super::*;
    use crate::agent::remote::node_manager::{
        CreateRemoteSession, DestroyRemoteSession, GetNodeInfo, ListRemoteSessions,
        RemoteNodeManager,
    };
    use kameo::actor::{ActorRef, Spawn};
    use kameo::error::SendError;
    use tokio::sync::Mutex;

    /// Spawn a `RemoteNodeManager` actor and return its `ActorRef`.
    ///
    /// The actor is spawned with no mesh (suitable for local-only tests).
    async fn spawn_test_node_manager() -> (ActorRef<RemoteNodeManager>, Arc<AgentConfig>, TempDir) {
        let (config, td) = test_agent_config().await;
        let registry = Arc::new(Mutex::new(SessionRegistry::new(config.clone())));
        let nm = RemoteNodeManager::new(config.clone(), registry, None);
        let actor_ref = RemoteNodeManager::spawn(nm);
        (actor_ref, config, td)
    }

    #[tokio::test]
    async fn test_create_session_no_cwd() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create session should succeed");

        assert!(
            !resp.session_id.is_empty(),
            "session_id should not be empty"
        );
        assert!(resp.actor_id > 0, "actor_id should be positive");
    }

    #[tokio::test]
    async fn test_create_session_valid_cwd() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;
        let cwd_dir = TempDir::new().unwrap();

        let resp = nm_ref
            .ask(CreateRemoteSession {
                cwd: Some(cwd_dir.path().to_string_lossy().to_string()),
            })
            .await
            .expect("create session with valid cwd should succeed");

        assert!(!resp.session_id.is_empty());
    }

    #[tokio::test]
    async fn test_create_session_nonexistent_cwd() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        let resp = nm_ref
            .ask(CreateRemoteSession {
                cwd: Some("/this/path/does/not/exist/anywhere".to_string()),
            })
            .await
            .expect("create session with nonexistent cwd should succeed (workspace index skipped)");

        assert!(!resp.session_id.is_empty());
    }

    #[tokio::test]
    async fn test_create_session_relative_cwd_rejected() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        let result = nm_ref
            .ask(CreateRemoteSession {
                cwd: Some("relative/path".to_string()),
            })
            .await;

        match result {
            Err(SendError::HandlerError(err)) => {
                assert!(
                    err.to_string().contains("absolute"),
                    "error should mention absolute path: {:?}",
                    err
                );
            }
            Ok(_) => panic!("relative cwd should be rejected, but got Ok"),
            Err(e) => panic!("unexpected error variant: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_list_sessions_empty() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        let sessions = nm_ref
            .ask(ListRemoteSessions)
            .await
            .expect("list should succeed");

        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_list_sessions_after_create() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        let sessions = nm_ref.ask(ListRemoteSessions).await.expect("list");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, resp.session_id);
        assert!(!sessions[0].peer_label.is_empty());
    }

    #[tokio::test]
    async fn test_create_multiple_sessions() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        let mut session_ids = Vec::new();
        for _ in 0..3 {
            let resp = nm_ref
                .ask(CreateRemoteSession { cwd: None })
                .await
                .expect("create");
            session_ids.push(resp.session_id);
        }

        // All IDs should be unique
        let mut sorted = session_ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "all session IDs should be unique");

        let sessions = nm_ref.ask(ListRemoteSessions).await.expect("list");
        assert_eq!(sessions.len(), 3);
    }

    #[tokio::test]
    async fn test_destroy_session() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        nm_ref
            .ask(DestroyRemoteSession {
                session_id: resp.session_id.clone(),
            })
            .await
            .expect("destroy should succeed");

        let sessions = nm_ref.ask(ListRemoteSessions).await.expect("list");
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_destroy_nonexistent_session() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        let result = nm_ref
            .ask(DestroyRemoteSession {
                session_id: "does-not-exist".to_string(),
            })
            .await;

        assert!(
            matches!(result, Err(SendError::HandlerError(_))),
            "destroying nonexistent session should fail with HandlerError"
        );
    }

    #[tokio::test]
    async fn test_get_node_info() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        let info = nm_ref
            .ask(GetNodeInfo)
            .await
            .expect("get_node_info should succeed");

        assert!(!info.hostname.is_empty(), "hostname should not be empty");
        assert_eq!(info.active_sessions, 0);
        assert!(!info.capabilities.is_empty());
    }

    #[tokio::test]
    async fn test_get_node_info_reflects_session_count() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        for _ in 0..2 {
            nm_ref
                .ask(CreateRemoteSession { cwd: None })
                .await
                .expect("create");
        }

        let info = nm_ref.ask(GetNodeInfo).await.expect("info");
        assert_eq!(info.active_sessions, 2);
    }

    #[tokio::test]
    async fn test_create_session_emits_events() {
        let (nm_ref, config, _td) = spawn_test_node_manager().await;

        let mut rx = config.event_bus.subscribe();

        let resp = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create");

        // Should receive SessionCreated event
        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("should receive event within timeout")
            .expect("recv");

        assert_eq!(event.session_id, resp.session_id);
        assert!(matches!(event.kind, AgentEventKind::SessionCreated));
    }

    #[tokio::test]
    async fn test_create_session_cwd_tracked_in_list() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;
        let cwd_dir = TempDir::new().unwrap();
        let cwd_str = cwd_dir.path().to_string_lossy().to_string();

        let resp = nm_ref
            .ask(CreateRemoteSession {
                cwd: Some(cwd_str.clone()),
            })
            .await
            .expect("create");

        let sessions = nm_ref.ask(ListRemoteSessions).await.expect("list");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, resp.session_id);
        assert_eq!(sessions[0].cwd, Some(cwd_str));
    }

    #[tokio::test]
    async fn test_destroy_then_create_reuses_slot() {
        let (nm_ref, _config, _td) = spawn_test_node_manager().await;

        // Create + destroy
        let resp1 = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create 1");

        nm_ref
            .ask(DestroyRemoteSession {
                session_id: resp1.session_id.clone(),
            })
            .await
            .expect("destroy");

        // Create again — should succeed and produce a different session ID
        let resp2 = nm_ref
            .ask(CreateRemoteSession { cwd: None })
            .await
            .expect("create 2");

        assert_ne!(resp1.session_id, resp2.session_id);

        let sessions = nm_ref.ask(ListRemoteSessions).await.expect("list");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, resp2.session_id);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  SessionActorRef identification edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_from_actor_ref_into_local() {
    let (config, _td) = test_agent_config().await;
    let runtime = SessionRuntime::new(None, HashMap::new(), HashMap::new(), Vec::new());
    let actor = SessionActor::new(config, "from-test".to_string(), runtime);
    let actor_ref = SessionActor::spawn(actor);

    // Test the From<ActorRef<SessionActor>> conversion
    let session_ref: SessionActorRef = actor_ref.into();
    assert!(!session_ref.is_remote());
    assert_eq!(session_ref.node_label(), "local");
}

#[tokio::test]
async fn test_session_actor_ref_clone() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config, "clone-test");
    let cloned = session_ref.clone();

    // Both should work independently
    let m1 = session_ref.get_mode().await.expect("get_mode original");
    let m2 = cloned.get_mode().await.expect("get_mode clone");
    assert_eq!(m1, m2);
}

// ═══════════════════════════════════════════════════════════════════════════
//  SessionRegistry — interaction with SessionActorRef
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_registry_get_returns_working_ref() {
    let (config, _td) = test_agent_config().await;
    let session_ref = spawn_test_session(config.clone(), "reg-work-1");

    let mut registry = SessionRegistry::new(config);
    registry.insert("reg-work-1".to_string(), session_ref);

    // Get the ref from registry and use it
    let stored_ref = registry.get("reg-work-1").expect("should exist");
    let mode = stored_ref
        .get_mode()
        .await
        .expect("get_mode via registry ref");
    assert_eq!(mode, AgentMode::Build);
}

#[tokio::test]
async fn test_registry_insert_via_actor_ref() {
    let (config, _td) = test_agent_config().await;
    let runtime = SessionRuntime::new(None, HashMap::new(), HashMap::new(), Vec::new());
    let actor = SessionActor::new(config.clone(), "raw-actor".to_string(), runtime);
    let actor_ref = SessionActor::spawn(actor);

    let mut registry = SessionRegistry::new(config);
    // insert() accepts `impl Into<SessionActorRef>`, including bare ActorRef
    registry.insert("raw-actor".to_string(), actor_ref);

    let stored = registry.get("raw-actor").expect("should exist");
    assert!(!stored.is_remote());
}

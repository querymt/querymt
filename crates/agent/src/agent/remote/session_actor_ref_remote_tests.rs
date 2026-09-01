//! Module E — `SessionActorRef::Remote` variant tests.
//!
//! Exercises every method on `SessionActorRef::Remote` by:
//! 1. Bootstrapping the shared test mesh.
//! 2. Spawning a `SessionActor` locally.
//! 3. Registering it in the DHT under `"session::test-{uuid}"`.
//! 4. Resolving it back via `RemoteActorRef::lookup`.
//! 5. Wrapping in `SessionActorRef::Remote { actor_ref, peer_label }`.
//! 6. Asserting behaviour.

#[cfg(all(test, feature = "remote"))]
#[allow(clippy::module_inception)]
mod session_actor_ref_remote_tests {
    use std::collections::HashMap;

    use kameo::actor::Spawn;
    use querymt::LLMParams;

    use crate::agent::core::{AgentMode, SessionRuntime};
    use crate::agent::remote::SessionActorRef;
    use crate::agent::remote::test_helpers::fixtures::{AgentConfigFixture, get_test_mesh};
    use crate::agent::session_actor::SessionActor;

    /// Set up a remote `SessionActorRef` for one test.
    ///
    /// Returns `(SessionActorRef::Remote, uuid_string)`.  Keeps the local
    /// `SessionActor` alive via the returned `ActorRef`.
    async fn remote_session_ref(
        label: &str,
    ) -> (SessionActorRef, kameo::actor::ActorRef<SessionActor>) {
        let mesh = get_test_mesh().await;
        let f = AgentConfigFixture::new().await;
        let store = f.config.provider.history_store();
        let session = store
            .create_session(Some(format!("remote-e-{label}")), None, None, None)
            .await
            .expect("create test session");
        let session_id = session.public_id;
        let llm_config = store
            .create_or_get_llm_config(&LLMParams::new().provider("mock").model("mock"))
            .await
            .expect("create test LLM config");
        store
            .set_session_llm_config(&session_id, llm_config.id)
            .await
            .expect("bind test LLM config");

        let runtime = SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );
        let actor = SessionActor::new(f.config.clone(), session_id.clone(), runtime);
        let local_ref = SessionActor::spawn(actor);

        let dht_name = crate::agent::remote::scope::scoped_session(
            &crate::agent::remote::scope::MeshScopeId::lan_default(),
            &session_id,
        );
        mesh.register_actor(local_ref.clone(), dht_name.clone())
            .await;

        // Give DHT a moment to propagate (same-process, should be instant).
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let remote_ref = kameo::actor::RemoteActorRef::<SessionActor>::lookup(&*dht_name)
            .await
            .expect("DHT lookup succeeded")
            .unwrap_or_else(|| panic!("session '{}' not found in DHT", dht_name));

        let session_ref = SessionActorRef::Remote {
            actor_ref: remote_ref,
            peer_label: label.to_string(),
            remote_node_id: None,
        };

        (session_ref, local_ref)
    }

    // ── E.1 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_is_remote_and_has_peer_label() {
        let (session_ref, _local) = remote_session_ref("e1").await;
        assert!(
            session_ref.is_remote(),
            "Remote variant should report is_remote() = true"
        );
        assert_eq!(session_ref.node_label(), "e1");
    }

    // ── E.2 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_get_mode_default_is_build() {
        let (session_ref, _local) = remote_session_ref("e2").await;
        let mode = session_ref
            .get_mode()
            .await
            .expect("get_mode should succeed via remote ref");
        assert_eq!(
            mode,
            AgentMode::Build,
            "default mode should be Build, got {:?}",
            mode
        );
    }

    // ── E.3 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_set_mode_roundtrip() {
        let (session_ref, _local) = remote_session_ref("e3").await;

        session_ref
            .set_mode(AgentMode::Plan)
            .await
            .expect("set_mode(Plan)");

        // Brief pause — tell() is fire-and-forget; wait for delivery.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mode = session_ref.get_mode().await.expect("get_mode");
        assert_eq!(mode, AgentMode::Plan, "mode should be Plan after set");
    }

    // ── E.4 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_cancel_idle_session_is_noop() {
        let (session_ref, _local) = remote_session_ref("e4").await;
        let result = session_ref.cancel().await;
        assert!(
            result.is_ok(),
            "cancel on idle remote session should return Ok"
        );
    }

    // ── E.5 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_get_session_limits_none() {
        let (session_ref, _local) = remote_session_ref("e5").await;
        let limits = session_ref
            .get_session_limits()
            .await
            .expect("get_session_limits");
        assert!(
            limits.is_none(),
            "no limits configured → should be None, got {:?}",
            limits
        );
    }

    // ── E.6 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_get_history_empty_session() {
        let (session_ref, _local) = remote_session_ref("e6").await;
        let history = session_ref.get_history().await.expect("get history");
        assert!(history.is_empty());
    }

    // ── E.7 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_get_llm_config() {
        let (session_ref, _local) = remote_session_ref("e7").await;
        let config = session_ref
            .get_llm_config()
            .await
            .expect("get LLM config")
            .expect("test session should have an LLM config");
        assert_eq!(config.provider, "mock");
        assert_eq!(config.model, "mock");
    }

    // ── E.8 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_subscribe_unsubscribe_roundtrip() {
        let (session_ref, _local) = remote_session_ref("e8").await;

        let result = session_ref
            .subscribe_events(99, "event_relay::test::peer-e8".to_string())
            .await;
        assert!(result.is_ok(), "subscribe_events should return Ok");

        let result = session_ref
            .unsubscribe_events(99, "event_relay::test::peer-e8".to_string())
            .await;
        assert!(result.is_ok(), "unsubscribe_events should return Ok");
    }

    // ── E.9 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_shutdown_returns_ok() {
        let (session_ref, _local) = remote_session_ref("e9").await;
        // Remote shutdown is currently a no-op that returns Ok.
        let result = session_ref.shutdown().await;
        assert!(result.is_ok(), "shutdown on remote ref should return Ok");
    }

    // ── E.10 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_set_bridge_returns_ok() {
        let (session_ref, _local) = remote_session_ref("e10").await;
        // set_bridge on Remote variant is a no-op that returns Ok regardless of sender.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let bridge = crate::acp::client_bridge::ClientBridgeSender::new(tx);
        let result = session_ref.set_bridge(bridge).await;
        assert!(
            result.is_ok(),
            "set_bridge on remote ref should return Ok (no-op)"
        );
    }

    // ── E.11 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_clone_both_work() {
        let (session_ref, _local) = remote_session_ref("e11").await;
        let cloned = session_ref.clone();

        let m1 = session_ref.get_mode().await.expect("original get_mode");
        let m2 = cloned.get_mode().await.expect("clone get_mode");
        assert_eq!(m1, m2, "original and clone should return the same mode");
    }
}

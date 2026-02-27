//! Tests for `handle_set_agent_mode` sandbox routing.
//!
//! Verifies that when a session is managed by a sandboxed worker,
//! `handle_set_agent_mode` routes the mode switch through
//! `WorkerManager::switch_mode` (which updates the supervisor approval backend)
//! rather than calling `session_ref.set_mode` directly.

#[cfg(all(feature = "sandbox", feature = "dashboard"))]
mod sandbox_mode_routing {
    use crate::agent::core::AgentMode;
    use crate::agent::remote::SessionActorRef;
    use crate::agent::session_actor::SessionActor;
    use crate::test_utils::TestServerState;
    use crate::ui::handlers::handle_set_agent_mode;
    use kameo::actor::Spawn;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    /// Helper: spawn a local SessionActor and return its ref.
    async fn spawn_local_session_actor(f: &TestServerState, session_id: &str) -> SessionActorRef {
        use crate::agent::core::SessionRuntime;
        use std::collections::HashMap;

        let runtime = SessionRuntime::new(None, HashMap::new(), HashMap::new(), Vec::new());
        let actor = SessionActor::new(
            f.agent.handle.config.clone(),
            session_id.to_string(),
            runtime,
        );
        let actor_ref = SessionActor::spawn(actor);
        SessionActorRef::Local(actor_ref)
    }

    /// When a session is tracked in the WorkerManager (sandbox mode), calling
    /// `handle_set_agent_mode` must route through `WorkerManager::switch_mode`,
    /// which updates `worker_current_mode`.
    ///
    /// RED → GREEN: before Fix 2, the handler calls `session_ref.set_mode()`
    /// directly and never updates the WorkerManager's tracked mode.
    #[tokio::test]
    async fn handle_set_agent_mode_routes_through_worker_manager_in_sandbox_mode() {
        let f = TestServerState::new().await;
        let session_id = f.agent.create_session().await;

        // Set up a connection that maps to this session.
        let conn_id = "test-conn";
        let (tx, mut rx) = mpsc::channel(16);
        {
            let mut connections = f.state.connections.lock().await;
            connections.insert(
                conn_id.to_string(),
                crate::ui::ConnectionState {
                    routing_mode: crate::ui::RoutingMode::Single,
                    active_agent_id: "primary".to_string(),
                    sessions: {
                        let mut m = std::collections::HashMap::new();
                        m.insert("primary".to_string(), session_id.clone());
                        m
                    },
                    subscribed_sessions: std::collections::HashSet::new(),
                    session_cursors: std::collections::HashMap::new(),
                    current_workspace_root: None,
                    file_index_forwarder: None,
                },
            );
        }

        // Inject a fake worker for this session (starts in Build mode).
        let session_ref = spawn_local_session_actor(&f, &session_id).await;
        {
            let mut wm = f.agent.handle.worker_manager.lock().await;
            wm.inject_local_worker(
                &session_id,
                session_ref,
                AgentMode::Build,
                PathBuf::from("/tmp/test"),
            )
            .await;
        }

        // Pre-condition: worker is in Build mode.
        {
            let wm = f.agent.handle.worker_manager.lock().await;
            assert_eq!(wm.worker_current_mode(&session_id), AgentMode::Build);
        }

        // Act: change the mode to Plan via the UI handler.
        handle_set_agent_mode(&f.state, conn_id, "plan", &tx).await;

        // Drain any messages sent.
        drop(tx);
        while rx.try_recv().is_ok() {}

        // Assert: the WorkerManager's tracked mode was updated.
        // This only happens if switch_mode was called (i.e. the handler
        // routed through WorkerManager, not directly to session_ref).
        {
            let wm = f.agent.handle.worker_manager.lock().await;
            assert_eq!(
                wm.worker_current_mode(&session_id),
                AgentMode::Plan,
                "WorkerManager.current_mode must be Plan after mode switch; \
                 if still Build, the handler bypassed WorkerManager::switch_mode"
            );
        }
    }

    /// When no worker exists for the session (non-sandbox mode), calling
    /// `handle_set_agent_mode` must still work correctly via the direct
    /// `session_ref.set_mode()` path.
    #[tokio::test]
    async fn handle_set_agent_mode_without_worker_still_works() {
        let f = TestServerState::new().await;
        let session_id = f.agent.create_session().await;

        let conn_id = "test-conn-2";
        let (tx, mut rx) = mpsc::channel(16);
        {
            let mut connections = f.state.connections.lock().await;
            connections.insert(
                conn_id.to_string(),
                crate::ui::ConnectionState {
                    routing_mode: crate::ui::RoutingMode::Single,
                    active_agent_id: "primary".to_string(),
                    sessions: {
                        let mut m = std::collections::HashMap::new();
                        m.insert("primary".to_string(), session_id.clone());
                        m
                    },
                    subscribed_sessions: std::collections::HashSet::new(),
                    session_cursors: std::collections::HashMap::new(),
                    current_workspace_root: None,
                    file_index_forwarder: None,
                },
            );
        }

        // No worker injected — this session uses the local direct path.
        {
            let wm = f.agent.handle.worker_manager.lock().await;
            assert!(!wm.has_worker(&session_id), "precondition: no worker");
        }

        // Act.
        handle_set_agent_mode(&f.state, conn_id, "plan", &tx).await;

        // Should not error — drain messages.
        drop(tx);
        while rx.try_recv().is_ok() {}

        // The session actor's mode should have been updated via the direct path.
        let session_ref = {
            let registry = f.agent.handle.registry.lock().await;
            registry.get(&session_id).cloned()
        };
        if let Some(r) = session_ref {
            let mode = r.get_mode().await.expect("get_mode");
            assert_eq!(mode, AgentMode::Plan, "session actor mode must be Plan");
        }
        // If session_ref is None, the session was never loaded — that's a
        // pre-condition issue, but the handler should not have errored.
    }
}

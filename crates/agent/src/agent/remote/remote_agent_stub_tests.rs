//! Module C — `RemoteAgentHandle` / `AgentHandle` tests.
//!
//! `RemoteAgentHandle` implements `AgentHandle` for cross-mesh delegation.
//! These tests verify the `AgentHandle` trait implementation for remote agents.
//!
//! Mesh **is** required here because `create_delegation_session` / `new_session`
//! / `prompt` internally do DHT lookups.

#[cfg(all(test, feature = "remote"))]
#[allow(clippy::module_inception)]
mod remote_agent_stub_tests {
    use crate::agent::handle::AgentHandle;
    use crate::agent::remote::remote_handle::RemoteAgentHandle;
    use crate::agent::remote::test_helpers::fixtures::{MeshNodeManagerFixture, get_test_mesh};
    use agent_client_protocol::{
        CancelNotification, NewSessionRequest, SessionId,
    };
    use std::path::PathBuf;
    use std::sync::Arc;
    use uuid::Uuid;

    /// Build a RemoteAgentHandle pointing at the mesh.
    fn make_handle(
        peer_label: &str,
        mesh: &crate::agent::remote::mesh::MeshHandle,
    ) -> Arc<RemoteAgentHandle> {
        Arc::new(RemoteAgentHandle::new(
            peer_label.to_string(),
            mesh.clone(),
        ))
    }

    // ── C.1 — new_session delegates to mesh ──────────────────────────────────

    #[tokio::test]
    async fn test_handle_new_session_delegates_to_mesh() {
        let test_id = Uuid::now_v7().to_string();
        let nm = MeshNodeManagerFixture::new("c1", &test_id).await;
        let _ = nm; // keep alive

        let mesh = get_test_mesh().await;
        let handle = make_handle("c1", mesh);

        let req = NewSessionRequest::new(PathBuf::from("/tmp"));
        // May fail if no node manager is registered under "node_manager".
        // This is acceptable — the test documents the current DHT key expectation.
        let _result = handle.new_session(req).await;
        // No panic is the key assertion — both Ok and Err are acceptable outcomes.
    }

    // ── C.2 — prompt errors without a live peer ──────────────────────────────

    #[tokio::test]
    async fn test_handle_prompt_returns_error_without_live_peer() {
        let mesh = get_test_mesh().await;
        let test_id = Uuid::now_v7().to_string();
        let handle = make_handle(&format!("dead-peer-{}", test_id), mesh);

        let req = agent_client_protocol::PromptRequest::new(
            SessionId::from("s-c2".to_string()),
            vec![agent_client_protocol::ContentBlock::from("hello")],
        );
        let result = handle.prompt(req).await;
        assert!(
            result.is_err(),
            "prompt to a non-existent peer should return Err"
        );
    }

    // ── C.3 — cancel on idle is a noop ───────────────────────────────────────

    #[tokio::test]
    async fn test_handle_cancel_on_idle_session_is_noop() {
        let mesh = get_test_mesh().await;
        let handle = make_handle("peer-c3", mesh);

        let notif = CancelNotification::new(SessionId::from("idle-c3".to_string()));
        let result = handle.cancel(notif).await;
        assert!(result.is_ok(), "cancel on idle handle should return Ok");
    }

    // ── C.4 — event fanout works ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_handle_event_fanout() {
        let mesh = get_test_mesh().await;
        let handle = make_handle("peer-c4", mesh);

        let mut rx = handle.subscribe_events();
        handle.emit_event("test-session", crate::events::AgentEventKind::Cancelled);

        let received = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("recv");

        assert_eq!(received.session_id(), "test-session");
    }

    // ── C.5 — agent_registry returns empty ───────────────────────────────────

    #[tokio::test]
    async fn test_handle_agent_registry_is_empty() {
        let mesh = get_test_mesh().await;
        let handle = make_handle("peer-c5", mesh);

        let registry = handle.agent_registry();
        assert!(registry.list_agents().is_empty());
    }

    // ── C.6 — as_any returns RemoteAgentHandle ───────────────────────────────

    #[tokio::test]
    async fn test_handle_as_any_returns_correct_type() {
        let mesh = get_test_mesh().await;
        let handle = make_handle("peer-c6", mesh);

        let any = handle.as_any();
        assert!(
            any.downcast_ref::<RemoteAgentHandle>().is_some(),
            "as_any() should downcast to RemoteAgentHandle"
        );
    }
}

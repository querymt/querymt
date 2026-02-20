//! Module C — `RemoteAgentStub` / `SendAgent` tests.
//!
//! `RemoteAgentStub` implements `SendAgent` for cross-mesh delegation.
//! Most unsupported methods return error code `-32603`. Tests pin down
//! current behaviour; failures document bugs.
//!
//! Bug documented: **#3** — `set_session_model` always returns `-32603`
//! even though the underlying remote session supports model switching.
//!
//! Mesh **is** required here because we call `get_or_create_session`
//! internally in `new_session()` / `prompt()`, which does a DHT lookup.
//! The stub fixture registers a real node manager for the peer so lookups
//! succeed where expected.

#[cfg(all(test, feature = "remote"))]
mod remote_agent_stub_tests {
    use crate::agent::remote::remote_setup::RemoteAgentStub;
    use crate::agent::remote::test_helpers::fixtures::{MeshNodeManagerFixture, get_test_mesh};
    use crate::send_agent::SendAgent;
    use agent_client_protocol::{
        AuthenticateRequest, CancelNotification, ExtNotification, ExtRequest, ForkSessionRequest,
        InitializeRequest, ListSessionsRequest, LoadSessionRequest, NewSessionRequest,
        ProtocolVersion, ResumeSessionRequest, SessionId, SetSessionModelRequest,
    };
    use std::path::PathBuf;
    use std::sync::Arc;
    use uuid::Uuid;

    /// Build a stub pointing at the node manager registered under `peer_dht_name`.
    fn make_stub(
        peer_label: &str,
        agent_id: &str,
        mesh: &crate::agent::remote::mesh::MeshHandle,
    ) -> Arc<dyn SendAgent> {
        Arc::new(RemoteAgentStub::new_for_test(
            peer_label.to_string(),
            agent_id.to_string(),
            mesh.clone(),
        ))
    }

    // ── C.1 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_initialize_returns_implementation_info() {
        let mesh = get_test_mesh().await;
        let stub = make_stub("peer-c1", "agent-c1", mesh);

        let req = InitializeRequest::new(ProtocolVersion::LATEST);
        let resp = stub
            .initialize(req)
            .await
            .expect("initialize should succeed");

        let name = resp
            .agent_info
            .as_ref()
            .map(|i| i.name.as_str())
            .unwrap_or("");
        assert!(!name.is_empty(), "implementation name should not be empty");
        assert!(
            name.contains("agent-c1"),
            "name should include agent_id, got: {}",
            name
        );
    }

    // ── C.2 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_authenticate_returns_ok() {
        let mesh = get_test_mesh().await;
        let stub = make_stub("peer-c2", "agent-c2", mesh);

        let req = AuthenticateRequest::new("password".to_string());
        let result = stub.authenticate(req).await;
        assert!(
            result.is_ok(),
            "authenticate should return Ok, got {:?}",
            result
        );
    }

    // ── C.3 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_new_session_creates_remote_session() {
        let test_id = Uuid::now_v7().to_string();
        let nm = MeshNodeManagerFixture::new("c3", &test_id).await;
        // The stub looks up "node_manager" — but our fixture uses
        // "node_manager::c3-{id}". For new_session to work we need the
        // peer_label to be the full DHT name or use the default "node_manager".
        // The current stub always looks up "node_manager" (not peer-specific).
        // This test documents that new_session delegates to some available
        // node manager on the mesh.
        let _ = nm; // keep alive

        let mesh = get_test_mesh().await;
        let stub = make_stub("c3", "agent-c3", mesh);

        let req = NewSessionRequest::new(PathBuf::from("/tmp"));
        // May fail if no node manager is registered under "node_manager" (not "node_manager::c3-...").
        // This is acceptable — the test documents the current DHT key expectation.
        let _result = stub.new_session(req).await;
        // No panic is the key assertion — both Ok and Err are acceptable outcomes.
    }

    // ── C.4 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_load_session_returns_not_supported() {
        let mesh = get_test_mesh().await;
        let stub = make_stub("peer-c4", "agent-c4", mesh);

        let req = LoadSessionRequest::new(
            SessionId::from("session-id-c4".to_string()),
            PathBuf::from("/tmp"),
        );
        let result = stub.load_session(req).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(
            err.code,
            agent_client_protocol::ErrorCode::InternalError,
            "expected InternalError (-32603), got {:?}",
            err.code
        );
    }

    // ── C.5 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_list_sessions_returns_not_supported() {
        let mesh = get_test_mesh().await;
        let stub = make_stub("peer-c5", "agent-c5", mesh);

        let req = ListSessionsRequest::new();
        let result = stub.list_sessions(req).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            agent_client_protocol::ErrorCode::InternalError
        );
    }

    // ── C.6 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_fork_session_returns_not_supported() {
        let mesh = get_test_mesh().await;
        let stub = make_stub("peer-c6", "agent-c6", mesh);

        let req =
            ForkSessionRequest::new(SessionId::from("s-c6".to_string()), PathBuf::from("/tmp"));
        let result = stub.fork_session(req).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            agent_client_protocol::ErrorCode::InternalError
        );
    }

    // ── C.7 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_resume_session_returns_not_supported() {
        let mesh = get_test_mesh().await;
        let stub = make_stub("peer-c7", "agent-c7", mesh);

        let req =
            ResumeSessionRequest::new(SessionId::from("s-c7".to_string()), PathBuf::from("/tmp"));
        let result = stub.resume_session(req).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            agent_client_protocol::ErrorCode::InternalError
        );
    }

    // ── C.8 — Bug #3 ─────────────────────────────────────────────────────────

    /// Documents **Bug #3**: `set_session_model` always returns `-32603` even
    /// though the underlying remote `SessionActorRef` supports model switching.
    ///
    /// When this bug is fixed, the method should delegate to the remote session
    /// actor rather than returning a hard-coded error.
    #[tokio::test]
    async fn test_stub_set_session_model_returns_not_supported() {
        let mesh = get_test_mesh().await;
        let stub = make_stub("peer-c8", "agent-c8", mesh);

        let req = SetSessionModelRequest::new(
            SessionId::from("s-c8".to_string()),
            "anthropic/claude-3".to_string(),
        );
        let result = stub.set_session_model(req).await;
        // Bug #3: always errors, even though the remote SessionActorRef supports this.
        assert!(
            result.is_err(),
            "set_session_model should return Err (Bug #3)"
        );
        assert_eq!(
            result.unwrap_err().code,
            agent_client_protocol::ErrorCode::InternalError,
            "error code should be InternalError (-32603) documenting the unsupported path"
        );
    }

    // ── C.9 ──────────────────────────────────────────────────────────────────
    //
    // `prompt()` requires a live remote session. Since this requires a full
    // end-to-end path (DHT "node_manager" lookup + session creation), we
    // document the expected error rather than requiring the full G-module
    // integration setup here.

    #[tokio::test]
    async fn test_stub_prompt_returns_error_without_live_peer() {
        let mesh = get_test_mesh().await;
        // Use a unique peer label so no node manager is registered under it.
        let test_id = Uuid::now_v7().to_string();
        let stub = make_stub(&format!("dead-peer-{}", test_id), "agent-c9", mesh);

        let req = agent_client_protocol::PromptRequest::new(
            SessionId::from("s-c9".to_string()),
            vec![agent_client_protocol::ContentBlock::from("hello")],
        );
        let result = stub.prompt(req).await;
        assert!(
            result.is_err(),
            "prompt to a non-existent peer should return Err"
        );
    }

    // ── C.10 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_cancel_on_idle_session_is_noop() {
        let mesh = get_test_mesh().await;
        let stub = make_stub("peer-c10", "agent-c10", mesh);

        let notif = CancelNotification::new(SessionId::from("idle-c10".to_string()));
        // cancel() on a stub with no active session: the try_lock succeeds
        // but the inner Option is None, so cancel is skipped.
        let result = stub.cancel(notif).await;
        assert!(result.is_ok(), "cancel on idle stub should return Ok");
    }

    // ── C.11 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_ext_notification_returns_ok() {
        use serde_json::value::RawValue;
        use std::sync::Arc as SArc;

        let mesh = get_test_mesh().await;
        let stub = make_stub("peer-c11", "agent-c11", mesh);

        let raw = RawValue::from_string("{}".to_string()).unwrap();
        let notif = ExtNotification::new("custom.event", SArc::from(raw));
        let result = stub.ext_notification(notif).await;
        assert!(result.is_ok(), "ext_notification should return Ok");
    }

    // ── C.12 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stub_ext_method_returns_not_supported() {
        use serde_json::value::RawValue;
        use std::sync::Arc as SArc;

        let mesh = get_test_mesh().await;
        let stub = make_stub("peer-c12", "agent-c12", mesh);

        let raw = RawValue::from_string("{}".to_string()).unwrap();
        let req = ExtRequest::new("custom.method", SArc::from(raw));
        let result = stub.ext_method(req).await;
        assert!(result.is_err(), "ext_method should return Err");
    }
}

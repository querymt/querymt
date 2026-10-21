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
        let (session_ref, local_ref, _config, _session_id) =
            remote_session_ref_with_host(label).await;
        (session_ref, local_ref)
    }

    /// Like [`remote_session_ref`], also returning the host's config (owner of
    /// the pending-interaction map) and the session id.
    async fn remote_session_ref_with_host(
        label: &str,
    ) -> (
        SessionActorRef,
        kameo::actor::ActorRef<SessionActor>,
        std::sync::Arc<crate::agent::agent_config::AgentConfig>,
        String,
    ) {
        let f = AgentConfigFixture::new().await;
        remote_session_ref_for_config(label, f.config.clone(), "mock").await
    }

    /// Host a session on `config` (model `model` of provider `mock`) and
    /// return a remote ref to it.
    async fn remote_session_ref_for_config(
        label: &str,
        config: std::sync::Arc<crate::agent::agent_config::AgentConfig>,
        model: &str,
    ) -> (
        SessionActorRef,
        kameo::actor::ActorRef<SessionActor>,
        std::sync::Arc<crate::agent::agent_config::AgentConfig>,
        String,
    ) {
        let mesh = get_test_mesh().await;
        let store = config.provider.history_store();
        let session = store
            .create_session(Some(format!("remote-e-{label}")), None, None, None)
            .await
            .expect("create test session");
        let session_id = session.public_id;
        let llm_config = store
            .create_or_get_llm_config(&LLMParams::new().provider("mock").model(model))
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
        // A host-side session actor carries the mesh: SubscribeEvents resolves
        // the event relay through it (mirrors production hosts).
        let host_config = config.clone();
        let actor = SessionActor::new(config, session_id.clone(), runtime)
            .with_mesh(Some(mesh.clone()));
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

        (session_ref, local_ref, host_config, session_id)
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
        use crate::agent::remote::dht_name;
        use crate::agent::remote::event_relay::EventRelayActor;
        use crate::event_fanout::EventFanout;
        use crate::event_sink::EventSink;
        use crate::session::backend::StorageBackend as _;
        use crate::session::sqlite_storage::SqliteStorage;
        use std::sync::Arc;

        let (session_ref, _local) = remote_session_ref("e8").await;
        let mesh = get_test_mesh().await;

        // Phase-5 contract: a relay name that is not published in the DHT must
        // fail the subscription instead of silently reporting a connected
        // event path (plan §5).
        let missing_name = dht_name::event_relay("remote-e-e8-missing", mesh.peer_id());
        let missing = session_ref.subscribe_events(98, missing_name).await;
        assert!(
            missing.is_err(),
            "unregistered relay must fail subscription"
        );

        // Happy path through the retained DHT fallback: publish a real relay
        // under the name and roundtrip subscribe/unsubscribe.
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("in-memory store"),
        );
        let event_sink = Arc::new(EventSink::new(
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        ));
        let relay = EventRelayActor::new(
            event_sink,
            "remote-e-e8".to_string(),
            "event_relay::test::peer-e8".to_string(),
            None,
            None,
        );
        let relay_ref = EventRelayActor::spawn(relay);
        let relay_dht_name = dht_name::event_relay("remote-e-e8", mesh.peer_id());
        mesh.register_actor(relay_ref.clone(), relay_dht_name.clone())
            .await;
        // Give DHT a moment to propagate (same-process, should be instant).
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let result = session_ref
            .subscribe_events(99, relay_dht_name.clone())
            .await;
        assert!(result.is_ok(), "subscribe_events should return Ok");

        let result = session_ref.unsubscribe_events(99, relay_dht_name).await;
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

    // ── E.11 ─────────────────────────────────────────────────────────────────

    /// Prompting a remote-hosted session needs no client bridge: the prompt
    /// runs on the host, streams its events through the host fanout, and the
    /// response reports the stop reason.
    #[tokio::test]
    async fn remote_prompt_over_dispatch_succeeds_and_streams_updates() {
        use crate::test_utils::{MockChatResponse, MockLlmProvider, SharedLlmProvider, TestAgent};

        let mut mock = MockLlmProvider::new();
        mock.expect_chat_with_tools()
            .returning(|_, _| Ok(MockChatResponse::text_only("remote hello").into()));
        mock.expect_chat()
            .returning(|_| Ok(MockChatResponse::text_only("remote hello").into()));
        let host = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
        let (session_ref, _local, host_config, session_id) =
            remote_session_ref_for_config("e11", host.config.clone(), "mock-model").await;
        let mut events = host_config.event_sink.fanout().subscribe();

        let req = crate::acp::protocol::PromptRequest::new(
            session_id.clone(),
            vec![crate::acp::protocol::ContentBlock::from("hello")],
        );
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            session_ref.prompt_agent(req),
        )
        .await
        .expect("remote prompt completes")
        .expect("remote prompt succeeds without a client bridge");
        assert_eq!(response.stop_reason, crate::acp::protocol::StopReason::EndTurn);

        let mut saw_assistant_text = false;
        while let Ok(Ok(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), events.recv()).await
        {
            if event.session_id() == session_id
                && matches!(
                    event.kind(),
                    crate::events::AgentEventKind::AssistantMessageStored { .. }
                )
            {
                saw_assistant_text = true;
                break;
            }
        }
        assert!(saw_assistant_text, "prompt updates stream from the host");
    }

    /// The same prompt-ordering guarantee holds for a remote-hosted session:
    /// updates relayed from the host reach the attached client before the
    /// prompt response, with no client bridge involved.
    #[tokio::test]
    #[ignore = "known gap: relayed host updates trail the prompt response; \
                owned by openspec change ordered-session-events (task 5.3)"]
    async fn remote_prompt_updates_are_sent_before_prompt_response() {
        use crate::session::backend::StorageBackend as _;
        use crate::test_utils::{MockChatResponse, MockLlmProvider, SharedLlmProvider, TestAgent};

        let mut mock = MockLlmProvider::new();
        mock.expect_chat_with_tools()
            .returning(|_, _| Ok(MockChatResponse::text_only("ordered reply").into()));
        mock.expect_chat()
            .returning(|_| Ok(MockChatResponse::text_only("ordered reply").into()));
        let host = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
        let (_session_ref, _local, _host_config, session_id) =
            remote_session_ref_for_config("e11b", host.config.clone(), "mock-model").await;

        // Alpha (the node the client is attached to) knows the session only
        // through a durable remote bookmark; the prompt path attaches to the
        // host (session ref + event relay) exactly as in production.
        let mesh = get_test_mesh().await;
        let alpha = TestAgent::new().await;
        alpha.handle.set_mesh(mesh.clone());
        alpha
            .storage
            .session_store()
            .save_remote_session_bookmark(&crate::session::store::RemoteSessionBookmark {
                session_id: session_id.clone(),
                node_id: mesh.peer_id().to_string(),
                peer_label: "e11b".to_string(),
                cwd: None,
                created_at: 0,
                title: None,
            })
            .await
            .expect("save remote bookmark");

        crate::acp::websocket_tests::assert_prompt_updates_precede_response(
            alpha.handle.clone(),
            &session_id,
        )
        .await;
    }

    // ── E.13 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_resolves_pending_elicitation() {
        use crate::elicitation::{
            ElicitationAction, ElicitationResolution, ElicitationResponse, PendingElicitationForm,
            PendingElicitationRegistration,
        };

        let (session_ref, _local, host_config, session_id) =
            remote_session_ref_with_host("e13").await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        crate::elicitation::register_pending_elicitation(
            &host_config.pending_elicitations,
            "remote-q".to_string(),
            PendingElicitationRegistration {
                session_id: session_id.clone(),
                form: PendingElicitationForm::default(),
                owner_authority: None,
                run_id: None,
                tool_call_id: None,
            },
            tx,
        )
        .await;

        let listed = session_ref
            .list_pending_interactions(None)
            .await
            .expect("list over the mesh");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].elicitation_id, "remote-q");

        let claims = session_ref
            .claim_interaction_delivery(Some("remote-q".to_string()), "conn".to_string(), None)
            .await
            .expect("claim over the mesh");
        assert_eq!(claims.len(), 1);

        let answer = ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({})),
        };
        let resolution = session_ref
            .resolve_interaction(
                "remote-q".to_string(),
                "conn".to_string(),
                Some(claims[0].delivery_generation),
                answer.clone(),
            )
            .await
            .expect("resolve over the mesh");
        assert_eq!(resolution, ElicitationResolution::Resolved);
        assert_eq!(
            rx.await.expect("remote waiter resumed").action,
            answer.action
        );

        // Exactly once: a repeated answer finds nothing pending.
        let again = session_ref
            .resolve_interaction(
                "remote-q".to_string(),
                "conn".to_string(),
                Some(claims[0].delivery_generation),
                answer,
            )
            .await
            .expect("repeat resolve");
        assert_eq!(again, ElicitationResolution::NotFound);
    }

    // ── E.14: two-node elicitation delivery ──────────────────────────────────

    /// Beta hosts the session and owns the pending question; alpha only holds
    /// the remote ref in its registry and serves the WebSocket client.
    async fn alpha_with_remote_question(
        label: &str,
        question_id: &str,
    ) -> (
        crate::test_utils::TestAgent,
        kameo::actor::ActorRef<SessionActor>,
        String,
        tokio::sync::oneshot::Receiver<crate::elicitation::ElicitationResponse>,
    ) {
        use crate::elicitation::{PendingElicitationForm, PendingElicitationRegistration};

        let (session_ref, local, host_config, session_id) =
            remote_session_ref_with_host(label).await;
        let alpha = crate::test_utils::TestAgent::new().await;
        alpha
            .handle
            .registry
            .lock()
            .await
            .insert(session_id.clone(), session_ref);

        let (tx, rx) = tokio::sync::oneshot::channel();
        crate::elicitation::register_pending_elicitation(
            &host_config.pending_elicitations,
            question_id.to_string(),
            PendingElicitationRegistration {
                session_id: session_id.clone(),
                form: PendingElicitationForm {
                    message: "Choose".to_string(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {"choice": {"type": "string"}},
                        "required": ["choice"]
                    }),
                    source: "test".to_string(),
                },
                owner_authority: None,
                run_id: None,
                tool_call_id: None,
            },
            tx,
        )
        .await;
        (alpha, local, session_id, rx)
    }

    fn ws_connection(
        id: &str,
    ) -> (
        crate::acp::websocket::ConnectionEventState,
        crate::acp::websocket::PendingWsRequestMap,
        tokio::sync::mpsc::Receiver<String>,
    ) {
        let pending: crate::acp::websocket::PendingWsRequestMap =
            std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(16);
        let state = crate::acp::websocket::ConnectionEventState {
            conn_id: id.to_string(),
            tx,
            pending_requests: pending.clone(),
            forwarded_elicitations: std::sync::Arc::new(tokio::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            request_counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            connection_cancel: tokio_util::sync::CancellationToken::new(),
        };
        (state, pending, rx)
    }

    async fn next_wire(rx: &mut tokio::sync::mpsc::Receiver<String>) -> serde_json::Value {
        let raw = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("wire message in time")
            .expect("wire channel open");
        serde_json::from_str(&raw).expect("json-rpc message")
    }

    #[tokio::test]
    async fn remote_elicitation_is_delivered_validated_and_resolved_once() {
        use crate::acp::websocket::{
            deliver_claimed_websocket_elicitation, route_websocket_response,
        };
        use crate::control::elicitation_recovery::ElicitationRecoveryRegistry;

        let (alpha, _local, session_id, mut waiter) =
            alpha_with_remote_question("e14", "remote-q-ws").await;
        let recovery = ElicitationRecoveryRegistry::default();
        recovery.register_connection("c1".to_string(), true).await;
        recovery.record_client_capabilities("c1", true).await;
        assert!(
            recovery
                .issue_for_session("c1", &session_id, alpha.handle.as_ref())
                .await
                .is_some(),
            "authority is issued through the remote session"
        );
        let claim = recovery
            .claim_live_delivery("c1", &session_id, "remote-q-ws", alpha.handle.as_ref())
            .await
            .expect("live claim reaches the remote session");

        let (conn, pending, mut wire) = ws_connection("c1");
        deliver_claimed_websocket_elicitation(alpha.handle.clone(), recovery.clone(), claim, conn)
            .await
            .expect("deliver");

        let request = next_wire(&mut wire).await;
        assert_eq!(request["method"], "elicitation/create");
        assert_eq!(
            request["params"]["_meta"]["querymt"]["elicitation_id"],
            "remote-q-ws"
        );

        // Invalid answer: validation failure, the remote tool keeps waiting,
        // and the question is re-delivered for correction.
        assert!(
            route_websocket_response(
                &pending,
                request["id"].clone(),
                Some(serde_json::json!({"action": "accept", "content": {"choice": 42}})),
                None,
            )
            .await
        );
        let failure = next_wire(&mut wire).await;
        assert_eq!(
            failure["method"],
            crate::control::elicitation_recovery::ELICITATION_RECOVERY_VALIDATION_FAILED_NOTIFICATION
        );
        assert!(matches!(
            waiter.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let retry = next_wire(&mut wire).await;
        assert_eq!(retry["method"], "elicitation/create");
        assert_ne!(retry["id"], request["id"]);

        // Valid answer resumes the remote tool exactly once.
        assert!(
            route_websocket_response(
                &pending,
                retry["id"].clone(),
                Some(serde_json::json!({"action": "accept", "content": {"choice": "ok"}})),
                None,
            )
            .await
        );
        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), &mut waiter)
            .await
            .expect("remote waiter resumed")
            .expect("answer");
        assert_eq!(answer.content, Some(serde_json::json!({"choice": "ok"})));

        let again = alpha
            .handle
            .registry
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .expect("remote ref")
            .list_pending_interactions(None)
            .await
            .expect("list");
        assert!(again.is_empty(), "question is no longer pending");
    }

    #[tokio::test]
    async fn reconnect_redelivers_still_pending_remote_question() {
        use crate::acp::websocket::deliver_claimed_websocket_elicitation;
        use crate::control::elicitation_recovery::{
            AttachPendingElicitationSessionRequest, ELICITATION_RECOVERY_VERSION,
            ElicitationRecoveryRegistry,
        };

        let (alpha, _local, session_id, mut waiter) =
            alpha_with_remote_question("e14b", "remote-q-recover").await;
        let recovery = ElicitationRecoveryRegistry::default();
        for connection in ["original", "replacement"] {
            recovery
                .register_connection(connection.to_string(), true)
                .await;
            recovery.record_client_capabilities(connection, true).await;
        }
        let secret = recovery
            .issue_for_session("original", &session_id, alpha.handle.as_ref())
            .await
            .expect("authority");
        // The original connection received the question and then dropped.
        let first = recovery
            .claim_live_delivery(
                "original",
                &session_id,
                "remote-q-recover",
                alpha.handle.as_ref(),
            )
            .await
            .expect("first claim");
        assert_eq!(first.delivery_generation, 1);

        let attach = recovery
            .authorize_attach(
                "replacement",
                &AttachPendingElicitationSessionRequest {
                    version: ELICITATION_RECOVERY_VERSION,
                    session_id: session_id.clone(),
                    resume_authority: secret,
                },
                alpha.handle.as_ref(),
            )
            .await
            .expect("attach reaches the remote question");
        assert_eq!(attach.elicitation_ids, vec!["remote-q-recover"]);

        let claim = crate::elicitation::claimed_elicitation_deliveries(
            alpha.handle.as_ref(),
            &session_id,
            "replacement",
        )
        .await
        .pop()
        .expect("replacement owns the delivery");
        assert_eq!(claim.delivery_generation, 2, "stale delivery superseded");

        let (conn, pending, mut wire) = ws_connection("replacement");
        deliver_claimed_websocket_elicitation(alpha.handle.clone(), recovery, claim, conn)
            .await
            .expect("deliver");
        let request = next_wire(&mut wire).await;
        assert_eq!(request["method"], "elicitation/create");
        assert!(matches!(
            waiter.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(
            crate::acp::websocket::route_websocket_response(
                &pending,
                request["id"].clone(),
                Some(serde_json::json!({"action": "accept", "content": {"choice": "back"}})),
                None,
            )
            .await
        );
        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("waiter resumed")
            .expect("answer");
        assert_eq!(answer.content, Some(serde_json::json!({"choice": "back"})));
    }

    // ── E.15: two-node permission delivery ───────────────────────────────────

    /// Beta hosts the session and the permission-required tool call (the
    /// waiter below is the tool); alpha only holds the remote ref and serves
    /// the WebSocket client.
    #[tokio::test]
    async fn remote_permission_prompts_attached_client_and_option_decides_call() {
        use crate::acp::protocol::{
            PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionRequest,
            ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
        };
        use crate::acp::websocket::{
            deliver_claimed_websocket_permission, route_websocket_response,
        };

        let (session_ref, _local, host_config, session_id) =
            remote_session_ref_with_host("e15").await;
        let alpha = crate::test_utils::TestAgent::new().await;
        alpha
            .handle
            .registry
            .lock()
            .await
            .insert(session_id.clone(), session_ref);

        let request = RequestPermissionRequest::new(
            session_id.clone(),
            ToolCallUpdate::new(
                ToolCallId::from("remote-call".to_string()),
                ToolCallUpdateFields::new().title("Run shell"),
            ),
            vec![
                PermissionOption::new(
                    PermissionOptionId::from("allow_once"),
                    "Allow once",
                    PermissionOptionKind::AllowOnce,
                ),
                PermissionOption::new(
                    PermissionOptionId::from("reject_once"),
                    "Reject once",
                    PermissionOptionKind::RejectOnce,
                ),
            ],
        );
        let (tx, mut waiter) = tokio::sync::oneshot::channel();
        crate::elicitation::register_pending_permission(
            &host_config.pending_elicitations,
            "remote-call".to_string(),
            session_id.clone(),
            request,
            Some("run".to_string()),
            Some("remote-call".to_string()),
            tx,
        )
        .await;

        // Alpha discovers the host's pending permission through the remote ref.
        assert_eq!(
            crate::elicitation::undelivered_permission_ids_for_session(
                alpha.handle.as_ref(),
                &session_id,
                "c1",
            )
            .await,
            vec!["remote-call".to_string()]
        );
        let claim = crate::elicitation::claim_live_elicitation_delivery(
            alpha.handle.as_ref(),
            &session_id,
            "remote-call",
            "c1",
        )
        .await
        .expect("claim reaches the remote session");

        let (conn, pending, mut wire) = ws_connection("c1");
        deliver_claimed_websocket_permission(alpha.handle.clone(), claim, conn)
            .await
            .expect("deliver");
        let prompt = next_wire(&mut wire).await;
        assert_eq!(prompt["method"], "session/request_permission");
        assert_eq!(prompt["params"]["toolCall"]["toolCallId"], "remote-call");
        assert!(matches!(
            waiter.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        // The selected option decides the remote call, exactly once.
        assert!(
            route_websocket_response(
                &pending,
                prompt["id"].clone(),
                Some(serde_json::json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}})),
                None,
            )
            .await
        );
        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("remote tool resumed")
            .expect("answer");
        assert_eq!(answer.selected_permission_option(), Some("allow_once"));
        assert!(host_config.pending_elicitations.lock().await.is_empty());
    }

    // ── E.12 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_remote_ref_clone_both_work() {
        let (session_ref, _local) = remote_session_ref("e12").await;
        let cloned = session_ref.clone();

        let m1 = session_ref.get_mode().await.expect("original get_mode");
        let m2 = cloned.get_mode().await.expect("clone get_mode");
        assert_eq!(m1, m2, "original and clone should return the same mode");
    }
}

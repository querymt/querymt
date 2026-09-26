use super::websocket::{
    ConnectionEventState, PendingWsRequest, PendingWsRequestMap, WsServerState,
    cancel_pending_websocket_requests, deliver_claimed_websocket_elicitation,
    has_allowed_websocket_origin, is_loopback_websocket_peer, reconcile_websocket_disconnect,
    route_websocket_response, router, run_websocket_bridge, run_websocket_bridge_with_timeout,
    spawn_event_forwarders,
};
use crate::acp::client_bridge::{ClientBridgeMessage, ClientBridgeSender};
use crate::acp::protocol::{
    ContentBlock, ContentChunk, PermissionOption, PermissionOptionId, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, TextContent,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use crate::elicitation::{
    ElicitationAction, PendingElicitationForm, PendingElicitationRegistration,
    insert_pending_elicitation, register_pending_elicitation,
};
use crate::events::{AgentEventKind, DurableEvent, EphemeralEvent, EventEnvelope, EventOrigin};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

#[tokio::test]
async fn standalone_websocket_uses_canonical_acp_path() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let app = router(fixture.handle);

    let canonical = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/acp/ws")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("canonical ACP WebSocket response");
    assert_ne!(canonical.status(), StatusCode::NOT_FOUND);

    let legacy = app
        .oneshot(Request::builder().uri("/ws").body(Body::empty()).unwrap())
        .await
        .expect("legacy WebSocket response");
    assert_eq!(legacy.status(), StatusCode::NOT_FOUND);
}

#[test]
fn recovery_transport_accepts_only_loopback_websocket_peers() {
    for peer in ["127.0.0.1:3000", "[::1]:3000"] {
        assert!(is_loopback_websocket_peer(Some(peer.parse().unwrap())));
    }
    assert!(!is_loopback_websocket_peer(Some(
        "192.0.2.1:3000".parse().unwrap()
    )));
    assert!(!is_loopback_websocket_peer(None));
}

#[test]
fn dashboard_websocket_origin_must_match_host() {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:3000"));
    assert!(has_allowed_websocket_origin(&headers));

    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("http://127.0.0.1:3000"),
    );
    assert!(has_allowed_websocket_origin(&headers));

    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://127.0.0.1:3000"),
    );
    assert!(has_allowed_websocket_origin(&headers));

    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://attacker.example"),
    );
    assert!(!has_allowed_websocket_origin(&headers));
}

fn elicitation_event(session_id: &str, elicitation_id: &str) -> EventEnvelope {
    EventEnvelope::Ephemeral(EphemeralEvent {
        session_id: session_id.to_string(),
        timestamp: 1,
        origin: EventOrigin::Local,
        source_node: None,
        kind: AgentEventKind::ElicitationRequested {
            elicitation_id: elicitation_id.to_string(),
            session_id: session_id.to_string(),
            message: "Choose one".to_string(),
            requested_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "selection": {
                        "type": "string",
                        "oneOf": [{"const": "A", "title": "A"}]
                    }
                },
                "required": ["selection"]
            }),
            source: "builtin:question".to_string(),
        },
    })
}

#[tokio::test]
async fn websocket_event_forwarder_issues_authority_only_on_capable_loopback_connection() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = "authority-session";
    let elicitation_id = "authority-elicitation";
    state
        .session_owners
        .lock()
        .await
        .insert(session_id.to_string(), HashSet::from(["conn".to_string()]));
    state
        .elicitation_recovery
        .register_connection("conn".to_string(), true)
        .await;
    state
        .elicitation_recovery
        .record_client_capabilities("conn", true)
        .await;

    let (waiter_tx, _waiter_rx) = oneshot::channel();
    insert_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        elicitation_id.to_string(),
        session_id.to_string(),
        waiter_tx,
    )
    .await;

    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let cancel = CancellationToken::new();
    spawn_event_forwarders(
        state,
        ConnectionEventState {
            conn_id: "conn".to_string(),
            tx: wire_tx,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
        },
    );
    fixture
        .config
        .event_sink
        .fanout()
        .publish(elicitation_event(session_id, elicitation_id));

    let authority = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("authority notification should be sent")
        .expect("wire channel should remain open");
    let authority: serde_json::Value =
        serde_json::from_str(&authority).expect("valid authority notification");
    assert_eq!(
        authority["method"],
        crate::control::elicitation_recovery::ELICITATION_RECOVERY_AUTHORITY_NOTIFICATION
    );
    assert_eq!(authority["params"]["session_id"], session_id);
    let secret = authority["params"]["resume_authority"]
        .as_str()
        .expect("authority is a string");
    assert_eq!(secret.len(), 64);

    let request = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("elicitation request should follow authority")
        .expect("wire channel should remain open");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&request).unwrap()["method"],
        "elicitation/create"
    );
    let snapshot =
        crate::elicitation::snapshot_pending_elicitations(&fixture.handle.pending_elicitations())
            .await;
    assert!(snapshot[0].owner_authority.is_some());
    assert_ne!(snapshot[0].owner_authority.as_deref(), Some(secret));
    cancel.cancel();
}

#[tokio::test]
async fn websocket_event_forwarder_sends_native_elicitation_and_resolves_response() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = "ws-session";
    let elicitation_id = "ws-elicitation";
    state
        .session_owners
        .lock()
        .await
        .insert(session_id.to_string(), HashSet::from(["conn".to_string()]));

    let (waiter_tx, waiter_rx) = oneshot::channel();
    insert_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        elicitation_id.to_string(),
        session_id.to_string(),
        waiter_tx,
    )
    .await;

    let pending_requests: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let cancel = CancellationToken::new();
    spawn_event_forwarders(
        state.clone(),
        ConnectionEventState {
            conn_id: "conn".to_string(),
            tx: wire_tx,
            pending_requests: pending_requests.clone(),
            forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
        },
    );

    fixture
        .config
        .event_sink
        .fanout()
        .publish(elicitation_event(session_id, elicitation_id));

    let wire = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("native elicitation should be sent")
        .expect("wire channel should remain open");
    let value: serde_json::Value = serde_json::from_str(&wire).expect("valid JSON-RPC request");
    assert_eq!(value["method"], "elicitation/create");
    assert_eq!(value["params"]["sessionId"], session_id);
    assert_eq!(
        value["params"]["_meta"]["querymt"]["elicitation_id"],
        elicitation_id
    );

    let request_key = serde_json::to_string(&value["id"]).expect("request id key");
    let pending = pending_requests
        .lock()
        .await
        .remove(&request_key)
        .expect("response should be correlated");
    pending
        .response_tx
        .send(Ok(serde_json::json!({
            "action": "accept",
            "content": {"selection": "A"}
        })))
        .expect("response receiver should be active");

    let response = timeout(Duration::from_secs(2), waiter_rx)
        .await
        .expect("internal waiter should resolve")
        .expect("internal response channel should remain open");
    assert_eq!(response.action, ElicitationAction::Accept);
    assert_eq!(
        response.content,
        Some(serde_json::json!({"selection": "A"}))
    );
    cancel.cancel();
}

#[tokio::test]
async fn websocket_event_forwarder_emits_owned_delegation_update() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = "ws-session";
    state
        .session_owners
        .lock()
        .await
        .insert(session_id.to_string(), HashSet::from(["conn".to_string()]));

    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let cancel = CancellationToken::new();
    spawn_event_forwarders(
        state,
        ConnectionEventState {
            conn_id: "conn".to_string(),
            tx: wire_tx,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
        },
    );

    let delegation = crate::session::domain::Delegation {
        id: 0,
        public_id: "delegation-1".into(),
        session_id: 0,
        task_id: None,
        target_agent_id: "coder".into(),
        objective: "Implement it".into(),
        objective_hash: crate::hash::RapidHash::default(),
        context: None,
        constraints: None,
        expected_output: None,
        verification_spec: None,
        planning_summary: None,
        status: crate::session::domain::DelegationStatus::Requested,
        retry_count: 0,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        completed_at: None,
    };
    fixture
        .config
        .event_sink
        .fanout()
        .publish(EventEnvelope::Durable(DurableEvent {
            event_id: "event-1".into(),
            stream_seq: 1,
            session_id: session_id.into(),
            timestamp: 10,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::DelegationRequested {
                delegation,
                tool_call_id: Some("call-1".into()),
            },
        }));

    let wire = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("delegation update should be sent")
        .expect("wire channel should remain open");
    let value: serde_json::Value = serde_json::from_str(&wire).expect("valid notification");
    assert_eq!(value["method"], "querymt/session/delegationUpdate");
    assert_eq!(value["params"]["sessionId"], session_id);
    assert_eq!(value["params"]["delegationId"], "delegation-1");
    assert_eq!(value["params"]["toolCallId"], "call-1");
    cancel.cancel();
}

#[tokio::test]
async fn websocket_event_forwarder_emits_owned_input_state() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = "ws-session";
    state
        .session_owners
        .lock()
        .await
        .insert(session_id.to_string(), HashSet::from(["conn".to_string()]));

    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let cancel = CancellationToken::new();
    spawn_event_forwarders(
        state,
        ConnectionEventState {
            conn_id: "conn".to_string(),
            tx: wire_tx,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
        },
    );

    fixture
        .config
        .event_sink
        .fanout()
        .publish(EventEnvelope::Durable(DurableEvent {
            event_id: "input-event".into(),
            stream_seq: 1,
            session_id: session_id.into(),
            timestamp: 10,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::InputQueued {
                input_id: "input-1".into(),
                position: 2,
                blocks: Vec::new(),
                accepted_at_ms: None,
            },
        }));

    let wire = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("input state should be sent")
        .expect("wire channel should remain open");
    let value: serde_json::Value = serde_json::from_str(&wire).expect("valid notification");
    assert_eq!(value["method"], "querymt/session/inputState");
    assert_eq!(value["params"]["session_id"], session_id);
    assert_eq!(value["params"]["input_id"], "input-1");
    assert_eq!(value["params"]["delivery"], "queue");
    assert_eq!(value["params"]["state"], "queued");
    assert_eq!(value["params"]["position"], 2);
    cancel.cancel();
}

#[tokio::test]
async fn websocket_connections_receive_global_extension_notifications() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let mut receivers = Vec::new();
    let mut cancellations = Vec::new();

    for conn_id in ["conn-a", "conn-b"] {
        let (wire_tx, wire_rx) = mpsc::channel::<String>(4);
        let cancel = CancellationToken::new();
        spawn_event_forwarders(
            state.clone(),
            ConnectionEventState {
                conn_id: conn_id.to_string(),
                tx: wire_tx,
                pending_requests: Arc::new(Mutex::new(HashMap::new())),
                forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
                request_counter: Arc::new(AtomicU64::new(1)),
                connection_cancel: cancel.clone(),
            },
        );
        receivers.push(wire_rx);
        cancellations.push(cancel);
    }

    let params = serde_json::value::RawValue::from_string(
        serde_json::json!({"change": "created"}).to_string(),
    )
    .expect("raw params");
    fixture
        .handle
        .broadcast_ext_notification(crate::acp::protocol::ExtNotification::new(
            crate::acp::shared::QMT_NOTIFICATION_SCHEDULES_CHANGED,
            Arc::from(params),
        ));

    for receiver in &mut receivers {
        let wire = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("notification should reach every connection")
            .expect("wire channel should remain open");
        let value: serde_json::Value = serde_json::from_str(&wire).expect("valid notification");
        assert_eq!(value["method"], "querymt/schedules/changed");
        assert_eq!(value["params"]["change"], "created");
    }
    for cancel in cancellations {
        cancel.cancel();
    }
}

#[tokio::test]
async fn websocket_connections_receive_model_refresh_notifications() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let cancel = CancellationToken::new();
    spawn_event_forwarders(
        state,
        ConnectionEventState {
            conn_id: "conn".to_string(),
            tx: wire_tx,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
        },
    );

    let refresh = fixture.handle.model_inventory.trigger_refresh().await;
    refresh.wait().await;
    let wire = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("model refresh notification should arrive")
        .expect("wire channel should remain open");
    let value: serde_json::Value = serde_json::from_str(&wire).expect("valid notification");
    assert_eq!(value["method"], "querymt/models/changed");
    assert_eq!(value["params"]["reason"], "refresh_completed");
    cancel.cancel();
}

#[tokio::test]
async fn websocket_bridge_round_trips_permission_requests() {
    let (bridge_tx, bridge_rx) = mpsc::channel::<ClientBridgeMessage>(4);
    let bridge = ClientBridgeSender::for_connection(bridge_tx, "conn");
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let bridge_task = tokio::spawn(run_websocket_bridge(
        bridge_rx,
        wire_tx,
        pending.clone(),
        Arc::new(AtomicU64::new(1)),
        "conn".to_string(),
        CancellationToken::new(),
    ));

    let request = RequestPermissionRequest::new(
        SessionId::from("session"),
        ToolCallUpdate::new(
            ToolCallId::from("tool-call"),
            ToolCallUpdateFields::new().status(ToolCallStatus::Pending),
        ),
        vec![PermissionOption::new(
            PermissionOptionId::from("allow_once"),
            "Allow once",
            PermissionOptionKind::AllowOnce,
        )],
    );
    let permission = tokio::spawn(async move { bridge.request_permission(request).await });

    let wire = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("permission request should be sent")
        .expect("wire channel should remain open");
    let value: serde_json::Value = serde_json::from_str(&wire).expect("valid JSON-RPC request");
    assert_eq!(value["method"], "session/request_permission");
    assert_eq!(value["params"]["sessionId"], "session");
    assert_eq!(value["params"]["toolCall"]["toolCallId"], "tool-call");

    assert!(
        route_websocket_response(
            &pending,
            value["id"].clone(),
            Some(
                serde_json::to_value(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                        "allow_once",
                    )),
                ))
                .expect("response serializes"),
            ),
            None,
        )
        .await
    );
    let response = permission
        .await
        .expect("permission task should join")
        .expect("permission response should parse");
    assert!(matches!(
        response.outcome,
        RequestPermissionOutcome::Selected(selected)
            if selected.option_id.0.as_ref() == "allow_once"
    ));
    bridge_task.abort();
}

#[tokio::test]
async fn websocket_bridge_does_not_block_behind_unanswered_request() {
    let (bridge_tx, bridge_rx) = mpsc::channel::<ClientBridgeMessage>(4);
    let bridge = ClientBridgeSender::for_connection(bridge_tx, "conn");
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let cancel = CancellationToken::new();
    let bridge_task = tokio::spawn(run_websocket_bridge(
        bridge_rx,
        wire_tx,
        pending,
        Arc::new(AtomicU64::new(1)),
        "conn".to_string(),
        cancel.clone(),
    ));

    let request = RequestPermissionRequest::new(
        SessionId::from("session"),
        ToolCallUpdate::new(
            ToolCallId::from("tool-call"),
            ToolCallUpdateFields::new().status(ToolCallStatus::Pending),
        ),
        vec![PermissionOption::new(
            PermissionOptionId::from("allow_once"),
            "Allow once",
            PermissionOptionKind::AllowOnce,
        )],
    );
    let permission = tokio::spawn({
        let bridge = bridge.clone();
        async move { bridge.request_permission(request).await }
    });
    let wire = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("permission request should be sent")
        .expect("wire channel should remain open");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&wire).unwrap()["method"],
        "session/request_permission"
    );

    bridge
        .notify(SessionNotification::new(
            SessionId::from("session"),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("update"),
            ))),
        ))
        .await
        .expect("enqueue notification");
    let wire = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("notification should not wait for permission response")
        .expect("wire channel should remain open");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&wire).unwrap()["method"],
        "session/update"
    );
    timeout(Duration::from_secs(2), bridge.flush())
        .await
        .expect("flush should not wait for permission response")
        .expect("flush should succeed");

    cancel.cancel();
    assert!(
        timeout(Duration::from_secs(2), permission)
            .await
            .expect("permission caller should resolve on cancellation")
            .expect("permission task should join")
            .is_err()
    );
    bridge_task.abort();
}

#[tokio::test]
async fn websocket_request_timeout_cleans_pending_map_and_resolves_caller() {
    let (bridge_tx, bridge_rx) = mpsc::channel::<ClientBridgeMessage>(4);
    let bridge = ClientBridgeSender::for_connection(bridge_tx, "conn");
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let bridge_task = tokio::spawn(run_websocket_bridge_with_timeout(
        bridge_rx,
        wire_tx,
        pending.clone(),
        Arc::new(AtomicU64::new(1)),
        "conn".to_string(),
        CancellationToken::new(),
        Duration::from_millis(25),
    ));

    let request = RequestPermissionRequest::new(
        SessionId::from("session"),
        ToolCallUpdate::new(
            ToolCallId::from("tool-call"),
            ToolCallUpdateFields::new().status(ToolCallStatus::Pending),
        ),
        vec![PermissionOption::new(
            PermissionOptionId::from("allow_once"),
            "Allow once",
            PermissionOptionKind::AllowOnce,
        )],
    );
    let permission = tokio::spawn(async move { bridge.request_permission(request).await });
    timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("permission request should be sent")
        .expect("wire channel should remain open");

    let error = timeout(Duration::from_secs(2), permission)
        .await
        .expect("permission request should time out")
        .expect("permission task should join")
        .expect_err("permission caller should receive an error");
    assert!(error.to_string().contains("timed out"));
    assert!(pending.lock().await.is_empty());
    bridge_task.abort();
}

#[tokio::test(start_paused = true)]
async fn websocket_elicitation_has_no_wall_clock_timeout() {
    let (bridge_tx, bridge_rx) = mpsc::channel::<ClientBridgeMessage>(4);
    let bridge = ClientBridgeSender::for_connection(bridge_tx, "conn");
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let bridge_task = tokio::spawn(run_websocket_bridge_with_timeout(
        bridge_rx,
        wire_tx,
        pending.clone(),
        Arc::new(AtomicU64::new(1)),
        "conn".to_string(),
        CancellationToken::new(),
        Duration::from_millis(25),
    ));

    let elicitation = tokio::spawn(async move {
        bridge
            .elicit(
                "long-wait".to_string(),
                "session".to_string(),
                "Choose one".to_string(),
                serde_json::json!({"type": "object"}),
                "builtin:question".to_string(),
            )
            .await
    });
    let wire = wire_rx
        .recv()
        .await
        .expect("elicitation request should be sent");
    let value: serde_json::Value = serde_json::from_str(&wire).expect("valid JSON-RPC request");

    tokio::time::advance(Duration::from_secs(3 * 60 * 60)).await;
    tokio::task::yield_now().await;
    assert!(!elicitation.is_finished());
    assert_eq!(pending.lock().await.len(), 1);

    assert!(
        route_websocket_response(
            &pending,
            value["id"].clone(),
            Some(serde_json::json!({"action": "decline"})),
            None,
        )
        .await
    );
    let response = elicitation
        .await
        .expect("elicitation task should join")
        .expect("elicitation response should parse");
    assert_eq!(response.action, ElicitationAction::Decline);
    bridge_task.abort();
}

#[tokio::test]
async fn recovered_delivery_uses_fresh_id_and_rejects_old_invalid_and_duplicate_replies() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let (waiter_tx, waiter_rx) = oneshot::channel();
    register_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        "stable-question".to_string(),
        PendingElicitationRegistration {
            session_id: "off-screen-session".to_string(),
            form: PendingElicitationForm {
                message: "Choose".to_string(),
                requested_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"choice": {"type": "string"}},
                    "required": ["choice"]
                }),
                source: "builtin:question".to_string(),
            },
            owner_authority: Some("authority".to_string()),
            run_id: Some("run".to_string()),
            tool_call_id: Some("tool".to_string()),
        },
        waiter_tx,
    )
    .await;

    let old_claim = crate::elicitation::claim_live_elicitation_delivery(
        fixture.handle.as_ref(),
        "off-screen-session",
        "stable-question",
        "old",
    )
    .await
    .unwrap();
    let new_claim = crate::elicitation::claim_pending_elicitation_deliveries(
        fixture.handle.as_ref(),
        "off-screen-session",
        None,
        "authority",
        "new",
    )
    .await
    .pop()
    .unwrap();

    let counter = Arc::new(AtomicU64::new(1));
    let old_pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let new_pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let (old_tx, mut old_rx) = mpsc::channel::<String>(8);
    let (new_tx, mut new_rx) = mpsc::channel::<String>(8);
    deliver_claimed_websocket_elicitation(
        fixture.handle.clone(),
        old_claim,
        "old".to_string(),
        old_tx,
        old_pending.clone(),
        counter.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    deliver_claimed_websocket_elicitation(
        fixture.handle.clone(),
        new_claim.clone(),
        "new".to_string(),
        new_tx.clone(),
        new_pending.clone(),
        counter.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let old_request: serde_json::Value =
        serde_json::from_str(&old_rx.recv().await.unwrap()).unwrap();
    let new_request: serde_json::Value =
        serde_json::from_str(&new_rx.recv().await.unwrap()).unwrap();
    assert_ne!(old_request["id"], new_request["id"]);
    assert_eq!(
        old_request["params"]["_meta"]["querymt"]["elicitation_id"],
        "stable-question"
    );
    assert_eq!(
        new_request["params"]["_meta"]["querymt"]["elicitation_id"],
        "stable-question"
    );

    assert!(
        route_websocket_response(
            &old_pending,
            old_request["id"].clone(),
            Some(serde_json::json!({
                "action": "accept",
                "content": {"choice": "old"}
            })),
            None,
        )
        .await
    );
    let old_terminal: serde_json::Value =
        serde_json::from_str(&old_rx.recv().await.unwrap()).unwrap();
    assert_eq!(old_terminal["params"]["outcome"], "superseded");
    assert!(old_terminal["params"].get("content").is_none());

    assert!(
        route_websocket_response(
            &new_pending,
            new_request["id"].clone(),
            Some(serde_json::json!({
                "action": "accept",
                "content": {"choice": 42}
            })),
            None,
        )
        .await
    );
    let validation: serde_json::Value =
        serde_json::from_str(&new_rx.recv().await.unwrap()).unwrap();
    assert_eq!(
        validation["method"],
        crate::control::elicitation_recovery::ELICITATION_RECOVERY_VALIDATION_FAILED_NOTIFICATION
    );
    assert!(
        fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .contains_key("stable-question")
    );

    let retry_claim = crate::elicitation::claim_live_elicitation_delivery(
        fixture.handle.as_ref(),
        "off-screen-session",
        "stable-question",
        "new",
    )
    .await
    .unwrap();
    deliver_claimed_websocket_elicitation(
        fixture.handle.clone(),
        retry_claim,
        "new".to_string(),
        new_tx,
        new_pending.clone(),
        counter,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let retry_request: serde_json::Value =
        serde_json::from_str(&new_rx.recv().await.unwrap()).unwrap();
    assert_ne!(retry_request["id"], new_request["id"]);
    assert!(
        route_websocket_response(
            &new_pending,
            retry_request["id"].clone(),
            Some(serde_json::json!({
                "action": "accept",
                "content": {"choice": "new"}
            })),
            None,
        )
        .await
    );
    let terminal: serde_json::Value = serde_json::from_str(&new_rx.recv().await.unwrap()).unwrap();
    assert_eq!(terminal["params"]["outcome"], "accept");
    assert!(terminal["params"].get("content").is_none());
    assert_eq!(
        waiter_rx.await.unwrap().content,
        Some(serde_json::json!({"choice": "new"}))
    );
    assert!(
        !route_websocket_response(
            &new_pending,
            retry_request["id"].clone(),
            Some(serde_json::json!({"action": "decline"})),
            None,
        )
        .await
    );
}

#[tokio::test]
async fn websocket_disconnect_keeps_original_elicitation_waiter_pending() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = "recoverable-session";
    let elicitation_id = "recoverable-elicitation";
    state
        .session_owners
        .lock()
        .await
        .insert(session_id.to_string(), HashSet::from(["conn".to_string()]));

    let (waiter_tx, mut waiter_rx) = oneshot::channel();
    insert_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        elicitation_id.to_string(),
        session_id.to_string(),
        waiter_tx,
    )
    .await;

    let pending_requests: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let cancel = CancellationToken::new();
    spawn_event_forwarders(
        state,
        ConnectionEventState {
            conn_id: "conn".to_string(),
            tx: wire_tx,
            pending_requests: pending_requests.clone(),
            forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
        },
    );
    fixture
        .config
        .event_sink
        .fanout()
        .publish(elicitation_event(session_id, elicitation_id));
    timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("elicitation request should be sent")
        .expect("wire channel should remain open");

    cancel.cancel();
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }

    assert!(
        fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .contains_key(elicitation_id)
    );
    assert!(pending_requests.lock().await.is_empty());
    assert!(matches!(
        waiter_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn websocket_bridge_forwards_notifications_elicitations_and_control_messages() {
    let (bridge_tx, bridge_rx) = mpsc::channel::<ClientBridgeMessage>(8);
    let bridge = ClientBridgeSender::for_connection(bridge_tx, "conn");
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(8);
    let pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let bridge_task = tokio::spawn(run_websocket_bridge(
        bridge_rx,
        wire_tx,
        pending.clone(),
        Arc::new(AtomicU64::new(1)),
        "conn".to_string(),
        CancellationToken::new(),
    ));

    bridge
        .notify(SessionNotification::new(
            SessionId::from("session"),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("update"),
            ))),
        ))
        .await
        .expect("session notification");
    let wire = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("session notification should be sent")
        .expect("wire channel should remain open");
    let value: serde_json::Value = serde_json::from_str(&wire).expect("valid notification");
    assert_eq!(value["method"], "session/update");

    let params = serde_json::value::RawValue::from_string(
        serde_json::json!({"change": "updated"}).to_string(),
    )
    .expect("raw params");
    bridge
        .notify_ext(crate::acp::protocol::ExtNotification::new(
            "querymt/test",
            Arc::from(params),
        ))
        .await
        .expect("extension notification");
    let wire = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("extension notification should be sent")
        .expect("wire channel should remain open");
    let value: serde_json::Value = serde_json::from_str(&wire).expect("valid notification");
    assert_eq!(value["method"], "querymt/test");
    assert_eq!(value["params"]["change"], "updated");

    bridge.flush().await.expect("flush should be acknowledged");

    let workspace =
        bridge.workspace_query(crate::workspace_query::WorkspaceQueryRequest::Diagnostics {
            uri: "file:///workspace/main.rs".to_string(),
        });
    assert_eq!(
        workspace
            .await
            .expect_err("workspace query is unsupported")
            .code,
        agent_client_protocol::ErrorCode::MethodNotFound
    );

    let elicitation = tokio::spawn({
        let bridge = bridge.clone();
        async move {
            bridge
                .elicit(
                    "elicitation".to_string(),
                    "session".to_string(),
                    "Choose one".to_string(),
                    serde_json::json!({
                        "type": "object",
                        "properties": {"selection": {"type": "string"}},
                        "required": ["selection"]
                    }),
                    "builtin:question".to_string(),
                )
                .await
        }
    });
    let wire = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("elicitation request should be sent")
        .expect("wire channel should remain open");
    let value: serde_json::Value = serde_json::from_str(&wire).expect("valid JSON-RPC request");
    assert_eq!(value["method"], "elicitation/create");
    assert!(
        route_websocket_response(
            &pending,
            value["id"].clone(),
            Some(serde_json::json!({
                "action": "accept",
                "content": {"selection": "A"}
            })),
            None,
        )
        .await
    );
    let response = elicitation
        .await
        .expect("elicitation task should join")
        .expect("elicitation response should parse");
    assert_eq!(response.action, ElicitationAction::Accept);
    assert_eq!(
        response.content,
        Some(serde_json::json!({"selection": "A"}))
    );

    bridge_task.abort();
}

#[tokio::test]
async fn authorized_recovery_rpc_discovers_and_attaches_pending_session() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = fixture
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            std::path::PathBuf::from("/tmp"),
        ))
        .await
        .expect("create session")
        .session_id
        .to_string();
    let elicitation_id = "recovery-rpc-elicitation";
    let (waiter_tx, _waiter_rx) = oneshot::channel();
    insert_pending_elicitation(
        &state.pending_elicitations,
        elicitation_id.to_string(),
        session_id.clone(),
        waiter_tx,
    )
    .await;

    state
        .elicitation_recovery
        .register_connection("original".to_string(), true)
        .await;
    state
        .elicitation_recovery
        .record_client_capabilities("original", true)
        .await;
    let secret = state
        .elicitation_recovery
        .issue_for_session("original", &session_id, state.agent.as_ref())
        .await
        .expect("issue original authority");
    state
        .elicitation_recovery
        .register_connection("replacement".to_string(), true)
        .await;
    state
        .elicitation_recovery
        .record_client_capabilities("replacement", true)
        .await;

    let (bridge_tx, mut bridge_rx) = mpsc::channel(4);
    let bridge = ClientBridgeSender::for_connection(bridge_tx, "replacement");
    tokio::spawn(async move { while bridge_rx.recv().await.is_some() {} });
    let (wire_tx, mut wire_rx) = mpsc::channel(4);
    let context = crate::acp::shared::RpcDispatchContext {
        session_hooks: None,
        session_bridge: Some(bridge),
        elicitation_recovery: Some(state.elicitation_recovery.clone()),
    };

    for (id, method, params) in [
        (
            1,
            crate::control::elicitation_recovery::ELICITATION_RECOVERY_LIST_PENDING_METHOD,
            serde_json::json!({"version": 1, "resume_authority": secret.clone()}),
        ),
        (
            2,
            crate::control::elicitation_recovery::ELICITATION_RECOVERY_ATTACH_METHOD,
            serde_json::json!({
                "version": 1,
                "session_id": session_id.clone(),
                "resume_authority": secret.clone()
            }),
        ),
    ] {
        crate::acp::shared::dispatch_rpc_message_with_context(
            crate::acp::shared::RpcDispatchState {
                agent: fixture.handle.clone(),
                session_owners: state.session_owners.clone(),
                pending_permissions: state.pending_permissions.clone(),
                pending_elicitations: state.pending_elicitations.clone(),
                conn_id: "replacement".to_string(),
                tx: wire_tx.clone(),
            },
            crate::acp::shared::RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: method.to_string(),
                params,
                id: Some(serde_json::json!(id)),
            },
            context.clone(),
        )
        .await;
        let response = timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("recovery RPC should respond")
            .expect("wire channel should remain open");
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(response.get("error").is_none(), "response={response}");
        if id == 1 {
            assert_eq!(
                response["result"]["session_ids"],
                serde_json::json!([session_id])
            );
        } else {
            assert_eq!(response["result"]["session_id"], session_id);
            assert_eq!(
                response["result"]["elicitation_ids"],
                serde_json::json!([elicitation_id])
            );
        }
    }

    assert_eq!(
        state.session_owners.lock().await.get(&session_id).cloned(),
        Some(HashSet::from(["replacement".to_string()]))
    );
}

#[tokio::test]
async fn concurrent_websocket_disconnects_restore_live_session_bridge() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = fixture
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            std::path::PathBuf::from("/tmp"),
        ))
        .await
        .expect("create session")
        .session_id
        .to_string();
    state.session_owners.lock().await.insert(
        session_id.clone(),
        HashSet::from([
            "conn-a".to_string(),
            "conn-b".to_string(),
            "conn-c".to_string(),
        ]),
    );
    for conn_id in ["conn-a", "conn-b", "conn-c"] {
        let (bridge_tx, mut bridge_rx) = mpsc::channel(4);
        let bridge = ClientBridgeSender::for_connection(bridge_tx, conn_id);
        state
            .connection_bridges
            .lock()
            .await
            .insert(conn_id.to_string(), bridge.clone());
        if conn_id == "conn-c" {
            fixture
                .handle
                .set_session_bridge(&session_id, bridge)
                .await
                .expect("set initial session bridge");
        }
        tokio::spawn(async move { while bridge_rx.recv().await.is_some() {} });
    }
    fixture
        .handle
        .set_session_bridge(
            &session_id,
            state.connection_bridges.lock().await["conn-a"].clone(),
        )
        .await
        .expect("make conn-a active");

    let ((), ()) = tokio::join!(
        reconcile_websocket_disconnect(&state, "conn-a"),
        reconcile_websocket_disconnect(&state, "conn-b"),
    );

    assert_eq!(
        state.session_owners.lock().await.get(&session_id).cloned(),
        Some(HashSet::from(["conn-c".to_string()]))
    );
    let route = fixture
        .config
        .session_bridges
        .lock()
        .expect("bridge routes lock")
        .get(&session_id)
        .cloned()
        .expect("live subscriber should own the session bridge");
    assert_eq!(route.bridge.connection_id(), Some("conn-c"));
}

#[tokio::test]
async fn disconnected_websocket_cannot_be_reattached_by_in_flight_dispatch() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let conn_id = "conn-race";
    let (bridge_tx, _bridge_rx) = mpsc::channel(4);
    let bridge = ClientBridgeSender::for_connection(bridge_tx, conn_id);
    let connection_state = bridge.connection_state().expect("connection state");
    state
        .connection_bridges
        .lock()
        .await
        .insert(conn_id.to_string(), bridge.clone());

    let attachment_guard = connection_state.lock_attachment().await;
    let disconnect_state = state.clone();
    let disconnect = tokio::spawn(async move {
        reconcile_websocket_disconnect(&disconnect_state, conn_id).await;
    });
    timeout(Duration::from_secs(2), async {
        loop {
            if !state.connection_bridges.lock().await.contains_key(conn_id) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect should remove the connection bridge");

    let (wire_tx, mut wire_rx) = mpsc::channel(4);
    let dispatch = tokio::spawn(crate::acp::shared::dispatch_rpc_message_with_context(
        crate::acp::shared::RpcDispatchState {
            agent: fixture.handle.clone(),
            session_owners: state.session_owners.clone(),
            pending_permissions: state.pending_permissions.clone(),
            pending_elicitations: state.pending_elicitations.clone(),
            conn_id: conn_id.to_string(),
            tx: wire_tx,
        },
        crate::acp::shared::RpcMessage {
            jsonrpc: "2.0".to_string(),
            method: crate::acp::protocol::AGENT_METHOD_NAMES
                .session_new
                .to_string(),
            params: serde_json::json!({"cwd": "/tmp", "mcpServers": []}),
            id: Some(serde_json::json!(1)),
        },
        crate::acp::shared::RpcDispatchContext {
            session_hooks: None,
            session_bridge: Some(bridge),
            elicitation_recovery: None,
        },
    ));
    tokio::task::yield_now().await;
    drop(attachment_guard);

    dispatch.await.expect("dispatch should finish");
    disconnect.await.expect("disconnect should finish");
    let response = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("lifecycle response should arrive")
        .expect("wire channel should remain open");
    let response: serde_json::Value = serde_json::from_str(&response).expect("JSON response");
    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("session id in lifecycle response");

    assert!(
        state
            .session_owners
            .lock()
            .await
            .get(session_id)
            .is_none_or(|owners| !owners.contains(conn_id)),
        "disconnected connection must not regain session ownership"
    );
    assert!(
        fixture
            .config
            .session_bridges
            .lock()
            .expect("bridge routes lock")
            .get(session_id)
            .is_none_or(|route| route.bridge.connection_id() != Some(conn_id)),
        "disconnected connection must not retain a session bridge"
    );
    assert!(!connection_state.is_active());
}

#[tokio::test]
async fn websocket_response_router_handles_errors_unknown_ids_and_disconnects() {
    let pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let (error_tx, error_rx) = oneshot::channel();
    pending.lock().await.insert(
        "\"request-1\"".to_string(),
        PendingWsRequest {
            method: "test/error",
            response_tx: error_tx,
        },
    );

    assert!(
        route_websocket_response(
            &pending,
            serde_json::json!("request-1"),
            None,
            Some(serde_json::json!({"code": -32603, "message": "failed"})),
        )
        .await
    );
    assert_eq!(
        error_rx.await.expect("response receiver"),
        Err(serde_json::json!({"code": -32603, "message": "failed"}))
    );
    assert!(!route_websocket_response(&pending, serde_json::json!("unknown"), None, None).await);

    let (disconnect_tx, disconnect_rx) = oneshot::channel();
    pending.lock().await.insert(
        "\"request-2\"".to_string(),
        PendingWsRequest {
            method: "test/disconnect",
            response_tx: disconnect_tx,
        },
    );
    cancel_pending_websocket_requests(&pending).await;
    assert!(disconnect_rx.await.expect("disconnect response").is_err());
    assert!(pending.lock().await.is_empty());
}

#[tokio::test]
async fn websocket_event_forwarder_deduplicates_elicitation_requests() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    state.session_owners.lock().await.insert(
        "ws-session".to_string(),
        HashSet::from(["conn".to_string()]),
    );
    let (waiter_tx, _waiter_rx) = oneshot::channel();
    insert_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        "duplicate".to_string(),
        "ws-session".to_string(),
        waiter_tx,
    )
    .await;
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let cancel = CancellationToken::new();
    spawn_event_forwarders(
        state,
        ConnectionEventState {
            conn_id: "conn".to_string(),
            tx: wire_tx,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
        },
    );

    let event = elicitation_event("ws-session", "duplicate");
    fixture.config.event_sink.fanout().publish(event.clone());
    fixture.config.event_sink.fanout().publish(event);

    let first = timeout(Duration::from_secs(2), wire_rx.recv())
        .await
        .expect("first request should arrive")
        .expect("wire channel should remain open");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&first).unwrap()["method"],
        "elicitation/create"
    );
    assert!(
        timeout(Duration::from_millis(100), wire_rx.recv())
            .await
            .is_err()
    );
    cancel.cancel();
}

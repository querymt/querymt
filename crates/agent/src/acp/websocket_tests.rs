use super::websocket::{
    ConnectionEventState, PendingWsRequest, PendingWsRequestMap, WsServerState,
    cancel_pending_websocket_requests, has_allowed_websocket_origin, route_websocket_response,
    router, run_websocket_bridge, spawn_event_forwarders,
};
use crate::acp::client_bridge::{ClientBridgeMessage, ClientBridgeSender};
use crate::acp::protocol::{
    PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, SessionId,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use crate::elicitation::{ElicitationAction, insert_pending_elicitation};
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

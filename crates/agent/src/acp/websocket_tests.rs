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

/// Fresh connection-scoped translator for event-forwarder test fixtures.
fn test_translator() -> Arc<std::sync::Mutex<super::shared::AcpLiveEventTranslator>> {
    Arc::new(std::sync::Mutex::new(
        super::shared::AcpLiveEventTranslator::new(),
    ))
}

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

#[tokio::test]
async fn websocket_event_forwarder_preserves_multi_update_order() {
    use crate::acp::shared::AcpSessionUpdateCapabilities;

    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = "ordered-session";
    state
        .session_owners
        .lock()
        .await
        .insert(session_id.to_string(), HashSet::from(["conn".to_string()]));

    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(8);
    let cancel = CancellationToken::new();
    // A capable translator projects one event into two ordered updates.
    let translator = Arc::new(std::sync::Mutex::new(
        super::shared::AcpLiveEventTranslator::new(),
    ));
    translator
        .lock()
        .unwrap()
        .set_capabilities(AcpSessionUpdateCapabilities {
            plan_operations: false,
            notices: false,
            compaction: true,
        });
    translator.lock().unwrap().seed_config_options(
        session_id,
        &crate::agent::session_registry::config_options(crate::agent::core::AgentMode::Build, None),
    );

    spawn_event_forwarders(
        state,
        ConnectionEventState {
            conn_id: "conn".to_string(),
            tx: wire_tx,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
            delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
        },
        translator,
    );

    let publish = |seq: i64, kind: AgentEventKind| {
        EventEnvelope::Durable(DurableEvent {
            session_seq: 0,
            event_id: format!("evt-{seq}"),
            stream_seq: seq,
            timestamp: seq,
            session_id: session_id.to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind,
        })
    };

    fixture.config.event_sink.fanout().publish(publish(
        1,
        AgentEventKind::ProviderChanged {
            provider: "local".to_string(),
            model: "m".to_string(),
            config_id: 1,
            context_limit: Some(1_000),
            provider_node_id: None,
        },
    ));
    fixture.config.event_sink.fanout().publish(publish(
        2,
        AgentEventKind::CompactionEnd {
            summary: "done".to_string(),
            summary_len: 4,
            compaction_id: Some("c-1".to_string()),
            context_tokens: Some(10),
        },
    ));

    let model: serde_json::Value = serde_json::from_str(
        &timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        model["params"]["update"]["sessionUpdate"],
        "config_option_update"
    );
    let options = model["params"]["update"]["configOptions"]
        .as_array()
        .unwrap();
    assert_eq!(
        options.len(),
        3,
        "wire updates replace the complete configuration"
    );
    assert_eq!(
        options
            .iter()
            .find(|option| option["id"] == "model")
            .unwrap()["currentValue"],
        "local/m"
    );
    let first: serde_json::Value = serde_json::from_str(
        &timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("first update")
            .expect("wire open"),
    )
    .unwrap();
    let second: serde_json::Value = serde_json::from_str(
        &timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("second update")
            .expect("wire open"),
    )
    .unwrap();

    assert_eq!(
        first["params"]["update"]["sessionUpdate"],
        "compaction_update"
    );
    assert_eq!(second["params"]["update"]["sessionUpdate"], "usage_update");
    assert_eq!(second["params"]["update"]["used"], 10);
}

/// A connection that falls behind the live buffer must recover the missed
/// durable updates from the journal and keep receiving later ones, in order.
/// Regression for the forwarder exiting on `RecvError::Lagged`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_event_forwarder_recovers_from_broadcast_lag_in_order() {
    const TOTAL: usize = 1_400; // more than the 1024-slot live buffer

    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = "lagging-session";
    state
        .session_owners
        .lock()
        .await
        .insert(session_id.to_string(), HashSet::from(["conn".to_string()]));

    // A one-slot wire that nobody drains yet blocks the forwarder on its
    // second notification while the session keeps producing events.
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(1);
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
            delivery: crate::acp::delivery::ConnectionDelivery::new(
                Some(fixture.config.event_sink.journal().clone()),
                cancel.clone(),
            ),
        },
        Arc::new(std::sync::Mutex::new(
            super::shared::AcpLiveEventTranslator::new(),
        )),
    );

    let stream = fixture.config.event_sink.session_stream(session_id);
    for n in 1..=TOTAL {
        stream.emit(AgentEventKind::AssistantMessageStored {
            content: format!("m{n}"),
            thinking: None,
            reasoning_parts: Vec::new(),
            message_id: Some(format!("id-{n}")),
        });
    }

    // Let every event be persisted and published while the forwarder is stuck.
    timeout(Duration::from_secs(20), async {
        while stream.published_tip() < TOTAL as i64 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all events published");

    let mut received = Vec::with_capacity(TOTAL);
    while received.len() < TOTAL {
        let raw = timeout(Duration::from_secs(20), wire_rx.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "forwarder stopped after {} of {TOTAL} updates",
                    received.len()
                )
            })
            .expect("wire open");
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let text = json["params"]["update"]["content"]["text"]
            .as_str()
            .expect("agent message text")
            .to_string();
        received.push(text);
    }

    let expected: Vec<String> = (1..=TOTAL).map(|n| format!("m{n}")).collect();
    assert_eq!(
        received, expected,
        "every durable update arrives once, in production order"
    );

    // And the connection keeps working after the recovery.
    stream.emit(AgentEventKind::AssistantMessageStored {
        content: "after-lag".to_string(),
        thinking: None,
        reasoning_parts: Vec::new(),
        message_id: Some("id-after".to_string()),
    });
    let raw = timeout(Duration::from_secs(5), wire_rx.recv())
        .await
        .expect("update after recovery")
        .expect("wire open");
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["params"]["update"]["content"]["text"], "after-lag");
    cancel.cancel();
}

fn elicitation_event(session_id: &str, elicitation_id: &str) -> EventEnvelope {
    EventEnvelope::Ephemeral(EphemeralEvent {
        session_seq: 0,
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
            delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
        },
        test_translator(),
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
async fn unauthorized_subscriber_receives_no_elicitation_content() {
    let (fixture, session) = prompt_fixture().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = session.as_str();
    let elicitation_id = "protected-elicitation";
    // Establish the original owner through attachment. A bare subscriber set
    // elects a fallback and would make the observer a legitimate live owner.
    for connection in ["original", "observer"] {
        let output = super::shared::handle_rpc_message(
            fixture.handle.as_ref(), &state.session_owners, &state.pending_elicitations,
            connection, super::shared::RpcMessage {
                jsonrpc: "2.0".into(), id: Some(serde_json::json!(1)), method: "session/load".into(),
                params: serde_json::json!({"sessionId": session_id, "cwd": "/tmp", "mcpServers": []}),
            },
        ).await;
        assert!(output.response.unwrap().error.is_none());
    }
    for connection_id in ["original", "observer"] {
        state
            .elicitation_recovery
            .register_connection(connection_id.to_string(), true)
            .await;
        state
            .elicitation_recovery
            .record_client_capabilities(connection_id, true)
            .await;
    }
    let (waiter_tx, _waiter_rx) = oneshot::channel();
    insert_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        elicitation_id.to_string(),
        session_id.to_string(),
        waiter_tx,
    )
    .await;
    state
        .elicitation_recovery
        .issue_for_session("original", session_id, fixture.handle.as_ref())
        .await
        .expect("original connection receives recovery authority");

    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);
    let cancel = CancellationToken::new();
    let forwarded = Arc::new(Mutex::new(HashSet::new()));
    spawn_event_forwarders(
        state,
        ConnectionEventState {
            conn_id: "observer".to_string(),
            tx: wire_tx,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            forwarded_elicitations: forwarded.clone(),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
            delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
        },
        test_translator(),
    );
    fixture
        .config
        .event_sink
        .fanout()
        .publish(elicitation_event(session_id, elicitation_id));

    assert!(
        timeout(Duration::from_millis(100), wire_rx.recv())
            .await
            .is_err()
    );
    assert!(
        forwarded.lock().await.is_empty(),
        "a denied claim must remain retryable"
    );
    cancel.cancel();
}

#[tokio::test]
async fn observer_loaded_after_prompt_cannot_capture_the_question() {
    let (fixture, session_id) = prompt_fixture().await;
    let state = WsServerState::new(fixture.handle.clone());
    for connection in ["observer", "controller"] {
        state
            .elicitation_recovery
            .register_connection(connection.into(), true)
            .await;
        state
            .elicitation_recovery
            .record_client_capabilities(connection, true)
            .await;
    }
    for (connection, method, params) in [
        (
            "observer",
            "session/load",
            serde_json::json!({"sessionId": session_id, "cwd": "/tmp", "mcpServers": []}),
        ),
        (
            "controller",
            "session/prompt",
            serde_json::json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}]}),
        ),
        (
            "observer",
            "session/load",
            serde_json::json!({"sessionId": session_id, "cwd": "/tmp", "mcpServers": []}),
        ),
    ] {
        let output = super::shared::handle_rpc_message(
            fixture.handle.as_ref(),
            &state.session_owners,
            &state.pending_elicitations,
            connection,
            super::shared::RpcMessage {
                jsonrpc: "2.0".into(),
                id: Some(serde_json::json!(1)),
                method: method.into(),
                params,
            },
        )
        .await;
        assert!(
            output.response.unwrap().error.is_none(),
            "{method} must succeed"
        );
    }
    let (sender, receiver) = oneshot::channel();
    insert_pending_elicitation(
        &state.pending_elicitations,
        "question".into(),
        session_id.clone(),
        sender,
    )
    .await;
    let mut connections = Vec::new();
    for id in ["observer", "controller"] {
        let (tx, rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        connections.push((
            ConnectionEventState {
                conn_id: id.into(),
                tx,
                pending_requests: Default::default(),
                forwarded_elicitations: Default::default(),
                request_counter: Arc::new(AtomicU64::new(1)),
                connection_cancel: cancel.clone(),
                delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel),
            },
            rx,
        ));
    }
    let translator = test_translator();
    // Force the observer to process the event first, instead of relying on task scheduling.
    for (connection, rx) in &mut connections {
        assert!(
            super::websocket::forward_session_event(
                &state,
                connection,
                &translator,
                elicitation_event(&session_id, "question"),
            )
            .await
        );
        if connection.conn_id == "observer" {
            assert!(
                rx.try_recv().is_err(),
                "an observer must not receive authority or question content"
            );
            assert!(connection.forwarded_elicitations.lock().await.is_empty());
        }
    }
    let (controller, rx) = &mut connections[1];
    let authority: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    assert_eq!(
        authority["method"],
        crate::control::elicitation_recovery::ELICITATION_RECOVERY_AUTHORITY_NOTIFICATION
    );
    let request: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    assert_eq!(request["method"], "elicitation/create");
    assert!(
        route_websocket_response(
            &controller.pending_requests,
            request["id"].clone(),
            Some(serde_json::json!({"action": "accept", "content": {"selection": "A"}})),
            None,
        )
        .await
    );
    assert_eq!(
        timeout(Duration::from_secs(2), receiver)
            .await
            .unwrap()
            .unwrap()
            .action,
        ElicitationAction::Accept
    );
    for (connection, _) in connections {
        connection.connection_cancel.cancel();
    }
}

#[tokio::test]
async fn pending_question_follows_explicit_and_fallback_owner_changes() {
    for handoff in ["prompt", "disconnect", "lazy-fallback"] {
        let (fixture, session_id) = prompt_fixture().await;
        let state = WsServerState::new(fixture.handle.clone());
        for connection in ["original", "next", "observer"] {
            state
                .elicitation_recovery
                .register_connection(connection.into(), true)
                .await;
            state
                .elicitation_recovery
                .record_client_capabilities(connection, true)
                .await;
            let output = super::shared::handle_rpc_message(
                fixture.handle.as_ref(), &state.session_owners, &state.pending_elicitations,
                connection, super::shared::RpcMessage {
                    jsonrpc: "2.0".into(), id: Some(serde_json::json!(1)), method: "session/load".into(),
                    params: serde_json::json!({"sessionId": session_id, "cwd": "/tmp", "mcpServers": []}),
                },
            ).await;
            assert!(output.response.unwrap().error.is_none());
        }
        let (sender, receiver) = oneshot::channel();
        insert_pending_elicitation(
            &state.pending_elicitations,
            "pending".into(),
            session_id.clone(),
            sender,
        )
        .await;
        let authority = state
            .elicitation_recovery
            .issue_for_session("original", &session_id, &fixture.handle)
            .await
            .expect("original authority");
        let before =
            crate::elicitation::snapshot_pending_elicitations(&state.pending_elicitations).await;
        match handoff {
            "prompt" => {
                let output = super::shared::handle_rpc_message_with_context(
                    fixture.handle.as_ref(), &state.session_owners, &state.pending_elicitations,
                    "next", super::shared::RpcMessage {
                        jsonrpc: "2.0".into(), id: Some(serde_json::json!(2)), method: "session/prompt".into(),
                        params: serde_json::json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}]}),
                    }, super::shared::RpcDispatchContext {
                        elicitation_recovery: Some(state.elicitation_recovery.clone()), ..Default::default()
                    },
                ).await;
                assert!(output.response.unwrap().error.is_none());
            }
            "disconnect" => {
                state
                    .elicitation_recovery
                    .remove_connection("original", &fixture.handle)
                    .await;
                reconcile_websocket_disconnect(&state, "original").await;
            }
            _ => {
                state
                    .session_owners
                    .lock()
                    .await
                    .get_mut(&session_id)
                    .unwrap()
                    .remove("original");
            }
        }
        assert!(
            state
                .session_owners
                .owns_interactions(&session_id, "next")
                .await
        );
        if handoff != "lazy-fallback" {
            assert!(
                state
                    .elicitation_recovery
                    .claim_live_delivery("next", &session_id, "pending", &fixture.handle)
                    .await
                    .is_some(),
                "{handoff}: ownership establishment must authorize the new connection"
            );
        }
        let (tx, mut rx) = mpsc::channel(8);
        let connection =
            connection_state_for_test("next", tx, Default::default(), Arc::new(AtomicU64::new(1)));
        assert!(
            super::websocket::forward_session_event(
                &state,
                &connection,
                &test_translator(),
                elicitation_event(&session_id, "pending")
            )
            .await
        );
        let request: serde_json::Value = serde_json::from_str(
            &timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("new owner receives the pending question")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            request["method"], "elicitation/create",
            "{handoff}: do not rotate the authority"
        );
        let after =
            crate::elicitation::snapshot_pending_elicitations(&state.pending_elicitations).await;
        assert_eq!(after[0].owner_authority, before[0].owner_authority);
        assert!(
            state
                .elicitation_recovery
                .claim_live_delivery("observer", &session_id, "pending", &fixture.handle)
                .await
                .is_none()
        );
        let sessions = state
            .elicitation_recovery
            .list_pending_sessions(
                "next",
                &crate::control::elicitation_recovery::ListPendingElicitationSessionsRequest {
                    version: 1,
                    resume_authority: authority,
                },
                &fixture.handle,
            )
            .await
            .unwrap();
        assert_eq!(sessions.session_ids, vec![session_id]);
        assert!(
            route_websocket_response(
                &connection.pending_requests,
                request["id"].clone(),
                Some(serde_json::json!({"action": "accept", "content": {"selection": "A"}})),
                None
            )
            .await
        );
        assert_eq!(
            timeout(Duration::from_secs(2), receiver)
                .await
                .unwrap()
                .unwrap()
                .action,
            ElicitationAction::Accept
        );
        connection.connection_cancel.cancel();
    }
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
            delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
        },
        test_translator(),
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
            delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
        },
        test_translator(),
    );

    let delegation = crate::session::domain::Delegation {
        isolation: Default::default(),
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
            session_seq: 0,
            event_id: "event-1".into(),
            stream_seq: 1,
            session_id: session_id.into(),
            timestamp: 10,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::DelegationRequested {
                delegation,
                tool_call_id: Some("call-1".into()),
                batch_id: None,
                cycle_id: None,
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
async fn loaded_session_emits_live_delegation_completion_without_reloading() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let session_id = fixture.create_session().await;
    let stream = fixture.config.event_sink.session_stream(&session_id);
    let delegation = crate::session::domain::Delegation {
        isolation: Default::default(),
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
    stream.emit(AgentEventKind::DelegationRequested {
        delegation,
        tool_call_id: Some("call-1".into()),
        batch_id: None,
        cycle_id: None,
    });
    stream.emit(AgentEventKind::SessionForked {
        parent_session_id: session_id.clone(),
        child_session_id: "child-1".into(),
        target_agent_id: "coder".into(),
        origin: crate::session::domain::ForkOrigin::Delegation,
        fork_point_type: crate::session::domain::ForkPointType::ProgressEntry,
        fork_point_ref: "delegation-1".into(),
        instructions: None,
        selected_model_id: None,
        selected_provider_node_id: None,
    });
    timeout(Duration::from_secs(5), async {
        while stream.published_tip() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delegation starts before the connection is opened");

    let (mut connection, mut rx) =
        super::websocket::EmbeddedAcpConnection::open(fixture.handle.clone(), None).await;
    connection
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": "init", "method": "initialize",
                "params": {"protocolVersion": 1, "clientCapabilities": {
                    "_meta": {"querymt": {"sessionLoadSnapshot": {"epoch": 1}}}
                }}
            })
            .to_string(),
        )
        .await
        .unwrap();
    embedded_messages_until(&mut rx, |m| m["id"] == "init").await;
    load_embedded_session(&connection, &mut rx, &session_id).await;

    stream.emit(AgentEventKind::DelegationCompleted {
        delegation_id: "delegation-1".into(),
        result: Some("Done".into()),
    });
    let messages = embedded_messages_until(&mut rx, |m| {
        m["method"] == "querymt/session/delegationUpdate" && m["params"]["state"] == "completed"
    })
    .await;
    let update = &messages.last().unwrap()["params"];
    assert_eq!(update["sessionId"], session_id);
    assert_eq!(update["toolCallId"], "call-1");
    assert_eq!(update["childSessionId"], "child-1");
    connection.close().await;
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
            delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
        },
        test_translator(),
    );

    fixture
        .config
        .event_sink
        .fanout()
        .publish(EventEnvelope::Durable(DurableEvent {
            session_seq: 0,
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
                delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
            },
            test_translator(),
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
            delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
        },
        test_translator(),
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

fn connection_state_for_test(
    conn_id: &str,
    tx: mpsc::Sender<String>,
    pending_requests: PendingWsRequestMap,
    request_counter: Arc<AtomicU64>,
) -> ConnectionEventState {
    ConnectionEventState {
        conn_id: conn_id.to_string(),
        tx,
        pending_requests,
        forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
        request_counter,
        connection_cancel: CancellationToken::new(),
        delivery: crate::acp::delivery::ConnectionDelivery::new(None, CancellationToken::new()),
    }
}

#[tokio::test]
async fn invalid_outgoing_schema_is_nonfatal_and_keeps_waiter_pending() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let recovery = crate::control::elicitation_recovery::ElicitationRecoveryRegistry::default();
    let (waiter_tx, mut waiter_rx) = oneshot::channel();
    register_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        "invalid-schema".to_string(),
        PendingElicitationRegistration {
            session_id: "session".to_string(),
            form: PendingElicitationForm {
                message: "Choose".to_string(),
                requested_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"choice": {"type": "object"}}
                }),
                source: "builtin:question".to_string(),
            },
            owner_authority: None,
            run_id: Some("run".to_string()),
            tool_call_id: Some("tool".to_string()),
        },
        waiter_tx,
    )
    .await;
    let claim = crate::elicitation::claim_live_elicitation_delivery(
        fixture.handle.as_ref(),
        "session",
        "invalid-schema",
        "conn",
    )
    .await
    .unwrap();
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(1);

    // Keep the original sender alive so an empty channel reports Empty, not Disconnected.
    deliver_claimed_websocket_elicitation(
        fixture.handle.clone(),
        recovery,
        claim,
        connection_state_for_test(
            "conn",
            wire_tx.clone(),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(AtomicU64::new(1)),
        ),
    )
    .await
    .expect("local schema rejection is not a transport failure");

    assert!(matches!(
        wire_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(
        fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .contains_key("invalid-schema")
    );
    assert!(matches!(
        waiter_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn closed_delivery_channel_is_a_transport_failure() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let (waiter_tx, mut waiter_rx) = oneshot::channel();
    insert_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        "transport-failure".to_string(),
        "session".to_string(),
        waiter_tx,
    )
    .await;
    let claim = crate::elicitation::claim_live_elicitation_delivery(
        fixture.handle.as_ref(),
        "session",
        "transport-failure",
        "conn",
    )
    .await
    .unwrap();
    let (wire_tx, wire_rx) = mpsc::channel::<String>(1);
    drop(wire_rx);

    assert!(
        deliver_claimed_websocket_elicitation(
            fixture.handle.clone(),
            crate::control::elicitation_recovery::ElicitationRecoveryRegistry::default(),
            claim,
            connection_state_for_test(
                "conn",
                wire_tx,
                Arc::new(Mutex::new(HashMap::new())),
                Arc::new(AtomicU64::new(1)),
            ),
        )
        .await
        .is_err()
    );
    assert!(
        fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .contains_key("transport-failure")
    );
    assert!(matches!(
        waiter_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn client_error_and_malformed_response_keep_waiter_pending() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let recovery = crate::control::elicitation_recovery::ElicitationRecoveryRegistry::default();
    let (waiter_tx, mut waiter_rx) = oneshot::channel();
    register_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        "retryable-question".to_string(),
        PendingElicitationRegistration {
            session_id: "session".to_string(),
            form: PendingElicitationForm {
                message: "Choose".to_string(),
                requested_schema: serde_json::json!({"type": "object", "properties": {}}),
                source: "builtin:question".to_string(),
            },
            owner_authority: None,
            run_id: Some("run".to_string()),
            tool_call_id: Some("tool".to_string()),
        },
        waiter_tx,
    )
    .await;
    let pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(AtomicU64::new(1));
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(4);

    for response_error in [true, false] {
        let claim = crate::elicitation::claim_live_elicitation_delivery(
            fixture.handle.as_ref(),
            "session",
            "retryable-question",
            "conn",
        )
        .await
        .unwrap();
        deliver_claimed_websocket_elicitation(
            fixture.handle.clone(),
            recovery.clone(),
            claim,
            connection_state_for_test("conn", wire_tx.clone(), pending.clone(), counter.clone()),
        )
        .await
        .unwrap();
        let request: serde_json::Value =
            serde_json::from_str(&wire_rx.recv().await.unwrap()).unwrap();
        if response_error {
            assert!(
                route_websocket_response(
                    &pending,
                    request["id"].clone(),
                    None,
                    Some(serde_json::json!({"code": -32000, "message": "client failed"})),
                )
                .await
            );
        } else {
            assert!(
                route_websocket_response(
                    &pending,
                    request["id"].clone(),
                    Some(serde_json::json!({"action": "unsupported"})),
                    None,
                )
                .await
            );
        }
        tokio::task::yield_now().await;
        assert!(
            fixture
                .handle
                .pending_elicitations()
                .lock()
                .await
                .contains_key("retryable-question")
        );
        assert!(matches!(
            waiter_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
    }
}

#[tokio::test]
async fn recovered_delivery_uses_fresh_id_and_rejects_old_invalid_and_duplicate_replies() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let recovery = crate::control::elicitation_recovery::ElicitationRecoveryRegistry::default();
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
        recovery.clone(),
        old_claim,
        connection_state_for_test("old", old_tx, old_pending.clone(), counter.clone()),
    )
    .await
    .unwrap();
    deliver_claimed_websocket_elicitation(
        fixture.handle.clone(),
        recovery.clone(),
        new_claim.clone(),
        connection_state_for_test("new", new_tx.clone(), new_pending.clone(), counter.clone()),
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

    // The agent re-delivers the still-pending question automatically with a
    // fresh JSON-RPC id and the same stable elicitation id.
    let retry_request: serde_json::Value =
        serde_json::from_str(&new_rx.recv().await.unwrap()).unwrap();
    assert_eq!(retry_request["method"], "elicitation/create");
    assert_ne!(retry_request["id"], new_request["id"]);
    assert_eq!(
        retry_request["params"]["_meta"]["querymt"]["elicitation_id"],
        "stable-question"
    );
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
async fn exhausted_validation_redeliveries_cancel_the_pending_elicitation() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let recovery = crate::control::elicitation_recovery::ElicitationRecoveryRegistry::default();
    let (waiter_tx, waiter_rx) = oneshot::channel();
    register_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        "never-valid".to_string(),
        PendingElicitationRegistration {
            session_id: "session".to_string(),
            form: PendingElicitationForm {
                message: "Choose".to_string(),
                requested_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"choice": {"type": "string"}},
                    "required": ["choice"]
                }),
                source: "mcp:server".to_string(),
            },
            owner_authority: None,
            run_id: Some("run".to_string()),
            tool_call_id: Some("tool".to_string()),
        },
        waiter_tx,
    )
    .await;
    let claim = crate::elicitation::claim_live_elicitation_delivery(
        fixture.handle.as_ref(),
        "session",
        "never-valid",
        "conn",
    )
    .await
    .unwrap();
    let pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(32);
    deliver_claimed_websocket_elicitation(
        fixture.handle.clone(),
        recovery,
        claim,
        connection_state_for_test(
            "conn",
            wire_tx,
            pending.clone(),
            Arc::new(AtomicU64::new(1)),
        ),
    )
    .await
    .unwrap();

    // Initial delivery plus every permitted redelivery gets an invalid answer.
    for attempt in 0..=super::websocket::MAX_VALIDATION_REDELIVERIES {
        let request: serde_json::Value = serde_json::from_str(
            &timeout(Duration::from_secs(2), wire_rx.recv())
                .await
                .expect("request should be (re)delivered")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(request["method"], "elicitation/create", "attempt {attempt}");
        assert!(
            route_websocket_response(
                &pending,
                request["id"].clone(),
                Some(serde_json::json!({"action": "accept", "content": {"choice": 42}})),
                None,
            )
            .await
        );
        let validation: serde_json::Value = serde_json::from_str(
            &timeout(Duration::from_secs(2), wire_rx.recv())
                .await
                .expect("validation failure should be reported")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            validation["method"],
            crate::control::elicitation_recovery::ELICITATION_RECOVERY_VALIDATION_FAILED_NOTIFICATION
        );
    }

    let terminal: serde_json::Value = serde_json::from_str(
        &timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("terminal notification should be sent")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        terminal["method"],
        crate::control::elicitation_recovery::ELICITATION_RECOVERY_COMPLETED_NOTIFICATION
    );
    assert_eq!(terminal["params"]["outcome"], "cancel");
    assert_eq!(
        timeout(Duration::from_secs(2), waiter_rx)
            .await
            .expect("waiting tool must be released")
            .unwrap()
            .action,
        crate::elicitation::ElicitationAction::Cancel
    );
    assert!(
        !fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .contains_key("never-valid")
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
            delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
        },
        test_translator(),
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
        translator: None,
        session_load: None,
        delivery: None,
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
            translator: None,
            session_load: None,
            delivery: None,
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
            delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
        },
        test_translator(),
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

// ── Permission interactions over WebSocket ───────────────────────────────────

fn permission_request_for_test(session_id: &str, tool_call_id: &str) -> RequestPermissionRequest {
    RequestPermissionRequest::new(
        session_id.to_string(),
        ToolCallUpdate::new(
            ToolCallId::from(tool_call_id.to_string()),
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
    )
}

async fn register_permission_for_test(
    fixture: &crate::test_utils::TestAgent,
    session_id: &str,
    permission_id: &str,
) -> oneshot::Receiver<crate::elicitation::ElicitationResponse> {
    let (tx, rx) = oneshot::channel();
    crate::elicitation::register_pending_permission(
        &fixture.handle.pending_elicitations(),
        permission_id.to_string(),
        session_id.to_string(),
        permission_request_for_test(session_id, permission_id),
        Some("run".to_string()),
        Some(permission_id.to_string()),
        tx,
    )
    .await;
    rx
}

async fn deliver_permission_for_test(
    fixture: &crate::test_utils::TestAgent,
    conn_id: &str,
    session_id: &str,
    permission_id: &str,
) -> (
    PendingWsRequestMap,
    mpsc::Receiver<String>,
    CancellationToken,
) {
    let claim = crate::elicitation::claim_live_elicitation_delivery(
        fixture.handle.as_ref(),
        session_id,
        permission_id,
        conn_id,
    )
    .await
    .expect("permission interaction is claimable");
    let pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let (wire_tx, wire_rx) = mpsc::channel::<String>(16);
    let connection = connection_state_for_test(
        conn_id,
        wire_tx,
        pending.clone(),
        Arc::new(AtomicU64::new(1)),
    );
    let cancel = connection.connection_cancel.clone();
    super::websocket::deliver_claimed_websocket_permission(
        fixture.handle.clone(),
        claim,
        connection,
    )
    .await
    .expect("permission request is written to the wire");
    (pending, wire_rx, cancel)
}

fn selected(option_id: &str) -> serde_json::Value {
    serde_json::json!({"outcome": {"outcome": "selected", "optionId": option_id}})
}

#[tokio::test]
async fn permission_is_delivered_as_request_permission_and_selected_option_resolves() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let waiter = register_permission_for_test(&fixture, "session", "call-1").await;
    let (pending, mut wire, _cancel) =
        deliver_permission_for_test(&fixture, "conn", "session", "call-1").await;

    let request: serde_json::Value = serde_json::from_str(&wire.recv().await.unwrap()).unwrap();
    assert_eq!(request["method"], "session/request_permission");
    assert_eq!(request["params"]["toolCall"]["toolCallId"], "call-1");

    assert!(
        route_websocket_response(
            &pending,
            request["id"].clone(),
            Some(selected("allow_once")),
            None
        )
        .await
    );
    let answer = timeout(Duration::from_secs(2), waiter)
        .await
        .expect("answer reaches the waiting tool")
        .unwrap();
    assert_eq!(answer.selected_permission_option(), Some("allow_once"));
    assert!(
        !fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .contains_key("call-1")
    );
}

#[tokio::test]
async fn permission_event_issues_authority_then_request_on_secure_connection_without_form_support()
{
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let session_id = "perm-authority-session";
    state
        .session_owners
        .lock()
        .await
        .insert(session_id.to_string(), HashSet::from(["conn".to_string()]));
    // Secure connection that did NOT advertise form-elicitation support.
    state
        .elicitation_recovery
        .register_connection("conn".to_string(), true)
        .await;
    let _waiter = register_permission_for_test(&fixture, session_id, "perm-a").await;

    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(8);
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
            delivery: crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone()),
        },
        test_translator(),
    );
    fixture
        .config
        .event_sink
        .fanout()
        .publish(EventEnvelope::Ephemeral(EphemeralEvent {
            session_seq: 0,
            session_id: session_id.to_string(),
            timestamp: 1,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::PermissionRequested {
                permission_id: "perm-a".to_string(),
                task_id: None,
                tool_name: "shell".to_string(),
                reason: "needs permission".to_string(),
                tool_call_id: "perm-a".to_string(),
                request: Some(permission_request_for_test(session_id, "perm-a")),
            },
        }));

    let authority: serde_json::Value = serde_json::from_str(
        &timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("authority notification is sent for a permission request")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        authority["method"],
        crate::control::elicitation_recovery::ELICITATION_RECOVERY_AUTHORITY_NOTIFICATION
    );
    assert_eq!(authority["params"]["session_id"], session_id);
    assert_eq!(
        authority["params"]["resume_authority"]
            .as_str()
            .map(str::len),
        Some(64)
    );

    let request: serde_json::Value = serde_json::from_str(
        &timeout(Duration::from_secs(2), wire_rx.recv())
            .await
            .expect("the permission request follows the authority")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(request["method"], "session/request_permission");
    cancel.cancel();
}

#[tokio::test]
async fn recovery_attach_delivers_claimed_permission_as_request_permission() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let state = WsServerState::new(fixture.handle.clone());
    let waiter = register_permission_for_test(&fixture, "session", "perm-1").await;

    for conn in ["original", "replacement"] {
        state
            .elicitation_recovery
            .register_connection(conn.to_string(), true)
            .await;
        state
            .elicitation_recovery
            .record_client_capabilities(conn, true)
            .await;
    }
    let secret = state
        .elicitation_recovery
        .issue_for_session("original", "session", state.agent.as_ref())
        .await
        .expect("a pending permission is covered by the issued authority");

    let attached = state
        .elicitation_recovery
        .authorize_attach(
            "replacement",
            &crate::control::elicitation_recovery::AttachPendingElicitationSessionRequest {
                version: crate::control::elicitation_recovery::ELICITATION_RECOVERY_VERSION,
                session_id: "session".to_string(),
                resume_authority: secret,
            },
            state.agent.as_ref(),
        )
        .await
        .expect("attach succeeds");
    assert_eq!(attached.elicitation_ids, vec!["perm-1".to_string()]);

    let pending: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let (wire_tx, mut wire) = mpsc::channel::<String>(16);
    let connection = connection_state_for_test(
        "replacement",
        wire_tx,
        pending.clone(),
        Arc::new(AtomicU64::new(1)),
    );
    super::websocket::deliver_attached_interactions(
        &state.agent,
        &state.elicitation_recovery,
        "session",
        &connection,
    )
    .await;

    let request: serde_json::Value = serde_json::from_str(
        &timeout(Duration::from_secs(2), wire.recv())
            .await
            .expect("the claimed permission reaches the attaching connection")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(request["method"], "session/request_permission");
    assert_eq!(request["params"]["toolCall"]["toolCallId"], "perm-1");
    assert!(
        route_websocket_response(
            &pending,
            request["id"].clone(),
            Some(selected("allow_once")),
            None
        )
        .await
    );
    let answer = timeout(Duration::from_secs(2), waiter)
        .await
        .expect("answer reaches the waiting tool")
        .unwrap();
    assert_eq!(answer.selected_permission_option(), Some("allow_once"));
}

#[tokio::test]
async fn permission_disconnect_keeps_request_pending_and_next_client_decides() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let mut waiter = register_permission_for_test(&fixture, "session", "call-d").await;
    let (_pending, mut wire, cancel) =
        deliver_permission_for_test(&fixture, "old", "session", "call-d").await;
    let _ = wire.recv().await.unwrap();

    // The first connection closes without answering: not a decision.
    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(
        waiter.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(
        fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .contains_key("call-d")
    );

    // A later client attaching is offered the same request again.
    assert_eq!(
        crate::elicitation::undelivered_permission_ids_for_session(
            fixture.handle.as_ref(),
            "session",
            "new"
        )
        .await,
        vec!["call-d".to_string()]
    );
    let (pending, mut new_wire, _cancel) =
        deliver_permission_for_test(&fixture, "new", "session", "call-d").await;
    let request: serde_json::Value = serde_json::from_str(&new_wire.recv().await.unwrap()).unwrap();
    assert_eq!(request["method"], "session/request_permission");
    assert!(
        route_websocket_response(
            &pending,
            request["id"].clone(),
            Some(selected("reject_once")),
            None
        )
        .await
    );
    let answer = timeout(Duration::from_secs(2), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.selected_permission_option(), Some("reject_once"));
}

#[tokio::test]
async fn permission_malformed_answer_stays_pending_and_is_redelivered() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let mut waiter = register_permission_for_test(&fixture, "session", "call-m").await;
    let (pending, mut wire, _cancel) =
        deliver_permission_for_test(&fixture, "conn", "session", "call-m").await;
    let first: serde_json::Value = serde_json::from_str(&wire.recv().await.unwrap()).unwrap();

    // Unknown option, then an unparseable payload: neither grants nor rejects.
    assert!(
        route_websocket_response(
            &pending,
            first["id"].clone(),
            Some(selected("not-an-option")),
            None
        )
        .await
    );
    let second: serde_json::Value = serde_json::from_str(
        &timeout(Duration::from_secs(2), wire.recv())
            .await
            .expect("request is re-delivered")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(second["method"], "session/request_permission");
    assert_ne!(second["id"], first["id"]);
    assert!(
        route_websocket_response(
            &pending,
            second["id"].clone(),
            Some(serde_json::json!({"nonsense": true})),
            None
        )
        .await
    );
    let third: serde_json::Value = serde_json::from_str(
        &timeout(Duration::from_secs(2), wire.recv())
            .await
            .expect("request is re-delivered again")
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        waiter.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(
        fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .contains_key("call-m")
    );

    // A dismissed (cancelled) outcome is not a decision either.
    assert!(
        route_websocket_response(
            &pending,
            third["id"].clone(),
            Some(serde_json::json!({"outcome": {"outcome": "cancelled"}})),
            None
        )
        .await
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(
        waiter.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(
        fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .contains_key("call-m")
    );
}

#[tokio::test]
async fn permission_two_connections_first_valid_answer_wins_second_is_stale() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let waiter = register_permission_for_test(&fixture, "session", "call-w").await;
    let (pending_a, mut wire_a, _cancel_a) =
        deliver_permission_for_test(&fixture, "a", "session", "call-w").await;
    let (pending_b, mut wire_b, _cancel_b) =
        deliver_permission_for_test(&fixture, "b", "session", "call-w").await;
    let request_a: serde_json::Value = serde_json::from_str(&wire_a.recv().await.unwrap()).unwrap();
    let request_b: serde_json::Value = serde_json::from_str(&wire_b.recv().await.unwrap()).unwrap();

    // `b` claimed last, so `a`'s delivery is superseded and cannot decide.
    assert!(
        route_websocket_response(
            &pending_a,
            request_a["id"].clone(),
            Some(selected("allow_once")),
            None
        )
        .await
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .contains_key("call-w"),
        "a superseded delivery must not decide the call"
    );

    assert!(
        route_websocket_response(
            &pending_b,
            request_b["id"].clone(),
            Some(selected("reject_once")),
            None
        )
        .await
    );
    let answer = timeout(Duration::from_secs(2), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.selected_permission_option(), Some("reject_once"));

    // Resolved once: any later answer is rejected as not pending.
    let late = crate::elicitation::resolve_claimed_elicitation(
        fixture.handle.as_ref(),
        "session",
        "call-w",
        "b",
        2,
        crate::elicitation::ElicitationResponse::permission_selected("allow_once"),
    )
    .await
    .unwrap();
    assert_eq!(late, crate::elicitation::ElicitationResolution::NotFound);
}

/// Prompt `session/update` notifications reach the wire before the prompt
/// response. Sessions hold no client bridge, so this ordering comes from the
/// event forwarder alone.
#[tokio::test]
async fn local_prompt_updates_are_sent_before_prompt_response() {
    use crate::test_utils::{MockChatResponse, MockLlmProvider, SharedLlmProvider, TestAgent};

    let mut mock = MockLlmProvider::new();
    mock.expect_chat_with_tools()
        .returning(|_, _| Ok(MockChatResponse::text_only("ordered reply").into()));
    mock.expect_chat()
        .returning(|_| Ok(MockChatResponse::text_only("ordered reply").into()));
    let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
    let session_id = crate::send_agent::SendAgent::new_session(
        fixture.handle.as_ref(),
        crate::acp::protocol::NewSessionRequest::new(std::env::temp_dir()),
    )
    .await
    .expect("new session")
    .session_id
    .to_string();

    assert_prompt_updates_precede_response(fixture.handle.clone(), &session_id).await;
}

/// Build a fixture agent whose mock provider answers every prompt, plus one session.
async fn prompt_fixture() -> (crate::test_utils::TestAgent, String) {
    use crate::test_utils::{MockChatResponse, MockLlmProvider, SharedLlmProvider, TestAgent};

    let mut mock = MockLlmProvider::new();
    mock.expect_chat_with_tools()
        .returning(|_, _| Ok(MockChatResponse::text_only("held reply").into()));
    mock.expect_chat()
        .returning(|_| Ok(MockChatResponse::text_only("held reply").into()));
    let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
    let session_id = crate::send_agent::SendAgent::new_session(
        fixture.handle.as_ref(),
        crate::acp::protocol::NewSessionRequest::new(std::env::temp_dir()),
    )
    .await
    .expect("new session")
    .session_id
    .to_string();
    (fixture, session_id)
}

/// The prompt reply of a local session carries the run's `event_tip`, and the
/// tip is a real position of the session's stream.
#[tokio::test]
async fn local_prompt_reply_carries_event_tip() {
    let (fixture, session_id) = prompt_fixture().await;
    let mut response = crate::send_agent::SendAgent::prompt(
        fixture.handle.as_ref(),
        crate::acp::protocol::PromptRequest::new(
            session_id.clone(),
            vec![crate::acp::protocol::ContentBlock::from("hello")],
        ),
    )
    .await
    .expect("prompt");
    let tip = crate::acp::delivery::take_event_tip(&mut response)
        .expect("local prompt reply carries event_tip");
    assert!(tip > 0);
    assert!(tip <= fixture.config.event_sink.session_stream(&session_id).tip());
}

/// Drive a `session/prompt` through the dispatcher on a connection whose
/// forwarders are not attached, so its delivery position never reaches the
/// tip. Returns the wire receiver and the dispatch task.
async fn dispatch_prompt_without_forwarders(
    fixture: &crate::test_utils::TestAgent,
    session_id: &str,
    delivery: Arc<crate::acp::delivery::ConnectionDelivery>,
) -> (mpsc::Receiver<String>, tokio::task::JoinHandle<()>) {
    let state = WsServerState::new(fixture.handle.clone());
    let (wire_tx, wire_rx) = mpsc::channel::<String>(256);
    let agent = fixture.handle.clone();
    let session_id = session_id.to_string();
    let task = tokio::spawn(async move {
        super::shared::dispatch_rpc_message_with_context(
            super::shared::RpcDispatchState {
                agent,
                session_owners: state.session_owners.clone(),
                pending_elicitations: state.pending_elicitations.clone(),
                conn_id: "conn".to_string(),
                tx: wire_tx,
            },
            super::shared::RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: "session/prompt".to_string(),
                params: serde_json::json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": "hello"}]
                }),
                id: Some(serde_json::json!(7)),
            },
            super::shared::RpcDispatchContext {
                delivery: Some(delivery),
                ..Default::default()
            },
        )
        .await;
    });
    (wire_rx, task)
}

/// The response is withheld while the connection has not delivered the
/// prompt's updates, and is released (not lost) when the connection closes.
#[tokio::test]
async fn prompt_response_waits_for_updates_and_is_released_on_connection_close() {
    let (fixture, session_id) = prompt_fixture().await;
    let cancel = CancellationToken::new();
    let delivery = crate::acp::delivery::ConnectionDelivery::new(None, cancel.clone());
    let (mut wire_rx, task) =
        dispatch_prompt_without_forwarders(&fixture, &session_id, delivery).await;

    assert!(
        timeout(Duration::from_millis(400), wire_rx.recv())
            .await
            .is_err(),
        "response must be held while the updates are undelivered"
    );

    cancel.cancel();
    let raw = timeout(Duration::from_secs(5), wire_rx.recv())
        .await
        .expect("response released on close")
        .expect("wire open");
    let message: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(message["id"], serde_json::json!(7));
    assert!(message.get("error").is_none() || message["error"].is_null());
    assert!(
        message["result"].get("_meta").is_none(),
        "the internal event tip never reaches the client: {message}"
    );
    task.await.unwrap();
}

/// A slow second connection does not delay the requesting connection: each
/// connection waits on its own delivery position only.
#[tokio::test]
async fn slow_second_connection_does_not_delay_the_prompt_response() {
    let (fixture, session_id) = prompt_fixture().await;

    // Connection B observes the session but never handles its events.
    let slow_cancel = CancellationToken::new();
    let slow = crate::acp::delivery::ConnectionDelivery::new(None, slow_cancel.clone());
    let slow_state = WsServerState::new(fixture.handle.clone());
    let (slow_wire_tx, _slow_wire_rx) = mpsc::channel::<String>(1);
    let mut slow_events = fixture.config.event_sink.fanout().subscribe();
    let slow_holder = tokio::spawn({
        let slow = slow.clone();
        async move {
            let mut held = Vec::new();
            while let Ok(event) = slow_events.recv().await {
                // Admit but never finish: guards stay alive.
                held.extend(slow.admit(event).await);
            }
        }
    });
    drop((slow_state, slow_wire_tx));

    // Connection A is fully served.
    assert_prompt_updates_precede_response(fixture.handle.clone(), &session_id).await;

    slow_holder.abort();
    slow_cancel.cancel();
}

/// Drive `session/prompt` through the real dispatcher with the connection's
/// event forwarders attached, and assert every assistant update precedes the
/// response on the wire.
pub(crate) async fn assert_prompt_updates_precede_response(
    agent: Arc<crate::agent::LocalAgentHandle>,
    session_id: &str,
) {
    assert_prompt_text_updates_precede_response(agent, session_id, "hello").await;
}

/// Same as [`assert_prompt_updates_precede_response`] for an arbitrary prompt
/// text (for example a native `/worktree` command).
pub(crate) async fn assert_prompt_text_updates_precede_response(
    agent: Arc<crate::agent::LocalAgentHandle>,
    session_id: &str,
    text: &str,
) {
    let state = WsServerState::new(agent.clone());
    let (wire_tx, mut wire_rx) = mpsc::channel::<String>(256);
    let cancel = CancellationToken::new();
    let delivery = crate::acp::delivery::ConnectionDelivery::new(
        Some(agent.config.event_sink.journal().clone()),
        cancel.clone(),
    );
    spawn_event_forwarders(
        state.clone(),
        ConnectionEventState {
            conn_id: "conn".to_string(),
            tx: wire_tx.clone(),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
            delivery: delivery.clone(),
        },
        test_translator(),
    );

    // Dispatch with the connection's delivery context, exactly as the
    // WebSocket transport does, so the response waits for the prompt's updates.
    super::shared::dispatch_rpc_message_with_context(
        super::shared::RpcDispatchState {
            agent,
            session_owners: state.session_owners.clone(),
            pending_elicitations: state.pending_elicitations.clone(),
            conn_id: "conn".to_string(),
            tx: wire_tx,
        },
        super::shared::RpcMessage {
            jsonrpc: "2.0".to_string(),
            method: "session/prompt".to_string(),
            params: serde_json::json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}]
            }),
            id: Some(serde_json::json!(42)),
        },
        super::shared::RpcDispatchContext {
            delivery: Some(delivery),
            ..Default::default()
        },
    )
    .await;

    let mut saw_update = false;
    loop {
        let raw = timeout(Duration::from_secs(5), wire_rx.recv())
            .await
            .expect("prompt response arrives")
            .expect("wire open");
        let message: serde_json::Value = serde_json::from_str(&raw).unwrap();
        if message["id"] == serde_json::json!(42) {
            assert!(
                message.get("error").is_none() || message["error"].is_null(),
                "prompt failed: {message}"
            );
            break;
        }
        if message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
        {
            saw_update = true;
        }
    }
    cancel.cancel();
    assert!(
        saw_update,
        "assistant update must be on the wire before the prompt response"
    );
}

/// Attach a connection (forwarders plus delivery) that owns `session_id`.
pub(crate) async fn attach_test_connection(
    state: &WsServerState,
    conn_id: &str,
    session_id: &str,
) -> (
    mpsc::Sender<String>,
    mpsc::Receiver<String>,
    Arc<crate::acp::delivery::ConnectionDelivery>,
    CancellationToken,
) {
    let (wire_tx, wire_rx) = mpsc::channel::<String>(256);
    let cancel = CancellationToken::new();
    let delivery = crate::acp::delivery::ConnectionDelivery::new(
        Some(state.agent.config.event_sink.journal().clone()),
        cancel.clone(),
    );
    state
        .session_owners
        .lock()
        .await
        .entry(session_id.to_string())
        .or_default()
        .insert(conn_id.to_string());
    spawn_event_forwarders(
        state.clone(),
        ConnectionEventState {
            conn_id: conn_id.to_string(),
            tx: wire_tx.clone(),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            forwarded_elicitations: Arc::new(Mutex::new(HashSet::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            connection_cancel: cancel.clone(),
            delivery: delivery.clone(),
        },
        test_translator(),
    );
    (wire_tx, wire_rx, delivery, cancel)
}

async fn embedded_messages_until(
    rx: &mut mpsc::Receiver<String>,
    mut matches: impl FnMut(&serde_json::Value) -> bool,
) -> Vec<serde_json::Value> {
    timeout(Duration::from_secs(5), async {
        let mut messages = Vec::new();
        loop {
            let raw = rx.recv().await.expect("embedded output remains open");
            let message = serde_json::from_str(&raw).expect("valid embedded JSON");
            let done = matches(&message);
            messages.push(message);
            if done {
                return messages;
            }
        }
    })
    .await
    .expect("expected embedded message arrives")
}

async fn load_embedded_session(
    connection: &super::websocket::EmbeddedAcpConnection,
    rx: &mut mpsc::Receiver<String>,
    session_id: &str,
) {
    connection
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": "load", "method": "session/load",
                "params": {"sessionId": session_id, "cwd": "/tmp", "mcpServers": []}
            })
            .to_string(),
        )
        .await
        .unwrap();
    let loaded = embedded_messages_until(rx, |m| m["id"] == "load").await;
    assert!(loaded.last().unwrap()["error"].is_null(), "{loaded:?}");
}

#[tokio::test]
async fn embedded_connections_restore_surviving_bridge_after_close() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let session_id = fixture
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new("/tmp"))
        .await
        .unwrap()
        .session_id
        .to_string();
    let server = super::websocket::EmbeddedAcpServer::new(fixture.handle.clone());
    let ((mut first, mut first_rx), (mut second, mut second_rx)) =
        tokio::join!(server.open(None), server.open(None));
    drop(server);
    load_embedded_session(&first, &mut first_rx, &session_id).await;
    let first_bridge = fixture.config.session_bridges.lock().unwrap()[&session_id]
        .bridge
        .clone();
    load_embedded_session(&second, &mut second_rx, &session_id).await;
    let second_bridge = fixture.config.session_bridges.lock().unwrap()[&session_id]
        .bridge
        .clone();
    assert_ne!(first_bridge.connection_id(), second_bridge.connection_id());

    second.close().await;
    second.close().await;
    let restored = fixture
        .config
        .session_bridges
        .lock()
        .unwrap()
        .get(&session_id)
        .expect("surviving embedded connection must regain the session bridge")
        .bridge
        .clone();
    assert!(Arc::ptr_eq(
        &restored.connection_state().unwrap(),
        &first_bridge.connection_state().unwrap(),
    ));
    let request = permission_request_for_test(&session_id, "after-close");
    let permission = tokio::spawn(async move { restored.request_permission(request).await });
    let messages = embedded_messages_until(&mut first_rx, |m| {
        m["method"] == "session/request_permission"
    })
    .await;
    first
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": messages.last().unwrap()["id"],
                "result": selected("allow_once")
            })
            .to_string(),
        )
        .await
        .unwrap();
    let response = timeout(Duration::from_secs(2), permission)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        response.outcome,
        RequestPermissionOutcome::Selected(selected) if selected.option_id.0.as_ref() == "allow_once"
    ));
    first.close().await;
    first.close().await;
    assert!(
        !fixture
            .config
            .session_bridges
            .lock()
            .unwrap()
            .contains_key(&session_id)
    );
}

#[tokio::test]
async fn embedded_drop_restores_surviving_bridge() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let session_id = fixture
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new("/tmp"))
        .await
        .unwrap()
        .session_id
        .to_string();
    let server = super::websocket::EmbeddedAcpServer::new(fixture.handle.clone());
    let (mut first, mut first_rx) = server.open(None).await;
    let (second, mut second_rx) = server.open(None).await;
    load_embedded_session(&first, &mut first_rx, &session_id).await;
    let first_state = fixture.config.session_bridges.lock().unwrap()[&session_id]
        .bridge
        .connection_state()
        .unwrap();
    load_embedded_session(&second, &mut second_rx, &session_id).await;
    let second_state = fixture.config.session_bridges.lock().unwrap()[&session_id]
        .bridge
        .connection_state()
        .unwrap();

    drop(second);
    let restored = timeout(Duration::from_secs(2), async {
        loop {
            let bridge = fixture
                .config
                .session_bridges
                .lock()
                .unwrap()
                .get(&session_id)
                .map(|route| route.bridge.clone());
            if let Some(bridge) = bridge
                && bridge
                    .connection_state()
                    .is_some_and(|state| Arc::ptr_eq(&state, &first_state))
            {
                break bridge;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("drop cleanup restores the surviving bridge");
    assert!(!second_state.is_active());

    let request = permission_request_for_test(&session_id, "after-drop");
    let permission = tokio::spawn(async move { restored.request_permission(request).await });
    let messages = embedded_messages_until(&mut first_rx, |m| {
        m["method"] == "session/request_permission"
    })
    .await;
    first
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": messages.last().unwrap()["id"],
                "result": selected("allow_once")
            })
            .to_string(),
        )
        .await
        .unwrap();
    let response = timeout(Duration::from_secs(2), permission)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        response.outcome,
        RequestPermissionOutcome::Selected(selected) if selected.option_id.0.as_ref() == "allow_once"
    ));
    first.close().await;
}

#[tokio::test]
async fn embedded_drop_unselected_then_close_active_leaves_no_dead_bridge() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let session_id = fixture
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new("/tmp"))
        .await
        .unwrap()
        .session_id
        .to_string();
    let server = super::websocket::EmbeddedAcpServer::new(fixture.handle.clone());
    let (first, mut first_rx) = server.open(None).await;
    let (mut second, mut second_rx) = server.open(None).await;
    load_embedded_session(&first, &mut first_rx, &session_id).await;
    let first_state = fixture.config.session_bridges.lock().unwrap()[&session_id]
        .bridge
        .connection_state()
        .unwrap();
    load_embedded_session(&second, &mut second_rx, &session_id).await;

    drop(first);
    second.close().await;
    drop(second);
    timeout(Duration::from_secs(2), async {
        loop {
            let route_removed = !fixture
                .config
                .session_bridges
                .lock()
                .unwrap()
                .contains_key(&session_id);
            if route_removed && !first_state.is_active() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("drop cleanup prevents restoring a dead subscriber after active close");
}

#[tokio::test]
async fn embedded_concurrent_closes_restore_third_connection_bridge() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let session_id = fixture
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new("/tmp"))
        .await
        .unwrap()
        .session_id
        .to_string();
    let server = super::websocket::EmbeddedAcpServer::new(fixture.handle.clone());
    let (mut first, mut first_rx) = server.open(None).await;
    let (mut survivor, mut survivor_rx) = server.open(None).await;
    let (mut last, mut last_rx) = server.open(None).await;
    load_embedded_session(&first, &mut first_rx, &session_id).await;
    load_embedded_session(&survivor, &mut survivor_rx, &session_id).await;
    let survivor_bridge = fixture.config.session_bridges.lock().unwrap()[&session_id]
        .bridge
        .clone();
    load_embedded_session(&last, &mut last_rx, &session_id).await;

    tokio::join!(first.close(), last.close());
    let restored = fixture.config.session_bridges.lock().unwrap()[&session_id]
        .bridge
        .clone();
    assert!(Arc::ptr_eq(
        &restored.connection_state().unwrap(),
        &survivor_bridge.connection_state().unwrap(),
    ));
    first.close().await;
    last.close().await;
    survivor.close().await;
}

#[tokio::test]
async fn embedded_connection_dispatches_sessions_and_streams_before_prompt_response() {
    let (fixture, _) = prompt_fixture().await;
    let (mut connection, mut rx) =
        super::websocket::EmbeddedAcpConnection::open(fixture.handle.clone(), None).await;
    assert!(connection.receive_json("not JSON").await.is_err());
    connection
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": "init", "method": "initialize",
                "params": {"protocolVersion": 1, "clientCapabilities": {}}
            })
            .to_string(),
        )
        .await
        .unwrap();
    let initialized = embedded_messages_until(&mut rx, |m| m["id"] == "init").await;
    assert_eq!(initialized.last().unwrap()["result"]["protocolVersion"], 1);
    connection
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": "new", "method": "session/new",
                "params": {"cwd": fixture._tempdir.path(), "mcpServers": []}
            })
            .to_string(),
        )
        .await
        .unwrap();
    let created = embedded_messages_until(&mut rx, |m| m["id"] == "new").await;
    let result = &created.last().unwrap()["result"];
    let session_id = result["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    assert!(
        result["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["id"] == "model")
    );

    connection
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": "prompt", "method": "session/prompt",
                "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}]}
            })
            .to_string(),
        )
        .await
        .unwrap();
    let messages = embedded_messages_until(&mut rx, |m| m["id"] == "prompt").await;
    assert!(messages.last().unwrap()["error"].is_null(), "{messages:?}");
    let text: String = messages
        .iter()
        .filter_map(|m| {
            (m["method"] == "session/update"
                && m["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
                .then(|| m["params"]["update"]["content"]["text"].as_str())
                .flatten()
        })
        .collect();
    assert_eq!(
        text, "held reply",
        "assistant text precedes prompt completion"
    );
    for method in ["session/load", "session/resume"] {
        connection
            .receive_json(&serde_json::json!({
                "jsonrpc": "2.0", "id": method, "method": method,
                "params": {"sessionId": session_id, "cwd": fixture._tempdir.path(), "mcpServers": []}
            }).to_string())
            .await.unwrap();
        let loaded = embedded_messages_until(&mut rx, |m| m["id"] == method).await;
        let response = loaded.last().unwrap();
        assert!(response["error"].is_null(), "{response}");
        assert!(response["result"]["configOptions"].is_array());
    }
    let marker = serde_json::json!({"jsonrpc": "2.0", "method": "embedding/marker"});
    connection.send_json(marker.to_string()).await.unwrap();
    let sent = embedded_messages_until(&mut rx, |m| m["method"] == "embedding/marker").await;
    assert_eq!(sent.last().unwrap(), &marker);
    connection.close().await;
    connection.close().await;
    drop(connection);
    timeout(Duration::from_secs(2), async {
        while rx.recv().await.is_some() {}
    })
    .await
    .expect("close cancels owned forwarders and bridge");
}

#[tokio::test]
async fn embedded_question_tool_round_trip_with_an_observer() {
    use crate::test_utils::{MockChatResponse, MockLlmProvider, SharedLlmProvider, TestAgent};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut provider = MockLlmProvider::new();
    let calls = AtomicUsize::new(0);
    provider.expect_chat_with_tools().returning(move |_, _| {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(MockChatResponse::with_tools(
                "Choose an option",
                vec![querymt::ToolCall {
                    id: "question-tool".into(),
                    call_type: "function".into(),
                    function: querymt::FunctionCall {
                        name: "question".into(),
                        arguments: serde_json::json!({"questions": [{
                            "question": "Which option?", "header": "Choice",
                            "options": [{"label": "A", "description": "First option"}]
                        }]})
                        .to_string(),
                    },
                }],
            )
            .into())
        } else {
            Ok(MockChatResponse::text_only("Answer received").into())
        }
    });
    let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(provider, vec![])).await;
    let session_id = fixture
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            fixture._tempdir.path(),
        ))
        .await
        .unwrap()
        .session_id
        .to_string();
    let server = super::websocket::EmbeddedAcpServer::new(fixture.handle.clone());
    let (mut observer, mut observer_rx) = server.open(None).await;
    let (mut controller, mut controller_rx) = server.open(None).await;
    for (connection, rx) in [
        (&observer, &mut observer_rx),
        (&controller, &mut controller_rx),
    ] {
        connection.receive_json(&serde_json::json!({
            "jsonrpc": "2.0", "id": "init", "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {"elicitation": {"form": {}}}}
        }).to_string()).await.unwrap();
        embedded_messages_until(rx, |m| m["id"] == "init").await;
    }
    load_embedded_session(&observer, &mut observer_rx, &session_id).await;
    controller
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": "prompt", "method": "session/prompt",
                "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "Ask me"}]}
            })
            .to_string(),
        )
        .await
        .unwrap();
    let messages =
        embedded_messages_until(&mut controller_rx, |m| m["method"] == "elicitation/create").await;
    let question = messages.last().unwrap();
    assert_eq!(question["params"]["sessionId"], session_id);
    assert_eq!(question["params"]["message"], "Which option?");
    // Loading the busy session remains observation, not an interaction takeover.
    observer.receive_json(&serde_json::json!({
        "jsonrpc": "2.0", "id": "reload", "method": "session/load",
        "params": {"sessionId": session_id, "cwd": fixture._tempdir.path(), "mcpServers": []}
    }).to_string()).await.unwrap();
    let observed = embedded_messages_until(&mut observer_rx, |m| m["id"] == "reload").await;
    assert!(observed.last().unwrap()["error"].is_null());
    assert!(observed.iter().all(|m| m["method"] != "elicitation/create"
        && m["method"]
            != crate::control::elicitation_recovery::ELICITATION_RECOVERY_AUTHORITY_NOTIFICATION));
    assert!(
        observed
            .iter()
            .any(|m| m["params"]["update"]["sessionUpdate"] == "tool_call")
    );
    controller
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": question["id"],
                "result": {"action": "accept", "content": {"selection": "A"}}
            })
            .to_string(),
        )
        .await
        .unwrap();
    let completed = embedded_messages_until(&mut controller_rx, |m| m["id"] == "prompt").await;
    assert!(
        completed.last().unwrap()["error"].is_null(),
        "{completed:?}"
    );
    assert_eq!(
        completed.last().unwrap()["result"]["stopReason"],
        "end_turn"
    );
    assert!(completed.iter().any(
        |m| m["params"]["update"]["sessionUpdate"] == "tool_call_update"
            && m["params"]["update"]["status"] == "completed"
    ));
    assert!(
        fixture
            .handle
            .pending_elicitations()
            .lock()
            .await
            .is_empty()
    );
    controller.close().await;
    observer.close().await;
}

#[tokio::test]
async fn embedded_connection_routes_permission_and_form_response_envelopes() {
    let (fixture, session_id) = prompt_fixture().await;
    let (mut connection, mut rx) =
        super::websocket::EmbeddedAcpConnection::open(fixture.handle.clone(), None).await;
    connection.receive_json(&serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {"elicitation": {"form": {}}}}
    }).to_string()).await.unwrap();
    embedded_messages_until(&mut rx, |m| m["id"] == 1).await;
    let permission =
        register_permission_for_test(&fixture, &session_id, "embedded-permission").await;
    connection.receive_json(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/load",
        "params": {"sessionId": session_id, "cwd": fixture._tempdir.path(), "mcpServers": []}
    }).to_string()).await.unwrap();
    let messages =
        embedded_messages_until(&mut rx, |m| m["method"] == "session/request_permission").await;
    let request = messages.last().unwrap();
    assert_eq!(request["params"]["sessionId"], session_id);
    connection
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": request["id"], "result": selected("allow_once")
            })
            .to_string(),
        )
        .await
        .unwrap();
    let answer = timeout(Duration::from_secs(2), permission)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.action, ElicitationAction::Accept);

    let (form_tx, form_rx) = oneshot::channel();
    insert_pending_elicitation(
        &fixture.handle.pending_elicitations(),
        "embedded-form".into(),
        session_id.clone(),
        form_tx,
    )
    .await;
    fixture
        .config
        .event_sink
        .fanout()
        .publish(elicitation_event(&session_id, "embedded-form"));
    let messages = embedded_messages_until(&mut rx, |m| m["method"] == "elicitation/create").await;
    let request = messages.last().unwrap();
    assert_eq!(
        request["params"]["_meta"]["querymt"]["elicitation_id"],
        "embedded-form"
    );
    connection
        .receive_json(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": request["id"],
                "result": {"action": "accept", "content": {"selection": "A"}}
            })
            .to_string(),
        )
        .await
        .unwrap();
    let answer = timeout(Duration::from_secs(2), form_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.action, ElicitationAction::Accept);
    assert_eq!(answer.content, Some(serde_json::json!({"selection": "A"})));
    connection.close().await;
}

#[tokio::test]
async fn embedded_connection_returns_unsent_json_and_drop_stops_output() {
    let fixture = crate::test_utils::TestAgent::new().await;
    let (mut connection, rx) =
        super::websocket::EmbeddedAcpConnection::open(fixture.handle.clone(), None).await;
    drop(rx);
    let unsent = "host response".to_string();
    assert_eq!(
        connection.send_json(unsent.clone()).await.unwrap_err().0,
        unsent
    );
    connection.close().await;
    connection.close().await;
    let (connection, mut rx) =
        super::websocket::EmbeddedAcpConnection::open(fixture.handle.clone(), None).await;
    drop(connection);
    assert!(
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .is_none()
    );
}

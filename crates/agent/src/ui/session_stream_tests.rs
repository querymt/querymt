//! Tests for replay/live session stream cursor semantics.

use crate::events::{AgentEventKind, EventOrigin};
use crate::session::backend::StorageBackend;
use crate::test_utils::{TestAgent, TestServerState};
use crate::ui::connection::spawn_event_forwarders;
use crate::ui::handlers::{handle_load_session, handle_subscribe_session};
use crate::ui::messages::StreamCursor;
use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep, timeout};

fn parse_message(json: &str) -> Value {
    serde_json::from_str(json).expect("valid JSON UI message")
}

#[tokio::test]
async fn load_session_includes_cursor_seq_from_audit_tail() -> Result<()> {
    let f = TestServerState::new().await;
    let session_id = f.agent.create_session().await;

    // Persist two audit events so load replay has a known tail.
    f.agent.handle.emit_event(
        &session_id,
        AgentEventKind::PromptReceived {
            content: "first".to_string(),
            message_id: Some("m1".to_string()),
        },
    );
    f.agent.handle.emit_event(
        &session_id,
        AgentEventKind::PromptReceived {
            content: "second".to_string(),
            message_id: Some("m2".to_string()),
        },
    );

    // Event observers persist asynchronously; wait briefly for DB visibility.
    sleep(Duration::from_millis(50)).await;

    let (tx, mut rx) = f.add_connection("conn-load").await;
    handle_load_session(&f.state, "conn-load", &session_id, &tx).await;

    let mut loaded_msg: Option<Value> = None;
    for _ in 0..4 {
        if let Some(msg) = rx.recv().await {
            let parsed = parse_message(&msg);
            if parsed["type"] == "session_loaded" {
                loaded_msg = Some(parsed);
                break;
            }
        }
    }

    let loaded = loaded_msg.expect("session_loaded should be sent");
    let events = loaded["audit"]["events"]
        .as_array()
        .expect("audit.events should be an array");
    let replay_tail = events
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .max()
        .expect("expected replayed events with seq");

    // session_loaded exposes cursor.local_seq at replay tail.
    assert_eq!(loaded["cursor"]["local_seq"].as_u64(), Some(replay_tail));

    Ok(())
}

#[tokio::test]
async fn subscribe_session_includes_cursor_seq_from_replay_tail() -> Result<()> {
    let f = TestServerState::new().await;
    let session_id = f.agent.create_session().await;

    f.agent.handle.emit_event(
        &session_id,
        AgentEventKind::PromptReceived {
            content: "hello".to_string(),
            message_id: Some("mx".to_string()),
        },
    );

    sleep(Duration::from_millis(50)).await;

    let (tx, mut rx) = f.add_connection("conn-sub").await;
    handle_subscribe_session(&f.state, "conn-sub", &session_id, None, &tx).await;

    let msg = rx.recv().await.expect("session_events should be sent");
    let parsed = parse_message(&msg);
    assert_eq!(parsed["type"], "session_events");

    let events = parsed["events"]
        .as_array()
        .expect("events should be an array");
    let replay_tail = events
        .iter()
        .filter_map(|e| e["stream_seq"].as_u64())
        .max()
        .unwrap_or(0);

    // session_events exposes cursor.local_seq at replay tail.
    assert_eq!(parsed["cursor"]["local_seq"].as_u64(), Some(replay_tail));

    Ok(())
}

#[tokio::test]
async fn forwarder_drops_event_when_seq_is_at_or_below_cursor() -> Result<()> {
    let agent = TestAgent::new().await;
    let fanout = Arc::new(crate::event_fanout::EventFanout::new());

    let session_id = "s-overlap".to_string();
    let conn_id = "conn-overlap".to_string();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_secs() as i64;

    let state = super::ServerState {
        agent: agent.handle.clone(),
        view_store: agent.storage.view_store().expect("view store"),
        session_store: agent.storage.session_store(),
        default_cwd: None,
        event_sources: vec![fanout.clone()],
        connections: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(HashMap::from([(
            session_id.clone(),
            super::session::PRIMARY_AGENT_ID.to_string(),
        )]))),
        session_cwds: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        workspace_manager: crate::index::WorkspaceIndexManagerActor::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        ),
        model_cache: moka::future::Cache::new(100),
        oauth_flows: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        oauth_callback_listener: Arc::new(tokio::sync::Mutex::new(None)),
    };

    {
        let mut connections = state.connections.lock().await;
        connections.insert(
            conn_id.clone(),
            super::ConnectionState {
                routing_mode: crate::ui::messages::RoutingMode::Single,
                active_agent_id: super::session::PRIMARY_AGENT_ID.to_string(),
                sessions: HashMap::new(),
                subscribed_sessions: HashSet::from([session_id.clone()]),
                session_cursors: HashMap::from([(
                    session_id.clone(),
                    StreamCursor {
                        local_seq: 10,
                        remote_seq_by_source: HashMap::new(),
                    },
                )]),
                current_workspace_root: None,
                file_index_forwarder: None,
            },
        );
    }

    let (tx, mut rx) = mpsc::channel(8);
    spawn_event_forwarders(state.clone(), conn_id.clone(), tx);

    fanout.publish(crate::events::EventEnvelope::Durable(
        crate::events::DurableEvent {
            event_id: "e-dup".into(),
            stream_seq: 10,
            timestamp: now,
            session_id: session_id.clone(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::PromptReceived {
                content: "duplicate".to_string(),
                message_id: Some("dupe-1".to_string()),
            },
        },
    ));

    let received = timeout(Duration::from_millis(150), rx.recv()).await;
    assert!(
        received.is_err(),
        "expected no forwarded message for event at cursor boundary"
    );

    {
        let connections = state.connections.lock().await;
        let conn = connections.get(&conn_id).expect("connection should exist");
        assert_eq!(
            conn.session_cursors.get(&session_id).map(|c| c.local_seq),
            Some(10)
        );
    }

    fanout.publish(crate::events::EventEnvelope::Durable(
        crate::events::DurableEvent {
            event_id: "e-fresh".into(),
            stream_seq: 11,
            timestamp: now + 1,
            session_id: session_id.clone(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::PromptReceived {
                content: "fresh".to_string(),
                message_id: Some("fresh-1".to_string()),
            },
        },
    ));

    let forwarded = timeout(Duration::from_millis(250), rx.recv())
        .await
        .expect("newer event should be delivered")
        .expect("channel should remain open");
    let forwarded_msg = parse_message(&forwarded);
    assert_eq!(forwarded_msg["type"], "event");
    assert_eq!(forwarded_msg["session_id"], session_id);
    assert_eq!(forwarded_msg["event"]["stream_seq"].as_u64(), Some(11));

    {
        let connections = state.connections.lock().await;
        let conn = connections.get(&conn_id).expect("connection should exist");
        assert_eq!(
            conn.session_cursors.get(&session_id).map(|c| c.local_seq),
            Some(11)
        );
    }

    Ok(())
}

/// Ephemeral events (seq=0) must never be cursor-filtered.
///
/// The UI forwarder uses the durable cursor to dedup replay/live overlap.
/// Ephemeral events have no sequence (seq=0) and must always be forwarded
/// to live subscribers regardless of cursor position.
#[tokio::test]
async fn forwarder_does_not_drop_ephemeral_events_despite_cursor() -> Result<()> {
    let agent = TestAgent::new().await;
    let fanout = Arc::new(crate::event_fanout::EventFanout::new());

    let session_id = "s-ephemeral".to_string();
    let conn_id = "conn-ephemeral".to_string();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_secs() as i64;

    let state = super::ServerState {
        agent: agent.handle.clone(),
        view_store: agent.storage.view_store().expect("view store"),
        session_store: agent.storage.session_store(),
        default_cwd: None,
        event_sources: vec![fanout.clone()],
        connections: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(HashMap::from([(
            session_id.clone(),
            super::session::PRIMARY_AGENT_ID.to_string(),
        )]))),
        session_cwds: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        workspace_manager: crate::index::WorkspaceIndexManagerActor::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        ),
        model_cache: moka::future::Cache::new(100),
        oauth_flows: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        oauth_callback_listener: Arc::new(tokio::sync::Mutex::new(None)),
    };

    // Set cursor to 10 — any durable event with seq <= 10 would be dropped.
    // But ephemeral events (seq=0) must NOT be dropped.
    {
        let mut connections = state.connections.lock().await;
        connections.insert(
            conn_id.clone(),
            super::ConnectionState {
                routing_mode: crate::ui::messages::RoutingMode::Single,
                active_agent_id: super::session::PRIMARY_AGENT_ID.to_string(),
                sessions: HashMap::new(),
                subscribed_sessions: HashSet::from([session_id.clone()]),
                session_cursors: HashMap::from([(
                    session_id.clone(),
                    StreamCursor {
                        local_seq: 10,
                        remote_seq_by_source: HashMap::new(),
                    },
                )]),
                current_workspace_root: None,
                file_index_forwarder: None,
            },
        );
    }

    let (tx, mut rx) = mpsc::channel(8);
    spawn_event_forwarders(state.clone(), conn_id.clone(), tx);

    // Publish an ephemeral event (seq=0) through the fanout
    fanout.publish(crate::events::EventEnvelope::Ephemeral(
        crate::events::EphemeralEvent {
            timestamp: now,
            session_id: session_id.clone(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantContentDelta {
                content: "streaming token".to_string(),
                message_id: "m-eph".to_string(),
            },
        },
    ));

    // The ephemeral event MUST be forwarded despite cursor being at 10
    let forwarded = timeout(Duration::from_millis(250), rx.recv())
        .await
        .expect("ephemeral event must NOT be dropped by cursor filter")
        .expect("channel should remain open");

    let msg = parse_message(&forwarded);
    assert_eq!(msg["type"], "event");
    assert_eq!(msg["session_id"], session_id);
    assert_eq!(
        msg["event"]["kind"]["type"], "assistant_content_delta",
        "forwarded event should be the ephemeral content delta"
    );

    Ok(())
}

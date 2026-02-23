//! Tests for replay/live session stream cursor semantics.

use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::events::{AgentEvent, AgentEventKind, EventOrigin};
use crate::session::backend::StorageBackend;
use crate::session::sqlite_storage::SqliteStorage;
use crate::test_utils::empty_plugin_registry;
use crate::ui::connection::spawn_event_forwarders;
use crate::ui::handlers::{handle_load_session, handle_subscribe_session};
use anyhow::Result;
use querymt::LLMParams;
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
    let (registry, _config_dir) = empty_plugin_registry()?;
    let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await?);

    let builder = AgentConfigBuilder::new(
        Arc::new(registry),
        storage.session_store(),
        LLMParams::new().provider("mock").model("mock"),
    );
    builder.add_observer(storage.event_observer());

    let config = Arc::new(builder.build());
    let handle = Arc::new(crate::agent::AgentHandle::from_config(config));

    let session = storage
        .session_store()
        .create_session(None, None, None, None)
        .await?;
    let session_id = session.public_id.clone();

    // Persist two audit events so load replay has a known tail.
    handle.emit_event(
        &session_id,
        AgentEventKind::PromptReceived {
            content: "first".to_string(),
            message_id: Some("m1".to_string()),
        },
    );
    handle.emit_event(
        &session_id,
        AgentEventKind::PromptReceived {
            content: "second".to_string(),
            message_id: Some("m2".to_string()),
        },
    );

    // Event observers persist asynchronously; wait briefly for DB visibility.
    sleep(Duration::from_millis(50)).await;

    let state = super::ServerState {
        agent: handle,
        view_store: storage.clone(),
        session_store: storage.clone(),
        default_cwd: None,
        event_sources: vec![],
        connections: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
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
            "conn-load".to_string(),
            super::ConnectionState {
                routing_mode: crate::ui::messages::RoutingMode::Single,
                active_agent_id: super::session::PRIMARY_AGENT_ID.to_string(),
                sessions: HashMap::new(),
                subscribed_sessions: HashSet::new(),
                session_cursors: HashMap::new(),
                current_workspace_root: None,
                file_index_forwarder: None,
            },
        );
    }

    let (tx, mut rx) = mpsc::channel(16);
    handle_load_session(&state, "conn-load", &session_id, &tx).await;

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

    // RED expectation: session_loaded exposes cursor_seq at replay tail.
    assert_eq!(loaded["cursor_seq"].as_u64(), Some(replay_tail));

    Ok(())
}

#[tokio::test]
async fn subscribe_session_includes_cursor_seq_from_replay_tail() -> Result<()> {
    let (registry, _config_dir) = empty_plugin_registry()?;
    let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await?);

    let builder = AgentConfigBuilder::new(
        Arc::new(registry),
        storage.session_store(),
        LLMParams::new().provider("mock").model("mock"),
    );
    builder.add_observer(storage.event_observer());

    let config = Arc::new(builder.build());
    let handle = Arc::new(crate::agent::AgentHandle::from_config(config));

    let session = storage
        .session_store()
        .create_session(None, None, None, None)
        .await?;
    let session_id = session.public_id.clone();

    handle.emit_event(
        &session_id,
        AgentEventKind::PromptReceived {
            content: "hello".to_string(),
            message_id: Some("mx".to_string()),
        },
    );

    sleep(Duration::from_millis(50)).await;

    let state = super::ServerState {
        agent: handle,
        view_store: storage.clone(),
        session_store: storage.clone(),
        default_cwd: None,
        event_sources: vec![],
        connections: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
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
            "conn-sub".to_string(),
            super::ConnectionState {
                routing_mode: crate::ui::messages::RoutingMode::Single,
                active_agent_id: super::session::PRIMARY_AGENT_ID.to_string(),
                sessions: HashMap::new(),
                subscribed_sessions: HashSet::new(),
                session_cursors: HashMap::new(),
                current_workspace_root: None,
                file_index_forwarder: None,
            },
        );
    }

    let (tx, mut rx) = mpsc::channel(16);
    handle_subscribe_session(&state, "conn-sub", &session_id, None, &tx).await;

    let msg = rx.recv().await.expect("session_events should be sent");
    let parsed = parse_message(&msg);
    assert_eq!(parsed["type"], "session_events");

    let events = parsed["events"]
        .as_array()
        .expect("events should be an array");
    let replay_tail = events
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .max()
        .unwrap_or(0);

    // RED expectation: session_events exposes cursor_seq at replay tail.
    assert_eq!(parsed["cursor_seq"].as_u64(), Some(replay_tail));

    Ok(())
}

#[tokio::test]
async fn forwarder_drops_event_when_seq_is_at_or_below_cursor() -> Result<()> {
    let (registry, _config_dir) = empty_plugin_registry()?;
    let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await?);

    let builder = AgentConfigBuilder::new(
        Arc::new(registry),
        storage.session_store(),
        LLMParams::new().provider("mock").model("mock"),
    );

    let config = Arc::new(builder.build());
    let handle = Arc::new(crate::agent::AgentHandle::from_config(config));
    let bus = handle.event_bus();

    let session_id = "s-overlap".to_string();
    let conn_id = "conn-overlap".to_string();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_secs() as i64;

    let state = super::ServerState {
        agent: handle,
        view_store: storage.clone(),
        session_store: storage.clone(),
        default_cwd: None,
        event_sources: vec![bus.clone()],
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
                session_cursors: HashMap::from([(session_id.clone(), 10)]),
                current_workspace_root: None,
                file_index_forwarder: None,
            },
        );
    }

    let (tx, mut rx) = mpsc::channel(8);
    spawn_event_forwarders(state.clone(), conn_id.clone(), tx);

    bus.publish_raw(AgentEvent {
        seq: 10,
        timestamp: now,
        session_id: session_id.clone(),
        origin: EventOrigin::Local,
        source_node: None,
        kind: AgentEventKind::PromptReceived {
            content: "duplicate".to_string(),
            message_id: Some("dupe-1".to_string()),
        },
    });

    let received = timeout(Duration::from_millis(150), rx.recv()).await;
    assert!(
        received.is_err(),
        "expected no forwarded message for event at cursor boundary"
    );

    {
        let connections = state.connections.lock().await;
        let conn = connections.get(&conn_id).expect("connection should exist");
        assert_eq!(conn.session_cursors.get(&session_id).copied(), Some(10));
    }

    bus.publish_raw(AgentEvent {
        seq: 11,
        timestamp: now + 1,
        session_id: session_id.clone(),
        origin: EventOrigin::Local,
        source_node: None,
        kind: AgentEventKind::PromptReceived {
            content: "fresh".to_string(),
            message_id: Some("fresh-1".to_string()),
        },
    });

    let forwarded = timeout(Duration::from_millis(250), rx.recv())
        .await
        .expect("newer event should be delivered")
        .expect("channel should remain open");
    let forwarded_msg = parse_message(&forwarded);
    assert_eq!(forwarded_msg["type"], "event");
    assert_eq!(forwarded_msg["session_id"], session_id);
    assert_eq!(forwarded_msg["event"]["seq"].as_u64(), Some(11));

    {
        let connections = state.connections.lock().await;
        let conn = connections.get(&conn_id).expect("connection should exist");
        assert_eq!(conn.session_cursors.get(&session_id).copied(), Some(11));
    }

    Ok(())
}

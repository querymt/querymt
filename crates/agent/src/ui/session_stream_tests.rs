//! Tests for replay/live session stream cursor semantics.

use crate::api::{AgentInfra, ProfileRuntimeHandle};
use crate::event_fanout::EventFanout;
use crate::events::{AgentEventKind, EventOrigin};
use crate::profiles::{LocalProfileCatalog, ProfileRuntimeManager};
use crate::session::backend::StorageBackend;
use crate::session::sqlite_storage::SqliteStorage;
use crate::test_utils::{TestAgent, TestServerState, empty_plugin_registry};
use crate::ui::connection::{send_message, spawn_event_forwarders};
use crate::ui::handlers::{handle_load_session, handle_subscribe_session};
use crate::ui::messages::StreamCursor;
use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep, timeout};
use tokio_util::sync::CancellationToken;

fn parse_message(json: &str) -> Value {
    serde_json::from_str(json).expect("message should be valid json")
}

fn write_profile(dir: &Path, name: &str) {
    std::fs::write(
        dir.join(name),
        r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
    )
    .expect("failed to write profile");
}

fn write_quorum_profile(dir: &Path, name: &str) {
    std::fs::write(
        dir.join(name),
        r#"
[quorum]
delegation = true

[planner]
provider = "test"
model = "test-model"

[[delegates]]
id = "coder"
provider = "test"
model = "test-model"
"#,
    )
    .expect("failed to write quorum profile");
}

async fn test_profile_manager_with_infra(
    dir: &Path,
    storage: Arc<SqliteStorage>,
    event_fanout: Arc<EventFanout>,
) -> ProfileRuntimeHandle {
    let (registry, _registry_dir) = empty_plugin_registry().expect("empty plugin registry");
    let infra = AgentInfra {
        plugin_registry: Arc::new(registry),
        storage: Some(storage),
        session_mcp_attachment_source: None,
        event_fanout: Some(event_fanout),
    };
    let catalog: Arc<dyn crate::profiles::ProfileCatalog> = Arc::new(
        LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir)
            .build(),
    );
    let profiles: ProfileRuntimeHandle = Arc::new(ProfileRuntimeManager::with_infra_boxed(
        catalog, "alpha", infra,
    ));
    profiles
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
    let data = &loaded["data"];
    let events = data["audit"]["events"]
        .as_array()
        .expect("audit.events should be an array");
    let replay_tail = events
        .iter()
        .filter_map(|e| e["seq"].as_i64())
        .max()
        .expect("expected replayed events with seq");

    // session_loaded exposes cursor.local_seq at replay tail.
    assert_eq!(data["cursor"]["local_seq"].as_i64(), Some(replay_tail));

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

    let data = &parsed["data"];
    let events = data["events"]
        .as_array()
        .expect("events should be an array");
    // EventEnvelope is adjacently tagged: durable events have stream_seq under ["data"]["stream_seq"]
    let replay_tail = events
        .iter()
        .filter_map(|e| e["data"]["stream_seq"].as_i64())
        .max()
        .unwrap_or(0);

    // session_events exposes cursor.local_seq at replay tail.
    assert_eq!(data["cursor"]["local_seq"].as_i64(), Some(replay_tail));

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
        profiles: None,
        connections: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        connection_senders: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(HashMap::from([(
            session_id.clone(),
            super::session::PRIMARY_AGENT_ID.to_string(),
        )]))),
        session_cwds: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        workspace_manager: crate::index::WorkspaceIndexManagerActor::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        ),
        oauth_service: agent.handle.oauth_service.clone(),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        #[cfg(feature = "remote")]
        remote_node_cache: Arc::new(tokio::sync::Mutex::new(None)),
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
    assert_eq!(forwarded_msg["data"]["session_id"], session_id);
    // EventEnvelope is adjacently tagged: durable event fields are under ["data"]["event"]["data"]
    assert_eq!(
        forwarded_msg["data"]["event"]["data"]["stream_seq"].as_i64(),
        Some(11)
    );

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

#[tokio::test]
async fn forwarder_uses_shared_profile_runtime_fanout_without_polling() -> Result<()> {
    let agent = TestAgent::new().await;
    let dir = tempfile::TempDir::new().expect("create temp profile dir");
    write_profile(dir.path(), "alpha.toml");
    let profiles = test_profile_manager_with_infra(
        dir.path(),
        agent.storage.clone(),
        agent.handle.config.event_sink.fanout().clone(),
    )
    .await;

    let session_id = "s-profile-live".to_string();
    let conn_id = "conn-profile-live".to_string();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_secs() as i64;

    let state = super::ServerState {
        agent: agent.handle.clone(),
        view_store: agent.storage.view_store().expect("view store"),
        session_store: agent.storage.session_store(),
        default_cwd: None,
        event_sources: vec![agent.handle.config.event_sink.fanout().clone()],
        profiles: Some(profiles.clone()),
        connections: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        connection_senders: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(HashMap::from([(
            session_id.clone(),
            super::session::PRIMARY_AGENT_ID.to_string(),
        )]))),
        session_cwds: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        workspace_manager: crate::index::WorkspaceIndexManagerActor::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        ),
        oauth_service: agent.handle.oauth_service.clone(),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        #[cfg(feature = "remote")]
        remote_node_cache: Arc::new(tokio::sync::Mutex::new(None)),
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
                session_cursors: HashMap::new(),
                current_workspace_root: None,
                file_index_forwarder: None,
            },
        );
    }

    let (tx, mut rx) = mpsc::channel(8);
    spawn_event_forwarders(state.clone(), conn_id.clone(), tx);

    let runtime = profiles
        .runtime_for_profile("alpha")
        .await
        .expect("runtime materializes after forwarders start");
    profiles
        .bind_session_to_runtime(session_id.clone(), &runtime)
        .await
        .expect("session binds to profile runtime");

    assert!(
        Arc::ptr_eq(
            state.agent.config.event_sink.fanout(),
            runtime.agent().handle().config.event_sink.fanout()
        ),
        "profile runtimes should share the root live event fanout"
    );

    runtime.agent().handle().config.event_sink.fanout().publish(
        crate::events::EventEnvelope::Durable(crate::events::DurableEvent {
            event_id: "e-profile-live".into(),
            stream_seq: 1,
            timestamp: now,
            session_id: session_id.clone(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::PromptReceived {
                content: "from profile runtime".to_string(),
                message_id: Some("profile-msg-1".to_string()),
            },
        }),
    );

    let forwarded = timeout(Duration::from_millis(500), rx.recv())
        .await
        .expect("profile runtime event should be delivered without reconnect")
        .expect("channel should remain open");
    let msg = parse_message(&forwarded);
    assert_eq!(msg["type"], "event");
    assert_eq!(msg["data"]["session_id"], session_id);
    assert_eq!(msg["data"]["profile_id"], "alpha");
    assert_eq!(msg["data"]["agent_id"], super::session::PRIMARY_AGENT_ID);
    assert_eq!(msg["data"]["event"]["data"]["stream_seq"].as_i64(), Some(1));

    {
        let connections = state.connections.lock().await;
        let conn = connections.get(&conn_id).expect("connection should exist");
        assert_eq!(
            conn.session_cursors.get(&session_id).map(|c| c.local_seq),
            Some(1)
        );
    }

    profiles.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn forwarder_uses_shared_quorum_profile_fanout_without_polling() -> Result<()> {
    let agent = TestAgent::new().await;
    let dir = tempfile::TempDir::new().expect("create temp profile dir");
    write_quorum_profile(dir.path(), "alpha.toml");
    let root_fanout = agent.handle.config.event_sink.fanout().clone();
    let profiles =
        test_profile_manager_with_infra(dir.path(), agent.storage.clone(), root_fanout.clone())
            .await;

    let session_id = "s-quorum-profile-live".to_string();
    let conn_id = "conn-quorum-profile-live".to_string();
    let state = super::ServerState {
        agent: agent.handle.clone(),
        view_store: agent.storage.view_store().expect("view store"),
        session_store: agent.storage.session_store(),
        default_cwd: None,
        event_sources: vec![root_fanout.clone()],
        profiles: Some(profiles.clone()),
        connections: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        connection_senders: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(HashMap::from([(
            session_id.clone(),
            super::session::PRIMARY_AGENT_ID.to_string(),
        )]))),
        session_cwds: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        workspace_manager: crate::index::WorkspaceIndexManagerActor::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        ),
        oauth_service: agent.handle.oauth_service.clone(),
        shutdown_token: CancellationToken::new(),
        #[cfg(feature = "remote")]
        remote_node_cache: Arc::new(tokio::sync::Mutex::new(None)),
    };
    state.connections.lock().await.insert(
        conn_id.clone(),
        super::ConnectionState {
            routing_mode: crate::ui::messages::RoutingMode::Single,
            active_agent_id: super::session::PRIMARY_AGENT_ID.to_string(),
            sessions: HashMap::new(),
            subscribed_sessions: HashSet::from([session_id.clone()]),
            session_cursors: HashMap::new(),
            current_workspace_root: None,
            file_index_forwarder: None,
        },
    );

    let (tx, mut rx) = mpsc::channel(8);
    spawn_event_forwarders(state, conn_id, tx);

    let runtime = profiles
        .runtime_for_profile("alpha")
        .await
        .expect("quorum runtime materializes after forwarders start");
    profiles
        .bind_session_to_runtime(session_id.clone(), &runtime)
        .await
        .expect("session binds to quorum runtime");
    let quorum = runtime.agent().quorum().expect("quorum runtime");
    let planner = quorum.planner();
    let delegate = quorum.delegate("coder").expect("coder delegate");

    assert!(Arc::ptr_eq(&root_fanout, &quorum.event_fanout()));
    assert!(Arc::ptr_eq(&root_fanout, planner.event_fanout()));
    assert!(Arc::ptr_eq(&root_fanout, delegate.event_fanout()));

    for (stream_seq, event_id, content, source) in [
        (
            1,
            "e-quorum-planner",
            "from planner",
            planner.event_fanout(),
        ),
        (
            2,
            "e-quorum-delegate",
            "from delegate",
            delegate.event_fanout(),
        ),
    ] {
        source.publish(crate::events::EventEnvelope::Durable(
            crate::events::DurableEvent {
                event_id: event_id.into(),
                stream_seq,
                timestamp: 1,
                session_id: session_id.clone(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::PromptReceived {
                    content: content.into(),
                    message_id: Some(format!("{event_id}-message")),
                },
            },
        ));
    }

    let first = timeout(Duration::from_millis(500), rx.recv())
        .await
        .expect("planner event should arrive")
        .expect("channel should remain open");
    let second = timeout(Duration::from_millis(500), rx.recv())
        .await
        .expect("delegate event should arrive")
        .expect("channel should remain open");
    let event_ids = HashSet::from([
        parse_message(&first)["data"]["event"]["data"]["event_id"]
            .as_str()
            .unwrap()
            .to_string(),
        parse_message(&second)["data"]["event"]["data"]["event_id"]
            .as_str()
            .unwrap()
            .to_string(),
    ]);
    assert_eq!(
        event_ids,
        HashSet::from([
            "e-quorum-planner".to_string(),
            "e-quorum-delegate".to_string()
        ])
    );
    assert!(
        timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err()
    );

    profiles.shutdown().await;
    Ok(())
}

/// Ephemeral events (seq=0) must never be cursor-filtered.
///
/// The UI forwarder uses the durable cursor to dedup replay/live overlap.
/// Ephemeral events have no sequence (seq=0) and must always be forwarded
/// to live subscribers regardless of cursor position.
#[tokio::test]
async fn forwarder_stops_when_shutdown_token_is_cancelled() -> Result<()> {
    let agent = TestAgent::new().await;
    let fanout = Arc::new(crate::event_fanout::EventFanout::new());
    let shutdown_token = CancellationToken::new();

    let session_id = "s-shutdown".to_string();
    let conn_id = "conn-shutdown".to_string();
    let state = super::ServerState {
        agent: agent.handle.clone(),
        view_store: agent.storage.view_store().expect("view store"),
        session_store: agent.storage.session_store(),
        default_cwd: None,
        event_sources: vec![fanout.clone()],
        profiles: None,
        connections: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        connection_senders: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(HashMap::from([(
            session_id.clone(),
            super::session::PRIMARY_AGENT_ID.to_string(),
        )]))),
        session_cwds: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        workspace_manager: crate::index::WorkspaceIndexManagerActor::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        ),
        oauth_service: agent.handle.oauth_service.clone(),
        shutdown_token: shutdown_token.clone(),
        #[cfg(feature = "remote")]
        remote_node_cache: Arc::new(tokio::sync::Mutex::new(None)),
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
                session_cursors: HashMap::new(),
                current_workspace_root: None,
                file_index_forwarder: None,
            },
        );
    }

    let (tx, mut rx) = mpsc::channel(8);
    spawn_event_forwarders(state.clone(), conn_id.clone(), tx.clone());
    shutdown_token.cancel();

    fanout.publish(crate::events::EventEnvelope::Ephemeral(
        crate::events::EphemeralEvent {
            timestamp: 1,
            session_id,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantContentDelta {
                content: "after shutdown".to_string(),
                message_id: "m-shutdown".to_string(),
            },
        },
    ));

    let received = timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(
        matches!(received, Ok(None) | Err(_)),
        "forwarder should close or remain silent once the shutdown token is cancelled"
    );

    Ok(())
}

#[tokio::test]
async fn send_message_returns_error_when_shutdown_closes_channel() {
    let (tx, rx) = mpsc::channel::<String>(1);
    drop(rx);

    let err = send_message(
        &tx,
        crate::ui::messages::UiServerMessage::Error {
            message: "closed".to_string(),
        },
    )
    .await
    .expect_err("closed connection should return an error");
    assert!(err.contains("Failed to send"));
}

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
        profiles: None,
        connections: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        connection_senders: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(HashMap::from([(
            session_id.clone(),
            super::session::PRIMARY_AGENT_ID.to_string(),
        )]))),
        session_cwds: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        workspace_manager: crate::index::WorkspaceIndexManagerActor::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        ),
        oauth_service: agent.handle.oauth_service.clone(),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
        #[cfg(feature = "remote")]
        remote_node_cache: Arc::new(tokio::sync::Mutex::new(None)),
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
    assert_eq!(msg["data"]["session_id"], session_id);
    // EventEnvelope is adjacently tagged: ephemeral event fields are under ["data"]["event"]["data"]
    // AgentEventKind is also adjacently tagged: kind fields are under ["data"]["event"]["data"]["kind"]
    assert_eq!(
        msg["data"]["event"]["data"]["kind"]["type"], "assistant_content_delta",
        "forwarded event should be the ephemeral content delta"
    );

    Ok(())
}

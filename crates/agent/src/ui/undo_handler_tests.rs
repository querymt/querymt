//! Integration tests for UI undo/redo handler
//!
//! These tests validate the complete flow from UI handler through to file restoration.
//! They simulate what happens when a user clicks "undo" in the dashboard.

use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::{SessionRuntime, SnapshotPolicy};
use crate::elicitation::ElicitationAction;
use crate::model::{AgentMessage, MessagePart};
use crate::session::backend::StorageBackend;
use crate::session::domain::ForkOrigin;
use crate::session::sqlite_storage::SqliteStorage;
use crate::snapshot::backend::SnapshotBackend;
use crate::snapshot::git::GitSnapshotBackend;
use crate::test_utils::{DelegateTestFixture, TestServerState, empty_plugin_registry};
use crate::ui::handlers::{
    handle_elicitation_response, handle_load_session, handle_ui_message, handle_undo,
};
use crate::ui::messages::UiClientMessage;
use anyhow::Result;
use querymt::LLMParams;
use querymt::chat::ChatRole;
use serde_json::Value;
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::Duration;
use uuid::Uuid;

/// Build a default `ServerState` from an agent handle and storage backend.
fn test_server_state(
    handle: &Arc<crate::agent::AgentHandle>,
    storage: &dyn StorageBackend,
) -> super::ServerState {
    super::ServerState {
        agent: handle.clone(),
        view_store: storage.view_store().expect("view store"),
        session_store: storage.session_store(),
        default_cwd: None,
        event_sources: vec![],
        connections: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        session_cwds: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        workspace_manager: crate::index::WorkspaceIndexManagerActor::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        ),
        model_cache: moka::future::Cache::new(100),
        oauth_flows: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        oauth_callback_listener: Arc::new(tokio::sync::Mutex::new(None)),
    }
}

/// Helper to capture messages sent by the handler
async fn collect_message(rx: &mut mpsc::Receiver<String>) -> Option<Value> {
    if let Some(msg_str) = rx.recv().await {
        serde_json::from_str(&msg_str).ok()
    } else {
        None
    }
}

#[tokio::test]
async fn test_undo_handler_single_agent() -> Result<()> {
    // Setup
    let worktree = TempDir::new()?;
    let snapshot_base = TempDir::new()?;
    fs::write(worktree.path().join("test.txt"), "original")?;

    let (registry, _config_dir) = empty_plugin_registry()?;
    let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await?);
    let snapshot_backend = Arc::new(GitSnapshotBackend::with_snapshot_base(
        snapshot_base.path().to_path_buf(),
    ));

    let builder = AgentConfigBuilder::new(
        Arc::new(registry),
        storage.session_store(),
        storage.event_journal(),
        LLMParams::new().provider("mock").model("mock"),
    )
    .with_snapshot_policy(SnapshotPolicy::Diff)
    .with_snapshot_backend(snapshot_backend.clone());

    let config = Arc::new(builder.build());
    let handle = Arc::new(crate::agent::AgentHandle::from_config(config.clone()));

    // Create session with cwd
    let session = storage
        .session_store()
        .create_session(None, Some(worktree.path().to_path_buf()), None, None)
        .await?;
    let session_id = session.public_id.clone();

    // Spawn a SessionActor for undo to work (routes through kameo)
    {
        let runtime = SessionRuntime::new(
            Some(worktree.path().to_path_buf()),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            vec![],
        );
        let actor = crate::agent::session_actor::SessionActor::new(
            config.clone(),
            session_id.clone(),
            runtime,
        );
        let actor_ref = kameo::actor::Spawn::spawn(actor);
        let mut registry = handle.registry.lock().await;
        registry.insert(session_id.clone(), actor_ref);
    }

    // Add user message
    let user_msg_id = Uuid::new_v4().to_string();
    let user_msg = AgentMessage {
        id: user_msg_id.clone(),
        session_id: session_id.clone(),
        role: ChatRole::User,
        parts: vec![MessagePart::Text {
            content: "Change the file".to_string(),
        }],
        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        parent_message_id: None,
    };
    storage
        .session_store()
        .add_message(&session_id, user_msg)
        .await?;

    // Take pre-snapshot
    let pre_snapshot = snapshot_backend.track(worktree.path()).await?;
    let turn_id = Uuid::now_v7().to_string();
    storage
        .session_store()
        .add_message(
            &session_id,
            AgentMessage {
                id: Uuid::new_v4().to_string(),
                session_id: session_id.clone(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::TurnSnapshotStart {
                    turn_id: turn_id.clone(),
                    snapshot_id: pre_snapshot.clone(),
                }],
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
            },
        )
        .await?;

    // Modify file
    fs::write(worktree.path().join("test.txt"), "modified")?;

    // Take post-snapshot
    let post_snapshot = snapshot_backend.track(worktree.path()).await?;
    let changed_paths = snapshot_backend
        .diff(worktree.path(), &pre_snapshot, &post_snapshot)
        .await?;
    storage
        .session_store()
        .add_message(
            &session_id,
            AgentMessage {
                id: Uuid::new_v4().to_string(),
                session_id: session_id.clone(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::TurnSnapshotPatch {
                    turn_id,
                    snapshot_id: post_snapshot,
                    changed_paths: changed_paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                }],
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
            },
        )
        .await?;

    // Verify file is modified
    assert_eq!(
        fs::read_to_string(worktree.path().join("test.txt"))?,
        "modified"
    );

    // Create UI server state
    let state = test_server_state(&handle, &*storage);

    // Setup connection with session mapping
    {
        let mut connections = state.connections.lock().await;
        connections.insert(
            "test-conn".to_string(),
            super::ConnectionState {
                routing_mode: crate::ui::messages::RoutingMode::Single,
                active_agent_id: "agent".to_string(),
                sessions: vec![("agent".to_string(), session_id.clone())]
                    .into_iter()
                    .collect(),
                subscribed_sessions: std::collections::HashSet::new(),
                session_cursors: std::collections::HashMap::new(),
                current_workspace_root: None,
                file_index_forwarder: None,
            },
        );
    }

    // Create channel for handler response
    let (tx, mut rx) = mpsc::channel(10);

    // Call the undo handler
    handle_undo(&state, "test-conn", &user_msg_id, &tx).await;

    // Verify response
    let response = collect_message(&mut rx).await;
    assert!(response.is_some(), "Should receive undo result");

    let response = response.unwrap();
    assert_eq!(
        response["type"], "undo_result",
        "Should be undo_result message"
    );
    assert_eq!(response["success"], true, "Undo should succeed");

    let reverted_files = response["reverted_files"]
        .as_array()
        .expect("Should have reverted_files");
    assert_eq!(reverted_files.len(), 1);
    assert_eq!(reverted_files[0], "test.txt");
    assert_eq!(response["message_id"], user_msg_id);
    let undo_stack = response["undo_stack"]
        .as_array()
        .expect("Should have undo_stack");
    assert_eq!(undo_stack.len(), 1);
    assert_eq!(undo_stack[0]["message_id"], user_msg_id);

    // Verify file was actually reverted
    assert_eq!(
        fs::read_to_string(worktree.path().join("test.txt"))?,
        "original"
    );

    Ok(())
}

#[tokio::test]
async fn test_send_state_concurrent_calls_complete() -> Result<()> {
    let f = TestServerState::new().await;
    let (tx, mut rx) = f.add_connection("conn-1").await;
    let state = f.state;
    let first = tokio::spawn({
        let state = state.clone();
        let tx = tx.clone();
        async move { super::connection::send_state(&state, "conn-1", &tx).await }
    });
    let second = tokio::spawn({
        let state = state.clone();
        let tx = tx.clone();
        async move { super::connection::send_state(&state, "conn-1", &tx).await }
    });

    tokio::time::timeout(Duration::from_secs(2), async {
        let _ = tokio::join!(first, second);
    })
    .await
    .expect("concurrent send_state calls should not block");

    let mut state_messages = 0;
    for _ in 0..2 {
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected a state message")
            .expect("state sender dropped unexpectedly");
        let json: serde_json::Value = serde_json::from_str(&msg)?;
        if json["type"] == "state" {
            state_messages += 1;
        }
    }
    assert_eq!(state_messages, 2, "expected two state messages");

    Ok(())
}

#[tokio::test]
async fn test_undo_handler_cross_session() -> Result<()> {
    // Setup
    let worktree = TempDir::new()?;
    let snapshot_base = TempDir::new()?;
    fs::write(worktree.path().join("test.txt"), "original")?;

    let (registry, _config_dir) = empty_plugin_registry()?;
    let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await?);
    let snapshot_backend = Arc::new(GitSnapshotBackend::with_snapshot_base(
        snapshot_base.path().to_path_buf(),
    ));

    let builder = AgentConfigBuilder::new(
        Arc::new(registry),
        storage.session_store(),
        storage.event_journal(),
        LLMParams::new().provider("mock").model("mock"),
    )
    .with_snapshot_policy(SnapshotPolicy::Diff)
    .with_snapshot_backend(snapshot_backend.clone());

    let config = Arc::new(builder.build());
    let handle = Arc::new(crate::agent::AgentHandle::from_config(config.clone()));

    // Create parent and child sessions
    let parent = storage
        .session_store()
        .create_session(None, Some(worktree.path().to_path_buf()), None, None)
        .await?;
    let parent_id = parent.public_id.clone();
    let child = storage
        .session_store()
        .create_session(
            None,
            Some(worktree.path().to_path_buf()),
            Some(parent_id.clone()),
            Some(ForkOrigin::Delegation),
        )
        .await?;
    let child_id = child.public_id.clone();

    // Spawn SessionActors for undo to work (routes through kameo)
    {
        let runtime = SessionRuntime::new(
            Some(worktree.path().to_path_buf()),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            vec![],
        );
        let mut registry = handle.registry.lock().await;

        let parent_actor = crate::agent::session_actor::SessionActor::new(
            config.clone(),
            parent_id.clone(),
            runtime.clone(),
        );
        registry.insert(parent_id.clone(), kameo::actor::Spawn::spawn(parent_actor));

        let child_actor = crate::agent::session_actor::SessionActor::new(
            config.clone(),
            child_id.clone(),
            runtime,
        );
        registry.insert(child_id.clone(), kameo::actor::Spawn::spawn(child_actor));
    }

    // Add user message in parent
    let user_msg_id = Uuid::new_v4().to_string();
    storage
        .session_store()
        .add_message(
            &parent_id,
            AgentMessage {
                id: user_msg_id.clone(),
                session_id: parent_id.clone(),
                role: ChatRole::User,
                parts: vec![MessagePart::Text {
                    content: "Make changes".to_string(),
                }],
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
            },
        )
        .await?;

    // Take snapshot in CHILD session
    let pre_snapshot = snapshot_backend.track(worktree.path()).await?;
    let turn_id = Uuid::now_v7().to_string();
    storage
        .session_store()
        .add_message(
            &child_id,
            AgentMessage {
                id: Uuid::new_v4().to_string(),
                session_id: child_id.clone(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::TurnSnapshotStart {
                    turn_id: turn_id.clone(),
                    snapshot_id: pre_snapshot.clone(),
                }],
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
            },
        )
        .await?;

    // Modify file
    fs::write(worktree.path().join("test.txt"), "modified by delegate")?;

    // Take post-snapshot in child
    let post_snapshot = snapshot_backend.track(worktree.path()).await?;
    let changed_paths = snapshot_backend
        .diff(worktree.path(), &pre_snapshot, &post_snapshot)
        .await?;
    storage
        .session_store()
        .add_message(
            &child_id,
            AgentMessage {
                id: Uuid::new_v4().to_string(),
                session_id: child_id.clone(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::TurnSnapshotPatch {
                    turn_id,
                    snapshot_id: post_snapshot,
                    changed_paths: changed_paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                }],
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
            },
        )
        .await?;

    // Verify file is modified
    assert_eq!(
        fs::read_to_string(worktree.path().join("test.txt"))?,
        "modified by delegate"
    );

    // Create UI server state
    let state = test_server_state(&handle, &*storage);

    // Setup connection for PARENT session
    {
        let mut connections = state.connections.lock().await;
        connections.insert(
            "test-conn".to_string(),
            super::ConnectionState {
                routing_mode: crate::ui::messages::RoutingMode::Single,
                active_agent_id: "agent".to_string(),
                sessions: vec![("agent".to_string(), parent_id.clone())]
                    .into_iter()
                    .collect(),
                subscribed_sessions: std::collections::HashSet::new(),
                session_cursors: std::collections::HashMap::new(),
                current_workspace_root: None,
                file_index_forwarder: None,
            },
        );
    }

    // Create channel for handler response
    let (tx, mut rx) = mpsc::channel(10);

    // Call undo handler on PARENT session
    handle_undo(&state, "test-conn", &user_msg_id, &tx).await;

    // Verify response
    let response = collect_message(&mut rx).await;
    assert!(response.is_some(), "Should receive undo result");

    let response = response.unwrap();
    assert_eq!(
        response["type"], "undo_result",
        "Should be undo_result message"
    );
    assert_eq!(response["success"], true, "Undo should succeed");

    let reverted_files = response["reverted_files"]
        .as_array()
        .expect("Should have reverted_files");
    assert_eq!(reverted_files.len(), 1, "Should revert 1 file");
    assert_eq!(reverted_files[0], "test.txt");
    assert_eq!(response["message_id"], user_msg_id);
    let undo_stack = response["undo_stack"]
        .as_array()
        .expect("Should have undo_stack");
    assert_eq!(undo_stack.len(), 1);
    assert_eq!(undo_stack[0]["message_id"], user_msg_id);

    // Verify file was actually reverted (this is the critical test)
    assert_eq!(
        fs::read_to_string(worktree.path().join("test.txt"))?,
        "original",
        "File should be reverted even though changes were in child session"
    );

    Ok(())
}

#[tokio::test]
async fn test_load_session_hydrates_runtime_actor() -> Result<()> {
    let f = TestServerState::new().await;
    let session_id = f.agent.create_session().await;

    let (tx, _rx) = f.add_connection("conn-load").await;

    {
        let registry = f.agent.handle.registry.lock().await;
        assert!(registry.get(&session_id).is_none());
    }

    handle_load_session(&f.state, "conn-load", &session_id, &tx).await;

    let actor_loaded = {
        let registry = f.agent.handle.registry.lock().await;
        registry.get(&session_id).is_some()
    };
    assert!(actor_loaded, "load_session should hydrate runtime actor");

    Ok(())
}

#[tokio::test]
async fn test_set_session_model_hydrates_persisted_session() -> Result<()> {
    let f = TestServerState::new().await;
    let session_id = f.agent.create_session().await;

    {
        let registry = f.agent.handle.registry.lock().await;
        assert!(registry.get(&session_id).is_none());
    }

    let (tx, mut rx) = mpsc::channel(16);
    handle_ui_message(
        &f.state,
        "conn-model",
        &tx,
        UiClientMessage::SetSessionModel {
            session_id: session_id.clone(),
            model_id: "mock/new-model".to_string(),
            node_id: None,
        },
    )
    .await;

    tokio::time::sleep(Duration::from_millis(30)).await;

    let mut errors = Vec::new();
    while let Ok(Some(msg_str)) = tokio::time::timeout(Duration::from_millis(20), rx.recv()).await {
        let parsed: Value = serde_json::from_str(&msg_str)?;
        if parsed["type"] == "error" {
            errors.push(parsed["message"].as_str().unwrap_or_default().to_string());
        }
    }
    assert!(
        errors.is_empty(),
        "set_session_model should not emit error for persisted session: {errors:?}"
    );

    let actor_loaded = {
        let registry = f.agent.handle.registry.lock().await;
        registry.get(&session_id).is_some()
    };
    assert!(actor_loaded, "set_session_model should lazy-hydrate actor");

    let llm_cfg = f
        .agent
        .storage
        .session_store()
        .get_session_llm_config(&session_id)
        .await?;
    let llm_cfg = llm_cfg.expect("session llm config should be set");
    assert_eq!(llm_cfg.provider, "mock");
    assert_eq!(llm_cfg.model, "new-model");

    Ok(())
}

#[tokio::test]
async fn test_set_session_model_unknown_session_returns_not_found() -> Result<()> {
    let f = TestServerState::new().await;

    let missing_id = "019c0000-0000-7000-8000-000000000001".to_string();
    let (tx, mut rx) = mpsc::channel(16);
    handle_ui_message(
        &f.state,
        "conn-missing",
        &tx,
        UiClientMessage::SetSessionModel {
            session_id: missing_id.clone(),
            model_id: "mock/new-model".to_string(),
            node_id: None,
        },
    )
    .await;

    let mut got_not_found = false;
    while let Ok(Some(msg_str)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
    {
        let parsed: Value = serde_json::from_str(&msg_str)?;
        if parsed["type"] == "error"
            && parsed["message"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!("Session not found: {}", missing_id))
        {
            got_not_found = true;
            break;
        }
    }

    assert!(
        got_not_found,
        "missing session should return not found error"
    );
    Ok(())
}

#[tokio::test]
async fn test_elicitation_response_routes_to_delegate_pending_map() -> Result<()> {
    let fixture = DelegateTestFixture::new().await.unwrap();
    let state = test_server_state(&fixture.planner, &*fixture.planner_storage);

    let elicitation_id = "delegate-elicitation-42".to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    fixture
        .delegate
        .pending_elicitations()
        .lock()
        .await
        .insert(elicitation_id.clone(), tx);

    handle_elicitation_response(
        &state,
        &elicitation_id,
        "accept",
        Some(&serde_json::json!({"selection": "allow_once"})),
    )
    .await;

    let response = rx
        .await
        .expect("delegate elicitation response should be delivered");
    assert_eq!(response.action, ElicitationAction::Accept);
    assert_eq!(
        response.content,
        Some(serde_json::json!({"selection": "allow_once"}))
    );

    Ok(())
}

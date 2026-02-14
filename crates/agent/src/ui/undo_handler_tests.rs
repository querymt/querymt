//! Integration tests for UI undo/redo handler
//!
//! These tests validate the complete flow from UI handler through to file restoration.
//! They simulate what happens when a user clicks "undo" in the dashboard.

use crate::agent::builder::AgentBuilderExt;
use crate::agent::core::{QueryMTAgent, SnapshotPolicy};
use crate::model::{AgentMessage, MessagePart};
use crate::session::backend::StorageBackend;
use crate::session::domain::ForkOrigin;
use crate::session::sqlite_storage::SqliteStorage;
use crate::snapshot::backend::SnapshotBackend;
use crate::snapshot::git::GitSnapshotBackend;
use crate::test_utils::empty_plugin_registry;
use crate::ui::handlers::handle_undo;
use anyhow::Result;
use querymt::LLMParams;
use querymt::chat::ChatRole;
use serde_json::Value;
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::mpsc;
use uuid::Uuid;

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

    let agent = QueryMTAgent::new(
        Arc::new(registry),
        storage.session_store(),
        LLMParams::new().provider("mock").model("mock"),
    )
    .with_snapshot_policy(SnapshotPolicy::Diff)
    .with_snapshot_backend(snapshot_backend.clone());

    // Create session with cwd
    let session = storage
        .session_store()
        .create_session(None, Some(worktree.path().to_path_buf()), None, None)
        .await?;
    let session_id = session.public_id.clone();

    // Create SessionRuntime for undo to work
    {
        let runtime = Arc::new(crate::agent::core::SessionRuntime {
            cwd: Some(worktree.path().to_path_buf()),
            _mcp_services: std::collections::HashMap::new(),
            mcp_tools: std::collections::HashMap::new(),
            mcp_tool_defs: vec![],
            permission_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            current_tools_hash: std::sync::Mutex::new(None),
            function_index: Arc::new(tokio::sync::OnceCell::new()),
            turn_snapshot: std::sync::Mutex::new(None),
            turn_diffs: std::sync::Mutex::new(Default::default()),
            execution_permit: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        let mut runtimes = agent.session_runtime.lock().await;
        runtimes.insert(session_id.clone(), runtime);
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
    let turn_id = Uuid::new_v4().to_string();
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
    let state = super::ServerState {
        agent: Arc::new(agent),
        view_store: storage.clone(),
        default_cwd: None,
        event_sources: vec![],
        connections: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        session_cwds: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        workspace_manager: Arc::new(crate::index::WorkspaceIndexManager::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        )),
        model_cache: moka::future::Cache::new(100),
        oauth_flows: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    };

    // Setup connection state
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
                current_workspace_root: None,
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

    // Verify file was actually reverted
    assert_eq!(
        fs::read_to_string(worktree.path().join("test.txt"))?,
        "original"
    );

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

    let agent = QueryMTAgent::new(
        Arc::new(registry),
        storage.session_store(),
        LLMParams::new().provider("mock").model("mock"),
    )
    .with_snapshot_policy(SnapshotPolicy::Diff)
    .with_snapshot_backend(snapshot_backend.clone());

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

    // Create SessionRuntime for both parent and child
    {
        let runtime = Arc::new(crate::agent::core::SessionRuntime {
            cwd: Some(worktree.path().to_path_buf()),
            _mcp_services: std::collections::HashMap::new(),
            mcp_tools: std::collections::HashMap::new(),
            mcp_tool_defs: vec![],
            permission_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            current_tools_hash: std::sync::Mutex::new(None),
            function_index: Arc::new(tokio::sync::OnceCell::new()),
            turn_snapshot: std::sync::Mutex::new(None),
            turn_diffs: std::sync::Mutex::new(Default::default()),
            execution_permit: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        let mut runtimes = agent.session_runtime.lock().await;
        runtimes.insert(parent_id.clone(), runtime.clone());
        runtimes.insert(child_id.clone(), runtime);
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
    let turn_id = Uuid::new_v4().to_string();
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
    let state = super::ServerState {
        agent: Arc::new(agent),
        view_store: storage.clone(),
        default_cwd: None,
        event_sources: vec![],
        connections: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        session_agents: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        session_cwds: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        workspace_manager: Arc::new(crate::index::WorkspaceIndexManager::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        )),
        model_cache: moka::future::Cache::new(100),
        oauth_flows: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    };

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
                current_workspace_root: None,
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

    // Verify file was actually reverted (this is the critical test)
    assert_eq!(
        fs::read_to_string(worktree.path().join("test.txt"))?,
        "original",
        "File should be reverted even though changes were in child session"
    );

    Ok(())
}

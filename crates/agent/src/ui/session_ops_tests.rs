//! Focused tests for session list UI handlers and dispatch.

use crate::session::backend::StorageBackend;
use crate::session::domain::ForkOrigin;
use crate::session::projection::SessionScope;
use crate::ui::handlers::{
    ListSessionsRequest, handle_list_session_children, handle_list_sessions, handle_ui_message,
};
use crate::ui::messages::UiClientMessage;
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;
use tokio::time::{Duration, timeout};

struct SeededSessions {
    root: String,
    second_root: String,
    user_fork: String,
    delegate: String,
}

async fn next_json(rx: &mut tokio::sync::mpsc::Receiver<String>) -> Value {
    let msg = timeout(Duration::from_millis(400), rx.recv())
        .await
        .expect("message should arrive")
        .expect("channel should stay open");
    serde_json::from_str(&msg).expect("valid JSON UI message")
}

async fn seed_sessions(f: &crate::test_utils::TestServerState) -> Result<SeededSessions> {
    let store = f.agent.storage.session_store();
    let root = store
        .create_session(
            Some("root-alpha".to_string()),
            Some(PathBuf::from("/workspace-a")),
            None,
            None,
        )
        .await?
        .public_id;
    let second_root = store
        .create_session(
            Some("root-beta".to_string()),
            Some(PathBuf::from("/workspace-b")),
            None,
            None,
        )
        .await?
        .public_id;
    let user_fork = store
        .create_session(
            Some("user-fork".to_string()),
            Some(PathBuf::from("/workspace-a")),
            Some(root.clone()),
            Some(ForkOrigin::User),
        )
        .await?
        .public_id;
    let delegate = store
        .create_session(
            Some("delegate-child".to_string()),
            Some(PathBuf::from("/workspace-a")),
            Some(root.clone()),
            Some(ForkOrigin::Delegation),
        )
        .await?
        .public_id;

    Ok(SeededSessions {
        root,
        second_root,
        user_fork,
        delegate,
    })
}

fn sessions_in_groups(msg: &Value) -> Vec<&Value> {
    msg["data"]["groups"]
        .as_array()
        .expect("groups should be an array")
        .iter()
        .flat_map(|group| {
            group["sessions"]
                .as_array()
                .expect("sessions should be an array")
        })
        .collect()
}

fn session_ids(msg: &Value) -> Vec<String> {
    sessions_in_groups(msg)
        .into_iter()
        .map(|session| session["session_id"].as_str().unwrap().to_string())
        .collect()
}

fn find_session<'a>(msg: &'a Value, session_id: &str) -> &'a Value {
    sessions_in_groups(msg)
        .into_iter()
        .find(|session| session["session_id"] == session_id)
        .expect("session should be present")
}

#[tokio::test]
async fn handle_list_sessions_browse_root_scope_reports_user_fork_counts() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let seeded = seed_sessions(&f).await?;
    let (tx, mut rx) = f.add_connection("conn-list-browse").await;

    handle_list_sessions(
        &f.state,
        &tx,
        ListSessionsRequest {
            mode: Some("browse".to_string()),
            cursor: None,
            limit: Some(20),
            cwd: None,
            query: None,
            session_scope: Some(SessionScope::Root),
        },
    )
    .await;

    let msg = next_json(&mut rx).await;
    assert_eq!(msg["type"], "session_list");
    assert_eq!(msg["data"]["total_count"], 2);
    let ids = session_ids(&msg);
    assert!(ids.contains(&seeded.root));
    assert!(ids.contains(&seeded.second_root));
    assert!(!ids.contains(&seeded.user_fork));
    assert!(!ids.contains(&seeded.delegate));

    let root = find_session(&msg, &seeded.root);
    assert_eq!(root["has_children"], true);
    assert_eq!(root["fork_count"], 1);

    Ok(())
}

#[tokio::test]
async fn handle_list_sessions_group_and_search_respect_session_scope() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let seeded = seed_sessions(&f).await?;
    let (tx, mut rx) = f.add_connection("conn-list-filtered").await;

    handle_list_sessions(
        &f.state,
        &tx,
        ListSessionsRequest {
            mode: Some("group".to_string()),
            cursor: None,
            limit: Some(20),
            cwd: Some("/workspace-a".to_string()),
            query: None,
            session_scope: Some(SessionScope::Forks),
        },
    )
    .await;

    let group_msg = next_json(&mut rx).await;
    assert_eq!(group_msg["type"], "session_list");
    assert_eq!(session_ids(&group_msg), vec![seeded.user_fork.clone()]);
    assert_eq!(group_msg["data"]["groups"][0]["total_count"], 1);

    handle_list_sessions(
        &f.state,
        &tx,
        ListSessionsRequest {
            mode: Some("search".to_string()),
            cursor: None,
            limit: Some(20),
            cwd: None,
            query: Some("delegate".to_string()),
            session_scope: Some(SessionScope::Delegates),
        },
    )
    .await;

    let search_msg = next_json(&mut rx).await;
    assert_eq!(search_msg["type"], "session_list");
    assert_eq!(session_ids(&search_msg), vec![seeded.delegate]);
    assert_eq!(search_msg["data"]["total_count"], 1);

    Ok(())
}

#[tokio::test]
async fn handle_list_session_children_allows_default_and_forks_scope() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let seeded = seed_sessions(&f).await?;
    let (tx, mut rx) = f.add_connection("conn-children").await;

    handle_list_session_children(&f.state, &tx, seeded.root.clone(), None, Some(20), None).await;
    let default_msg = next_json(&mut rx).await;
    assert_eq!(default_msg["type"], "session_children");
    assert_eq!(default_msg["data"]["parent_session_id"], seeded.root);
    assert_eq!(default_msg["data"]["total_count"], 1);
    assert_eq!(
        default_msg["data"]["sessions"][0]["session_id"],
        seeded.user_fork
    );

    handle_list_session_children(
        &f.state,
        &tx,
        seeded.root.clone(),
        None,
        Some(20),
        Some(SessionScope::Forks),
    )
    .await;
    let forks_msg = next_json(&mut rx).await;
    assert_eq!(forks_msg["type"], "session_children");
    assert_eq!(forks_msg["data"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        forks_msg["data"]["sessions"][0]["session_id"],
        seeded.user_fork
    );

    Ok(())
}

#[tokio::test]
async fn handle_list_session_children_rejects_root_scope() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let seeded = seed_sessions(&f).await?;
    let (tx, mut rx) = f.add_connection("conn-children-root").await;

    handle_list_session_children(
        &f.state,
        &tx,
        seeded.root,
        None,
        Some(20),
        Some(SessionScope::Root),
    )
    .await;

    let msg = next_json(&mut rx).await;
    assert_eq!(msg["type"], "error");
    assert_eq!(
        msg["data"]["message"],
        "Session children list only supports user forks"
    );

    Ok(())
}

#[tokio::test]
async fn handle_ui_message_dispatches_list_sessions() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let seeded = seed_sessions(&f).await?;
    let (tx, mut rx) = f.add_connection("conn-dispatch-list").await;
    let (bin_tx, _bin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    handle_ui_message(
        &f.state,
        "conn-dispatch-list",
        &tx,
        &bin_tx,
        UiClientMessage::ListSessions {
            mode: Some("search".to_string()),
            cursor: None,
            limit: Some(20),
            cwd: None,
            query: Some("root-alpha".to_string()),
            session_scope: Some(SessionScope::Root),
        },
    )
    .await;

    let msg = next_json(&mut rx).await;
    assert_eq!(msg["type"], "session_list");
    assert_eq!(session_ids(&msg), vec![seeded.root]);
    assert_eq!(msg["data"]["total_count"], 1);

    Ok(())
}

#[tokio::test]
async fn handle_ui_message_dispatches_list_session_children() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let seeded = seed_sessions(&f).await?;
    let (tx, mut rx) = f.add_connection("conn-dispatch-children").await;
    let (bin_tx, _bin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    handle_ui_message(
        &f.state,
        "conn-dispatch-children",
        &tx,
        &bin_tx,
        UiClientMessage::ListSessionChildren {
            parent_session_id: seeded.root.clone(),
            cursor: None,
            limit: Some(20),
            session_scope: Some(SessionScope::Forks),
        },
    )
    .await;

    let msg = next_json(&mut rx).await;
    assert_eq!(msg["type"], "session_children");
    assert_eq!(msg["data"]["parent_session_id"], seeded.root);
    assert_eq!(msg["data"]["sessions"][0]["session_id"], seeded.user_fork);
    assert_eq!(msg["data"]["total_count"], 1);

    Ok(())
}

//! Focused tests for session list UI handlers and dispatch.

use crate::agent::messages::SessionRuntimeStatus;
use crate::agent::{SessionActor, core::SessionRuntime};
use crate::api::AgentInfra;
use crate::model::{AgentMessage, MessagePart};
use crate::profiles::{LocalProfileCatalog, ProfileCatalog, ProfileRuntimeManager};
use crate::session::backend::StorageBackend;
use crate::session::domain::ForkOrigin;
use crate::session::projection::SessionScope;
#[cfg(feature = "remote")]
use crate::session::store::RemoteSessionBookmark;
use crate::test_utils::empty_plugin_registry;
use crate::ui::handlers::{
    ListSessionsRequest, handle_cancel_session, handle_delete_session, handle_fork_session,
    handle_list_session_children, handle_list_sessions, handle_load_session, handle_ui_message,
};
use crate::ui::messages::UiClientMessage;
#[cfg(feature = "remote")]
use crate::{
    agent::remote::test_helpers::fixtures::get_test_mesh, ui::handlers::handle_set_session_model,
};
use anyhow::Result;
use kameo::actor::Spawn;
use querymt::chat::ChatRole;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::time::{Duration, timeout};
#[cfg(feature = "remote")]
use uuid::Uuid;

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

fn write_profile(dir: &Path, name: &str) {
    write_profile_with_content(
        dir,
        name,
        r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
    );
}

fn write_profile_with_content(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).expect("profile should be written");
}

async fn attach_profiles(
    fixture: &mut crate::test_utils::TestServerState,
    active_profile_id: &str,
    profile_dir: &Path,
) -> Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>> {
    let (registry, _config_dir) = empty_plugin_registry().expect("empty plugin registry");
    let infra = AgentInfra {
        plugin_registry: Arc::new(registry),
        storage: Some(fixture.agent.storage.clone()),
        session_mcp_attachment_source: None,
    };
    let catalog: Arc<dyn ProfileCatalog> = Arc::new(
        LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(profile_dir)
            .build(),
    );
    let profiles = Arc::new(ProfileRuntimeManager::with_infra_boxed(
        catalog,
        active_profile_id,
        infra,
    ));
    fixture.state.profiles = Some(profiles.clone());
    profiles
}

async fn next_message_of_type(
    rx: &mut tokio::sync::mpsc::Receiver<String>,
    expected_type: &str,
) -> Value {
    loop {
        let msg = next_json(rx).await;
        if msg["type"] == expected_type {
            return msg;
        }
    }
}

async fn insert_test_actor(agent: &Arc<crate::agent::LocalAgentHandle>, session_id: &str) {
    let actor = SessionActor::new(
        agent.config.clone(),
        session_id.to_string(),
        SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        ),
    );
    let actor_ref = SessionActor::spawn(actor);
    agent
        .registry
        .lock()
        .await
        .insert(session_id.to_string(), actor_ref);
}

#[cfg(feature = "remote")]
async fn attach_remote_session(
    fixture: &crate::test_utils::TestServerState,
    session_id: &str,
    peer_label: &str,
) {
    attach_remote_session_with_node_id(fixture, session_id, peer_label, None).await;
}

#[cfg(feature = "remote")]
async fn attach_remote_session_with_node_id(
    fixture: &crate::test_utils::TestServerState,
    session_id: &str,
    peer_label: &str,
    remote_node_id: Option<&str>,
) {
    let mesh = get_test_mesh().await.clone();
    let actor = SessionActor::new(
        fixture.agent.config.clone(),
        session_id.to_string(),
        SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        ),
    )
    .with_mesh(Some(mesh.clone()));
    let local_ref = SessionActor::spawn(actor);
    let dht_name = crate::agent::remote::scope::scoped_session(
        &crate::agent::remote::scope::MeshScopeId::lan_default(),
        session_id,
    );
    mesh.register_actor(local_ref, dht_name.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let remote_ref = mesh
        .lookup_actor::<SessionActor>(&dht_name)
        .await
        .expect("DHT lookup should succeed")
        .expect("remote actor should be available");
    fixture
        .agent
        .handle
        .attach_remote_session(
            session_id.to_string(),
            remote_ref,
            peer_label.to_string(),
            None,
            remote_node_id.map(str::to_string),
        )
        .await;
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
            include_remote: false,
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
            include_remote: false,
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
            include_remote: false,
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
            include_remote: None,
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
async fn handle_ui_message_list_sessions_with_remote_flag_returns_local_list_immediately()
-> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let seeded = seed_sessions(&f).await?;
    let (tx, mut rx) = f.add_connection("conn-dispatch-list-remote").await;
    let (bin_tx, _bin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    handle_ui_message(
        &f.state,
        "conn-dispatch-list-remote",
        &tx,
        &bin_tx,
        UiClientMessage::ListSessions {
            mode: Some("browse".to_string()),
            cursor: None,
            limit: Some(20),
            cwd: None,
            query: None,
            session_scope: Some(SessionScope::Root),
            include_remote: Some(true),
        },
    )
    .await;

    let msg = timeout(Duration::from_secs(2), next_json(&mut rx)).await?;
    assert_eq!(msg["type"], "session_list");
    let ids = session_ids(&msg);
    assert!(ids.contains(&seeded.root));
    assert_eq!(msg["data"]["total_count"], 2);

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

#[tokio::test]
async fn send_state_surfaces_profile_list_errors_and_continues() -> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    let duplicate = r#"
[profile]
id = "shared"

[agent]
provider = "test"
model = "test-model"
system = "inline"
"#;
    write_profile_with_content(dir.path(), "alpha.toml", duplicate);
    write_profile_with_content(dir.path(), "beta.toml", duplicate);
    attach_profiles(&mut f, "shared", dir.path()).await;
    let (tx, mut rx) = f.add_connection("conn-profile-list-error").await;

    crate::ui::connection::send_state(&f.state, "conn-profile-list-error", &tx).await;

    let error_msg = next_json(&mut rx).await;
    assert_eq!(error_msg["type"], "error");
    let message = error_msg["data"]["message"]
        .as_str()
        .expect("error message should be a string");
    assert!(
        message.contains("Failed to list profiles"),
        "message was: {message}"
    );
    assert!(
        message.contains("Duplicate profile id"),
        "message was: {message}"
    );

    let state_msg = next_json(&mut rx).await;
    assert_eq!(state_msg["type"], "state");
    assert_eq!(state_msg["data"]["profiles"], serde_json::json!([]));

    Ok(())
}

#[tokio::test]
async fn handle_ui_message_set_active_profile_updates_state() -> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "alpha.toml");
    write_profile(dir.path(), "beta.toml");
    let profiles = attach_profiles(&mut f, "alpha", dir.path()).await;
    let (tx, mut rx) = f.add_connection("conn-set-profile").await;
    let (bin_tx, _bin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    handle_ui_message(
        &f.state,
        "conn-set-profile",
        &tx,
        &bin_tx,
        UiClientMessage::SetActiveProfile {
            profile_id: "beta".to_string(),
        },
    )
    .await;

    let msg = next_json(&mut rx).await;
    assert_eq!(msg["type"], "state");
    assert_eq!(msg["data"]["active_profile_id"], "beta");
    let profile_ids: Vec<&str> = msg["data"]["profiles"]
        .as_array()
        .expect("profiles should be an array")
        .iter()
        .map(|profile| {
            profile["id"]
                .as_str()
                .expect("profile id should be a string")
        })
        .collect();
    assert_eq!(profile_ids, vec!["alpha", "beta"]);
    assert_eq!(profiles.active_profile_id().await, "beta");

    Ok(())
}

#[tokio::test]
async fn handle_ui_message_set_active_profile_reports_missing_prompt_file_cause() -> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    let missing_prompt = dir.path().join("missing-system-prompt.txt");
    write_profile(dir.path(), "alpha.toml");
    write_profile_with_content(
        dir.path(),
        "broken.toml",
        r#"
[agent]
provider = "test"
model = "test-model"
system = [{ file = "missing-system-prompt.txt" }]
"#,
    );
    attach_profiles(&mut f, "alpha", dir.path()).await;
    let (tx, mut rx) = f.add_connection("conn-set-broken-profile").await;
    let (bin_tx, _bin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    handle_ui_message(
        &f.state,
        "conn-set-broken-profile",
        &tx,
        &bin_tx,
        UiClientMessage::SetActiveProfile {
            profile_id: "broken".to_string(),
        },
    )
    .await;

    let msg = next_json(&mut rx).await;
    assert_eq!(msg["type"], "error");
    let message = msg["data"]["message"]
        .as_str()
        .expect("error message should be a string");
    assert!(
        message.contains("Failed to set active profile"),
        "message was: {message}"
    );
    assert!(
        message.contains("Failed to load agent prompt"),
        "message was: {message}"
    );
    assert!(
        message.contains(&missing_prompt.display().to_string()),
        "message was: {message}"
    );
    assert!(
        message.contains("No such file or directory") || message.contains("os error 2"),
        "message was: {message}"
    );

    Ok(())
}

#[tokio::test]
async fn handle_ui_message_new_session_binds_explicit_profile_and_reports_it() -> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "alpha.toml");
    write_profile(dir.path(), "beta.toml");
    let profiles = attach_profiles(&mut f, "alpha", dir.path()).await;
    let (tx, mut rx) = f.add_connection("conn-new-session-profile").await;
    let (bin_tx, _bin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    handle_ui_message(
        &f.state,
        "conn-new-session-profile",
        &tx,
        &bin_tx,
        UiClientMessage::NewSession {
            cwd: None,
            request_id: Some("req-profile".to_string()),
            profile_id: Some("beta".to_string()),
        },
    )
    .await;

    let created = next_message_of_type(&mut rx, "session_created").await;
    let session_id = created["data"]["session_id"]
        .as_str()
        .expect("session id should be a string")
        .to_string();
    assert_eq!(created["data"]["agent_id"], "primary");
    assert_eq!(created["data"]["profile_id"], "beta");
    assert_eq!(created["data"]["request_id"], "req-profile");

    let binding = profiles
        .session_binding(&session_id)
        .await
        .expect("new session should bind to requested profile");
    assert_eq!(binding.profile_id, "beta");

    Ok(())
}

#[tokio::test]
async fn handle_load_session_prefers_existing_bound_profile_after_active_profile_changes()
-> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "alpha.toml");
    write_profile(dir.path(), "beta.toml");
    let profiles = attach_profiles(&mut f, "alpha", dir.path()).await;
    let session_id = f.agent.create_session().await;
    profiles
        .bind_session_to_profile(session_id.clone(), "alpha")
        .await
        .expect("session should bind to alpha");
    profiles
        .set_active_profile("beta")
        .await
        .expect("active profile should switch for future sessions");
    let (tx, mut rx) = f.add_connection("conn-load-profile").await;

    handle_load_session(&f.state, "conn-load-profile", &session_id, &tx).await;

    let loaded = next_message_of_type(&mut rx, "session_loaded").await;
    assert_eq!(loaded["data"]["session_id"], session_id);
    assert_eq!(loaded["data"]["profile_id"], "alpha");

    let state_msg = next_message_of_type(&mut rx, "state").await;
    assert_eq!(state_msg["data"]["active_profile_id"], "beta");

    Ok(())
}

#[tokio::test]
async fn handle_load_session_uses_db_profile_binding_after_manager_rebuild() -> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "alpha.toml");
    write_profile(dir.path(), "beta.toml");
    let profiles = attach_profiles(&mut f, "alpha", dir.path()).await;
    let session_id = f.agent.create_session().await;
    profiles
        .bind_session_to_profile(session_id.clone(), "alpha")
        .await
        .expect("session should bind to alpha");
    profiles.shutdown().await;
    attach_profiles(&mut f, "beta", dir.path()).await;
    let (tx, mut rx) = f.add_connection("conn-load-db-profile").await;

    handle_load_session(&f.state, "conn-load-db-profile", &session_id, &tx).await;

    let loaded = next_message_of_type(&mut rx, "session_loaded").await;
    assert_eq!(loaded["data"]["session_id"], session_id);
    assert_eq!(loaded["data"]["profile_id"], "alpha");

    Ok(())
}

#[tokio::test]
async fn handle_delete_session_clears_bound_profile_registry_after_active_profile_changes()
-> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "alpha.toml");
    write_profile(dir.path(), "beta.toml");
    let profiles = attach_profiles(&mut f, "alpha", dir.path()).await;
    let session_id = f.agent.create_session().await;
    profiles
        .bind_session_to_profile(session_id.clone(), "alpha")
        .await
        .expect("session should bind to alpha");
    let alpha_agent = profiles
        .runtime_for_profile("alpha")
        .await
        .expect("alpha runtime should load")
        .agent()
        .handle();
    insert_test_actor(&alpha_agent, &session_id).await;
    profiles
        .set_active_profile("beta")
        .await
        .expect("active profile should switch for future sessions");
    let (tx, mut rx) = f.add_connection("conn-delete-profile").await;

    handle_delete_session(&f.state, "conn-delete-profile", &session_id, &tx).await;

    let root_registry = f.state.agent.registry.lock().await;
    assert!(root_registry.get(&session_id).is_none());
    drop(root_registry);
    let alpha_registry = alpha_agent.registry.lock().await;
    assert!(alpha_registry.get(&session_id).is_none());
    drop(alpha_registry);
    assert!(profiles.session_binding(&session_id).await.is_none());
    assert!(
        f.agent
            .storage
            .session_store()
            .get_profile_binding(&session_id)
            .await?
            .is_none()
    );

    let state_msg = next_message_of_type(&mut rx, "state").await;
    assert_eq!(state_msg["data"]["active_profile_id"], "beta");
    let list_msg = next_message_of_type(&mut rx, "session_list").await;
    assert!(!session_ids(&list_msg).contains(&session_id));

    Ok(())
}

#[tokio::test]
async fn handle_load_session_falls_back_active_when_db_profile_unavailable() -> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "beta.toml");
    let session_id = f.agent.create_session().await;
    f.agent
        .storage
        .session_store()
        .set_profile_binding(&session_id, "alpha")
        .await?;
    attach_profiles(&mut f, "beta", dir.path()).await;
    let (tx, mut rx) = f.add_connection("conn-load-missing-db-profile").await;

    handle_load_session(&f.state, "conn-load-missing-db-profile", &session_id, &tx).await;

    let loaded = next_message_of_type(&mut rx, "session_loaded").await;
    assert_eq!(loaded["data"]["session_id"], session_id);
    assert_eq!(loaded["data"]["profile_id"], "beta");

    Ok(())
}

#[tokio::test]
async fn handle_fork_session_preserves_bound_profile_after_active_profile_changes() -> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "alpha.toml");
    write_profile(dir.path(), "beta.toml");
    let profiles = attach_profiles(&mut f, "alpha", dir.path()).await;
    let session_id = f.agent.create_session().await;
    let message_id = uuid::Uuid::new_v4().to_string();
    f.agent
        .storage
        .session_store()
        .add_message(
            &session_id,
            AgentMessage {
                id: message_id.clone(),
                session_id: session_id.clone(),
                role: ChatRole::User,
                parts: vec![MessagePart::Text {
                    content: "Fork from alpha".to_string(),
                }],
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
        )
        .await?;
    profiles
        .bind_session_to_profile(session_id.clone(), "alpha")
        .await
        .expect("source session should bind to alpha");
    profiles
        .set_active_profile("beta")
        .await
        .expect("active profile should switch for future sessions");
    let (tx, mut rx) = f.add_connection("conn-fork-profile").await;

    {
        let mut connections = f.state.connections.lock().await;
        let conn = connections
            .get_mut("conn-fork-profile")
            .expect("connection should exist");
        conn.sessions
            .insert("primary".to_string(), session_id.clone());
    }

    handle_fork_session(&f.state, "conn-fork-profile", &message_id, &tx).await;

    let fork_result = next_message_of_type(&mut rx, "fork_result").await;
    assert_eq!(fork_result["data"]["success"], true);
    assert_eq!(fork_result["data"]["source_session_id"], session_id);
    let forked_session_id = fork_result["data"]["forked_session_id"]
        .as_str()
        .expect("forked session id should be present")
        .to_string();

    let binding = profiles
        .session_binding(&forked_session_id)
        .await
        .expect("forked session should inherit source profile binding");
    assert_eq!(binding.profile_id, "alpha");

    handle_load_session(&f.state, "conn-fork-profile", &forked_session_id, &tx).await;

    let loaded = next_message_of_type(&mut rx, "session_loaded").await;
    assert_eq!(loaded["data"]["session_id"], forked_session_id);
    assert_eq!(loaded["data"]["profile_id"], "alpha");

    let state_msg = next_message_of_type(&mut rx, "state").await;
    assert_eq!(state_msg["data"]["active_profile_id"], "beta");

    Ok(())
}

#[tokio::test]
async fn handle_set_session_model_uses_bound_profile_after_active_profile_changes() -> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "alpha.toml");
    write_profile(dir.path(), "beta.toml");
    let profiles = attach_profiles(&mut f, "alpha", dir.path()).await;
    let (tx, mut rx) = f.add_connection("conn-set-model-profile").await;
    let (bin_tx, _bin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    handle_ui_message(
        &f.state,
        "conn-set-model-profile",
        &tx,
        &bin_tx,
        UiClientMessage::NewSession {
            cwd: None,
            request_id: Some("req-set-model-profile".to_string()),
            profile_id: Some("alpha".to_string()),
        },
    )
    .await;

    let created = next_message_of_type(&mut rx, "session_created").await;
    let session_id = created["data"]["session_id"]
        .as_str()
        .expect("session id should be a string")
        .to_string();
    assert_eq!(created["data"]["profile_id"], "alpha");

    profiles
        .set_active_profile("beta")
        .await
        .expect("active profile should switch for future sessions");

    handle_ui_message(
        &f.state,
        "conn-set-model-profile",
        &tx,
        &bin_tx,
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
            errors.push(
                parsed["data"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
        }
    }
    assert!(
        errors.is_empty(),
        "set_session_model should not emit error for profile-backed session: {errors:?}"
    );

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

#[cfg(feature = "remote")]
#[tokio::test]
async fn handle_load_session_succeeds_for_attached_remote_session_with_local_profiles() -> Result<()>
{
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "alpha.toml");
    write_profile(dir.path(), "beta.toml");
    let profiles = attach_profiles(&mut f, "alpha", dir.path()).await;
    let session_id = format!("remote-load-{}", Uuid::now_v7());

    attach_remote_session(&f, &session_id, "remote-peer").await;
    profiles
        .bind_session_to_profile(&session_id, "alpha")
        .await
        .expect("session should bind to local profile");
    profiles
        .set_active_profile("beta")
        .await
        .expect("beta should become active for new sessions");

    let (tx, mut rx) = f.add_connection("conn-remote-load").await;
    handle_load_session(&f.state, "conn-remote-load", &session_id, &tx).await;

    let loaded = next_message_of_type(&mut rx, "session_loaded").await;
    assert_eq!(loaded["data"]["session_id"], session_id);

    Ok(())
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn handle_set_session_model_prefers_attached_remote_over_profile_local_actor() -> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "alpha.toml");
    write_profile(dir.path(), "beta.toml");
    let profiles = attach_profiles(&mut f, "alpha", dir.path()).await;
    let session_id = f
        .agent
        .storage
        .session_store()
        .create_session(Some("remote-model".to_string()), None, None, None)
        .await?
        .public_id;

    attach_remote_session(&f, &session_id, "remote-peer").await;
    profiles
        .bind_session_to_profile(&session_id, "alpha")
        .await
        .expect("session should bind to local profile");
    profiles
        .set_active_profile("beta")
        .await
        .expect("beta should become active for new sessions");

    let profile_runtime = profiles.runtime_for_profile("alpha").await?;
    insert_test_actor(&profile_runtime.agent().handle(), &session_id).await;

    handle_set_session_model(&f.state, &session_id, "mock/new-model", None)
        .await
        .expect("set_session_model should succeed for attached remote session");

    let local_llm_cfg = f
        .agent
        .storage
        .session_store()
        .get_session_llm_config(&session_id)
        .await?;
    let local_llm_cfg = local_llm_cfg.expect("session llm config should be set");
    assert_eq!(
        local_llm_cfg.model, "new-model",
        "remote actor should update shared session config instead of profile-local actor"
    );
    assert_eq!(
        local_llm_cfg.provider_node_id,
        f.agent
            .handle
            .mesh()
            .map(|mesh| crate::agent::remote::NodeId::from_peer_id(*mesh.peer_id()).to_string()),
        "remote actor should route local provider calls back to the local mesh peer"
    );

    Ok(())
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn list_sessions_returns_detached_remote_bookmarks_without_attaching() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let session_id = format!("remote-bookmark-{}", Uuid::now_v7());
    f.agent
        .storage
        .session_store()
        .save_remote_session_bookmark(&RemoteSessionBookmark {
            session_id: session_id.clone(),
            node_id: "node-bookmark".to_string(),
            peer_label: "remote-peer".to_string(),
            cwd: Some("/remote/workspace".to_string()),
            created_at: 123,
            title: Some("Bookmarked remote".to_string()),
        })
        .await?;

    let (tx, mut rx) = f.add_connection("conn-remote-bookmark").await;
    handle_list_sessions(
        &f.state,
        &tx,
        ListSessionsRequest {
            include_remote: true,
            ..ListSessionsRequest::root_browse()
        },
    )
    .await;

    let listed = next_message_of_type(&mut rx, "session_list").await;
    let summary = find_session(&listed, &session_id);
    assert_eq!(summary["attached"], false);
    assert_eq!(summary["runtime_state"], "stopped");
    assert!(
        f.agent
            .handle
            .registry
            .lock()
            .await
            .get(&session_id)
            .is_none(),
        "listing a bookmark must not attach or mutate the registry"
    );

    Ok(())
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn list_sessions_uses_attached_remote_node_id_over_hostname_lookup() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let session_id = format!("remote-node-id-{}", Uuid::now_v7());

    attach_remote_session_with_node_id(
        &f,
        &session_id,
        "shared-hostname",
        Some("stable-remote-node"),
    )
    .await;

    let (tx, mut rx) = f.add_connection("conn-remote-node-id").await;
    handle_list_sessions(
        &f.state,
        &tx,
        ListSessionsRequest {
            include_remote: true,
            ..ListSessionsRequest::root_browse()
        },
    )
    .await;

    let listed = next_message_of_type(&mut rx, "session_list").await;
    let summary = find_session(&listed, &session_id);
    assert_eq!(summary["node"], "shared-hostname");
    assert_eq!(summary["node_id"], "stable-remote-node");

    Ok(())
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn list_sessions_with_remote_returns_bookmarks_without_waiting_for_second_store_read() -> Result<()> {
    let f = crate::test_utils::TestServerState::new().await;
    let session_id = format!("remote-fast-bookmark-{}", Uuid::now_v7());
    f.agent
        .storage
        .session_store()
        .save_remote_session_bookmark(&RemoteSessionBookmark {
            session_id: session_id.clone(),
            node_id: "node-bookmark".to_string(),
            peer_label: "remote-peer".to_string(),
            cwd: Some("/remote/workspace".to_string()),
            created_at: 123,
            title: Some("Bookmarked remote".to_string()),
        })
        .await?;

    let (tx, mut rx) = f.add_connection("conn-remote-bookmark-fast").await;
    handle_list_sessions(
        &f.state,
        &tx,
        ListSessionsRequest {
            include_remote: true,
            ..ListSessionsRequest::root_browse()
        },
    )
    .await;

    let listed = timeout(Duration::from_millis(400), next_message_of_type(&mut rx, "session_list"))
        .await
        .expect("first session list should not wait on extra remote discovery or duplicate bookmark reads");
    let summary = find_session(&listed, &session_id);
    assert_eq!(summary["attached"], false);
    assert_eq!(summary["node_id"], "node-bookmark");

    Ok(())
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn handle_cancel_session_uses_root_remote_session_ref_even_with_stale_profile_binding()
-> Result<()> {
    let mut f = crate::test_utils::TestServerState::new().await;
    let dir = TempDir::new()?;
    write_profile(dir.path(), "alpha.toml");
    attach_profiles(&mut f, "alpha", dir.path()).await;
    let session_id = format!("remote-cancel-{}", Uuid::now_v7());

    attach_remote_session(&f, &session_id, "remote-peer").await;

    let (tx, mut rx) = f.add_connection("conn-cancel-remote").await;

    {
        let mut connections = f.state.connections.lock().await;
        let conn = connections
            .get_mut("conn-cancel-remote")
            .expect("test connection should exist");
        conn.active_agent_id = "primary".to_string();
        conn.sessions
            .insert("primary".to_string(), session_id.clone());
    }

    // Simulate a stale DB profile binding that no longer has a runtime.
    f.agent
        .storage
        .session_store()
        .set_profile_binding(&session_id, "missing-profile")
        .await?;

    handle_cancel_session(&f.state, "conn-cancel-remote", &tx).await;

    let recv = timeout(Duration::from_millis(200), rx.recv()).await;
    assert!(
        recv.is_err(),
        "unexpected UI message during remote cancel: {:?}",
        recv.ok().flatten()
    );

    let session_ref = {
        let registry = f.agent.handle.registry.lock().await;
        registry
            .get(&session_id)
            .cloned()
            .expect("remote session should remain attached")
    };
    let status = session_ref
        .get_runtime_status()
        .await
        .expect("status query should succeed");
    assert!(
        matches!(
            status,
            SessionRuntimeStatus::Idle | SessionRuntimeStatus::CancelRequested
        ),
        "unexpected runtime status after cancel: {:?}",
        status
    );

    Ok(())
}

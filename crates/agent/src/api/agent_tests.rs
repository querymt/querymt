use super::{
    Agent, AgentInfra, AgentProfiles, AgentSessions, ListSessionsOptions, RemoteSessionMode,
    SessionListMode,
};
use crate::profiles::{LocalProfileCatalog, ProfileCatalog};
#[cfg(feature = "remote")]
use crate::session::store::RemoteSessionBookmark;
use crate::test_utils::helpers::empty_plugin_registry;
use agent_client_protocol::schema::ListSessionsRequest as AcpListSessionsRequest;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

async fn test_agent() -> Result<Agent> {
    test_agent_with_storage(None).await
}

async fn test_agent_with_storage(
    storage: Option<Arc<crate::session::sqlite_storage::SqliteStorage>>,
) -> Result<Agent> {
    let (registry, _temp_dir) = empty_plugin_registry()?;
    let storage = match storage {
        Some(storage) => storage,
        None => Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?,
        ),
    };
    Agent::single()
        .provider("openai", "gpt-4o-mini")
        .infra(AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await
}

async fn attach_test_profiles(
    agent: Agent,
    storage: Arc<crate::session::sqlite_storage::SqliteStorage>,
    profile_dir: &std::path::Path,
) -> Result<Agent> {
    let (registry, _temp_dir) = empty_plugin_registry()?;
    let catalog: Arc<dyn ProfileCatalog> = Arc::new(
        LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(profile_dir)
            .build(),
    );
    Ok(agent.with_profiles(AgentProfiles::new(
        catalog,
        "alpha",
        AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        },
    )))
}

#[tokio::test]
async fn with_profiles_keeps_watcher_alive() -> Result<()> {
    let dir = tempfile::TempDir::new()?;
    std::fs::write(
        dir.path().join("alpha.toml"),
        "[agent]\nprovider = \"test\"\nmodel = \"test-model\"\nsystem = \"inline\"\n",
    )?;
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let agent = test_agent_with_storage(Some(storage.clone())).await?;
    let agent = attach_test_profiles(agent, storage, dir.path()).await?;

    assert!(agent.profiles().is_some());
    assert!(
        agent
            .profiles
            .as_ref()
            .expect("profiles attached")
            .has_watcher()
    );
    assert!(agent.handle().profiles().is_some());
    Ok(())
}

#[cfg(feature = "api")]
#[tokio::test]
async fn server_inherits_attached_profiles() -> Result<()> {
    let dir = tempfile::TempDir::new()?;
    std::fs::write(
        dir.path().join("alpha.toml"),
        "[agent]\nprovider = \"test\"\nmodel = \"test-model\"\nsystem = \"inline\"\n",
    )?;
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let agent = test_agent_with_storage(Some(storage.clone())).await?;
    let agent = attach_test_profiles(agent, storage, dir.path()).await?;

    let server = agent.server();
    assert!(server.profiles().is_some());
    Ok(())
}

#[tokio::test]
async fn list_sessions_groups_and_children_use_shared_api() -> Result<()> {
    let agent = test_agent().await?;
    let store = agent.storage_backend();
    let session_store = store.session_store();

    let root = session_store
        .create_session(
            Some("Root Session".to_string()),
            Some("/tmp/root".into()),
            None,
            None,
        )
        .await?;
    let child = session_store
        .create_session(
            Some("Child Session".to_string()),
            Some("/tmp/root".into()),
            Some(root.public_id.clone()),
            Some(crate::session::domain::ForkOrigin::User),
        )
        .await?;

    let page = agent
        .list_sessions(ListSessionsOptions {
            mode: SessionListMode::Browse,
            session_scope: Some(crate::session::projection::SessionScope::All),
            ..Default::default()
        })
        .await?;

    assert_eq!(page.total_count, 2);
    assert!(
        page.groups
            .iter()
            .flat_map(|group| group.sessions.iter())
            .any(|session| session.session_id == root.public_id)
    );

    let children = agent
        .sessions()
        .children(root.public_id.clone(), None, Some(20))
        .await?;
    assert_eq!(children.total_count, 1);
    assert_eq!(children.sessions[0].session_id, child.public_id);
    Ok(())
}

#[tokio::test]
async fn acp_list_sessions_orders_by_updated_at_and_paginates() -> Result<()> {
    let agent = test_agent().await?;
    let session_store = agent.storage_backend().session_store();

    let first = session_store
        .create_session(
            Some("First Session".to_string()),
            Some("/tmp/a".into()),
            None,
            None,
        )
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let second = session_store
        .create_session(
            Some("Second Session".to_string()),
            Some("/tmp/b".into()),
            None,
            None,
        )
        .await?;

    let view_store = agent.storage_backend().view_store().expect("view store");

    let all = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new(),
    )
    .await?;
    assert_eq!(all.total_count, 2);
    assert_eq!(all.sessions.len(), 2);
    assert_eq!(all.sessions[0].session_id.to_string(), second.public_id);
    assert_eq!(all.sessions[0].title.as_deref(), Some("Second Session"));
    assert_eq!(all.sessions[1].session_id.to_string(), first.public_id);
    assert!(all.next_cursor.is_none());

    let filtered = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new().cwd(PathBuf::from("/tmp/a")),
    )
    .await?;
    assert_eq!(filtered.total_count, 1);
    assert_eq!(filtered.sessions[0].session_id.to_string(), first.public_id);

    let cursor_page = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new().cursor("1"),
    )
    .await?;
    assert_eq!(cursor_page.total_count, 2);
    assert_eq!(cursor_page.sessions.len(), 1);
    assert_eq!(
        cursor_page.sessions[0].session_id.to_string(),
        first.public_id
    );
    assert!(cursor_page.next_cursor.is_none());

    let invalid_cursor_page = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new().cursor("not-a-number"),
    )
    .await?;
    assert_eq!(invalid_cursor_page.total_count, 2);
    assert_eq!(invalid_cursor_page.sessions.len(), 2);
    assert_eq!(
        invalid_cursor_page.sessions[0].session_id.to_string(),
        second.public_id
    );

    let untitled = session_store
        .create_session(None, Some("/tmp/c".into()), None, None)
        .await?;
    let with_untitled =
        AgentSessions::list_for_acp_from_view_store(view_store, AcpListSessionsRequest::new())
            .await?;
    let untitled_info = with_untitled
        .sessions
        .iter()
        .find(|info| info.session_id.to_string() == untitled.public_id)
        .expect("untitled session should be present");
    assert!(untitled_info.title.is_none());
    assert_eq!(untitled_info.cwd, PathBuf::from("/tmp/c"));

    let intent_only = session_store
        .create_session(None, Some("/tmp/d".into()), None, None)
        .await?;
    let intent_session = session_store
        .get_session(&intent_only.public_id)
        .await?
        .expect("session exists");
    session_store
        .create_intent_snapshot(crate::session::domain::IntentSnapshot {
            id: 0,
            session_id: intent_session.id,
            task_id: None,
            summary: "This title comes from the initial intent snapshot and should be truncated if it gets too long for the ACP session list".to_string(),
            constraints: None,
            next_step_hint: None,
            created_at: time::OffsetDateTime::now_utc(),
        })
        .await?;
    let with_intent_title = AgentSessions::list_for_acp_from_view_store(
        agent.storage_backend().view_store().expect("view store"),
        AcpListSessionsRequest::new(),
    )
    .await?;
    let intent_info = with_intent_title
        .sessions
        .iter()
        .find(|info| info.session_id.to_string() == intent_only.public_id)
        .expect("intent-titled session should be present");
    assert_eq!(
        intent_info.title.as_deref(),
        Some("This title comes from the initial intent snapshot and should be truncated if ...")
    );
    Ok(())
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn list_sessions_remote_bookmarks_mode_includes_detached_bookmarks() -> Result<()> {
    let agent = test_agent().await?;
    let storage = agent.storage_backend();
    storage
        .session_store()
        .save_remote_session_bookmark(&RemoteSessionBookmark {
            session_id: "remote-session-1".to_string(),
            node_id: "node-1".to_string(),
            peer_label: "remote-peer".to_string(),
            cwd: Some("/remote/worktree".to_string()),
            created_at: 1,
            title: Some("Remote Bookmark".to_string()),
        })
        .await?;

    let page = agent
        .list_sessions(ListSessionsOptions {
            mode: SessionListMode::Browse,
            remote: RemoteSessionMode::Bookmarks,
            session_scope: Some(crate::session::projection::SessionScope::All),
            ..Default::default()
        })
        .await?;

    let remote = page
        .groups
        .iter()
        .flat_map(|group| group.sessions.iter())
        .find(|session| session.session_id == "remote-session-1")
        .expect("remote bookmark should be present");
    assert_eq!(remote.attached, Some(false));
    assert_eq!(remote.node.as_deref(), Some("remote-peer"));
    Ok(())
}

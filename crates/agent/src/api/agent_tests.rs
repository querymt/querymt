#[cfg(feature = "remote")]
use super::RemoteSessionMode;
use super::{
    Agent, AgentInfra, AgentProfiles, AgentSessions, ListSessionsOptions, SessionListMode,
};
use crate::acp::protocol::ListSessionsRequest as AcpListSessionsRequest;
use crate::config::SlashCommandsConfig;
use crate::events::{AgentEventKind, EventOrigin, ExecutionMetrics};
use crate::model::AgentMessage;
use crate::profiles::{LocalProfileCatalog, ProfileCatalog};
use crate::send_agent::SendAgent;
use crate::session::backend::StorageBackend;
use crate::session::lifecycle::{
    ArchiveFilter, ResolutionFilter, SessionLifecycleFilter, SessionLifecyclePatch,
};
use crate::session::projection::{EventJournal, NewDurableEvent, SessionScope, ViewStore};
#[cfg(feature = "remote")]
use crate::session::store::RemoteSessionBookmark;
use crate::test_utils::helpers::empty_plugin_registry;
use anyhow::Result;
use async_trait::async_trait;
use querymt::chat::{ChatRole, FinishReason};
use std::path::PathBuf;
use std::sync::Arc;

async fn test_agent() -> Result<Agent> {
    test_agent_with_storage(None).await
}

async fn test_agent_with_storage(
    storage: Option<Arc<crate::session::sqlite_storage::SqliteStorage>>,
) -> Result<Agent> {
    let storage = match storage {
        Some(storage) => storage,
        None => Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?,
        ),
    };
    test_agent_with_backend(storage).await
}

async fn test_agent_with_backend(storage: Arc<dyn StorageBackend>) -> Result<Agent> {
    let (registry, _temp_dir) = empty_plugin_registry()?;
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

struct DefaultLifecycleViewStore(Arc<crate::session::sqlite_storage::SqliteStorage>);

#[async_trait]
impl ViewStore for DefaultLifecycleViewStore {
    async fn get_audit_view(
        &self,
        session_id: &str,
        include_children: bool,
    ) -> crate::session::error::SessionResult<crate::session::projection::AuditView> {
        self.0.get_audit_view(session_id, include_children).await
    }

    async fn get_redacted_view(
        &self,
        session_id: &str,
        policy: crate::session::projection::RedactionPolicy,
    ) -> crate::session::error::SessionResult<crate::session::projection::RedactedView> {
        self.0.get_redacted_view(session_id, policy).await
    }

    async fn get_summary_view(
        &self,
        session_id: &str,
    ) -> crate::session::error::SessionResult<crate::session::projection::SummaryView> {
        self.0.get_summary_view(session_id).await
    }

    async fn get_session_list_view(
        &self,
        filter: Option<crate::session::projection::SessionListFilter>,
    ) -> crate::session::error::SessionResult<crate::session::projection::SessionListView> {
        self.0.get_session_list_view(filter).await
    }

    async fn browse_session_groups(
        &self,
        cursor: Option<String>,
        group_limit: usize,
        session_limit_per_group: usize,
        session_scope: crate::session::projection::SessionScope,
    ) -> crate::session::error::SessionResult<(
        Vec<crate::session::projection::SessionGroup>,
        Option<String>,
        usize,
    )> {
        self.0
            .browse_session_groups(cursor, group_limit, session_limit_per_group, session_scope)
            .await
    }

    async fn list_group_sessions(
        &self,
        cwd: Option<String>,
        cursor: Option<String>,
        limit: usize,
        session_scope: crate::session::projection::SessionScope,
    ) -> crate::session::error::SessionResult<(crate::session::projection::SessionGroup, usize)>
    {
        self.0
            .list_group_sessions(cwd, cursor, limit, session_scope)
            .await
    }

    async fn list_session_items(
        &self,
        cwd: Option<String>,
        cursor: Option<String>,
        limit: usize,
        session_scope: crate::session::projection::SessionScope,
    ) -> crate::session::error::SessionResult<(
        Vec<crate::session::projection::SessionListItem>,
        Option<String>,
        usize,
    )> {
        self.0
            .list_session_items(cwd, cursor, limit, session_scope)
            .await
    }

    async fn get_session_list_meta_stats(
        &self,
        session_ids: &[String],
    ) -> crate::session::error::SessionResult<
        std::collections::HashMap<String, crate::session::projection::SessionListMetaStats>,
    > {
        self.0.get_session_list_meta_stats(session_ids).await
    }

    async fn search_sessions(
        &self,
        query: String,
        cursor: Option<String>,
        limit: usize,
        session_scope: crate::session::projection::SessionScope,
    ) -> crate::session::error::SessionResult<(
        Vec<crate::session::projection::SessionGroup>,
        Option<String>,
        usize,
    )> {
        self.0
            .search_sessions(query, cursor, limit, session_scope)
            .await
    }

    async fn list_session_children(
        &self,
        parent_session_id: String,
        cursor: Option<String>,
        limit: usize,
    ) -> crate::session::error::SessionResult<(crate::session::projection::SessionGroup, usize)>
    {
        self.0
            .list_session_children(parent_session_id, cursor, limit)
            .await
    }

    async fn get_atif(
        &self,
        session_id: &str,
        options: &crate::export::AtifExportOptions,
    ) -> crate::session::error::SessionResult<crate::export::ATIF> {
        self.0.get_atif(session_id, options).await
    }

    async fn get_recent_models_view(
        &self,
        limit_per_workspace: usize,
    ) -> crate::session::error::SessionResult<crate::session::projection::RecentModelsView> {
        self.0.get_recent_models_view(limit_per_workspace).await
    }
}

struct SplitLifecycleBackend(Arc<crate::session::sqlite_storage::SqliteStorage>);

#[async_trait]
impl StorageBackend for SplitLifecycleBackend {
    fn session_store(&self) -> Arc<dyn crate::session::store::SessionStore> {
        Arc::new(self.0.as_ref().clone())
    }

    fn event_journal(&self) -> Arc<dyn EventJournal> {
        Arc::new(self.0.as_ref().clone())
    }

    fn view_store(&self) -> Option<Arc<dyn ViewStore>> {
        Some(Arc::new(DefaultLifecycleViewStore(self.0.clone())))
    }
}

async fn agent_with_slash_config(config: SlashCommandsConfig) -> Result<Agent> {
    let (registry, _temp_dir) = empty_plugin_registry()?;
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    Agent::single()
        .provider("openai", "gpt-4o-mini")
        .infra(AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .slash_commands(config)
        .build()
        .await
}

async fn build_test_profile_agent(
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
    let infra = AgentInfra {
        plugin_registry: Arc::new(registry),
        storage: Some(storage),
        session_mcp_attachment_source: None,
        event_fanout: None,
    };
    let profiles = AgentProfiles::new(catalog, "alpha", infra);
    let runtime = profiles.active_runtime().await?;
    Ok(runtime.agent().clone().with_profiles(profiles))
}

async fn build_test_profile_multi_agent(
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
    let infra = AgentInfra {
        plugin_registry: Arc::new(registry),
        storage: Some(storage),
        session_mcp_attachment_source: None,
        event_fanout: Some(Arc::new(crate::event_fanout::EventFanout::new())),
    };
    let profiles = AgentProfiles::new(catalog, "team", infra);
    let runtime = profiles.active_runtime().await?;
    Ok(runtime.agent().clone().with_profiles(profiles))
}

#[tokio::test]
async fn with_profiles_reuses_root_agent_for_active_profile() -> Result<()> {
    let dir = tempfile::TempDir::new()?;
    std::fs::write(
        dir.path().join("alpha.toml"),
        "[agent]\nprovider = \"test\"\nmodel = \"test-model\"\nsystem = \"inline\"\n",
    )?;
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let agent = build_test_profile_agent(storage, dir.path()).await?;
    let root_handle = agent.handle();

    let runtime = agent
        .profiles()
        .expect("profiles attached")
        .active_runtime()
        .await?;

    assert!(Arc::ptr_eq(&root_handle, &runtime.agent().handle()));
    Ok(())
}

async fn agent_with_middleware_entries(
    entries: Vec<crate::config::MiddlewareEntry>,
) -> Result<Agent> {
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let (registry, _temp_dir) = empty_plugin_registry()?;
    Agent::single()
        .provider("openai", "gpt-4o-mini")
        .infra(AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .middleware_from_config(entries)
        .build()
        .await
}

fn guard_entry(config: serde_json::Value) -> crate::config::MiddlewareEntry {
    crate::config::MiddlewareEntry {
        middleware_type: "parallel_edit_guard".to_string(),
        config,
    }
}

fn guard_count(agent: &Agent) -> usize {
    agent
        .handle()
        .config
        .middleware_drivers
        .iter()
        .filter(|d| d.name() == "ParallelEditGuard")
        .count()
}

#[tokio::test]
async fn default_agent_has_parallel_edit_guard() -> Result<()> {
    let agent = test_agent().await?;
    assert_eq!(guard_count(&agent), 1);
    Ok(())
}

#[tokio::test]
async fn explicit_deny_guard_config_yields_single_instance() -> Result<()> {
    let agent =
        agent_with_middleware_entries(vec![guard_entry(serde_json::json!({"mode": "deny"}))])
            .await?;
    assert_eq!(guard_count(&agent), 1);
    Ok(())
}

#[tokio::test]
async fn disabled_guard_config_yields_no_guard() -> Result<()> {
    let agent =
        agent_with_middleware_entries(vec![guard_entry(serde_json::json!({"enabled": false}))])
            .await?;
    assert_eq!(guard_count(&agent), 0);
    Ok(())
}

#[tokio::test]
async fn programmatic_builder_does_not_discover_slash_commands_by_default() -> Result<()> {
    let agent = test_agent().await?;
    assert!(agent.handle().config.slash_command_registry.is_empty());
    Ok(())
}

#[tokio::test]
async fn slash_commands_config_disabled_does_not_scan_filesystem() -> Result<()> {
    let dir = tempfile::TempDir::new()?;
    let commands = dir.path().join("commands");
    std::fs::create_dir_all(&commands)?;
    std::fs::write(
        commands.join("docs.md"),
        "---\ndescription: Read the docs\n---\nBody\n",
    )?;

    let agent = agent_with_slash_config(SlashCommandsConfig {
        enabled: false,
        include_global: false,
        include_project: false,
        paths: vec![commands],
        scripts: Default::default(),
    })
    .await?;
    assert!(agent.handle().config.slash_command_registry.is_empty());
    Ok(())
}

#[tokio::test]
async fn slash_commands_config_enabled_discovers_configured_paths() -> Result<()> {
    let dir = tempfile::TempDir::new()?;
    let commands = dir.path().join("commands");
    std::fs::create_dir_all(&commands)?;
    std::fs::write(
        commands.join("docs.md"),
        "---\ndescription: Read the docs\n---\nBody\n",
    )?;

    let agent = agent_with_slash_config(SlashCommandsConfig {
        enabled: true,
        include_global: false,
        include_project: false,
        paths: vec![commands],
        scripts: Default::default(),
    })
    .await?;
    assert!(
        agent
            .handle()
            .config
            .slash_command_registry
            .get("docs")
            .is_some()
    );
    Ok(())
}

async fn agent_from_single_config(toml: &str) -> Result<Agent> {
    let config: crate::config::SingleAgentConfig = toml::from_str(toml)?;
    let (registry, _temp_dir) = empty_plugin_registry()?;
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    Agent::from_config(
        config,
        AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        },
    )
    .await
}

#[tokio::test]
async fn from_single_config_propagates_slash_command_paths_and_include_flags() -> Result<()> {
    let dir = tempfile::TempDir::new()?;
    let extra = dir.path().join("extra-commands");
    std::fs::create_dir_all(&extra)?;
    std::fs::write(
        extra.join("docs.md"),
        "---\ndescription: Read the docs\n---\nBody\n",
    )?;
    let project_commands = dir.path().join(".qmt/commands");
    std::fs::create_dir_all(&project_commands)?;
    std::fs::write(
        project_commands.join("project.md"),
        "---\ndescription: Project command\n---\nBody\n",
    )?;

    let extra_only = format!(
        r#"
[agent]
provider = "openai"
model = "gpt-4o-mini"
cwd = "{cwd}"

[agent.slash_commands]
enabled = true
include_global = false
include_project = false
paths = ["{extra}"]
"#,
        cwd = dir.path().display(),
        extra = extra.display(),
    );
    let extra_only_agent = agent_from_single_config(&extra_only).await?;
    assert!(
        extra_only_agent
            .handle()
            .config
            .slash_command_registry
            .get("docs")
            .is_some()
    );
    assert!(
        extra_only_agent
            .handle()
            .config
            .slash_command_registry
            .get("project")
            .is_none(),
        "include_project=false must not scan <cwd>/.qmt/commands"
    );

    let project_only = format!(
        r#"
[agent]
provider = "openai"
model = "gpt-4o-mini"
cwd = "{cwd}"

[agent.slash_commands]
enabled = true
include_global = false
include_project = true
"#,
        cwd = dir.path().display(),
    );
    let project_only_agent = agent_from_single_config(&project_only).await?;
    assert!(
        project_only_agent
            .handle()
            .config
            .slash_command_registry
            .get("project")
            .is_some()
    );
    assert!(
        project_only_agent
            .handle()
            .config
            .slash_command_registry
            .get("docs")
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn shutdown_with_profiles_also_stops_independent_root() -> Result<()> {
    let dir = tempfile::TempDir::new()?;
    std::fs::write(
        dir.path().join("alpha.toml"),
        "[agent]\nprovider = \"test\"\nmodel = \"test-model\"\nsystem = \"inline\"\n",
    )?;
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let root = test_agent_with_storage(Some(storage.clone())).await?;
    assert!(!root.handle().is_shutdown());
    let (registry, _temp_dir) = empty_plugin_registry()?;
    let catalog: Arc<dyn ProfileCatalog> = Arc::new(
        LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build(),
    );
    let profiles = AgentProfiles::new(
        catalog,
        "alpha",
        AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        },
    );
    let root = root.with_profiles(profiles);

    root.shutdown().await;

    assert!(root.handle().is_shutdown());
    Ok(())
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
    let agent = build_test_profile_agent(storage, dir.path()).await?;

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
    let agent = build_test_profile_agent(storage, dir.path()).await?;

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
async fn delete_session_clears_delegate_model_overrides() -> Result<()> {
    let agent = test_agent().await?;
    let session_id = agent.create_session().await?;
    agent
        .inner
        .config
        .delegate_model_overrides
        .set(
            &session_id,
            "coder",
            crate::delegation::DelegateModelOverride {
                model_id: "mock/alternate".into(),
                node_id: None,
            },
        )
        .await;

    agent.sessions().delete(&session_id).await?;

    assert!(
        agent
            .inner
            .config
            .delegate_model_overrides
            .get(&session_id, "coder")
            .await
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn delete_loaded_idle_session_does_not_wait_for_stop_grace() -> Result<()> {
    let agent = test_agent().await?;
    let session_id = agent.create_session().await?;

    let started = std::time::Instant::now();
    agent.sessions().delete(&session_id).await?;

    // Stop escalation allows 3s; deleting an idle session must not pay it.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "delete took {:?}",
        started.elapsed()
    );
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

    let invalid_cursor_error = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new().cursor("not-a-number"),
    )
    .await
    .expect_err("invalid ACP cursors must be rejected");
    assert_eq!(
        invalid_cursor_error.to_string(),
        "invalid session list cursor"
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
            revision: 1,
            source: "test".to_string(),
            source_ref: None,
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

#[tokio::test]
async fn acp_list_sessions_pages_workspace_sessions_without_cross_workspace_leaks() -> Result<()> {
    let agent = test_agent().await?;
    let session_store = agent.storage_backend().session_store();
    let workspace = PathBuf::from("/tmp/acp-workspace");
    let other_workspace = PathBuf::from("/tmp/acp-other-workspace");
    let mut workspace_session_ids = Vec::new();

    for index in 0..12 {
        let session = session_store
            .create_session(
                Some(format!("Workspace Session {index}")),
                Some(workspace.clone()),
                None,
                None,
            )
            .await?;
        workspace_session_ids.push(session.public_id);
    }
    for index in 0..90 {
        session_store
            .create_session(
                Some(format!("Other Workspace Session {index}")),
                Some(other_workspace.clone()),
                None,
                None,
            )
            .await?;
    }

    let view_store = agent.storage_backend().view_store().expect("view store");
    let first_page = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new().cwd(workspace.clone()),
    )
    .await?;
    assert_eq!(first_page.total_count, 12);
    assert_eq!(first_page.sessions.len(), 10);
    assert!(
        first_page
            .sessions
            .iter()
            .all(|session| session.cwd == workspace)
    );
    assert_eq!(
        first_page
            .sessions
            .iter()
            .map(|session| session.session_id.to_string())
            .collect::<Vec<_>>(),
        workspace_session_ids[2..]
            .iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>(),
    );
    let trailing_cwd = PathBuf::from(format!("{}/", workspace.display()));
    let trailing_page = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new().cwd(trailing_cwd.clone()),
    )
    .await?;
    assert_eq!(trailing_page.total_count, 12);
    assert_eq!(trailing_page.sessions.len(), 10);
    assert!(
        trailing_page
            .sessions
            .iter()
            .all(|session| session.cwd == workspace)
    );

    let group_page = agent
        .sessions()
        .list_group(
            Some(trailing_cwd.display().to_string()),
            ListSessionsOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(group_page.total_count, 12);
    assert_eq!(group_page.groups[0].cwd.as_deref(), workspace.to_str());
    assert_eq!(group_page.groups[0].sessions.len(), 1);
    assert!(group_page.next_cursor.is_some());

    let next_cursor = first_page
        .next_cursor
        .expect("workspace page should continue");

    let second_page = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new()
            .cwd(workspace.clone())
            .cursor(next_cursor),
    )
    .await?;
    assert_eq!(second_page.total_count, 12);
    assert_eq!(second_page.sessions.len(), 2);
    assert!(
        second_page
            .sessions
            .iter()
            .all(|session| session.cwd == workspace)
    );
    assert_eq!(
        second_page
            .sessions
            .iter()
            .map(|session| session.session_id.to_string())
            .collect::<Vec<_>>(),
        workspace_session_ids[..2]
            .iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>(),
    );
    assert!(second_page.next_cursor.is_none());

    let unfiltered =
        AgentSessions::list_for_acp_from_view_store(view_store, AcpListSessionsRequest::new())
            .await?;
    assert_eq!(unfiltered.total_count, 102);
    assert_eq!(unfiltered.sessions.len(), 100);
    assert_eq!(unfiltered.next_cursor.as_deref(), Some("100"));
    Ok(())
}

#[tokio::test]
async fn acp_list_sessions_filters_by_session_scope_meta() -> Result<()> {
    let agent = test_agent().await?;
    let session_store = agent.storage_backend().session_store();
    let root = session_store
        .create_session(
            Some("Root Session".to_string()),
            Some("/tmp/scope".into()),
            None,
            None,
        )
        .await?;
    let user_fork = session_store
        .create_session(
            Some("User Fork".to_string()),
            Some("/tmp/scope".into()),
            Some(root.public_id.clone()),
            Some(crate::session::domain::ForkOrigin::User),
        )
        .await?;
    let delegate = session_store
        .create_session(
            Some("Delegate Session".to_string()),
            Some("/tmp/scope".into()),
            Some(root.public_id.clone()),
            Some(crate::session::domain::ForkOrigin::Delegation),
        )
        .await?;
    let view_store = agent.storage_backend().view_store().expect("view store");

    let all = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new(),
    )
    .await?;
    assert_eq!(all.total_count, 3);

    let mut root_meta = crate::acp::protocol::Meta::new();
    root_meta.insert(
        "session_scope".to_string(),
        serde_json::Value::String("root".to_string()),
    );
    let roots = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new().meta(root_meta),
    )
    .await?;
    assert_eq!(
        roots
            .sessions
            .iter()
            .map(|session| session.session_id.to_string())
            .collect::<Vec<_>>(),
        vec![root.public_id.clone()]
    );

    let mut delegates_meta = crate::acp::protocol::Meta::new();
    delegates_meta.insert(
        "sessionScope".to_string(),
        serde_json::Value::String("delegates".to_string()),
    );
    let delegates = AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        AcpListSessionsRequest::new().meta(delegates_meta),
    )
    .await?;
    assert_eq!(
        delegates
            .sessions
            .iter()
            .map(|session| session.session_id.to_string())
            .collect::<Vec<_>>(),
        vec![delegate.public_id]
    );

    let mut forks_meta = crate::acp::protocol::Meta::new();
    forks_meta.insert(
        "session_scope".to_string(),
        serde_json::Value::String("forks".to_string()),
    );
    let forks = AgentSessions::list_for_acp_from_view_store(
        view_store,
        AcpListSessionsRequest::new().meta(forks_meta),
    )
    .await?;
    assert_eq!(
        forks
            .sessions
            .iter()
            .map(|session| session.session_id.to_string())
            .collect::<Vec<_>>(),
        vec![user_fork.public_id]
    );
    Ok(())
}

#[tokio::test]
async fn acp_session_list_includes_relationship_and_operational_meta() -> Result<()> {
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let agent = test_agent_with_storage(Some(storage.clone())).await?;
    let session_store = storage.session_store();
    let parent = session_store
        .create_session(
            Some("Parent Session".to_string()),
            Some("/tmp/meta".into()),
            None,
            None,
        )
        .await?;
    let child = session_store
        .create_session(
            Some("Child Session".to_string()),
            Some("/tmp/meta".into()),
            Some(parent.public_id.clone()),
            Some(crate::session::domain::ForkOrigin::User),
        )
        .await?;

    session_store
        .add_message(
            &child.public_id,
            AgentMessage::new(child.public_id.clone(), ChatRole::User),
        )
        .await?;
    session_store
        .add_message(
            &child.public_id,
            AgentMessage::new(child.public_id.clone(), ChatRole::Assistant),
        )
        .await?;
    storage
        .append_durable(&NewDurableEvent {
            session_seq: None,
            session_id: child.public_id.clone(),
            origin: EventOrigin::Local,
            source_node: None,
            source_node_id: None,
            source_seq: None,
            kind: AgentEventKind::LlmRequestEnd {
                usage: None,
                tool_calls: 0,
                finish_reason: Some(FinishReason::Error),
                cost_usd: None,
                cumulative_cost_usd: None,
                context_tokens: 0,
                metrics: ExecutionMetrics::default(),
                accounting: None,
            },
        })
        .await?;

    let page = agent
        .sessions()
        .list_for_acp(AcpListSessionsRequest::new())
        .await?;
    let parent_info = page
        .sessions
        .iter()
        .find(|info| info.session_id.to_string() == parent.public_id)
        .expect("parent session should be listed");
    let parent_meta = parent_info
        .meta
        .as_ref()
        .expect("parent ACP session meta should be set");
    assert_eq!(
        parent_meta.get("hasChildren"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(parent_meta.get("forkCount"), Some(&serde_json::json!(1)));
    assert!(!parent_meta.contains_key("parentSessionId"));
    assert!(!parent_meta.contains_key("forkOrigin"));
    assert!(!parent_meta.contains_key("sessionKind"));

    let child_info = page
        .sessions
        .iter()
        .find(|info| info.session_id.to_string() == child.public_id)
        .expect("child session should be listed");
    let child_meta = child_info
        .meta
        .as_ref()
        .expect("child ACP session meta should be set");
    assert_eq!(
        child_meta.get("parentSessionId"),
        Some(&serde_json::json!(parent.public_id))
    );
    assert_eq!(
        child_meta.get("forkOrigin"),
        Some(&serde_json::json!("user"))
    );
    assert_eq!(
        child_meta.get("hasChildren"),
        Some(&serde_json::json!(false))
    );
    assert_eq!(child_meta.get("forkCount"), Some(&serde_json::json!(0)));
    assert!(!child_meta.contains_key("sessionKind"));
    assert_eq!(child_meta.get("messageCount"), Some(&serde_json::json!(2)));
    assert_eq!(
        child_meta.get("userMessageCount"),
        Some(&serde_json::json!(1))
    );
    assert_eq!(child_meta.get("hasErrors"), Some(&serde_json::json!(true)));
    assert_eq!(
        child_meta
            .get("runtimeStatus")
            .and_then(|status| status.get("phase")),
        Some(&serde_json::json!("unknown"))
    );
    assert_eq!(
        child_meta["runtimeStatus"]["observation"]["availability"], "unavailable",
        "unmaterialized in-memory storage has no owner or run-lease evidence"
    );
    assert_eq!(
        child_meta
            .get("runtimeStatus")
            .and_then(|status| status.get("active_run_id")),
        Some(&serde_json::Value::Null)
    );
    Ok(())
}

#[tokio::test]
async fn acp_session_list_marks_scheduled_recurring_session() -> Result<()> {
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let agent = test_agent_with_storage(Some(storage.clone())).await?;
    let session = storage
        .session_store()
        .create_session(
            Some("Scheduled Session".to_string()),
            Some("/tmp/scheduled".into()),
            None,
            None,
        )
        .await?;

    crate::control::schedules::create_schedule(
        &agent.handle(),
        crate::control::schedules::CreateScheduleControlRequest {
            node_id: None,
            session_id: session.public_id.clone(),
            prompt: "daily summary".to_string(),
            trigger: crate::session::domain_schedule::ScheduleTrigger::Interval { seconds: 300 },
            max_steps: None,
            max_cost_usd: None,
            max_runs: None,
        },
    )
    .await?;

    let page = agent
        .sessions()
        .list_for_acp(AcpListSessionsRequest::new())
        .await?;
    let info = page
        .sessions
        .iter()
        .find(|info| info.session_id.to_string() == session.public_id)
        .expect("scheduled session should be listed");
    let meta = info.meta.as_ref().expect("ACP session meta should be set");
    assert_eq!(
        meta.get("sessionKind"),
        Some(&serde_json::json!("recurring"))
    );
    Ok(())
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn acp_list_sessions_opt_in_selected_peer_pages_offline_bookmarks() -> Result<()> {
    let agent = test_agent().await?;
    let store = agent.storage_backend().session_store();
    for index in 0..12 {
        store
            .save_remote_session_bookmark(&RemoteSessionBookmark {
                session_id: format!("remote-{index:02}"),
                node_id: "peer-a".into(),
                peer_label: "Remote A".into(),
                cwd: Some("/remote/work".into()),
                created_at: index,
                title: Some(format!("Remote {index}")),
            })
            .await?;
    }
    store
        .save_remote_session_bookmark(&RemoteSessionBookmark {
            session_id: "other-peer".into(),
            node_id: "peer-b".into(),
            peer_label: "Remote B".into(),
            cwd: Some("/remote/work".into()),
            created_at: 0,
            title: None,
        })
        .await?;

    let local_only = agent
        .sessions()
        .list_for_acp(AcpListSessionsRequest::new())
        .await?;
    assert!(local_only.sessions.is_empty());
    let mut meta = crate::acp::protocol::Meta::new();
    meta.insert("remoteNodeIds".into(), serde_json::json!(["peer-a"]));
    meta.insert("session_scope".into(), serde_json::json!("root"));
    let first = agent
        .sessions()
        .list_for_acp(
            AcpListSessionsRequest::new()
                .cwd("/remote/work")
                .meta(meta.clone()),
        )
        .await?;
    assert_eq!(first.sessions.len(), 10);
    assert_eq!(first.next_cursor.as_deref(), Some("remote:0:10:bookmarks"));
    let second = agent
        .sessions()
        .list_for_acp(
            AcpListSessionsRequest::new()
                .cwd("/remote/work")
                .meta(meta)
                .cursor(first.next_cursor),
        )
        .await?;
    assert_eq!(second.sessions.len(), 2);
    assert_eq!(second.next_cursor, None);
    for session in first.sessions.iter().chain(&second.sessions) {
        let fields = session.meta.as_ref().expect("remote metadata");
        assert_eq!(fields["location"], "remote");
        assert_eq!(fields["nodeId"], "peer-a");
        assert_eq!(fields["nodeLabel"], "Remote A");
        assert_eq!(fields["connectionState"], "disconnected");
        assert_eq!(session.cwd, PathBuf::from("/remote/work"));
        assert!(session.title.as_deref().unwrap().starts_with("Remote "));
        assert_eq!(fields["runtimeStatus"]["phase"], "unknown");
        assert_eq!(
            fields["runtimeStatus"]["observation"]["availability"],
            "unavailable"
        );
        for field in ["messageCount", "userMessageCount", "hasErrors"] {
            assert!(
                !fields.contains_key(field),
                "offline bookmark fabricated {field}"
            );
        }
    }
    Ok(())
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn acp_remote_pages_advance_empty_peers_and_exclude_local_conflicts() -> Result<()> {
    let agent = test_agent().await?;
    let store = agent.storage_backend().session_store();
    let local = store.create_session(None, None, None, None).await?;
    for (session_id, node_id) in [
        (local.public_id.as_str(), "peer-a"),
        ("remote-a", "peer-a"),
        ("remote-b", "peer-b"),
    ] {
        store
            .save_remote_session_bookmark(&RemoteSessionBookmark {
                session_id: session_id.into(),
                node_id: node_id.into(),
                peer_label: node_id.into(),
                cwd: Some("/remote/work".into()),
                created_at: 0,
                title: None,
            })
            .await?;
    }
    let mut meta = crate::acp::protocol::Meta::new();
    meta.insert(
        "remoteNodeIds".into(),
        serde_json::json!(["peer-b", "empty", "peer-a", "peer-a"]),
    );
    let empty = agent
        .sessions()
        .list_for_acp(
            AcpListSessionsRequest::new()
                .meta(meta.clone())
                .cursor("remote:0:0"),
        )
        .await?;
    assert!(empty.sessions.is_empty());
    assert_eq!(empty.next_cursor.as_deref(), Some("remote:1:0"));
    let first = agent
        .sessions()
        .list_for_acp(
            AcpListSessionsRequest::new()
                .meta(meta.clone())
                .cursor(empty.next_cursor),
        )
        .await?;
    assert_eq!(first.sessions.len(), 1);
    assert_eq!(first.sessions[0].session_id.to_string(), "remote-a");
    assert_eq!(first.next_cursor.as_deref(), Some("remote:2:0"));
    let second = agent
        .sessions()
        .list_for_acp(
            AcpListSessionsRequest::new()
                .meta(meta.clone())
                .cursor(first.next_cursor),
        )
        .await?;
    assert_eq!(second.sessions.len(), 1);
    assert_eq!(second.sessions[0].session_id.to_string(), "remote-b");
    assert_eq!(second.next_cursor, None);
    for cursor in [
        "remote:3:0",
        "remote:0:4294967296",
        "remote:0:-1",
        "remote:0:0:extra",
    ] {
        assert!(
            agent
                .sessions()
                .list_for_acp(
                    AcpListSessionsRequest::new()
                        .meta(meta.clone())
                        .cursor(cursor),
                )
                .await
                .is_err(),
            "accepted {cursor}"
        );
    }
    meta.insert("session_scope".into(), serde_json::json!("children"));
    let children = agent
        .sessions()
        .list_for_acp(
            AcpListSessionsRequest::new()
                .meta(meta)
                .cursor("remote:1:0"),
        )
        .await?;
    assert!(children.sessions.is_empty());
    assert_eq!(children.next_cursor.as_deref(), Some("remote:2:0"));
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

#[tokio::test]
async fn shutdown_with_profiles_and_quorum_executes_both() -> Result<()> {
    let dir = tempfile::TempDir::new()?;
    std::fs::write(
        dir.path().join("team.toml"),
        r#"
[quorum]
delegation = true
verification = false
snapshot_policy = "none"

[planner]
provider = "test"
model = "test-planner"
tools = ["delegate"]

[[delegates]]
id = "coder"
provider = "test"
model = "test-coder"
tools = []
"#,
    )?;
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let agent = build_test_profile_multi_agent(storage, dir.path()).await?;

    // Verify both profiles and quorum are present
    assert!(agent.profiles().is_some());
    assert!(agent.quorum().is_some());
    assert!(agent.is_multi());

    // Get references before shutdown
    let planner = agent.planner().expect("planner present");
    let delegate = agent.delegate("coder").expect("delegate present");

    let planner = planner
        .as_any()
        .downcast_ref::<crate::agent::LocalAgentHandle>()
        .expect("local planner");
    let delegate = delegate
        .as_any()
        .downcast_ref::<crate::agent::LocalAgentHandle>()
        .expect("local delegate");
    assert!(!planner.is_shutdown());
    assert!(!delegate.is_shutdown());

    agent.shutdown().await;

    assert!(planner.is_shutdown());
    assert!(delegate.is_shutdown());

    Ok(())
}

// ---------------------------------------------------------------------------
// 6.1 Protocol overlays applied before final single-agent AgentConfig
// ---------------------------------------------------------------------------

/// Write a `.agents` tree with a `system-prompt.md` and return the temp dir.
fn write_dotagents_dir(prompt: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join(".agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("system-prompt.md"), prompt).unwrap();
    dir
}

/// Create `<workspace>/.agents/` inside an existing temp dir.
fn dotagents_subdir(workspace: &std::path::Path) -> std::path::PathBuf {
    let agents = workspace.join(".agents");
    std::fs::create_dir_all(&agents).unwrap();
    agents
}

#[tokio::test]
async fn protocol_prompt_overlay_appends_after_explicit_system_parts() -> Result<()> {
    let dir = write_dotagents_dir("Protocol system instructions");
    let (registry, _temp_dir) = empty_plugin_registry()?;
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);

    let agent = Agent::single()
        .provider("openai", "gpt-4o-mini")
        .system("Explicit system prompt")
        .infra(AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .dotagents_options(
            crate::dotagents::DotagentsLoadOptions::enabled()
                .with_workspace(dir.path())
                .with_global_enabled(false),
        )
        .build()
        .await?;

    let params = agent.inner.config.provider.initial_config();
    assert_eq!(
        params.system,
        vec![
            "Explicit system prompt".to_string(),
            "Protocol system instructions".to_string(),
        ],
        "protocol prompt must follow explicit system parts"
    );

    agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn partial_protocol_overlay_preserves_explicit_settings() -> Result<()> {
    // Only `agents.md` exists: the protocol represents no model, MCP, tool, or
    // knowledge settings, so every explicit setting must survive untouched.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dotagents_subdir(dir.path()).join("agents.md"),
        "Protocol instructions only",
    )
    .unwrap();

    let (registry, _temp_dir) = empty_plugin_registry()?;
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);

    let agent = Agent::single()
        .provider("openai", "gpt-4o-mini")
        .system("Explicit system prompt")
        .infra(AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .dotagents_options(
            crate::dotagents::DotagentsLoadOptions::enabled()
                .with_workspace(dir.path())
                .with_global_enabled(false),
        )
        .build()
        .await?;

    let params = agent.inner.config.provider.initial_config();
    // Explicit model survives.
    assert_eq!(params.provider.as_deref(), Some("openai"));
    assert_eq!(params.model.as_deref(), Some("gpt-4o-mini"));
    // Explicit prompt plus protocol instructions.
    assert_eq!(
        params.system.first().map(|part| part.text()),
        Some("Explicit system prompt")
    );
    assert!(
        params
            .system
            .iter()
            .any(|s| s == "Protocol instructions only")
    );

    agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn selected_model_preset_overlays_only_llm_fields() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dotagents_subdir(dir.path()).join("models.json"),
        r#"{ "models": { "fast": { "provider": "mock", "model": "claude-3-5-haiku" } } }"#,
    )
    .unwrap();

    // The preset names `mock`, so that provider must actually be registered:
    // an unavailable provider is refused and the overlay is not applied.
    let (registry, _temp_dir) = empty_plugin_registry()?;
    crate::test_utils::register_mock_provider(&registry);
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);

    let agent = Agent::single()
        .provider("openai", "gpt-4o-mini")
        .system("Explicit system prompt")
        .infra(AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .dotagents_options(
            crate::dotagents::DotagentsLoadOptions::enabled()
                .with_workspace(dir.path())
                .with_global_enabled(false)
                .with_selected_model_preset("fast"),
        )
        .build()
        .await?;

    let params = agent.inner.config.provider.initial_config();
    // The preset overlaid provider and model.
    assert_eq!(params.provider.as_deref(), Some("mock"));
    assert_eq!(params.model.as_deref(), Some("claude-3-5-haiku"));
    // The explicit system prompt was not dropped by the preset overlay.
    assert!(params.system.iter().any(|s| s == "Explicit system prompt"));

    agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unknown_selected_preset_keeps_explicit_llm_configuration() -> Result<()> {
    // `models.json` exists but the selected preset is absent: the explicit base
    // configuration must remain in effect rather than being discarded.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dotagents_subdir(dir.path()).join("models.json"),
        r#"{ "models": { "other": { "provider": "anthropic", "model": "claude-3-5-haiku" } } }"#,
    )
    .unwrap();

    let (registry, _temp_dir) = empty_plugin_registry()?;
    let storage =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);

    let agent = Agent::single()
        .provider("openai", "gpt-4o-mini")
        .infra(AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(storage),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .dotagents_options(
            crate::dotagents::DotagentsLoadOptions::enabled()
                .with_workspace(dir.path())
                .with_global_enabled(false)
                .with_selected_model_preset("missing"),
        )
        .build()
        .await?;

    let params = agent.inner.config.provider.initial_config();
    assert_eq!(params.provider.as_deref(), Some("openai"));
    assert_eq!(params.model.as_deref(), Some("gpt-4o-mini"));

    agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn protocol_mcp_server_is_appended_without_replacing_explicit_servers() -> Result<()> {
    // Explicit MCP servers enter through TOML `[[mcp]]` config; protocol
    // servers from `mcp.json` are appended rather than replacing them.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dotagents_subdir(dir.path()).join("mcp.json"),
        r#"{ "mcpServers": { "protocol-server": { "url": "https://example.test/mcp", "transport": "streamable-http" } } }"#,
    )
    .unwrap();

    let toml = format!(
        r#"
[agent]
provider = "openai"
model = "gpt-4o-mini"

[[mcp]]
name = "explicit-server"
command = "explicit-cmd"
transport = "stdio"

[dotagents]
enabled = true
workspace_root = "{}"
global_enabled = false
"#,
        dotagents_subdir(dir.path()).display()
    );
    let config: crate::config::SingleAgentConfig = toml::from_str(&toml).unwrap();

    let builder = Agent::builder_from_config(config, None)?;
    let agent = builder
        .infra(AgentInfra {
            plugin_registry: Arc::new(empty_plugin_registry()?.0),
            storage: Some(Arc::new(
                crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?,
            )),
            session_mcp_attachment_source: None,
            event_fanout: None,
        })
        .build()
        .await?;

    let names: Vec<&str> = agent
        .inner
        .config
        .mcp_servers
        .iter()
        .map(|server| server.name())
        .collect();
    assert!(
        names.contains(&"explicit-server"),
        "explicit server preserved: {names:?}"
    );
    assert!(
        names.contains(&"protocol-server"),
        "protocol server appended: {names:?}"
    );

    agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn lifecycle_api_filters_before_paging_and_acp_returns_metadata() -> Result<()> {
    let agent = test_agent().await?;
    let store = agent.storage_backend().session_store();
    let open = store.create_session(None, None, None, None).await?;
    let resolved = store.create_session(None, None, None, None).await?;
    agent
        .sessions()
        .set_session_lifecycle(
            &resolved.public_id,
            crate::session::lifecycle::SessionLifecyclePatch {
                resolved: Some(true),
                archived: None,
            },
        )
        .await?;

    let page = agent
        .sessions()
        .list_with_lifecycle(
            ListSessionsOptions {
                limit: Some(1),
                ..ListSessionsOptions::default()
            },
            crate::session::lifecycle::SessionLifecycleFilter {
                resolution: crate::session::lifecycle::ResolutionFilter::Open,
                archive: crate::session::lifecycle::ArchiveFilter::Any,
            },
        )
        .await?;
    assert_eq!(page.total_count, 1);
    let row = page
        .groups
        .iter()
        .flat_map(|group| &group.sessions)
        .next()
        .expect("open session in filtered page");
    assert_eq!(row.summary.session_id, open.public_id);
    assert_eq!(
        row.lifecycle.as_ref().map(|state| state.resolved),
        Some(false)
    );

    let mut meta = serde_json::Map::new();
    meta.insert(
        "querymt".into(),
        serde_json::json!({ "sessionFilters": { "resolution": "resolved" } }),
    );
    let acp_page = agent
        .sessions()
        .list_for_acp(AcpListSessionsRequest::new().meta(meta))
        .await?;
    assert_eq!(acp_page.sessions.len(), 1);
    let acp_row = &acp_page.sessions[0];
    assert_eq!(acp_row.session_id.to_string(), resolved.public_id);
    assert_eq!(
        acp_row
            .meta
            .as_ref()
            .and_then(|meta| meta.get("querymt"))
            .and_then(|meta| meta.get("lifecycle"))
            .and_then(|lifecycle| lifecycle.get("resolved"))
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );

    let mut invalid_meta = serde_json::Map::new();
    invalid_meta.insert(
        "querymt".into(),
        serde_json::json!({ "sessionFilters": { "resolution": null } }),
    );
    assert!(
        agent
            .sessions()
            .list_for_acp(AcpListSessionsRequest::new().meta(invalid_meta))
            .await
            .is_err()
    );

    agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn lifecycle_query_modes_preserve_filters_pages_and_metadata() -> Result<()> {
    let sqlite =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let agent = test_agent_with_storage(Some(sqlite.clone())).await?;
    let store = agent.storage_backend().session_store();
    let mut ids = Vec::new();
    for (name, cwd, updated_at, resolved, archived) in [
        (
            "needle first",
            Some("/work"),
            "2025-02-01T00:00:00Z",
            false,
            true,
        ),
        (
            "needle second",
            Some("/work"),
            "2025-01-01T00:00:00Z",
            false,
            true,
        ),
        (
            "needle hidden",
            Some("/work"),
            "2025-03-01T00:00:00Z",
            true,
            true,
        ),
        (
            "unrelated",
            Some("/elsewhere"),
            "2024-12-01T00:00:00Z",
            false,
            true,
        ),
        ("ungrouped", None, "2024-11-01T00:00:00Z", false, true),
        (
            "needle unarchived",
            Some("/work"),
            "2025-04-01T00:00:00Z",
            false,
            false,
        ),
    ] {
        let session = store
            .create_session(Some(name.into()), cwd.map(PathBuf::from), None, None)
            .await?;
        store
            .patch_session_lifecycle(
                &session.public_id,
                &SessionLifecyclePatch {
                    resolved: Some(resolved),
                    archived: Some(archived),
                },
            )
            .await?;
        sqlite.conn_for_test().lock().unwrap().execute(
            "UPDATE sessions SET updated_at = ?1 WHERE public_id = ?2",
            rusqlite::params![updated_at, session.public_id],
        )?;
        ids.push(session.public_id);
    }
    let child = store
        .create_session(
            Some("needle child".into()),
            Some("/work".into()),
            Some(ids[2].clone()),
            Some(crate::session::domain::ForkOrigin::User),
        )
        .await?;
    store
        .patch_session_lifecycle(
            &child.public_id,
            &SessionLifecyclePatch {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await?;
    let filter = SessionLifecycleFilter {
        resolution: ResolutionFilter::Open,
        archive: ArchiveFilter::Only,
    };
    let options = ListSessionsOptions {
        // Convenience methods must replace the caller's mode, cwd and query.
        mode: SessionListMode::Search,
        cwd: Some("/wrong".into()),
        query: Some("not-found".into()),
        limit: Some(1),
        session_scope: Some(SessionScope::Root),
        ..Default::default()
    };
    let sessions = agent.sessions();
    let first = sessions
        .list_group_with_lifecycle(Some("/work/".into()), options.clone(), filter)
        .await?;
    assert_eq!(first.total_count, 2);
    assert_eq!(first.next_cursor.as_deref(), Some("1"));
    let group = &first.groups[0];
    assert_eq!(group.cwd.as_deref(), Some("/work"));
    assert_eq!(group.total_count, Some(2));
    assert_eq!(group.next_cursor, first.next_cursor);
    assert_eq!(
        group.latest_activity.as_deref(),
        Some("2025-02-01T00:00:00Z")
    );
    assert_eq!(group.sessions.len(), 1);
    assert_eq!(group.sessions[0].summary.session_id, ids[0]);
    assert_eq!(group.sessions[0].summary.cwd, group.cwd);
    assert_eq!(
        group.sessions[0].lifecycle,
        Some(sessions.get_session_lifecycle(&ids[0]).await?)
    );
    let second = sessions
        .list_group_with_lifecycle(
            Some("/work".into()),
            ListSessionsOptions {
                cursor: first.next_cursor,
                ..options.clone()
            },
            filter,
        )
        .await?;
    assert_eq!(second.total_count, 2);
    assert_eq!(second.groups[0].sessions[0].summary.session_id, ids[1]);
    assert_eq!(second.next_cursor, None);
    assert_eq!(second.groups[0].next_cursor, None);

    let ungrouped = sessions
        .list_group_with_lifecycle(Some("__none__".into()), options.clone(), filter)
        .await?;
    assert_eq!(ungrouped.total_count, 1);
    assert_eq!(ungrouped.groups[0].cwd, None);
    assert_eq!(ungrouped.groups[0].sessions[0].summary.session_id, ids[4]);

    let browse = sessions
        .browse_with_lifecycle(options.clone(), filter)
        .await?;
    assert_eq!(browse.total_count, 4);
    assert_eq!(browse.groups.len(), 1);
    assert_eq!(browse.groups[0].cwd, None);
    assert_eq!(browse.next_cursor.as_deref(), Some("1"));
    let browse = sessions
        .browse_with_lifecycle(
            ListSessionsOptions {
                cursor: browse.next_cursor,
                ..options.clone()
            },
            filter,
        )
        .await?;
    assert_eq!(browse.total_count, 4);
    assert_eq!(browse.groups[0].cwd.as_deref(), Some("/work"));
    assert_eq!(browse.groups[0].total_count, Some(2));
    assert_eq!(browse.groups[0].sessions.len(), 2);
    assert_eq!(browse.groups[0].latest_activity, group.latest_activity);
    assert_eq!(browse.next_cursor.as_deref(), Some("2"));

    let search = sessions
        .search_with_lifecycle(
            "needle",
            ListSessionsOptions {
                mode: SessionListMode::Browse,
                ..options.clone()
            },
            filter,
        )
        .await?;
    assert_eq!(search.total_count, 2);
    assert_eq!(search.groups.len(), 1);
    assert_eq!(search.groups[0].cwd.as_deref(), Some("/work"));
    assert_eq!(search.groups[0].total_count, Some(2));
    assert_eq!(search.groups[0].latest_activity, group.latest_activity);
    assert_eq!(search.groups[0].sessions[0].summary.session_id, ids[0]);
    assert_eq!(
        search.groups[0].sessions[0].lifecycle,
        group.sessions[0].lifecycle
    );
    assert_eq!(search.next_cursor.as_deref(), Some("1"));
    let search = sessions
        .search_with_lifecycle(
            "needle",
            ListSessionsOptions {
                cursor: search.next_cursor,
                ..options
            },
            filter,
        )
        .await?;
    assert_eq!(search.total_count, 2);
    assert_eq!(search.groups[0].sessions[0].summary.session_id, ids[1]);
    assert_eq!(search.next_cursor, None);

    let empty = sessions
        .list_with_lifecycle(
            ListSessionsOptions {
                mode: SessionListMode::Search,
                ..Default::default()
            },
            filter,
        )
        .await?;
    assert_eq!(empty.total_count, 0);
    assert!(empty.groups.is_empty());
    assert_eq!(empty.next_cursor, None);
    agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn lifecycle_query_children_keep_parent_and_authoritative_state() -> Result<()> {
    let agent = test_agent().await?;
    let store = agent.storage_backend().session_store();
    let parent = store
        .create_session(None, Some("/parent".into()), None, None)
        .await?;
    let mut children = Vec::new();
    for archived in [false, true, false] {
        let child = store
            .create_session(
                None,
                Some("/children".into()),
                Some(parent.public_id.clone()),
                Some(crate::session::domain::ForkOrigin::User),
            )
            .await?;
        store
            .patch_session_lifecycle(
                &child.public_id,
                &SessionLifecyclePatch {
                    resolved: Some(true),
                    archived: Some(archived),
                },
            )
            .await?;
        children.push(child.public_id);
    }
    // An open user fork must not leak through the resolution filter.
    store
        .create_session(
            None,
            Some("/children".into()),
            Some(parent.public_id.clone()),
            Some(crate::session::domain::ForkOrigin::User),
        )
        .await?;
    // The children endpoint retains its existing user-fork semantics, not delegates.
    store
        .create_session(
            None,
            Some("/children".into()),
            Some(parent.public_id.clone()),
            Some(crate::session::domain::ForkOrigin::Delegation),
        )
        .await?;
    let sessions = agent.sessions();
    let filter = SessionLifecycleFilter {
        resolution: ResolutionFilter::Resolved,
        archive: ArchiveFilter::Excluded,
    };
    let first = sessions
        .children_with_lifecycle(&parent.public_id, None, Some(0), filter)
        .await?;
    assert_eq!(first.parent_session_id, parent.public_id);
    assert_eq!(first.total_count, 2);
    assert_eq!(first.sessions.len(), 1, "zero limit is clamped to one");
    assert_eq!(first.next_cursor.as_deref(), Some("1"));
    let second = sessions
        .children_with_lifecycle(&parent.public_id, first.next_cursor, Some(1), filter)
        .await?;
    assert_eq!(second.total_count, 2);
    assert_eq!(second.next_cursor, None);
    let mut listed = Vec::new();
    for row in first.sessions.iter().chain(&second.sessions) {
        assert_eq!(
            row.summary.parent_session_id.as_deref(),
            Some(parent.public_id.as_str())
        );
        assert_eq!(row.summary.fork_origin.as_deref(), Some("user"));
        assert_eq!(row.summary.cwd.as_deref(), Some("/children"));
        assert_eq!(
            row.lifecycle,
            Some(
                sessions
                    .get_session_lifecycle(&row.summary.session_id)
                    .await?
            )
        );
        listed.push(row.summary.session_id.clone());
    }
    listed.sort();
    let mut expected = vec![children[0].clone(), children[2].clone()];
    expected.sort();
    assert_eq!(listed, expected);
    let all = sessions
        .children_with_lifecycle(
            &parent.public_id,
            None,
            None,
            SessionLifecycleFilter::default(),
        )
        .await?;
    assert_eq!(all.total_count, 4);
    let archived = all
        .sessions
        .iter()
        .find(|row| row.summary.session_id == children[1])
        .unwrap();
    assert_eq!(
        archived.lifecycle,
        Some(sessions.get_session_lifecycle(&children[1]).await?)
    );
    assert!(archived.lifecycle.as_ref().unwrap().archived);
    agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn lifecycle_query_metadata_errors_fail_instead_of_fabricating_open_state() -> Result<()> {
    let sqlite =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let agent = test_agent_with_storage(Some(sqlite.clone())).await?;
    let store = agent.storage_backend().session_store();
    store.create_session(None, None, None, None).await?;
    // Leave listing intact but make authoritative lifecycle hydration unavailable.
    sqlite
        .conn_for_test()
        .lock()
        .unwrap()
        .execute("DROP TABLE session_lifecycle", [])?;
    let error = agent
        .sessions()
        .list_with_lifecycle(
            ListSessionsOptions::default(),
            SessionLifecycleFilter::default(),
        )
        .await
        .expect_err("metadata backend failures must not become known-open rows");
    assert!(error.to_string().contains("session_lifecycle"));
    let error = agent
        .sessions()
        .list_for_acp(AcpListSessionsRequest::new())
        .await
        .expect_err("ACP must propagate metadata backend failures too");
    assert!(error.to_string().contains("session_lifecycle"));
    agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn lifecycle_query_unsupported_store_and_invalid_filters_fail_closed() -> Result<()> {
    let sqlite =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let agent = test_agent_with_backend(Arc::new(SplitLifecycleBackend(sqlite))).await?;
    let parent = agent
        .storage_backend()
        .session_store()
        .create_session(None, None, None, None)
        .await?;
    let filter = SessionLifecycleFilter {
        resolution: ResolutionFilter::Open,
        archive: ArchiveFilter::Any,
    };
    let error = agent
        .sessions()
        .children_with_lifecycle(&parent.public_id, None, None, filter)
        .await
        .expect_err("unsupported child filters must not silently list all children");
    assert!(
        error
            .to_string()
            .contains("lifecycle filtering is unsupported")
    );
    let all = agent
        .sessions()
        .list_with_lifecycle(
            ListSessionsOptions::default(),
            SessionLifecycleFilter::default(),
        )
        .await?;
    assert_eq!(all.total_count, 1);
    assert_eq!(
        all.groups[0].sessions[0].summary.session_id,
        parent.public_id
    );
    assert_eq!(all.groups[0].sessions[0].lifecycle, None);
    let acp = agent
        .sessions()
        .list_for_acp(AcpListSessionsRequest::new())
        .await?;
    assert_eq!(acp.sessions.len(), 1);
    assert!(
        acp.sessions[0]
            .meta
            .as_ref()
            .unwrap()
            .get("querymt")
            .and_then(|value| value.get("lifecycle"))
            .is_none()
    );
    for (querymt, expected_error) in [
        (serde_json::json!(false), "_meta.querymt must be an object"),
        (
            serde_json::json!({ "sessionFilters": { "archive": "invalid" } }),
            "invalid _meta.querymt.sessionFilters",
        ),
        (
            serde_json::json!({ "sessionFilters": { "resolved": true } }),
            "invalid _meta.querymt.sessionFilters",
        ),
        (
            serde_json::json!({ "sessionFilters": { "resolution": "open" } }),
            "lifecycle filtering is unsupported",
        ),
    ] {
        let mut meta = serde_json::Map::new();
        meta.insert("querymt".into(), querymt);
        let error = agent
            .sessions()
            .list_for_acp(AcpListSessionsRequest::new().meta(meta))
            .await
            .expect_err("malformed or unsupported filters must not be ignored");
        assert!(error.to_string().contains(expected_error), "{error}");
    }
    agent.shutdown().await;
    Ok(())
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn lifecycle_query_offline_bookmark_metadata_is_unknown_and_filters_reject_merges()
-> Result<()> {
    let agent = test_agent().await?;
    agent
        .storage_backend()
        .session_store()
        .save_remote_session_bookmark(&RemoteSessionBookmark {
            session_id: "offline-owner-session".into(),
            node_id: "offline-node".into(),
            peer_label: "offline-owner".into(),
            cwd: Some("/remote".into()),
            created_at: 1,
            title: None,
        })
        .await?;
    let all = agent
        .sessions()
        .list_with_lifecycle(
            ListSessionsOptions {
                remote: RemoteSessionMode::Bookmarks,
                ..Default::default()
            },
            SessionLifecycleFilter::default(),
        )
        .await?;
    let row = &all.groups[0].sessions[0];
    assert_eq!(row.summary.session_id, "offline-owner-session");
    assert_eq!(row.summary.node_id.as_deref(), Some("offline-node"));
    assert_eq!(
        row.lifecycle, None,
        "an offline bookmark is not a known-open session"
    );
    for remote in [RemoteSessionMode::Bookmarks, RemoteSessionMode::Live] {
        let error = agent
            .sessions()
            .list_with_lifecycle(
                ListSessionsOptions {
                    remote,
                    ..Default::default()
                },
                SessionLifecycleFilter {
                    resolution: ResolutionFilter::Open,
                    archive: ArchiveFilter::Any,
                },
            )
            .await
            .expect_err("merging remote rows cannot implement complete lifecycle filtering");
        assert!(
            error
                .to_string()
                .contains("remote bookmark/live merge is unsupported")
        );
    }
    agent.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn lifecycle_capabilities_require_session_store_and_view_filter_support() -> Result<()> {
    let sqlite =
        Arc::new(crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into()).await?);
    let backend: Arc<dyn StorageBackend> = Arc::new(SplitLifecycleBackend(sqlite));
    let agent = test_agent_with_backend(backend).await?;

    let store = agent.storage_backend().session_store();
    assert!(store.supports_session_lifecycle());
    let view_store = agent
        .storage_backend()
        .view_store()
        .expect("split backend view store");
    assert!(!view_store.supports_session_lifecycle());
    assert!(!view_store.supports_session_lifecycle_filtering());

    let handle = agent.handle();
    let capabilities = crate::control::capabilities::get_capabilities(&handle);
    assert!(
        !capabilities
            .methods
            .iter()
            .any(|method| method == "querymt/session/getLifecycle")
    );
    assert!(
        !capabilities
            .methods
            .iter()
            .any(|method| method == "querymt/session/setLifecycle")
    );

    let response = handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/capabilities",
            Arc::from(serde_json::value::RawValue::from_string("{}".into())?),
        ))
        .await?;
    let extension_capabilities: serde_json::Value = serde_json::from_str(response.0.get())?;
    assert_eq!(
        extension_capabilities["features"]["session_lifecycle"],
        false
    );
    assert!(
        !extension_capabilities["methods"]
            .as_array()
            .expect("methods")
            .iter()
            .any(|method| method == "querymt/session/getLifecycle")
    );

    let filter_error = agent
        .sessions()
        .list_with_lifecycle(
            ListSessionsOptions::default(),
            crate::session::lifecycle::SessionLifecycleFilter {
                resolution: crate::session::lifecycle::ResolutionFilter::Open,
                archive: crate::session::lifecycle::ArchiveFilter::Any,
            },
        )
        .await
        .expect_err("unsupported view lifecycle filters must fail");
    assert!(
        filter_error
            .to_string()
            .contains("lifecycle filtering is unsupported")
    );

    agent.shutdown().await;
    Ok(())
}

// ── Managed worktree workspaces (simple API) ─────────────────────────────────

mod workspace_api {
    use super::*;
    use crate::api::SessionOptions;
    use crate::config::WorkspaceSettings;
    use crate::worktree::WorkspaceTarget;
    use crate::worktree::test_support::{commit_files, init_repo, materialize};

    struct Fixture {
        agent: Agent,
        repo: tempfile::TempDir,
        storage_root: tempfile::TempDir,
        _plugins: tempfile::TempDir,
    }

    async fn fixture(workspace: Option<WorkspaceSettings>) -> Fixture {
        let repo = init_repo();
        commit_files(repo.path(), "initial", &[("README.md", "hello\n")]);
        materialize(repo.path(), &[("README.md", "hello\n")]);
        let storage_root = tempfile::TempDir::new().unwrap();
        let (registry, plugins) = empty_plugin_registry().unwrap();
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let mut builder = Agent::single()
            .provider("openai", "gpt-4o-mini")
            .cwd(repo.path())
            .infra(AgentInfra {
                plugin_registry: Arc::new(registry),
                storage: Some(storage),
                session_mcp_attachment_source: None,
                event_fanout: None,
            });
        if let Some(mut settings) = workspace {
            settings.storage_root = Some(storage_root.path().to_path_buf());
            builder = builder.workspace(settings);
        }
        Fixture {
            agent: builder.build().await.unwrap(),
            repo,
            storage_root,
            _plugins: plugins,
        }
    }

    /// 21.7: no settings -> no service, no facade, no `/worktree`.
    #[tokio::test]
    async fn agent_without_workspace_settings_has_no_service() {
        let f = fixture(None).await;
        assert!(f.agent.worktrees().is_none());
        assert!(f.agent.handle().config.workspace_service().is_none());
        let err = f
            .agent
            .chat_session_with(SessionOptions::worktree())
            .await
            .err()
            .expect("requested isolation fails closed without a service");
        assert_eq!(err.code(), "workspace_service_unavailable", "{err}");
    }

    /// 21.4: with the service enabled, option-less sessions stay local.
    #[tokio::test]
    async fn option_less_chat_session_stays_local_with_the_service_enabled() {
        let f = fixture(Some(WorkspaceSettings::enabled())).await;
        let worktrees = f.agent.worktrees().expect("service enabled");

        for session in [
            f.agent.chat_session().await.unwrap(),
            f.agent
                .chat_session_with(SessionOptions::default())
                .await
                .unwrap(),
        ] {
            assert!(session.workspace().is_none());
        }
        assert!(worktrees.list().await.unwrap().workspaces.is_empty());
        assert!(
            std::fs::read_dir(f.storage_root.path())
                .unwrap()
                .next()
                .is_none(),
            "no checkout directory is created for local sessions"
        );
    }

    /// 21.9: explicit isolation binds the session to its own checkout and the
    /// management facade operates on it with stable error codes.
    #[tokio::test]
    async fn explicit_worktree_session_and_management_facade() {
        let f = fixture(Some(WorkspaceSettings::enabled())).await;
        let worktrees = f.agent.worktrees().unwrap();

        let session = f
            .agent
            .chat_session_with(SessionOptions::worktree().name("iso"))
            .await
            .unwrap();
        let binding = session.workspace().expect("disclosed binding");
        let workspace_id = binding["workspaceId"].as_str().unwrap().to_string();
        let cwd = std::path::PathBuf::from(binding["sessionCwd"].as_str().unwrap());
        assert_ne!(cwd, f.repo.path());
        assert!(cwd.starts_with(f.storage_root.path()));
        assert_eq!(binding["baseline"], "commit");

        let listed = worktrees.list().await.unwrap();
        assert_eq!(listed.workspaces.len(), 1);
        assert_eq!(
            listed.workspaces[0].owner_session_id.as_deref(),
            Some(session.id())
        );
        assert_eq!(
            worktrees
                .status(&workspace_id)
                .await
                .unwrap()
                .summary
                .workspace_id,
            workspace_id
        );

        // Removal of an actively owned workspace is refused with a stable code.
        let err = worktrees
            .remove(WorkspaceTarget::Workspace(workspace_id.clone()), false)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "workspace_not_archived");
        let err = worktrees.status("ws-unknown").await.unwrap_err();
        assert_eq!(err.code(), "workspace_not_found");
        assert_eq!(
            worktrees.list().await.unwrap().workspaces.len(),
            1,
            "a refused removal loses nothing"
        );

        // The typed `create` operation reuses the agent's cwd by default.
        let created = worktrees
            .create(crate::worktree::WorktreeCreateRequest::default())
            .await
            .unwrap();
        assert_ne!(created.workspace_id, workspace_id);
        assert_eq!(created.baseline_mode, "commit");
    }

    /// A loaded isolated session reports its durable binding; a loaded local
    /// session still reports none.
    #[tokio::test]
    async fn loaded_sessions_report_their_durable_workspace_binding() {
        let f = fixture(Some(WorkspaceSettings::enabled())).await;
        let isolated = f
            .agent
            .chat_session_with(SessionOptions::worktree())
            .await
            .unwrap();
        let local = f.agent.chat_session().await.unwrap();

        let loaded = f.agent.load_session(isolated.id()).await.unwrap();
        let created = isolated.workspace().expect("disclosed at creation");
        let reloaded = loaded.workspace().expect("disclosed on load");
        assert_eq!(reloaded["workspaceId"], created["workspaceId"]);
        assert_eq!(reloaded["sessionCwd"], created["sessionCwd"]);
        assert_eq!(reloaded["baseline"], created["baseline"]);

        let loaded_local = f.agent.load_session(local.id()).await.unwrap();
        assert!(loaded_local.workspace().is_none());

        let with_snapshot = f
            .agent
            .sessions()
            .load_with_snapshot(isolated.id())
            .await
            .unwrap();
        assert_eq!(
            with_snapshot.session.workspace().expect("binding")["workspaceId"],
            created["workspaceId"]
        );
    }

    #[tokio::test]
    async fn contradictory_session_options_are_rejected_before_creation() {
        let f = fixture(Some(WorkspaceSettings::enabled())).await;
        let err = f
            .agent
            .chat_session_with(SessionOptions::local().base_ref("main"))
            .await
            .err()
            .expect("invalid options");
        assert_eq!(err.code(), "invalid_options", "{err}");
        assert!(
            f.agent
                .worktrees()
                .unwrap()
                .list()
                .await
                .unwrap()
                .workspaces
                .is_empty()
        );
    }

    /// 21.2 at build time: an unusable storage root fails `build()`.
    #[tokio::test]
    async fn unusable_storage_root_fails_the_agent_build() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let (registry, _plugins) = empty_plugin_registry().unwrap();
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let err = Agent::single()
            .provider("openai", "gpt-4o-mini")
            .workspace(WorkspaceSettings {
                storage_root: Some(file.path().join("nested")),
                ..WorkspaceSettings::enabled()
            })
            .infra(AgentInfra {
                plugin_registry: Arc::new(registry),
                storage: Some(storage),
                session_mcp_attachment_source: None,
                event_fanout: None,
            })
            .build()
            .await
            .err()
            .expect("build must fail");
        assert!(
            err.to_string().contains("invalid workspace configuration"),
            "{err}"
        );
    }

    /// 21.7: TOML translation threads the section through to the builder.
    #[tokio::test]
    async fn toml_workspace_section_enables_the_service() {
        let dir = tempfile::TempDir::new().unwrap();
        let toml = format!(
            "[agent]\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"\n\n[agent.workspace]\nenabled = true\nstorage_root = {:?}\n",
            dir.path().join("wt")
        );
        let crate::config::Config::Single(config) =
            crate::config::load_config(crate::config::ConfigSource::Toml(toml))
                .await
                .unwrap()
        else {
            panic!("single config");
        };
        let (registry, _plugins) = empty_plugin_registry().unwrap();
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let agent = Agent::from_config(
            *config,
            AgentInfra {
                plugin_registry: Arc::new(registry),
                storage: Some(storage),
                session_mcp_attachment_source: None,
                event_fanout: None,
            },
        )
        .await
        .unwrap();
        assert!(agent.worktrees().is_some());
        assert!(dir.path().join("wt").is_dir());
    }
}

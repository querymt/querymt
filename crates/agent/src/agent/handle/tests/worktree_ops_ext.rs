//! API consistency tests for the `querymt/worktree/*` extension surface:
//! list, status, result, and remove report workspace IDs, paths, baselines,
//! lifecycle state, retained recovery refs, and errors consistently, and
//! every operation fails closed when no workspace service is configured.

use super::*;

async fn native_command(
    handle: &LocalAgentHandle,
    session_id: &str,
    text: &str,
) -> Result<serde_json::Value, crate::acp::protocol::Error> {
    let response = SendAgent::prompt(
        handle,
        crate::acp::protocol::PromptRequest::new(
            session_id.to_owned(),
            vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new(text),
            )],
        ),
    )
    .await?;
    Ok(response.meta.unwrap()["querymt.worktree"].clone())
}

#[tokio::test]
async fn native_worktree_commands_are_session_scoped_and_never_invoke_the_model() {
    let f = worktree_fixture(true).await;
    assert!(
        native_command(&f.handle, "unknown", "/worktree list")
            .await
            .is_err()
    );
    let source = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    let source_id = source.session_id.to_string();
    std::fs::write(f.repo.path().join("README.md"), "parent edits\n").unwrap();
    let created = native_command(
        &f.handle,
        &source_id,
        "/worktree create --include-local-changes child",
    )
    .await
    .unwrap();
    let child_id = created["session_id"].as_str().unwrap();
    let workspace_id = created["workspace_id"].as_str().unwrap();
    assert_ne!(source_id, child_id);
    assert_eq!(created["baseline_mode"], "current_working_tree");
    assert_eq!(
        std::fs::read_to_string(
            std::path::Path::new(created["checkout_path"].as_str().unwrap()).join("README.md")
        )
        .unwrap(),
        "parent edits\n"
    );
    let source_row = f
        .storage
        .session_store()
        .get_session(&source_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source_row.cwd.as_deref(), Some(f.repo.path()));
    assert!(
        f.service
            .registry()
            .get_workspace_by_session(&source_id)
            .await
            .unwrap()
            .is_none()
    );
    let status = native_command(&f.handle, child_id, "/worktree status")
        .await
        .unwrap();
    assert_eq!(status["workspace_id"], workspace_id);
    let result = native_command(&f.handle, child_id, "/worktree result")
        .await
        .unwrap();
    assert_eq!(result["workspace_id"], workspace_id);
    let listed = native_command(&f.handle, &source_id, "/worktree list")
        .await
        .unwrap();
    assert_eq!(listed["workspaces"].as_array().unwrap().len(), 1);
    assert!(
        native_command(
            &f.handle,
            &source_id,
            &format!("/worktree remove {workspace_id}")
        )
        .await
        .is_err()
    );
    for invalid in [
        "/worktree",
        "/worktree list extra",
        "/worktree create --force",
        "/worktree create HEAD name extra",
        "/worktree create --include-local-changes HEAD name",
        "/worktree remove",
        "/worktree remove --force",
    ] {
        assert!(
            native_command(&f.handle, &source_id, invalid)
                .await
                .is_err(),
            "{invalid}"
        );
    }
    f.handle.shutdown().await;
}

#[tokio::test]
async fn managed_session_views_group_repository_without_losing_execution_paths() {
    use crate::session::projection::{SessionScope, ViewStore};
    let f = worktree_fixture(true).await;
    let source = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    let source_id = source.session_id.to_string();
    let first = native_command(&f.handle, &source_id, "/worktree create")
        .await
        .unwrap();
    let second = native_command(&f.handle, &source_id, "/worktree create")
        .await
        .unwrap();
    let view = f.storage.get_session_list_view(None).await.unwrap();
    // Managed sessions group under the cwd they were created from, i.e.
    // together with their source session, not in a repository group.
    let source_group = crate::worktree::normalize_cwd(f.repo.path());
    assert_eq!(view.groups.len(), 1);
    let group = &view.groups[0];
    assert_eq!(group.cwd.as_deref(), Some(source_group.as_str()));
    assert_eq!(group.sessions.len(), 3);
    let managed: Vec<_> = group
        .sessions
        .iter()
        .filter(|s| s.workspace.is_some())
        .collect();
    assert_eq!(managed.len(), 2);
    assert_ne!(
        managed[0].cwd, managed[1].cwd,
        "execution paths stay distinct"
    );
    let a = managed[0].workspace.as_ref().unwrap();
    let b = managed[1].workspace.as_ref().unwrap();
    assert_eq!(a.repository_id, b.repository_id);
    assert_eq!(a.source_cwd, source_group);
    assert_ne!(a.workspace_id, b.workspace_id);
    let (groups, _, total) = f
        .storage
        .browse_session_groups(None, 10, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(total, 3);
    assert_eq!(groups.len(), 1);
    let group_key = group.cwd.clone();
    let (page, count) = f
        .storage
        .list_group_sessions(group_key.clone(), None, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(count, 3);
    assert_eq!(page.sessions.len(), 3);
    let (items, _, count) = f
        .storage
        .list_session_items(group_key, None, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(count, 3);
    assert_eq!(items.iter().filter(|i| i.workspace.is_some()).count(), 2);
    for created in [first, second] {
        let id = created["session_id"].as_str().unwrap();
        let events = f
            .storage
            .event_journal()
            .load_session_stream(id, None, None)
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(&event.kind, crate::events::AgentEventKind::SessionWorkspaceBound { workspace, .. } if workspace.workspace_id == created["workspace_id"].as_str().unwrap())));
    }
    f.handle.shutdown().await;
}

struct WorktreeExtFixture {
    handle: LocalAgentHandle,
    service: std::sync::Arc<crate::worktree::WorkspaceService>,
    storage: std::sync::Arc<crate::session::sqlite_storage::SqliteStorage>,
    repo: tempfile::TempDir,
    llm: Arc<Mutex<MockLlmProvider>>,
    _storage_root: tempfile::TempDir,
    _plugin_dir: tempfile::TempDir,
}

async fn worktree_fixture(configure_service: bool) -> WorktreeExtFixture {
    worktree_fixture_with(configure_service, crate::worktree::BaselineMode::Commit).await
}

async fn worktree_fixture_with(
    configure_service: bool,
    default_baseline: crate::worktree::BaselineMode,
) -> WorktreeExtFixture {
    worktree_fixture_inner(configure_service, default_baseline, false).await
}

/// Journal that delegates to the real one but rejects the handoff
/// announcement, to prove a committed handoff survives an emit failure.
struct AnnouncementRejectingJournal(Arc<dyn crate::session::projection::EventJournal>);

/// Storage whose journal rejects handoff announcements; everything else is the
/// real SQLite backend.
struct AnnouncementRejectingStorage(Arc<crate::session::sqlite_storage::SqliteStorage>);

impl StorageBackend for AnnouncementRejectingStorage {
    fn session_store(&self) -> Arc<dyn SessionStore> {
        self.0.session_store()
    }
    fn event_journal(&self) -> Arc<dyn crate::session::projection::EventJournal> {
        Arc::new(AnnouncementRejectingJournal(self.0.event_journal()))
    }
    fn view_store(&self) -> Option<Arc<dyn crate::session::projection::ViewStore>> {
        self.0.view_store()
    }
    fn workspace_repository(&self) -> Option<Arc<dyn crate::worktree::WorkspaceRepository>> {
        self.0.workspace_repository()
    }
}

#[async_trait::async_trait]
impl crate::session::projection::EventJournal for AnnouncementRejectingJournal {
    async fn append_durable(
        &self,
        event: &crate::session::projection::NewDurableEvent,
    ) -> crate::session::error::SessionResult<crate::events::DurableEvent> {
        if matches!(
            event.kind,
            crate::events::AgentEventKind::WorktreeOwnershipTransferred { .. }
        ) {
            return Err(crate::session::error::SessionError::DatabaseError(
                "simulated announcement failure".into(),
            ));
        }
        self.0.append_durable(event).await
    }
    fn max_session_seq_blocking(&self, session_id: &str) -> i64 {
        self.0.max_session_seq_blocking(session_id)
    }
    async fn load_session_stream(
        &self,
        session_id: &str,
        after_seq: Option<i64>,
        limit: Option<usize>,
    ) -> crate::session::error::SessionResult<Vec<crate::events::DurableEvent>> {
        self.0
            .load_session_stream(session_id, after_seq, limit)
            .await
    }
    async fn load_global_stream(
        &self,
        after_seq: Option<i64>,
        limit: Option<usize>,
    ) -> crate::session::error::SessionResult<Vec<crate::events::DurableEvent>> {
        self.0.load_global_stream(after_seq, limit).await
    }
    async fn delete_session_events_from(
        &self,
        session_id: &str,
        from_seq: i64,
    ) -> crate::session::error::SessionResult<usize> {
        self.0
            .delete_session_events_from(session_id, from_seq)
            .await
    }
    async fn append_durable_from_source(
        &self,
        event: &crate::session::projection::NewDurableEvent,
    ) -> crate::session::error::SessionResult<Option<crate::events::DurableEvent>> {
        self.0.append_durable_from_source(event).await
    }
    async fn latest_source_seq(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> crate::session::error::SessionResult<Option<i64>> {
        self.0.latest_source_seq(session_id, source_node_id).await
    }
    async fn remote_sync_cursor(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> crate::session::error::SessionResult<Option<i64>> {
        self.0.remote_sync_cursor(session_id, source_node_id).await
    }
    async fn advance_remote_sync_cursor(
        &self,
        session_id: &str,
        source_node_id: &str,
        source_seq: i64,
        complete: bool,
    ) -> crate::session::error::SessionResult<()> {
        self.0
            .advance_remote_sync_cursor(session_id, source_node_id, source_seq, complete)
            .await
    }
    async fn load_remote_session_stream(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> crate::session::error::SessionResult<Vec<crate::events::DurableEvent>> {
        self.0
            .load_remote_session_stream(session_id, source_node_id)
            .await
    }
    async fn max_stream_seq(&self, session_id: &str) -> crate::session::error::SessionResult<i64> {
        self.0.max_stream_seq(session_id).await
    }
}

async fn worktree_fixture_inner(
    configure_service: bool,
    default_baseline: crate::worktree::BaselineMode,
    reject_announcements: bool,
) -> WorktreeExtFixture {
    let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
    let llm = provider.clone();
    let shared = SharedLlmProvider {
        inner: provider,
        tools: vec![].into_boxed_slice(),
    };
    let factory = Arc::new(TestProviderFactory::new(shared));
    let (plugin_registry, plugin_dir) = mock_plugin_registry(factory).expect("plugin registry");

    let storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .expect("create sqlite storage"),
    );
    let builder_storage: Arc<dyn StorageBackend> = if reject_announcements {
        Arc::new(AnnouncementRejectingStorage(storage.clone()))
    } else {
        storage.clone()
    };
    let mut builder = AgentConfigBuilder::new(
        Arc::new(plugin_registry),
        builder_storage,
        LLMParams::new().provider("mock").model("mock-model"),
    )
    .with_tool_policy(ToolPolicy::ProviderOnly);

    let repo = crate::worktree::test_support::init_repo();
    crate::worktree::test_support::commit_files(
        repo.path(),
        "initial",
        &[("README.md", "hello\n")],
    );
    crate::worktree::test_support::materialize(repo.path(), &[("README.md", "hello\n")]);

    let storage_root = tempfile::TempDir::new().unwrap();
    let service = Arc::new(
        crate::worktree::WorkspaceService::new(
            Arc::new(crate::worktree::GixRepoOps::new()),
            storage
                .workspace_repository()
                .expect("sqlite storage exposes a workspace repository"),
            storage_root.path(),
        )
        .with_default_baseline(default_baseline),
    );
    if configure_service {
        builder = builder.with_workspace_service(service.clone());
    }
    let handle = LocalAgentHandle::from_config(Arc::new(builder.build()));
    WorktreeExtFixture {
        handle,
        service,
        storage,
        repo,
        llm,
        _storage_root: storage_root,
        _plugin_dir: plugin_dir,
    }
}

fn new_session_request(
    cwd: &std::path::Path,
    session_workspace: Option<serde_json::Value>,
) -> crate::acp::protocol::NewSessionRequest {
    let request = crate::acp::protocol::NewSessionRequest::new(cwd.to_path_buf());
    match session_workspace {
        Some(value) => request.meta(
            serde_json::json!({ "querymt": { "sessionWorkspace": value } })
                .as_object()
                .cloned(),
        ),
        None => request,
    }
}

/// 21.10: workspace support is advertised only when a service is configured.
#[tokio::test]
async fn initialize_advertises_workspace_support_only_with_a_service() {
    use crate::acp::protocol::{InitializeRequest, ProtocolVersion};

    let workspace_meta = |resp: crate::acp::protocol::InitializeResponse| {
        resp.agent_capabilities
            .meta
            .and_then(|meta| meta.get("querymt").cloned())
            .and_then(|querymt| querymt.get("workspace").cloned())
    };

    let without = worktree_fixture(false).await;
    let resp = without
        .handle
        .initialize(InitializeRequest::new(ProtocolVersion::LATEST))
        .await
        .unwrap();
    assert!(workspace_meta(resp).is_none(), "no service, no capability");

    let with = worktree_fixture_with(true, crate::worktree::BaselineMode::CurrentWorkingTree).await;
    let resp = with
        .handle
        .initialize(InitializeRequest::new(ProtocolVersion::LATEST))
        .await
        .unwrap();
    let advertised = workspace_meta(resp).expect("service configured");
    assert_eq!(advertised["supported"], true);
    assert_eq!(advertised["defaultBaseline"], "current_working_tree");
    assert_eq!(advertised["sourceSession"], true);
    assert_eq!(
        advertised["executionModes"],
        serde_json::json!(["local", "worktree"])
    );
}

/// 21.4 + 21.10: an option-less `session/new` stays local with the service
/// enabled (no worktree, no disclosure); an explicit request is isolated and
/// the response discloses the resolved binding.
#[tokio::test]
async fn option_less_session_new_stays_local_and_explicit_request_discloses_binding() {
    let f = worktree_fixture(true).await;

    let local = f
        .handle
        .new_session(new_session_request(f.repo.path(), None))
        .await
        .unwrap();
    assert!(
        local.meta.is_none(),
        "local response carries no workspace meta"
    );
    assert!(
        f.service
            .registry()
            .list_workspaces()
            .await
            .unwrap()
            .is_empty()
    );
    let row = f
        .storage
        .session_store()
        .get_session(&local.session_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.cwd.as_deref(), Some(f.repo.path()));

    let isolated = f
        .handle
        .new_session(new_session_request(
            f.repo.path(),
            Some(serde_json::json!({ "execution": "worktree" })),
        ))
        .await
        .unwrap();
    let disclosed = isolated.meta.as_ref().unwrap()["querymt"]["sessionWorkspace"].clone();
    let record = f
        .service
        .registry()
        .get_workspace_by_session(&isolated.session_id.to_string())
        .await
        .unwrap()
        .expect("explicit request is bound");
    assert_eq!(disclosed["workspaceId"], record.id.as_str());
    assert_eq!(disclosed["baseline"], "commit");
    assert_eq!(disclosed["localChangesExcluded"], true);
    assert_eq!(
        disclosed["sessionCwd"],
        record.session_cwd().display().to_string()
    );
    assert_eq!(
        f.service.registry().list_workspaces().await.unwrap().len(),
        1
    );
    f.handle.shutdown().await;
}

/// 21.3: the configured default baseline fills in only an unmade choice; an
/// explicit request value and an explicit ref win; delegates/forks unaffected.
#[tokio::test]
async fn configured_default_baseline_applies_only_to_unpinned_isolation() {
    let f = worktree_fixture_with(true, crate::worktree::BaselineMode::CurrentWorkingTree).await;
    std::fs::write(f.repo.path().join("README.md"), "dirty\n").unwrap();

    let session = |workspace: serde_json::Value| {
        let handle = &f.handle;
        let cwd = f.repo.path().to_path_buf();
        async move {
            handle
                .new_session(new_session_request(&cwd, Some(workspace)))
                .await
                .unwrap()
                .meta
                .unwrap()["querymt"]["sessionWorkspace"]
                .clone()
        }
    };

    // Unpinned: takes the default and discloses it.
    let unpinned = session(serde_json::json!({ "execution": "worktree" })).await;
    assert_eq!(unpinned["baseline"], "current_working_tree");
    assert_eq!(unpinned["localChangesExcluded"], false);
    // Explicit choices win, including an explicit `false`.
    let pinned =
        session(serde_json::json!({ "execution": "worktree", "baseline": "commit" })).await;
    assert_eq!(pinned["baseline"], "commit");
    let off =
        session(serde_json::json!({ "execution": "worktree", "includeLocalChanges": false })).await;
    assert_eq!(off["baseline"], "commit");
    // An explicit ref resolves to commit mode instead of failing validation.
    let by_ref = session(serde_json::json!({ "execution": "worktree", "baseRef": "HEAD" })).await;
    assert_eq!(by_ref["baseline"], "commit");

    // The typed `create` operation resolves the same way and discloses it.
    let source = f
        .handle
        .new_session(new_session_request(f.repo.path(), None))
        .await
        .unwrap()
        .session_id
        .to_string();
    let created = native_command(&f.handle, &source, "/worktree create")
        .await
        .unwrap();
    assert_eq!(created["baseline_mode"], "current_working_tree");
    assert_eq!(created["local_changes_excluded"], false);
    let by_ref = native_command(&f.handle, &source, "/worktree create HEAD")
        .await
        .unwrap();
    assert_eq!(by_ref["baseline_mode"], "commit");
    f.handle.shutdown().await;
}

/// 21.5: errors carry a stable machine-readable code on the ACP surface.
#[tokio::test]
async fn worktree_errors_carry_stable_codes() {
    let code_of = |err: crate::acp::protocol::Error| {
        err.data
            .and_then(|data| data.get("code").and_then(|c| c.as_str().map(str::to_owned)))
    };
    let without = worktree_fixture(false).await;
    let err = without
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/list",
            raw_params("{}"),
        ))
        .await
        .expect_err("no service");
    assert_eq!(
        code_of(err).as_deref(),
        Some("workspace_service_unavailable")
    );

    let with = worktree_fixture(true).await;
    let err = with
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/status",
            raw_params(r#"{"workspace_id":"ws-unknown"}"#),
        ))
        .await
        .expect_err("unknown workspace");
    assert_eq!(code_of(err).as_deref(), Some("workspace_not_found"));
}

/// Management creation must surface the same stable isolation code as plain
/// `session/new`, not a generic `session_start_failed`.
#[tokio::test]
async fn management_create_preserves_session_creation_error_codes() {
    let code_of = |err: crate::acp::protocol::Error| {
        err.data
            .and_then(|data| data.get("code").and_then(|c| c.as_str().map(str::to_owned)))
    };
    let f = worktree_fixture(true).await;
    let not_a_repo = tempfile::TempDir::new().unwrap();

    let via_session = f
        .handle
        .new_session(new_session_request(
            not_a_repo.path(),
            Some(serde_json::json!({ "execution": "worktree" })),
        ))
        .await
        .expect_err("unsupported layout");
    let session_code = code_of(via_session).expect("session creation carries a code");
    assert_eq!(session_code, "isolation_unsupported_layout");

    let params = serde_json::json!({ "cwd": not_a_repo.path() }).to_string();
    let via_management = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/create",
            raw_params(&params),
        ))
        .await
        .expect_err("unsupported layout");
    assert_eq!(
        code_of(via_management).as_deref(),
        Some(session_code.as_str())
    );
}

#[tokio::test]
async fn local_workspace_support_reflects_target_readiness() {
    use crate::agent::handle::AgentHandle as _;

    // A target without its own workspace service must not claim support:
    // delegation preflight would otherwise provision a checkout that the
    // target then rejects, leaking an unowned Ready allocation.
    let without = worktree_fixture(false).await;
    assert!(!without.handle.supports_local_workspaces());
    let with = worktree_fixture(true).await;
    assert!(with.handle.supports_local_workspaces());
}

#[tokio::test]
async fn worktree_operations_fail_closed_without_a_workspace_service() {
    let f = worktree_fixture(false).await;

    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/list",
            raw_params("{}"),
        ))
        .await
        .expect_err("no workspace service: list must fail closed");
    assert_eq!(
        err.code,
        agent_client_protocol::ErrorCode::InternalError,
        "err: {err:?}"
    );

    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/status",
            raw_params(r#"{"workspace_id":"ws-any"}"#),
        ))
        .await
        .expect_err("no workspace service: status must fail closed");
    assert_eq!(
        err.code,
        agent_client_protocol::ErrorCode::InternalError,
        "err: {err:?}"
    );
}

/// Explicit top-level publication and immutable review: `/worktree publish`
/// and `querymt/worktree/result` expose baseline-relative changed paths and
/// change kinds, captured HEAD distinct from live state, and never republish
/// on inspection.
#[tokio::test]
async fn explicit_publication_exposes_immutable_baseline_relative_review() {
    let f = worktree_fixture(true).await;
    let session = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    let session_id = session.session_id.to_string();
    let created = native_command(
        &f.handle,
        &session_id,
        "/worktree create --include-local-changes review-target",
    )
    .await
    .unwrap();
    let child_id = created["session_id"].as_str().unwrap().to_string();
    let workspace_id = created["workspace_id"].as_str().unwrap().to_string();
    let checkout = std::path::PathBuf::from(created["checkout_path"].as_str().unwrap());
    let baseline = created["baseline_commit"].as_str().unwrap().to_string();

    // Uncommitted top-level work inside the new workspace.
    std::fs::write(checkout.join("README.md"), "published edit\n").unwrap();
    std::fs::write(checkout.join("new.txt"), "new\n").unwrap();

    // Publishing through the session's own binding.
    let published = native_command(&f.handle, &child_id, "/worktree publish")
        .await
        .unwrap();
    let entry = &published["result"];
    let result_commit = entry["result_commit"].as_str().expect("result commit");
    let result_ref = entry["result_ref"].as_str().expect("result ref");
    assert!(result_ref.starts_with("refs/qmt/results/"), "{result_ref}");
    assert_eq!(entry["baseline_commit"].as_str().unwrap(), baseline);
    assert_eq!(entry["captured_head"].as_str().unwrap(), baseline);
    assert_eq!(entry["verification"], "skipped");
    let changed = entry["changed_paths"].as_array().expect("changed paths");
    let pairs: Vec<(String, String)> = changed
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap().to_string(),
                c["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("README.md".to_string(), "modified".to_string()),
            ("new.txt".to_string(), "added".to_string()),
        ],
        "{published:?}"
    );
    assert_eq!(published["baseline_includes_uncommitted_edits"], true);
    assert!(published["integration_warning"].as_str().is_some());

    // Later edits and a moved HEAD never rewrite the retained review.
    std::fs::write(checkout.join("later.txt"), "later\n").unwrap();
    let reviewed = ext_method_json(
        &f.handle,
        "querymt/worktree/result",
        serde_json::json!({ "workspace_id": workspace_id }),
    )
    .await;
    let results = reviewed["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "inspection never republishes: {reviewed:?}"
    );
    assert_eq!(results[0]["result_commit"].as_str().unwrap(), result_commit);
    assert_eq!(results[0]["changed_paths"], entry["changed_paths"]);
    assert!(
        results[0]["changed_paths"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["path"].as_str() != Some("later.txt"))
    );
    assert_eq!(reviewed["dirty"], true, "live state is reported separately");

    // A second explicit publication gets its own create-only ref.
    let second = native_command(&f.handle, &child_id, "/worktree publish")
        .await
        .unwrap();
    let second_ref = second["result"]["result_ref"].as_str().unwrap();
    assert_ne!(second_ref, result_ref);
    let reviewed = ext_method_json(
        &f.handle,
        "querymt/worktree/result",
        serde_json::json!({ "workspace_id": workspace_id }),
    )
    .await;
    let results = reviewed["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let refs: Vec<&str> = results
        .iter()
        .filter_map(|r| r["result_ref"].as_str())
        .collect();
    assert!(
        refs.contains(&result_ref),
        "previous ref retained: {reviewed:?}"
    );
    assert!(refs.contains(&second_ref));
}

#[tokio::test]
async fn worktree_lifecycle_reports_ids_paths_baselines_and_errors_consistently() {
    use crate::worktree::SessionCreationOptions;

    let f = worktree_fixture(true).await;

    let session = f
        .storage
        .session_store()
        .create_session(None, None, None, None)
        .await
        .expect("session row");
    let binding = f
        .service
        .allocate_for_new_session(
            f.repo.path(),
            &SessionCreationOptions::isolated_worktree(crate::worktree::BaselineMode::Commit),
        )
        .await
        .unwrap()
        .expect("isolation allocates");
    f.service
        .bind_owner(&binding.record.id, &session.public_id)
        .await
        .unwrap();

    // list: the workspace ID, path, branch, baseline, and owner are all
    // reported consistently with the durable record.
    let list = ext_method_json(&f.handle, "querymt/worktree/list", serde_json::json!({})).await;
    let workspaces = list["workspaces"].as_array().expect("workspaces array");
    assert_eq!(workspaces.len(), 1);
    let entry = &workspaces[0];
    assert_eq!(
        entry["workspace_id"].as_str().unwrap(),
        binding.record.id.to_string()
    );
    assert_eq!(
        entry["checkout_path"].as_str().unwrap(),
        binding.record.checkout_path.display().to_string()
    );
    assert_eq!(
        entry["branch"].as_str().unwrap(),
        binding.record.branch.as_str()
    );
    assert_eq!(entry["lifecycle"].as_str().unwrap(), "ready");
    assert_eq!(entry["baseline_mode"].as_str().unwrap(), "commit");
    assert!(entry["baseline_commit"].as_str().is_some());
    assert_eq!(
        entry["owner_session_id"].as_str().unwrap(),
        session.public_id
    );

    // status: live Git state for the same workspace.
    let status = ext_method_json(
        &f.handle,
        "querymt/worktree/status",
        serde_json::json!({ "workspace_id": binding.record.id.to_string() }),
    )
    .await;
    assert_eq!(status["lifecycle"].as_str().unwrap(), "ready");
    assert_eq!(status["checkout_exists"], true);
    assert_eq!(status["registered"], true);
    assert_eq!(status["dirty"], false);
    assert_eq!(status["clean_for_removal"], true);

    // result: no captured results yet; absence never implies integration.
    let result = ext_method_json(
        &f.handle,
        "querymt/worktree/result",
        serde_json::json!({ "workspace_id": binding.record.id.to_string() }),
    )
    .await;
    assert_eq!(result["results"].as_array().map(Vec::len), Some(0));

    // remove is refused while the workspace has an active owner.
    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/remove",
            raw_params(&format!(r#"{{"workspace_id":"{}"}}"#, binding.record.id)),
        ))
        .await
        .expect_err("active owner blocks removal");
    assert_eq!(
        err.code,
        agent_client_protocol::ErrorCode::InvalidRequest,
        "err: {err:?}"
    );

    // Unknown workspace IDs are rejected consistently.
    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/status",
            raw_params(r#"{"workspace_id":"ws-unknown"}"#),
        ))
        .await
        .expect_err("unknown workspace");
    assert_eq!(
        err.code,
        agent_client_protocol::ErrorCode::InvalidRequest,
        "err: {err:?}"
    );

    // Archive inactive execution rather than deleting the session or binding.
    let archived = ext_method_json(
        &f.handle,
        "querymt/worktree/archive",
        serde_json::json!({ "workspace_id": binding.record.id.to_string() }),
    )
    .await;
    assert_eq!(archived["lifecycle"], "archived");
    assert_eq!(archived["owner_session_id"], session.public_id);
    assert!(
        f.storage
            .get_session(&session.public_id)
            .await
            .unwrap()
            .is_some()
    );
    let removed = ext_method_json(
        &f.handle,
        "querymt/worktree/remove",
        serde_json::json!({ "workspace_id": binding.record.id.to_string() }),
    )
    .await;
    assert_eq!(removed["checkout_removed"], true);
    let retained = removed["retained_refs"].as_array().expect("retained refs");
    assert!(
        retained
            .iter()
            .any(|r| r.as_str().unwrap_or_default().contains("baselines")),
        "removal retains the baseline ref: {removed:?}"
    );
}

/// Session-scoped recovery: a fork interrupted after the conversation copy
/// stays non-runnable (pending isolation barrier) until
/// `querymt/worktree/recover` is called with its session id; recovery
/// completes provisioning, releases the barrier, and discloses the fresh
/// working state without ever exposing the source checkout.
#[tokio::test]
async fn ext_recover_releases_a_pending_fork_barrier_by_session_id() {
    use crate::session::store::SessionStore as _;

    let f = worktree_fixture(true).await;
    let source = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    let source_id = source.session_id.to_string();
    std::fs::write(f.repo.path().join("README.md"), "parent edits\n").unwrap();

    // Seed one message so the fork has a fork point, then simulate a crash
    // right after the conversation copy: intent persisted, no workspace.
    let seeded = crate::model::AgentMessage {
        id: "msg-1".to_string(),
        session_id: source_id.clone(),
        role: querymt::chat::ChatRole::User,
        parts: vec![crate::model::MessagePart::Text {
            content: "hello".to_string(),
        }],
        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    };
    f.storage
        .session_store()
        .add_message(&source_id, seeded)
        .await
        .unwrap();
    let intent = crate::session::store::PendingForkIsolation {
        source_session_id: source_id.clone(),
        source_cwd: f.repo.path().to_path_buf(),
        options: crate::worktree::WorkspaceCreationOptions {
            execution: crate::worktree::ExecutionMode::Worktree,
            baseline: crate::worktree::BaselineMode::CurrentWorkingTree,
            base_ref: None,
        },
        handoff_workspace_id: None,
    };
    let fork_id = f
        .storage
        .fork_session_with_isolation_intent(
            &source_id,
            "msg-1",
            crate::session::domain::ForkOrigin::User,
            Some(intent),
        )
        .await
        .unwrap();
    assert!(
        f.storage
            .get_pending_fork_isolation(&fork_id)
            .await
            .unwrap()
            .is_some(),
        "the interrupted fork keeps its startup barrier"
    );
    assert!(
        f.storage
            .workspace_repository()
            .unwrap()
            .get_workspace_by_session(&fork_id)
            .await
            .unwrap()
            .is_none(),
        "no workspace was allocated before the crash"
    );

    // Ambiguous or empty requests are rejected.
    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/recover",
            raw_params(r#"{"workspace_id":"ws-1","session_id":"sess-1"}"#),
        ))
        .await
        .expect_err("ambiguous recover request");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);

    // Session-scoped recovery completes provisioning and releases the barrier.
    let recovered = ext_method_json(
        &f.handle,
        "querymt/worktree/recover",
        serde_json::json!({ "session_id": fork_id }),
    )
    .await;
    assert_eq!(recovered["session_id"], fork_id);
    let workspace = &recovered["sessionWorkspace"];
    assert_eq!(workspace["execution"], "worktree");
    assert_eq!(workspace["disclosure"], "fresh-working-state");
    let session_cwd = std::path::PathBuf::from(workspace["sessionCwd"].as_str().unwrap());
    assert_ne!(session_cwd, f.repo.path());
    assert_eq!(
        std::fs::read_to_string(session_cwd.join("README.md")).unwrap(),
        "parent edits\n",
        "the recovered checkout reflects the source's current contents"
    );
    assert!(
        f.storage
            .get_pending_fork_isolation(&fork_id)
            .await
            .unwrap()
            .is_none(),
        "the startup barrier is released"
    );
    let owned = f
        .service
        .registry()
        .get_workspace_by_session(&fork_id)
        .await
        .unwrap()
        .expect("recovery binds the fork as single owner");
    assert_eq!(owned.lifecycle, crate::worktree::WorkspaceLifecycle::Ready);
    // Source row was never moved.
    let source_row = f.storage.get_session(&source_id).await.unwrap().unwrap();
    assert_eq!(source_row.cwd.as_deref(), Some(f.repo.path()));
}

#[tokio::test]
async fn worktree_create_child_is_nested_under_its_source_in_the_same_group() {
    use crate::session::projection::{SessionScope, ViewStore};
    let f = worktree_fixture(true).await;
    let source = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    let source_id = source.session_id.to_string();
    let created = native_command(&f.handle, &source_id, "/worktree create")
        .await
        .unwrap();
    let child_id = created["session_id"].as_str().unwrap().to_string();

    // Lineage is persisted with the session row; it is not delegation.
    let child = f
        .storage
        .session_store()
        .get_session(&child_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        child.fork_origin,
        Some(crate::session::domain::ForkOrigin::Worktree)
    );
    let record = f
        .service
        .registry()
        .get_workspace_by_session(&child_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        record.ownership.parent_session_id.as_deref(),
        Some(source_id.as_str())
    );
    assert_eq!(
        record.source_cwd,
        crate::worktree::normalize_cwd(f.repo.path())
    );

    // Same group as the source, nested and counted, with workspace metadata.
    let view = f.storage.get_session_list_view(None).await.unwrap();
    assert_eq!(view.groups.len(), 1);
    let listed = view.groups[0]
        .sessions
        .iter()
        .find(|s| s.session_id == child_id)
        .unwrap();
    assert_eq!(
        listed.parent_session_id.as_deref(),
        Some(source_id.as_str())
    );
    assert!(listed.workspace.is_some());
    let parent = view.groups[0]
        .sessions
        .iter()
        .find(|s| s.session_id == source_id)
        .unwrap();
    assert_eq!(parent.fork_count, 1);
    assert!(parent.has_children);

    // Scoped listings: forks include it, delegates do not, children page it.
    let (forks, _, _) = f
        .storage
        .list_session_items(None, None, 10, SessionScope::Forks)
        .await
        .unwrap();
    assert!(forks.iter().any(|s| s.session_id == child_id));
    let (delegates, _, _) = f
        .storage
        .list_session_items(None, None, 10, SessionScope::Delegates)
        .await
        .unwrap();
    assert!(delegates.is_empty());
    let (children, total) = f
        .storage
        .list_session_children(source_id.clone(), None, 10)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(children.sessions[0].session_id, child_id);

    // Shared-checkout permission is not granted by lineage.
    let shared = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            std::path::PathBuf::from(created["session_cwd"].as_str().unwrap()),
        ))
        .await;
    assert!(shared.is_err(), "lineage must not grant checkout sharing");
    f.handle.shutdown().await;
}

#[tokio::test]
async fn worktree_create_rejects_unknown_source_before_allocating() {
    let f = worktree_fixture(true).await;
    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/create",
            raw_params(&format!(
                r#"{{"cwd":"{}","source_session_id":"no-such-session"}}"#,
                f.repo.path().display()
            )),
        ))
        .await
        .expect_err("unknown source");
    let code = err
        .data
        .and_then(|d| d.get("code").and_then(|c| c.as_str().map(str::to_owned)));
    assert_eq!(code.as_deref(), Some("invalid_options"));
    assert!(
        f.service
            .registry()
            .list_workspaces()
            .await
            .unwrap()
            .is_empty()
    );
    f.handle.shutdown().await;
}

async fn isolated_session(f: &WorktreeExtFixture, cwd: &std::path::Path) -> String {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "querymt".into(),
        serde_json::json!({ "sessionWorkspace": { "execution": "worktree" } }),
    );
    f.handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(cwd.to_path_buf()).meta(meta))
        .await
        .unwrap()
        .session_id
        .to_string()
}

/// Groups, children, disclosure and search agree across the lineage cases:
/// worktree child nested, isolated root in the source group, nested isolation
/// staying in the original group, archived/removed workspaces not moving,
/// delegates hidden, and local sessions carrying no workspace metadata.
#[tokio::test]
async fn source_cwd_grouping_lineage_and_disclosure_agree_across_views() {
    use crate::session::projection::{SessionScope, ViewStore};
    use crate::worktree::WorkspaceLifecycle as L;

    let f = worktree_fixture(true).await;
    let group = crate::worktree::normalize_cwd(f.repo.path());
    let source_id = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap()
        .session_id
        .to_string();

    // Isolated root without a source sits in the source cwd group.
    let root_id = isolated_session(&f, f.repo.path()).await;
    let registry = f.service.registry();
    let root = registry
        .get_workspace_by_session(&root_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root.source_cwd, group);
    assert_eq!(root.ownership.parent_session_id, None);

    // Nested isolation (from inside the managed checkout) keeps the group.
    let nested_id = isolated_session(&f, &root.session_cwd()).await;
    let nested = registry
        .get_workspace_by_session(&nested_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(nested.source_cwd, group);

    // A worktree child of the source nests under it.
    let created = native_command(&f.handle, &source_id, "/worktree create")
        .await
        .unwrap();
    let child_id = created["session_id"].as_str().unwrap().to_string();

    // A delegate stays hidden from fork/children views.
    let delegate = f
        .storage
        .session_store()
        .create_session(
            None,
            Some(f.repo.path().to_path_buf()),
            Some(source_id.clone()),
            Some(crate::session::domain::ForkOrigin::Delegation),
        )
        .await
        .unwrap();

    let view = f.storage.get_session_list_view(None).await.unwrap();
    assert_eq!(view.groups.len(), 1, "one project group: {view:?}");
    assert_eq!(view.groups[0].cwd.as_deref(), Some(group.as_str()));
    let (children, total) = f
        .storage
        .list_session_children(source_id.clone(), None, 10)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(children.sessions[0].session_id, child_id);
    assert!(
        children
            .sessions
            .iter()
            .all(|s| s.session_id != delegate.public_id)
    );
    let (delegates, _, _) = f
        .storage
        .list_session_items(None, None, 20, SessionScope::Delegates)
        .await
        .unwrap();
    assert_eq!(delegates.len(), 1);

    // ACP list: `children` + parentSessionId pages one parent's children;
    // managed entries carry `querymt.workspace`, local ones do not.
    let view_store: std::sync::Arc<dyn ViewStore> = f.storage.clone();
    let mut meta = serde_json::Map::new();
    meta.insert("sessionScope".into(), "children".into());
    meta.insert(
        "querymt".into(),
        serde_json::json!({ "parentSessionId": source_id }),
    );
    let page = crate::api::AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        crate::acp::protocol::ListSessionsRequest::new().meta(meta),
    )
    .await
    .unwrap();
    assert_eq!(page.sessions.len(), 1);
    let ws = &page.sessions[0].meta.as_ref().unwrap()["querymt"]["workspace"];
    assert_eq!(ws["sourceCwd"], group.as_str());
    assert_eq!(ws["lifecycle"], "ready");
    assert_eq!(ws["ownerSessionId"], child_id.as_str());
    assert_eq!(ws["baselineMode"], "commit");
    assert!(ws["workspaceId"].is_string() && ws["branch"].is_string());
    let all = crate::api::AgentSessions::list_for_acp_from_view_store(
        view_store.clone(),
        crate::acp::protocol::ListSessionsRequest::new(),
    )
    .await
    .unwrap();
    let local = all
        .sessions
        .iter()
        .find(|s| s.session_id.to_string() == source_id)
        .unwrap();
    assert!(
        local
            .meta
            .as_ref()
            .is_none_or(|m| m.get("querymt").is_none()),
        "local sessions carry no workspace metadata"
    );

    // Managed `session/load` discloses the binding; a local load does not.
    let loaded = f
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            child_id.clone(),
            std::path::PathBuf::from(created["session_cwd"].as_str().unwrap()),
        ))
        .await
        .unwrap();
    let disclosed = &loaded.meta.as_ref().unwrap()["querymt"]["sessionWorkspace"];
    assert_eq!(disclosed["workspaceId"], created["workspace_id"]);
    assert_eq!(disclosed["checkoutPath"], created["checkout_path"]);
    let local_load = f
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            source_id.clone(),
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    assert!(
        local_load
            .meta
            .as_ref()
            .is_none_or(|m| m.get("querymt").is_none())
    );

    // Search groups match browse groups.
    for id in [&source_id, &root_id, &nested_id, &child_id] {
        f.storage
            .session_store()
            .set_session_name(id, Some("grpneedle".into()))
            .await
            .unwrap();
    }
    let (found, _, _) = f
        .storage
        .search_sessions("grpneedle".into(), None, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].cwd.as_deref(), Some(group.as_str()));
    let (browsed, _, _) = f
        .storage
        .browse_session_groups(None, 10, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(browsed.len(), 1);
    assert_eq!(browsed[0].cwd, found[0].cwd);

    // Archived and removed workspaces keep their group.
    for (from, to) in [
        (L::Ready, L::Archived),
        (L::Archived, L::Removing),
        (L::Removing, L::Removed),
    ] {
        assert!(
            registry
                .transition_lifecycle(&root.id, from, to)
                .await
                .unwrap()
        );
        let (groups, _, _) = f
            .storage
            .browse_session_groups(None, 10, 10, SessionScope::All)
            .await
            .unwrap();
        assert_eq!(groups.len(), 1, "{to} must not move the session");
        let (_, count) = f
            .storage
            .list_group_sessions(Some(group.clone()), None, 20, SessionScope::All)
            .await
            .unwrap();
        assert_eq!(count, 5);
    }
    let kept = registry.get_workspace(&root.id).await.unwrap().unwrap();
    assert_eq!(kept.source_cwd, group);
    f.handle.shutdown().await;
}

/// 24.2: handoff moves ownership to a fresh session in the same checkout and
/// leaves files, branch and baseline untouched; refusals create nothing.
#[tokio::test]
async fn handoff_moves_ownership_without_touching_the_checkout() {
    use crate::session::store::SessionStore as _;
    use crate::worktree::{WorkspaceTarget, WorktreeOps};

    let f = worktree_fixture(true).await;
    let ops = WorktreeOps::new(f.service.clone());
    let previous = isolated_session(&f, f.repo.path()).await;
    let before = f
        .service
        .registry()
        .get_workspace_by_session(&previous)
        .await
        .unwrap()
        .unwrap();
    std::fs::write(before.session_cwd().join("README.md"), "owner edits\n").unwrap();

    let sessions_before = f.storage.list_sessions().await.unwrap().len();
    let response = ops
        .handoff(
            &f.handle,
            &WorkspaceTarget::Session(previous.clone()),
            Default::default(),
        )
        .await
        .unwrap();

    assert_eq!(response.previous_session_id, previous);
    assert_eq!(response.workspace_id, before.id.to_string());
    assert_eq!(
        response.checkout_path,
        before.checkout_path.display().to_string()
    );
    assert_eq!(response.branch, before.branch.as_str());
    assert!(!response.summary_seeded);
    assert_eq!(
        std::fs::read_to_string(before.session_cwd().join("README.md")).unwrap(),
        "owner edits\n",
        "files are untouched"
    );

    let after = f
        .service
        .registry()
        .get_workspace_by_session(&response.session_id)
        .await
        .unwrap()
        .expect("successor owns the workspace");
    assert_eq!(after.id, before.id);
    assert_eq!(after.baseline, before.baseline);
    assert_eq!(after.branch, before.branch);
    assert!(
        f.service
            .registry()
            .get_workspace_by_session(&previous)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.service
            .registry()
            .transferred_to(&previous)
            .await
            .unwrap()
            .as_deref(),
        Some(response.session_id.as_str())
    );

    // Successor: parent = previous, origin worktree, no copied history, barrier released.
    let row = f
        .storage
        .get_session(&response.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.fork_origin.map(|o| o.to_string()).as_deref(),
        Some("worktree")
    );
    assert_eq!(row.cwd.as_deref(), Some(before.session_cwd().as_path()));
    assert!(
        f.storage
            .get_pending_fork_isolation(&response.session_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        f.storage
            .get_history(&response.session_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.storage.list_sessions().await.unwrap().len(),
        sessions_before + 1
    );

    // The previous owner no longer owns anything: a second handoff is refused
    // with a stable code and creates nothing.
    let err = ops
        .handoff(
            &f.handle,
            &WorkspaceTarget::Session(previous),
            Default::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "owner_transferred");
    assert_eq!(
        f.storage.list_sessions().await.unwrap().len(),
        sessions_before + 1
    );
}

/// 24.2: a checkout with other live execution refuses the handoff and
/// nothing is created or moved.
#[tokio::test]
async fn handoff_is_refused_while_the_checkout_has_other_activity() {
    use crate::session::store::SessionStore as _;
    use crate::worktree::{WorkspaceTarget, WorktreeOps};

    let f = worktree_fixture(true).await;
    let ops = WorktreeOps::new(f.service.clone());
    let owner = isolated_session(&f, f.repo.path()).await;
    let record = f
        .service
        .registry()
        .get_workspace_by_session(&owner)
        .await
        .unwrap()
        .unwrap();

    // A shared delegate (or another process) holds a shared activity lease.
    let _delegate = crate::worktree::gate::CheckoutActivityLease::acquire(
        f.service.storage_root(),
        &record.checkout_path,
        false,
    )
    .unwrap();
    let sessions_before = f.storage.list_sessions().await.unwrap().len();

    let err = ops
        .handoff(
            &f.handle,
            &WorkspaceTarget::Session(owner.clone()),
            Default::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "owner_active");
    assert_eq!(
        f.storage.list_sessions().await.unwrap().len(),
        sessions_before
    );
    let unchanged = f
        .service
        .registry()
        .get_workspace(&record.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        unchanged.ownership.owner_session_id.as_deref(),
        Some(owner.as_str())
    );
    assert_eq!(
        f.service.registry().transferred_to(&owner).await.unwrap(),
        None
    );
}

/// 24.3: crash after the successor row, before the transfer. The successor is
/// non-runnable; recovery completes the transfer, leaving exactly one owner.
#[tokio::test]
async fn handoff_crash_before_transfer_is_completed_by_recovery() {
    use crate::session::store::SessionStore as _;
    use crate::worktree::HandoffHost as _;

    let f = worktree_fixture(true).await;
    let owner = isolated_session(&f, f.repo.path()).await;
    let record = f
        .service
        .registry()
        .get_workspace_by_session(&owner)
        .await
        .unwrap()
        .unwrap();

    // Interrupted right after the barrier session was written.
    let successor = f
        .handle
        .create_pending_successor(&owner, &record, None)
        .await
        .unwrap();
    assert!(
        f.storage
            .get_pending_fork_isolation(&successor)
            .await
            .unwrap()
            .is_some()
    );
    let registry = f.service.registry();
    assert_eq!(
        registry
            .get_workspace_by_session(&owner)
            .await
            .unwrap()
            .unwrap()
            .id,
        record.id,
        "the previous owner still owns the workspace"
    );
    assert!(
        registry
            .get_workspace_by_session(&successor)
            .await
            .unwrap()
            .is_none()
    );

    // The successor never runs unowned: load is refused while pending.
    let load = f
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            successor.clone(),
            record.session_cwd(),
        ))
        .await;
    assert!(load.is_err(), "a pending successor must not load");

    // Explicit recovery completes the transfer.
    f.handle
        .session_materializer
        .recover_pending_fork(&successor)
        .await
        .unwrap();
    assert!(
        f.storage
            .get_pending_fork_isolation(&successor)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        registry
            .get_workspace_by_session(&successor)
            .await
            .unwrap()
            .unwrap()
            .id,
        record.id
    );
    assert!(
        registry
            .get_workspace_by_session(&owner)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        registry.transferred_to(&owner).await.unwrap().as_deref(),
        Some(successor.as_str())
    );
    // Recovery is idempotent: a retry changes nothing and creates no second owner.
    f.handle
        .session_materializer
        .recover_pending_fork(&successor)
        .await
        .unwrap_err();
    assert_eq!(registry.list_workspaces().await.unwrap().len(), 1);
}

/// 24.3: crash after the transfer, before the barrier release. The successor
/// already owns the workspace but stays non-runnable until recovery.
#[tokio::test]
async fn handoff_crash_after_transfer_leaves_a_recoverable_non_runnable_successor() {
    use crate::session::store::SessionStore as _;
    use crate::worktree::HandoffHost as _;

    let f = worktree_fixture(true).await;
    let owner = isolated_session(&f, f.repo.path()).await;
    let registry = f.service.registry();
    let record = registry
        .get_workspace_by_session(&owner)
        .await
        .unwrap()
        .unwrap();

    let successor = f
        .handle
        .create_pending_successor(&owner, &record, None)
        .await
        .unwrap();
    registry
        .transfer_owner(&record.id, &owner, &successor)
        .await
        .unwrap();

    assert!(
        f.storage
            .get_pending_fork_isolation(&successor)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        f.handle
            .load_session(crate::acp::protocol::LoadSessionRequest::new(
                successor.clone(),
                record.session_cwd(),
            ))
            .await
            .is_err(),
        "an owning successor stays non-runnable until the barrier is released"
    );

    f.handle
        .session_materializer
        .recover_pending_fork(&successor)
        .await
        .unwrap();
    assert!(
        f.storage
            .get_pending_fork_isolation(&successor)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        registry
            .get_workspace_by_session(&successor)
            .await
            .unwrap()
            .unwrap()
            .id,
        record.id
    );
    assert_eq!(registry.list_workspaces().await.unwrap().len(), 1);
}

async fn seed_history(f: &WorktreeExtFixture, session_id: &str) {
    f.storage
        .session_store()
        .add_message(
            session_id,
            crate::model::AgentMessage {
                id: format!("seed-{session_id}"),
                session_id: session_id.to_string(),
                role: querymt::chat::ChatRole::User,
                parts: vec![crate::model::MessagePart::Text {
                    content: "please refactor the parser".to_string(),
                }],
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
        )
        .await
        .unwrap();
}

/// 24.4: the default handoff carries no history; `summary` seeds the
/// successor with a summary of the previous conversation.
#[tokio::test]
async fn handoff_summary_is_opt_in_and_seeds_the_successor() {
    use crate::session::store::SessionStore as _;
    use crate::worktree::{HandoffOptions, WorkspaceTarget, WorktreeOps};

    let f = worktree_fixture(true).await;
    f.llm.lock().await.expect_chat().returning(|_| {
        Ok(crate::test_utils::MockChatResponse::text_only("parser refactor in progress").into())
    });
    let ops = WorktreeOps::new(f.service.clone());

    let first = isolated_session(&f, f.repo.path()).await;
    seed_history(&f, &first).await;
    let clean = ops
        .handoff(
            &f.handle,
            &WorkspaceTarget::Session(first),
            HandoffOptions::default(),
        )
        .await
        .unwrap();
    assert!(!clean.summary_seeded);
    assert!(
        f.storage
            .get_history(&clean.session_id)
            .await
            .unwrap()
            .is_empty()
    );

    // Hand off again from the successor, this time with a summary of the
    // (seeded) conversation.
    seed_history(&f, &clean.session_id).await;
    let seeded = ops
        .handoff(
            &f.handle,
            &WorkspaceTarget::Session(clean.session_id.clone()),
            HandoffOptions {
                name: Some("next".into()),
                summary: true,
            },
        )
        .await
        .unwrap();
    assert!(seeded.summary_seeded);
    let history = f.storage.get_history(&seeded.session_id).await.unwrap();
    assert!(
        history.iter().any(|m| m.parts.iter().any(|p| matches!(
            p,
            crate::model::MessagePart::Compaction { summary, .. }
                if summary == "parser refactor in progress"
        ))),
        "the summary is the successor's initial context: {history:?}"
    );
}

/// 24.4: a summarizer failure fails the handoff before the transfer.
#[tokio::test]
async fn handoff_summary_failure_creates_nothing_and_keeps_ownership() {
    use crate::session::store::SessionStore as _;
    use crate::worktree::{HandoffOptions, WorkspaceTarget, WorktreeOps};

    let f = worktree_fixture(true).await;
    f.llm
        .lock()
        .await
        .expect_chat()
        .returning(|_| Err(querymt::error::LLMError::GenericError("boom".into())));
    let ops = WorktreeOps::new(f.service.clone());
    let owner = isolated_session(&f, f.repo.path()).await;
    seed_history(&f, &owner).await;
    let sessions_before = f.storage.list_sessions().await.unwrap().len();

    let err = ops
        .handoff(
            &f.handle,
            &WorkspaceTarget::Session(owner.clone()),
            HandoffOptions {
                name: None,
                summary: true,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "summary_failed");
    assert_eq!(
        f.storage.list_sessions().await.unwrap().len(),
        sessions_before
    );
    assert!(
        f.service
            .registry()
            .get_workspace_by_session(&owner)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        f.service.registry().transferred_to(&owner).await.unwrap(),
        None
    );
}

/// 24.5: native command, ext method and the typed core agree, with stable
/// error codes and capability advertisement.
#[tokio::test]
async fn handoff_surfaces_agree_and_advertise_capabilities() {
    let f = worktree_fixture(true).await;
    let methods = ext_method_json(&f.handle, "querymt/capabilities", serde_json::json!({})).await;
    assert!(
        methods["methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "querymt/worktree/handoff")
    );
    let meta = f.service.capability_meta();
    assert_eq!(meta["handoff"], true);
    assert_eq!(meta["handoffSummary"], true);

    // Native command from the owner session.
    let owner = isolated_session(&f, f.repo.path()).await;
    let via_native = native_command(&f.handle, &owner, "/worktree handoff next")
        .await
        .unwrap();
    assert_eq!(via_native["previous_session_id"], owner.as_str());
    assert_eq!(via_native["summary_seeded"], false);
    let second = via_native["session_id"].as_str().unwrap().to_string();

    // Ext method from the new owner, addressed by workspace id.
    let ws = via_native["workspace_id"].as_str().unwrap();
    let via_ext = ext_method_json(
        &f.handle,
        "querymt/worktree/handoff",
        serde_json::json!({ "workspaceId": ws }),
    )
    .await;
    assert_eq!(via_ext["previous_session_id"], second.as_str());
    assert_eq!(via_ext["workspace_id"], ws);
    assert_eq!(via_ext["checkout_path"], via_native["checkout_path"]);

    // The first owner is no longer an owner: stable code on the ext surface.
    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/handoff",
            raw_params(&serde_json::json!({ "session_id": owner }).to_string()),
        ))
        .await
        .expect_err("previous owner cannot hand off");
    assert_eq!(err.data.unwrap()["code"], "owner_transferred");

    // Malformed native usage is rejected.
    assert!(
        native_command(&f.handle, &second, "/worktree handoff a b")
            .await
            .is_err()
    );
}

/// 24.6: a previous owner replays its history read-only, never materializes
/// again, and every run/mutate entry point refuses with `owner_transferred`.
#[tokio::test]
async fn previous_owner_is_read_only_after_handoff() {
    use crate::worktree::{WorkspaceTarget, WorktreeOps};

    let f = worktree_fixture(true).await;
    let ops = WorktreeOps::new(f.service.clone());
    let previous = isolated_session(&f, f.repo.path()).await;
    seed_history(&f, &previous).await;
    let record = f
        .service
        .registry()
        .get_workspace_by_session(&previous)
        .await
        .unwrap()
        .unwrap();
    let handed = ops
        .handoff(
            &f.handle,
            &WorkspaceTarget::Session(previous.clone()),
            Default::default(),
        )
        .await
        .unwrap();

    let code = |err: &crate::acp::protocol::Error| {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .map(str::to_owned)
    };

    // Load replays and discloses read-only access with the current owner.
    let loaded = f
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            previous.clone(),
            record.session_cwd(),
        ))
        .await
        .unwrap();
    let access = &loaded.meta.as_ref().unwrap()["querymt"]["sessionAccess"];
    assert_eq!(access["mode"], "read_only");
    assert_eq!(access["reason"], "owner_transferred");
    assert_eq!(access["ownerSessionId"], handed.session_id.as_str());
    assert!(
        loaded
            .meta
            .as_ref()
            .unwrap()
            .contains_key("querymt/sessionLoadSnapshot.v1")
            || f.storage.view_store().is_none(),
        "history is replayed through the load snapshot"
    );
    assert!(
        f.handle.registry.lock().await.get(&previous).is_none(),
        "the previous owner never materializes an actor"
    );

    // Prompt (including a native command), queue/steer, undo, redo, fork.
    let prompt = SendAgent::prompt(
        &f.handle,
        crate::acp::protocol::PromptRequest::new(
            previous.clone(),
            vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new("edit a file"),
            )],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(code(&prompt).as_deref(), Some("owner_transferred"));
    assert_eq!(
        code(
            &native_command(&f.handle, &previous, "/worktree list")
                .await
                .unwrap_err()
        )
        .as_deref(),
        Some("owner_transferred")
    );
    for (method, params) in [
        (
            "querymt/session/queue",
            serde_json::json!({"session_id": previous, "prompt": []}),
        ),
        (
            "querymt/session/steer",
            serde_json::json!({"session_id": previous, "prompt": []}),
        ),
        (
            "querymt/session/undo",
            serde_json::json!({"session_id": previous, "message_id": "m"}),
        ),
        (
            "querymt/session/redo",
            serde_json::json!({"session_id": previous}),
        ),
    ] {
        let err = f
            .handle
            .ext_method(crate::acp::protocol::ExtRequest::new(
                method,
                raw_params(&params.to_string()),
            ))
            .await
            .expect_err(method);
        assert_eq!(code(&err).as_deref(), Some("owner_transferred"), "{method}");
    }
    let fork = f
        .handle
        .fork_session(crate::acp::protocol::ForkSessionRequest::new(
            previous.clone(),
            record.session_cwd(),
        ))
        .await
        .unwrap_err();
    assert_eq!(code(&fork).as_deref(), Some("owner_transferred"));

    // Nothing changed: still one workspace owned by the successor, no actor for the old session.
    assert_eq!(
        f.service.registry().list_workspaces().await.unwrap().len(),
        1
    );
    assert!(
        f.service
            .registry()
            .get_workspace_by_session(&previous)
            .await
            .unwrap()
            .is_none()
    );
    assert!(f.handle.registry.lock().await.get(&previous).is_none());

    // The successor remains fully usable (loads as the owner).
    f.handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            handed.session_id.clone(),
            record.session_cwd(),
        ))
        .await
        .unwrap();
}

/// 24.8: native command output is an ephemeral session update on the issuing
/// session's ordered stream, and the response carries its event tip. No
/// bridge route is involved.
#[tokio::test]
async fn native_command_output_is_a_session_update_covered_by_the_event_tip() {
    use crate::events::{AgentEventKind, EventEnvelope};

    let f = worktree_fixture(true).await;
    let session = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap()
        .session_id
        .to_string();
    assert!(
        f.handle.config.session_bridges.lock().unwrap().is_empty(),
        "no bridge route is registered"
    );
    let mut events = f.handle.config.subscribe_events();

    let response = SendAgent::prompt(
        &f.handle,
        crate::acp::protocol::PromptRequest::new(
            session.clone(),
            vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new("/worktree list"),
            )],
        ),
    )
    .await
    .unwrap();

    let meta = response.meta.as_ref().unwrap();
    assert!(meta.contains_key("querymt.worktree"));
    let tip = meta["querymt"]["eventTip"].as_i64().expect("event tip");

    let mut output_seq = None;
    while let Ok(envelope) = events.try_recv() {
        if let EventEnvelope::Ephemeral(event) = &envelope
            && event.session_id == session
            && matches!(event.kind, AgentEventKind::WorktreeCommandOutput { .. })
        {
            output_seq = Some(event.session_seq);
        }
    }
    assert_eq!(output_seq, Some(tip), "the tip is the output event itself");

    // Ephemeral: nothing about the command is journaled.
    let journaled = f
        .storage
        .event_journal()
        .load_session_stream(&session, None, None)
        .await
        .unwrap();
    assert!(
        !journaled
            .iter()
            .any(|event| matches!(event.kind, AgentEventKind::WorktreeCommandOutput { .. }))
    );
    f.handle.shutdown().await;
}

/// 24.9 + 24.10: the handoff is announced durably on the previous owner's
/// stream, and a load of that actor-less session replays its history followed
/// by the announcement, without a configuration update.
#[tokio::test]
async fn handoff_announcement_is_durable_and_replayed_without_an_actor() {
    use crate::acp::protocol::SessionUpdate;
    use crate::events::AgentEventKind;
    use crate::worktree::{WorkspaceTarget, WorktreeOps};

    let f = worktree_fixture(true).await;
    let ops = WorktreeOps::new(f.service.clone());
    let previous = isolated_session(&f, f.repo.path()).await;
    f.handle
        .config
        .emit_event_persisted(
            &previous,
            AgentEventKind::PromptReceived {
                content: "earlier question".into(),
                message_id: Some("m-1".into()),
            },
        )
        .await
        .unwrap();

    let (handed, tip) = ops
        .handoff_announced(
            &f.handle,
            &WorkspaceTarget::Session(previous.clone()),
            Default::default(),
        )
        .await
        .unwrap();
    let tip = tip.expect("announcement recorded");

    let journaled = f
        .storage
        .event_journal()
        .load_session_stream(&previous, None, None)
        .await
        .unwrap();
    let announced = journaled
        .iter()
        .find(|event| {
            matches!(&event.kind, AgentEventKind::WorktreeOwnershipTransferred {
                workspace_id, successor_session_id, summary_seeded: false,
            } if *workspace_id == handed.workspace_id && *successor_session_id == handed.session_id)
        })
        .expect("durable announcement");
    assert_eq!(announced.session_seq, tip);
    assert!(f.handle.registry.lock().await.get(&previous).is_none());

    let (notifications, count) = crate::acp::stdio::replay_loaded_session(
        &f.handle,
        &previous,
        &crate::acp::shared::AcpSessionUpdateCapabilities::default(),
    )
    .await
    .expect("actor-less replay");
    assert!(count >= 2);
    assert!(
        !notifications
            .iter()
            .any(|n| matches!(n.update, SessionUpdate::ConfigOptionUpdate(_)))
    );
    let position =
        |pred: &dyn Fn(&SessionUpdate) -> bool| notifications.iter().position(|n| pred(&n.update));
    let history =
        position(&|u| matches!(u, SessionUpdate::UserMessageChunk(_))).expect("history replayed");
    let handoff = position(&|u| {
        serde_json::to_value(u).is_ok_and(|v| {
            v["_meta"]["querymt"]["worktreeHandoff"]["successorSessionId"]
                == handed.session_id.as_str()
        })
    })
    .expect("handoff update replayed");
    assert!(history < handoff, "history precedes the handoff update");
    f.handle.shutdown().await;
}

/// 24.8: through the real connection dispatcher (event forwarders plus the
/// delivery hold, no bridge route), `/worktree list` output reaches the wire
/// as a session update before the prompt response.
#[tokio::test]
async fn native_command_output_precedes_the_response_on_a_connection() {
    let f = worktree_fixture(true).await;
    let session = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap()
        .session_id
        .to_string();
    let handle = Arc::new(f.handle);
    assert!(handle.config.session_bridges.lock().unwrap().is_empty());
    crate::acp::websocket_tests::assert_prompt_text_updates_precede_response(
        handle.clone(),
        &session,
        "/worktree list",
    )
    .await;
    handle.shutdown().await;
}

/// 24.9: a native `/worktree handoff` is announced to every connection
/// attached to the previous owner, ahead of the issuing connection's response.
#[tokio::test]
async fn handoff_announcement_reaches_every_attached_connection_before_the_response() {
    use crate::acp::websocket::WsServerState;
    use crate::acp::websocket_tests::attach_test_connection;
    use tokio::time::{Duration, timeout};

    let f = worktree_fixture(true).await;
    let owner = isolated_session(&f, f.repo.path()).await;
    let agent = Arc::new(f.handle);
    let state = WsServerState::new(agent.clone());

    let (tx_a, mut rx_a, delivery_a, cancel_a) =
        attach_test_connection(&state, "conn-a", &owner).await;
    let (_tx_b, mut rx_b, _delivery_b, cancel_b) =
        attach_test_connection(&state, "conn-b", &owner).await;

    crate::acp::shared::dispatch_rpc_message_with_context(
        crate::acp::shared::RpcDispatchState {
            agent: agent.clone(),
            session_owners: state.session_owners.clone(),
            pending_elicitations: state.pending_elicitations.clone(),
            conn_id: "conn-a".to_string(),
            tx: tx_a,
        },
        crate::acp::shared::RpcMessage {
            jsonrpc: "2.0".to_string(),
            method: "session/prompt".to_string(),
            params: serde_json::json!({
                "sessionId": owner,
                "prompt": [{"type": "text", "text": "/worktree handoff next"}]
            }),
            id: Some(serde_json::json!(7)),
        },
        crate::acp::shared::RpcDispatchContext {
            delivery: Some(delivery_a),
            ..Default::default()
        },
    )
    .await;

    let is_handoff = |message: &serde_json::Value| {
        message["method"] == "session/update"
            && message["params"]["update"]["_meta"]["querymt"]["worktreeHandoff"]["workspaceId"]
                .is_string()
    };

    let mut saw_update = false;
    loop {
        let raw = timeout(Duration::from_secs(5), rx_a.recv())
            .await
            .expect("response arrives")
            .expect("wire open");
        let message: serde_json::Value = serde_json::from_str(&raw).unwrap();
        if message["id"] == serde_json::json!(7) {
            assert!(message["error"].is_null(), "handoff failed: {message}");
            break;
        }
        saw_update |= is_handoff(&message);
    }
    assert!(saw_update, "handoff update precedes the response");

    let mut other = false;
    while let Ok(Some(raw)) = timeout(Duration::from_secs(2), rx_b.recv()).await {
        let message: serde_json::Value = serde_json::from_str(&raw).unwrap();
        if is_handoff(&message) {
            other = true;
            break;
        }
    }
    assert!(other, "second connection receives the announcement");
    cancel_a.cancel();
    cancel_b.cancel();
    agent.shutdown().await;
}

/// 24.9: an announcement that cannot be recorded never fails the committed
/// handoff; ownership has moved and no tip is reported.
#[tokio::test]
async fn handoff_succeeds_when_the_announcement_cannot_be_recorded() {
    use crate::worktree::{WorkspaceTarget, WorktreeOps};

    let f = worktree_fixture_inner(true, crate::worktree::BaselineMode::Commit, true).await;
    let ops = WorktreeOps::new(f.service.clone());
    let previous = isolated_session(&f, f.repo.path()).await;

    let (handed, tip) = ops
        .handoff_announced(
            &f.handle,
            &WorkspaceTarget::Session(previous.clone()),
            Default::default(),
        )
        .await
        .expect("handoff succeeds despite the emit failure");
    assert_eq!(tip, None, "no announcement was recorded");

    let record = f
        .service
        .registry()
        .get_workspace_by_session(&handed.session_id)
        .await
        .unwrap()
        .expect("successor owns the workspace");
    assert_eq!(record.id.as_str(), handed.workspace_id);
    assert_eq!(
        f.service
            .registry()
            .transferred_to(&previous)
            .await
            .unwrap()
            .as_deref(),
        Some(handed.session_id.as_str())
    );
    f.handle.shutdown().await;
}

/// 24.10: the public load-with-replay entry point works for a transferred
/// previous owner (no actor): history first, then the handoff update, and no
/// configuration update.
#[tokio::test]
async fn load_with_replay_serves_a_previous_owner_without_an_actor() {
    use crate::acp::protocol::SessionUpdate;
    use crate::worktree::{WorkspaceTarget, WorktreeOps};

    let f = worktree_fixture(true).await;
    let ops = WorktreeOps::new(f.service.clone());
    let previous = isolated_session(&f, f.repo.path()).await;
    f.handle
        .config
        .emit_event_persisted(
            &previous,
            crate::events::AgentEventKind::PromptReceived {
                content: "earlier question".into(),
                message_id: Some("m-1".into()),
            },
        )
        .await
        .unwrap();
    let record = f
        .service
        .registry()
        .get_workspace_by_session(&previous)
        .await
        .unwrap()
        .unwrap();
    let handed = ops
        .handoff(
            &f.handle,
            &WorkspaceTarget::Session(previous.clone()),
            Default::default(),
        )
        .await
        .unwrap();

    let cwd = record.session_cwd();
    let handle = Arc::new(f.handle);
    let outcome = crate::acp::load_session_with_replay(
        &handle,
        crate::acp::protocol::LoadSessionRequest::new(previous.clone(), cwd),
    )
    .await
    .expect("read-only load with replay");

    let updates: Vec<_> = outcome.notifications.iter().map(|n| &n.update).collect();
    assert!(
        !updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::ConfigOptionUpdate(_)))
    );
    let history = updates
        .iter()
        .position(|u| matches!(u, SessionUpdate::UserMessageChunk(_)))
        .expect("history replayed");
    let handoff = updates
        .iter()
        .position(|u| {
            serde_json::to_value(u).is_ok_and(|v| {
                v["_meta"]["querymt"]["worktreeHandoff"]["successorSessionId"]
                    == handed.session_id.as_str()
            })
        })
        .expect("handoff update replayed");
    assert!(history < handoff);
    assert!(handle.registry.lock().await.get(&previous).is_none());
    handle.shutdown().await;
}

/// Review F1 + F4: a previous owner stays in its project group with the
/// workspace view disclosing `handedOffTo`, and deleting the successor does
/// not erase the marker that keeps it read-only.
#[tokio::test]
async fn previous_owner_keeps_group_and_handoff_target_and_survives_successor_delete() {
    use crate::session::projection::{SessionScope, ViewStore};
    use crate::worktree::{WorkspaceTarget, WorktreeOps};

    let f = worktree_fixture(true).await;
    let ops = WorktreeOps::new(f.service.clone());
    let previous = isolated_session(&f, f.repo.path()).await;
    let handed = ops
        .handoff(
            &f.handle,
            &WorkspaceTarget::Session(previous.clone()),
            Default::default(),
        )
        .await
        .unwrap();

    let (groups, _, total) = f
        .storage
        .browse_session_groups(None, 10, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(groups.len(), 1, "both owners stay in the source group");
    let (items, _, count) = f
        .storage
        .list_session_items(groups[0].cwd.clone(), None, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(count, 2);
    let old = items.iter().find(|i| i.session_id == previous).unwrap();
    let old_view = old
        .workspace
        .as_ref()
        .expect("previous owner keeps its view");
    assert_eq!(
        old_view.handed_off_to.as_deref(),
        Some(handed.session_id.as_str())
    );
    let new = items
        .iter()
        .find(|i| i.session_id == handed.session_id)
        .unwrap();
    assert_eq!(
        new.workspace.as_ref().unwrap().owner_session_id.as_deref(),
        Some(handed.session_id.as_str())
    );

    // Deleting the successor keeps the previous owner read-only.
    f.storage
        .session_store()
        .delete_session(&handed.session_id)
        .await
        .unwrap();
    assert!(
        f.service
            .registry()
            .transferred_to(&previous)
            .await
            .unwrap()
            .is_some(),
        "the handoff marker outlives the successor"
    );
    f.handle
        .session_materializer
        .ensure_not_transferred(&previous)
        .await
        .unwrap_err();
}

/// Review F3: recovering a handoff successor refuses while the previous
/// owner's checkout has other activity, leaves ownership untouched, and
/// announces the handoff once recovery completes the transfer.
#[tokio::test]
async fn recovery_of_a_pending_successor_respects_owner_activity_and_announces() {
    use crate::events::AgentEventKind;
    use crate::worktree::HandoffHost as _;

    let f = worktree_fixture(true).await;
    let owner = isolated_session(&f, f.repo.path()).await;
    let record = f
        .service
        .registry()
        .get_workspace_by_session(&owner)
        .await
        .unwrap()
        .unwrap();
    let successor = f
        .handle
        .create_pending_successor(&owner, &record, None)
        .await
        .unwrap();

    // Another process is active in the checkout: recovery must refuse.
    let busy = crate::worktree::gate::CheckoutActivityLease::acquire(
        f.service.storage_root(),
        &record.checkout_path,
        false,
    )
    .unwrap();
    // Unload the owner so only the foreign lease can block.
    f.handle.unload_session_if_idle(&owner).await.unwrap();
    let err = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/worktree/recover",
            raw_params(&serde_json::json!({ "session_id": successor }).to_string()),
        ))
        .await
        .expect_err("recovery must not move ownership under activity");
    assert_eq!(err.data.as_ref().unwrap()["code"], "owner_active");
    let registry = f.service.registry();
    assert_eq!(
        registry
            .get_workspace_by_session(&owner)
            .await
            .unwrap()
            .unwrap()
            .id,
        record.id
    );
    drop(busy);

    // Without the foreign activity, recovery completes and announces.
    ext_method_json(
        &f.handle,
        "querymt/worktree/recover",
        serde_json::json!({ "session_id": successor }),
    )
    .await;
    assert!(
        registry
            .get_workspace_by_session(&successor)
            .await
            .unwrap()
            .is_some()
    );
    let announced = f
        .storage
        .event_journal()
        .load_session_stream(&owner, None, None)
        .await
        .unwrap()
        .iter()
        .any(|event| {
            matches!(&event.kind, AgentEventKind::WorktreeOwnershipTransferred {
                successor_session_id, ..
            } if *successor_session_id == successor)
        });
    assert!(announced, "recovery records the missing announcement");
    let bound = f
        .storage
        .event_journal()
        .load_session_stream(&successor, None, None)
        .await
        .unwrap()
        .iter()
        .any(|event| matches!(event.kind, AgentEventKind::SessionWorkspaceBound { .. }));
    assert!(bound, "the successor records its workspace binding");
}

/// Review F7: actor-less replay is reserved for transferred owners; any other
/// session without a runtime ref still fails loudly.
#[tokio::test]
async fn actorless_replay_is_refused_for_sessions_that_never_handed_off() {
    let f = worktree_fixture(true).await;
    let session = isolated_session(&f, f.repo.path()).await;
    f.handle.unload_session_if_idle(&session).await.unwrap();
    assert!(f.handle.registry.lock().await.get(&session).is_none());
    crate::acp::stdio::replay_loaded_session(
        &f.handle,
        &session,
        &crate::acp::shared::AcpSessionUpdateCapabilities::default(),
    )
    .await
    .expect_err("a non-transferred session must have a runtime ref");
}

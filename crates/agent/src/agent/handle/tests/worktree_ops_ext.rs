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
    assert_eq!(view.groups.len(), 2);
    let managed = view
        .groups
        .iter()
        .find(|g| {
            g.cwd
                .as_deref()
                .is_some_and(|key| key.starts_with("repository:"))
        })
        .unwrap();
    assert_eq!(managed.sessions.len(), 2);
    assert_ne!(managed.sessions[0].cwd, managed.sessions[1].cwd);
    let a = managed.sessions[0].workspace.as_ref().unwrap();
    let b = managed.sessions[1].workspace.as_ref().unwrap();
    assert_eq!(a.repository_id, b.repository_id);
    assert_ne!(a.workspace_id, b.workspace_id);
    let (groups, _, total) = f
        .storage
        .browse_session_groups(None, 10, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(total, 3);
    assert_eq!(groups.len(), 2);
    let group_key = managed.cwd.clone();
    let (page, count) = f
        .storage
        .list_group_sessions(group_key.clone(), None, 1, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(count, 2);
    assert!(page.sessions[0].workspace.is_some());
    let (items, _, count) = f
        .storage
        .list_session_items(group_key, None, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(count, 2);
    assert!(items.iter().all(|item| item.workspace.is_some()));
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
    let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
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
    let mut builder = AgentConfigBuilder::new(
        Arc::new(plugin_registry),
        storage.clone(),
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

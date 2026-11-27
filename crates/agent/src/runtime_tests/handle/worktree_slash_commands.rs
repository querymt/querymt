//! Workspace-scoped slash-command execution (F6).
//!
//! Managed sessions advertise and expand slash commands from their own
//! bound checkout — including commands that exist only in the isolated
//! checkout — while global command policy, scripts-disabled behavior,
//! native `/worktree` interception, and legacy local sessions keep their
//! existing behavior. Expansion is exercised through real handle/actor
//! prompt execution, before and after load/resume, not just registry
//! discovery.

use super::*;

struct SlashWorktreeFixture {
    handle: LocalAgentHandle,
    #[allow(dead_code)]
    service: std::sync::Arc<crate::worktree::WorkspaceService>,
    storage: std::sync::Arc<crate::session::sqlite_storage::SqliteStorage>,
    repo: tempfile::TempDir,
    _storage_root: tempfile::TempDir,
    _plugin_dir: tempfile::TempDir,
}

fn workspace_slash_config() -> crate::config::SlashCommandsConfig {
    crate::config::SlashCommandsConfig {
        enabled: true,
        include_global: false,
        include_project: true,
        ..Default::default()
    }
}

fn test_command(name: &str, template: &str) -> crate::slash_commands::SlashCommand {
    crate::slash_commands::SlashCommand {
        arguments: None,
        name: name.to_string(),
        source: crate::slash_commands::SlashCommandSource::Global(std::path::PathBuf::from(
            "/agent-level",
        )),
        path: std::path::PathBuf::from(format!("/agent-level/{name}.md")),
        description: format!("{name} (agent level)"),
        argument_hint: None,
        tags: Vec::new(),
        template: template.to_string(),
        script: None,
        requires_script: false,
    }
}

/// Agent-level registry whose definitions deliberately differ from the
/// checkout's, so a passing test proves the workspace copy was used.
fn agent_level_registry() -> crate::slash_commands::SlashCommandRegistry {
    let mut registry = crate::slash_commands::SlashCommandRegistry::new();
    registry.register(test_command("deploy", "AGENTLEVEL-DEPLOY: $ARGUMENTS"));
    registry.register(test_command("agent-only", "AGENTLEVEL-ONLY: $ARGUMENTS"));
    registry
}

async fn slash_worktree_fixture(
    agent_registry: crate::slash_commands::SlashCommandRegistry,
) -> SlashWorktreeFixture {
    let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
    provider
        .lock()
        .await
        .expect_chat()
        .times(0..)
        .returning(|_| Ok(crate::test_utils::MockChatResponse::text_only("ok").into()));
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
    let builder = AgentConfigBuilder::new(
        Arc::new(plugin_registry),
        storage.clone(),
        LLMParams::new().provider("mock").model("mock-model"),
    )
    .with_tool_policy(ToolPolicy::ProviderOnly)
    .with_slash_command_registry(agent_registry)
    .with_slash_commands_config(Some(workspace_slash_config()));

    let repo = crate::worktree::test_support::init_repo();
    let storage_root = tempfile::TempDir::new().unwrap();
    let service = Arc::new(crate::worktree::WorkspaceService::new(
        Arc::new(crate::worktree::GixRepoOps::new()),
        storage
            .workspace_repository()
            .expect("sqlite storage exposes a workspace repository"),
        storage_root.path(),
    ));
    let builder = builder.with_workspace_service(service.clone());
    let handle = LocalAgentHandle::from_config(Arc::new(builder.build()));
    SlashWorktreeFixture {
        handle,
        service,
        storage,
        repo,
        _storage_root: storage_root,
        _plugin_dir: plugin_dir,
    }
}

async fn prompt(
    handle: &LocalAgentHandle,
    session_id: &str,
    text: &str,
) -> Result<crate::acp::protocol::PromptResponse, crate::acp::protocol::Error> {
    SendAgent::prompt(
        handle,
        crate::acp::protocol::PromptRequest::new(
            session_id.to_owned(),
            vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new(text),
            )],
        ),
    )
    .await
}

/// Latest stored user message for the session. The actor stores the prompt
/// after slash-command expansion, so this reflects what the actor actually
/// executed — not merely what a registry contains.
async fn stored_user_text(f: &SlashWorktreeFixture, session_id: &str) -> String {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match f.storage.session_store().get_history(session_id).await {
            Ok(messages) => {
                if let Some(text) = messages.iter().rev().find_map(|message| {
                    if !matches!(message.role, querymt::chat::ChatRole::User) {
                        return None;
                    }
                    message.parts.iter().find_map(|part| match part {
                        crate::model::MessagePart::Text { content } => Some(content.clone()),
                        crate::model::MessagePart::Prompt { blocks } => Some(
                            blocks
                                .iter()
                                .filter_map(|block| match block {
                                    crate::acp::protocol::ContentBlock::Text(text) => {
                                        Some(text.text.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        _ => None,
                    })
                }) {
                    return text;
                }
            }
            Err(err) => panic!("history lookup failed for {session_id}: {err}"),
        }
        if tokio::time::Instant::now() >= deadline {
            let all = f
                .storage
                .session_store()
                .get_history(session_id)
                .await
                .unwrap_or_default();
            panic!(
                "timed out waiting for the stored user message in {session_id}; messages: {all:?}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

async fn native_command(
    handle: &LocalAgentHandle,
    session_id: &str,
    text: &str,
) -> Result<serde_json::Value, crate::acp::protocol::Error> {
    let response = prompt(handle, session_id, text).await?;
    Ok(response.meta.unwrap()["querymt.worktree"].clone())
}

fn advertised_names(notification: &crate::acp::protocol::SessionNotification) -> Vec<String> {
    serde_json::to_value(notification).expect("serialize catalog")["update"]["availableCommands"]
        .as_array()
        .expect("availableCommands")
        .iter()
        .map(|command| command["name"].as_str().expect("command name").to_string())
        .collect()
}

/// The catalog of a local session on an agent with the workspace service
/// describes `/worktree` as a builtin action that needs input and runs from
/// `session/prompt` only.
#[tokio::test]
async fn worktree_descriptor() {
    let f = slash_worktree_fixture(agent_level_registry()).await;
    let session = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    let catalog = f
        .handle
        .slash_command_catalog(&session.session_id.to_string())
        .await;
    let value = serde_json::to_value(&catalog).expect("serialize catalog");
    let entry = value["update"]["availableCommands"]
        .as_array()
        .expect("availableCommands")
        .iter()
        .find(|command| command["name"] == "worktree")
        .unwrap_or_else(|| panic!("catalog has no worktree: {value}"));
    assert_eq!(
        entry["_meta"]["querymt"],
        serde_json::json!({
            "source": "builtin",
            "effect": "action",
            "input": "required",
            "accepts": ["prompt"],
        })
    );
    f.handle.shutdown().await;
}

/// Two checkouts with differently defined commands expand and advertise
/// their own versions; an isolated-only command is invocable in its
/// checkout and is not consulted from the original checkout. Expansion is
/// verified through actor prompt execution before and after load/resume.
#[tokio::test]
async fn managed_sessions_expand_and_advertise_their_own_checkout_commands() {
    let f = slash_worktree_fixture(agent_level_registry()).await;
    let command = "---\ndescription: Deploy the service\n---\nROOT-DEPLOY: $ARGUMENTS\n";
    crate::worktree::test_support::commit_files(
        f.repo.path(),
        "initial",
        &[
            ("README.md", "hello\n"),
            (".qmt/commands/deploy.md", command),
        ],
    );
    crate::worktree::test_support::materialize(
        f.repo.path(),
        &[
            ("README.md", "hello\n"),
            (".qmt/commands/deploy.md", command),
        ],
    );

    // Legacy local session: the agent-level registry still drives expansion.
    let source = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    let source_id = source.session_id.to_string();
    prompt(&f.handle, &source_id, "/deploy from-source")
        .await
        .unwrap();
    assert!(
        stored_user_text(&f, &source_id)
            .await
            .contains("AGENTLEVEL-DEPLOY: from-source"),
        "legacy local sessions keep the agent-level registry"
    );

    // Isolated child materializes with the workspace registry.
    let created = native_command(
        &f.handle,
        &source_id,
        "/worktree create --include-local-changes w1",
    )
    .await
    .unwrap();
    let child_id = created["session_id"].as_str().unwrap().to_owned();
    let checkout = std::path::PathBuf::from(created["checkout_path"].as_str().unwrap());

    let catalog = f.handle.slash_command_catalog(&child_id).await;
    let names = advertised_names(&catalog);
    assert!(names.contains(&"deploy".to_string()), "names: {names:?}");
    assert!(names.contains(&"worktree".to_string()), "names: {names:?}");
    assert!(
        !names.contains(&"agent-only".to_string()),
        "agent-level commands are not advertised for managed sessions: {names:?}"
    );

    // Actor expansion uses the workspace copy, before any load/resume.
    prompt(&f.handle, &child_id, "/deploy from-child")
        .await
        .unwrap();
    assert!(
        stored_user_text(&f, &child_id)
            .await
            .contains("ROOT-DEPLOY: from-child"),
        "managed expansion must use the checkout's definition"
    );

    // Diverge the child checkout's definition and add an isolated-only
    // command; load/resume re-resolves the registry from the checkout.
    std::fs::write(
        checkout.join(".qmt/commands/deploy.md"),
        "---\ndescription: Deploy the service\n---\nW1-DEPLOY: $ARGUMENTS\n",
    )
    .unwrap();
    std::fs::write(
        checkout.join(".qmt/commands/isolated-only.md"),
        "---\ndescription: Only in the isolated checkout\n---\nISOLATED-ONLY: $ARGUMENTS\n",
    )
    .unwrap();
    f.handle.registry.lock().await.remove(&child_id);
    f.handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            SessionId::from(child_id.clone()),
            std::path::PathBuf::new(),
        ))
        .await
        .unwrap();

    let catalog = f.handle.slash_command_catalog(&child_id).await;
    let names = advertised_names(&catalog);
    assert!(
        names.contains(&"isolated-only".to_string()),
        "the catalog advertises the isolated-only command: {names:?}"
    );

    prompt(&f.handle, &child_id, "/deploy after-resume")
        .await
        .unwrap();
    assert!(
        stored_user_text(&f, &child_id)
            .await
            .contains("W1-DEPLOY: after-resume"),
        "expansion follows the checkout's own definition after load/resume"
    );

    // An isolated-only command is invocable in its checkout...
    prompt(&f.handle, &child_id, "/isolated-only run")
        .await
        .unwrap();
    assert!(
        stored_user_text(&f, &child_id)
            .await
            .contains("ISOLATED-ONLY: run"),
        "the isolated-only command expands in its own checkout"
    );

    // ...and is not consulted from the original checkout.
    prompt(&f.handle, &source_id, "/isolated-only run")
        .await
        .unwrap();
    assert!(
        stored_user_text(&f, &source_id)
            .await
            .contains("/isolated-only run"),
        "the original checkout passes the unknown command through unexpanded"
    );

    // Agent-level commands do not leak into managed expansion either.
    prompt(&f.handle, &child_id, "/agent-only x").await.unwrap();
    assert!(
        stored_user_text(&f, &child_id)
            .await
            .contains("/agent-only x"),
        "agent-level commands are not expanded in managed sessions"
    );

    f.handle.shutdown().await;
}

/// A shared (non-isolated) child runs inside its parent's managed checkout.
/// It does not own the workspace, but its catalog and its prompt expansion
/// must still come from that checkout, not from the agent-level registry.
#[tokio::test]
async fn shared_child_in_a_managed_checkout_uses_the_checkout_commands() {
    let f = slash_worktree_fixture(agent_level_registry()).await;
    let command = "---\ndescription: Deploy the service\n---\nROOT-DEPLOY: $ARGUMENTS\n";
    crate::worktree::test_support::commit_files(
        f.repo.path(),
        "initial",
        &[
            ("README.md", "hello\n"),
            (".qmt/commands/deploy.md", command),
        ],
    );
    crate::worktree::test_support::materialize(
        f.repo.path(),
        &[
            ("README.md", "hello\n"),
            (".qmt/commands/deploy.md", command),
        ],
    );

    let source = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    let source_id = source.session_id.to_string();
    let created = native_command(
        &f.handle,
        &source_id,
        "/worktree create --include-local-changes w1",
    )
    .await
    .unwrap();
    let owner_id = created["session_id"].as_str().unwrap().to_owned();
    let checkout = std::path::PathBuf::from(created["checkout_path"].as_str().unwrap());

    // The checkout's definition diverges from the original's.
    std::fs::write(
        checkout.join(".qmt/commands/deploy.md"),
        "---\ndescription: Deploy the service\n---\nW1-DEPLOY: $ARGUMENTS\n",
    )
    .unwrap();

    let mut meta = serde_json::Map::new();
    meta.insert("parent_session_id".into(), owner_id.clone().into());
    let shared = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(checkout.clone()).meta(meta))
        .await
        .expect("a shared child attaches to its owner's managed checkout");
    let shared_id = shared.session_id.to_string();

    let catalog = f.handle.slash_command_catalog(&shared_id).await;
    let names = advertised_names(&catalog);
    assert!(names.contains(&"deploy".to_string()), "names: {names:?}");
    assert!(
        !names.contains(&"agent-only".to_string()),
        "agent-level commands are not advertised inside a managed checkout: {names:?}"
    );

    prompt(&f.handle, &shared_id, "/deploy from-shared")
        .await
        .unwrap();
    assert!(
        stored_user_text(&f, &shared_id)
            .await
            .contains("W1-DEPLOY: from-shared"),
        "the shared child expands against the checkout it executes in"
    );

    f.handle.shutdown().await;
}

/// An active session advertises exactly the registry it expands prompts
/// with. A command file added to the checkout while the session is running
/// (no reload) must not appear in the catalog while remaining
/// un-invocable, for both the owner and a shared child.
#[tokio::test]
async fn active_session_catalog_and_invocation_share_one_registry() {
    let f = slash_worktree_fixture(agent_level_registry()).await;
    let command = "---\ndescription: Deploy the service\n---\nROOT-DEPLOY: $ARGUMENTS\n";
    crate::worktree::test_support::commit_files(
        f.repo.path(),
        "initial",
        &[
            ("README.md", "hello\n"),
            (".qmt/commands/deploy.md", command),
        ],
    );
    crate::worktree::test_support::materialize(
        f.repo.path(),
        &[
            ("README.md", "hello\n"),
            (".qmt/commands/deploy.md", command),
        ],
    );

    let source = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    let source_id = source.session_id.to_string();
    let created = native_command(
        &f.handle,
        &source_id,
        "/worktree create --include-local-changes w1",
    )
    .await
    .unwrap();
    let owner_id = created["session_id"].as_str().unwrap().to_owned();
    let checkout = std::path::PathBuf::from(created["checkout_path"].as_str().unwrap());

    let mut meta = serde_json::Map::new();
    meta.insert("parent_session_id".into(), owner_id.clone().into());
    let shared = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(checkout.clone()).meta(meta))
        .await
        .expect("a shared child attaches to its owner's managed checkout");
    let shared_id = shared.session_id.to_string();

    // A command appears on disk after both sessions started.
    std::fs::write(
        checkout.join(".qmt/commands/late.md"),
        "---\ndescription: Added mid-session\n---\nLATE: $ARGUMENTS\n",
    )
    .unwrap();

    for session_id in [&owner_id, &shared_id] {
        let catalog = f.handle.slash_command_catalog(session_id).await;
        let names = advertised_names(&catalog);
        assert!(names.contains(&"deploy".to_string()), "names: {names:?}");

        assert!(
            !names.contains(&"late".to_string()),
            "the catalog must not advertise a command the running session cannot \
             expand: {names:?}"
        );
        prompt(&f.handle, session_id, "/late now").await.unwrap();
        assert!(
            stored_user_text(&f, session_id).await.contains("/late now"),
            "an unadvertised command stays unexpanded in the active session"
        );
    }

    // After the runtime is rebuilt both surfaces pick the command up
    // together.
    f.handle.registry.lock().await.remove(&owner_id);
    f.handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            SessionId::from(owner_id.clone()),
            std::path::PathBuf::new(),
        ))
        .await
        .unwrap();
    let catalog = f.handle.slash_command_catalog(&owner_id).await;
    assert!(advertised_names(&catalog).contains(&"late".to_string()));
    prompt(&f.handle, &owner_id, "/late again").await.unwrap();
    assert!(
        stored_user_text(&f, &owner_id)
            .await
            .contains("LATE: again")
    );

    f.handle.shutdown().await;
}

/// Native `/worktree` interception, global command policy, and
/// scripts-disabled behavior are preserved: a checkout command named
/// `worktree` never competes with native dispatch, and script-required
/// commands stay hidden and pass through unexpanded while scripts are off.
#[tokio::test]
async fn native_worktree_interception_and_command_policy_are_preserved() {
    let f = slash_worktree_fixture(agent_level_registry()).await;
    let worktree_cmd = "---\ndescription: Worktree command file\n---\nWORKTREE-CMD: $ARGUMENTS\n";
    let scripted_cmd = "---\ndescription: Needs a script\nrequires-script: true\n---\nBody\n";
    crate::worktree::test_support::commit_files(
        f.repo.path(),
        "initial",
        &[
            ("README.md", "hello\n"),
            (".qmt/commands/worktree.md", worktree_cmd),
            (".qmt/commands/scripted.md", scripted_cmd),
        ],
    );
    crate::worktree::test_support::materialize(
        f.repo.path(),
        &[
            ("README.md", "hello\n"),
            (".qmt/commands/worktree.md", worktree_cmd),
            (".qmt/commands/scripted.md", scripted_cmd),
        ],
    );

    let source = f
        .handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            f.repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    let source_id = source.session_id.to_string();

    // `/worktree` is dispatched natively even though a command file with
    // that name exists; it never reaches the actor's expansion.
    let response = prompt(&f.handle, &source_id, "/worktree list")
        .await
        .unwrap();
    assert!(
        response
            .meta
            .as_ref()
            .is_some_and(|meta| meta.get("querymt.worktree").is_some()),
        "native /worktree dispatch answers the prompt"
    );
    let history = f
        .storage
        .session_store()
        .get_history(&source_id)
        .await
        .unwrap_or_default();
    assert!(
        !history.iter().any(|message| message.parts.iter().any(|part| match part {
            crate::model::MessagePart::Text { content } => content.contains("WORKTREE-CMD"),
            crate::model::MessagePart::Prompt { blocks } => blocks.iter().any(|block| {
                matches!(block, crate::acp::protocol::ContentBlock::Text(text) if text.text.contains("WORKTREE-CMD"))
            }),
            _ => false,
        })),
        "the /worktree prompt must not be expanded as a slash command"
    );

    // Managed session: the checkout's worktree command file is suppressed in
    // favor of the native entry, and script-required commands stay hidden.
    let created = native_command(&f.handle, &source_id, "/worktree create HEAD w2")
        .await
        .unwrap();
    let child_id = created["session_id"].as_str().unwrap().to_owned();
    let catalog = f.handle.slash_command_catalog(&child_id).await;
    let names = advertised_names(&catalog);
    assert_eq!(
        names.iter().filter(|name| *name == "worktree").count(),
        1,
        "the native /worktree entry is advertised exactly once: {names:?}"
    );
    assert!(
        !names.contains(&"scripted".to_string()),
        "script-required commands stay hidden while scripts are disabled: {names:?}"
    );

    // Script-required commands pass through unexpanded at the actor.
    prompt(&f.handle, &child_id, "/scripted run").await.unwrap();
    assert!(
        stored_user_text(&f, &child_id)
            .await
            .contains("/scripted run"),
        "scripts-disabled commands pass through unexpanded"
    );

    // Native interception also applies to managed sessions.
    let response = prompt(&f.handle, &child_id, "/worktree status")
        .await
        .unwrap();
    assert!(
        response
            .meta
            .as_ref()
            .is_some_and(|meta| meta.get("querymt.worktree").is_some()),
        "managed sessions keep native /worktree dispatch"
    );

    f.handle.shutdown().await;
}

/// Profile TOML enabling managed worktrees under `storage_root`.
fn isolated_profile_toml(storage_root: &std::path::Path) -> String {
    format!(
        r#"
[agent]
provider = "test"
model = "test-model"
system = "isolated"

[agent.workspace]
enabled = true
storage_root = "{}"
baseline = "commit"
"#,
        storage_root.display()
    )
}

/// Two profiles share one database: `alpha` (active, no worktrees) and
/// `isolated` (worktrees enabled); the connection's root runtime has no
/// workspace service of its own, as in a server hosting several profiles.
async fn two_profile_fixture(storage_root: &std::path::Path) -> (HandleFixture, tempfile::TempDir) {
    let profile_dir = tempfile::tempdir().expect("profile dir");
    write_profile(profile_dir.path(), "alpha.toml", ALPHA_PROFILE_TOML);
    write_profile(
        profile_dir.path(),
        "isolated.toml",
        &isolated_profile_toml(storage_root),
    );
    let base = HandleFixture::new().await;
    let storage = base.handle.config.storage.clone();
    let f = base
        .with_profile_storage("alpha", profile_dir.path(), storage)
        .await;
    assert!(
        f.handle.config.workspace_service().is_none(),
        "the root runtime has no workspace service"
    );
    (f, profile_dir)
}

fn committed_repo() -> tempfile::TempDir {
    let repo = crate::worktree::test_support::init_repo();
    crate::worktree::test_support::commit_files(repo.path(), "initial", &[("README.md", "hi\n")]);
    crate::worktree::test_support::materialize(repo.path(), &[("README.md", "hi\n")]);
    repo
}

fn new_session_in(
    cwd: &std::path::Path,
    profile_id: &str,
    session_workspace: Option<serde_json::Value>,
) -> crate::acp::protocol::NewSessionRequest {
    let mut querymt = serde_json::json!({ "profile_id": profile_id });
    if let Some(workspace) = session_workspace {
        querymt[crate::worktree::SESSION_WORKSPACE_META_KEY] = workspace;
    }
    crate::acp::protocol::NewSessionRequest::new(cwd.to_path_buf()).meta(
        serde_json::Map::from_iter([("querymt".to_string(), querymt)]),
    )
}

async fn bound_profile(f: &HandleFixture, session_id: &str) -> String {
    f.handle
        .profiles()
        .expect("profiles configured")
        .session_binding(session_id)
        .await
        .unwrap_or_else(|| panic!("session {session_id} has no profile binding"))
        .profile_id
}

/// A session bound to a profile runtime is administered by that runtime:
/// its workspace service creates the checkout, and the new session is bound
/// to the source's profile even when the active profile is another one and
/// the connection's root runtime could not isolate at all. The catalog of
/// such a session advertises `/worktree`, so invoking it must work.
#[tokio::test]
async fn worktree_create_runs_in_the_profile_runtime_hosting_the_session() {
    let repo = committed_repo();
    let storage_root = tempfile::tempdir().expect("storage root");
    let (f, _profiles) = two_profile_fixture(storage_root.path()).await;

    let source = f
        .handle
        .new_session(new_session_in(repo.path(), "isolated", None))
        .await
        .expect("source session");
    let source_id = source.session_id.to_string();
    assert_eq!(bound_profile(&f, &source_id).await, "isolated");
    let names = advertised_names(&f.handle.slash_command_catalog(&source_id).await);
    assert!(
        names.contains(&"worktree".to_string()),
        "the hosting profile advertises /worktree: {names:?}"
    );

    // Native command.
    let created = native_command(&f.handle, &source_id, "/worktree create HEAD w1")
        .await
        .expect("native create answers");
    assert!(created.get("error").is_none(), "create failed: {created}");
    let child_id = created["session_id"].as_str().expect("child id").to_owned();
    assert!(
        std::path::Path::new(created["checkout_path"].as_str().unwrap()).exists(),
        "checkout allocated: {created}"
    );
    assert_eq!(
        bound_profile(&f, &child_id).await,
        "isolated",
        "the child keeps the source session's profile, not the active one"
    );
    let status = native_command(&f.handle, &child_id, "/worktree status")
        .await
        .expect("status answers");
    assert!(status.get("error").is_none(), "status failed: {status}");

    // Extension method used by clients' create dialogs.
    let ext = ext_method_json(
        &f.handle,
        "querymt/worktree/create",
        serde_json::json!({ "cwd": "", "source_session_id": source_id, "name": "w2" }),
    )
    .await;
    let ext_child = ext["session_id"].as_str().expect("ext child id").to_owned();
    assert_eq!(bound_profile(&f, &ext_child).await, "isolated");

    // Management by workspace id reaches the owner's service.
    let list = ext_method_json(&f.handle, "querymt/worktree/list", serde_json::json!({})).await;
    assert_eq!(
        list["workspaces"].as_array().map(Vec::len),
        Some(2),
        "{list}"
    );
    let ext_status = ext_method_json(
        &f.handle,
        "querymt/worktree/status",
        serde_json::json!({ "workspace_id": ext["workspace_id"] }),
    )
    .await;
    assert_eq!(
        ext_status["owner_session_id"],
        ext_child.as_str(),
        "{ext_status}"
    );

    f.handle.shutdown().await;
}

/// A session of a profile without worktrees neither advertises `/worktree`
/// nor administers another runtime's service: it fails closed with the
/// stable code instead of borrowing one.
#[tokio::test]
async fn worktree_commands_fail_closed_in_a_profile_without_worktrees() {
    let repo = committed_repo();
    let storage_root = tempfile::tempdir().expect("storage root");
    let (f, _profiles) = two_profile_fixture(storage_root.path()).await;
    // Load the isolated runtime so a borrowable service exists.
    f.handle
        .new_session(new_session_in(repo.path(), "isolated", None))
        .await
        .expect("isolated session");

    let plain = f
        .handle
        .new_session(new_session_in(repo.path(), "alpha", None))
        .await
        .expect("plain session")
        .session_id
        .to_string();
    let names = advertised_names(&f.handle.slash_command_catalog(&plain).await);
    assert!(!names.contains(&"worktree".to_string()), "names: {names:?}");
    let reply = native_command(&f.handle, &plain, "/worktree create")
        .await
        .expect("native create answers");
    assert_eq!(
        reply["error"]["code"], "workspace_service_unavailable",
        "{reply}"
    );

    f.handle.shutdown().await;
}

/// `session/new` in profile mode isolates in the requested profile and
/// discloses the workspace, like a connection without profiles.
#[tokio::test]
async fn profile_session_new_discloses_its_workspace() {
    let repo = committed_repo();
    let storage_root = tempfile::tempdir().expect("storage root");
    let (f, _profiles) = two_profile_fixture(storage_root.path()).await;

    let response = f
        .handle
        .new_session(new_session_in(
            repo.path(),
            "isolated",
            Some(serde_json::json!({ "execution": "worktree" })),
        ))
        .await
        .expect("isolated session/new");
    let querymt = &response.meta.as_ref().expect("response meta")["querymt"];
    assert_eq!(querymt["profile_id"], "isolated");
    let workspace = &querymt[crate::worktree::SESSION_WORKSPACE_META_KEY];
    assert_eq!(workspace["execution"], "worktree", "{querymt}");
    assert!(
        workspace["workspaceId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{querymt}"
    );

    f.handle.shutdown().await;
}

/// `initialize` advertises worktrees when only a profile enables them, and
/// names the profiles that do.
#[tokio::test]
async fn initialize_advertises_profile_worktree_support() {
    let storage_root = tempfile::tempdir().expect("storage root");
    let (f, _profiles) = two_profile_fixture(storage_root.path()).await;

    let response = f
        .handle
        .initialize(crate::acp::protocol::InitializeRequest::new(
            crate::acp::protocol::ProtocolVersion::LATEST,
        ))
        .await
        .expect("initialize");
    let meta = response.agent_capabilities.meta.expect("capability meta");
    let workspace = &meta["querymt"]["workspace"];
    assert_eq!(workspace["supported"], true, "{workspace}");
    assert_eq!(workspace["defaultBaseline"], "commit");
    assert_eq!(workspace["profiles"], serde_json::json!(["isolated"]));

    f.handle.shutdown().await;
}

/// A worktree owned by a session of a profile runtime is merged by that
/// runtime's service, from the source session (native, by id) and by the
/// extension method alike, even though the connection's root runtime has no
/// workspace service of its own.
#[tokio::test]
async fn worktree_merge_runs_in_the_profile_runtime_hosting_the_worktree() {
    let repo = committed_repo();
    // gix fixtures leave no index: build it so the checkout is clean.
    let status = std::process::Command::new("git")
        .args(["reset", "-q"])
        .current_dir(repo.path())
        .status()
        .expect("run git");
    assert!(status.success());
    let storage_root = tempfile::tempdir().expect("storage root");
    let (f, _profiles) = two_profile_fixture(storage_root.path()).await;

    let source_id = f
        .handle
        .new_session(new_session_in(repo.path(), "isolated", None))
        .await
        .expect("source session")
        .session_id
        .to_string();

    // Native: the source session merges the first worktree by id.
    let created = native_command(&f.handle, &source_id, "/worktree create HEAD w1")
        .await
        .expect("native create answers");
    assert!(created.get("error").is_none(), "create failed: {created}");
    let first_checkout = std::path::PathBuf::from(created["checkout_path"].as_str().unwrap());
    std::fs::write(first_checkout.join("one.txt"), "first\n").unwrap();
    let merged = native_command(
        &f.handle,
        &source_id,
        &format!(
            "/worktree merge {}",
            created["workspace_id"].as_str().unwrap()
        ),
    )
    .await
    .expect("native merge answers");
    assert!(merged.get("error").is_none(), "merge failed: {merged}");
    assert_eq!(merged["outcome"], "merged", "{merged}");
    assert_eq!(merged["changed_paths"][0]["path"], "one.txt");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("one.txt")).unwrap(),
        "first\n"
    );

    // Extension method: a second worktree, merged by workspace id.
    let ext = ext_method_json(
        &f.handle,
        "querymt/worktree/create",
        serde_json::json!({ "cwd": "", "source_session_id": source_id, "name": "w2" }),
    )
    .await;
    let second_checkout = std::path::PathBuf::from(ext["checkout_path"].as_str().unwrap());
    std::fs::write(second_checkout.join("two.txt"), "second\n").unwrap();
    let ext_merged = ext_method_json(
        &f.handle,
        "querymt/worktree/merge",
        serde_json::json!({ "workspace_id": ext["workspace_id"] }),
    )
    .await;
    assert_eq!(ext_merged["outcome"], "merged", "{ext_merged}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("two.txt")).unwrap(),
        "second\n"
    );
    // Two squashed commits on top of the initial one.
    let count = std::process::Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(repo.path())
        .output()
        .expect("run git");
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "3");

    // A profile without worktrees cannot merge another runtime's workspace
    // as its own: the unavailable code is stable.
    let plain = f
        .handle
        .new_session(new_session_in(repo.path(), "alpha", None))
        .await
        .expect("plain session")
        .session_id
        .to_string();
    let reply = native_command(&f.handle, &plain, "/worktree merge")
        .await
        .expect("native merge answers");
    assert_eq!(
        reply["error"]["code"], "workspace_service_unavailable",
        "{reply}"
    );

    f.handle.shutdown().await;
}

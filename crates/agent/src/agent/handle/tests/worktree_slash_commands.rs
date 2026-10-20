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

    let catalog = f.handle.slash_command_catalog(&child_id).await.unwrap();
    let names = advertised_names(&catalog);
    assert!(names.contains(&"/deploy".to_string()), "names: {names:?}");
    assert!(names.contains(&"/worktree".to_string()), "names: {names:?}");
    assert!(
        !names.contains(&"/agent-only".to_string()),
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

    let catalog = f.handle.slash_command_catalog(&child_id).await.unwrap();
    let names = advertised_names(&catalog);
    assert!(
        names.contains(&"/isolated-only".to_string()),
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

    let catalog = f.handle.slash_command_catalog(&shared_id).await.unwrap();
    let names = advertised_names(&catalog);
    assert!(names.contains(&"/deploy".to_string()), "names: {names:?}");
    assert!(
        !names.contains(&"/agent-only".to_string()),
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
    let catalog = f.handle.slash_command_catalog(&child_id).await.unwrap();
    let names = advertised_names(&catalog);
    assert_eq!(
        names.iter().filter(|name| *name == "/worktree").count(),
        1,
        "the native /worktree entry is advertised exactly once: {names:?}"
    );
    assert!(
        !names.contains(&"/scripted".to_string()),
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

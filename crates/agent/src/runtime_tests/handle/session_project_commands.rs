//! A local session reads its project commands from its own folder.
//!
//! The session's project root is the Git root of its working directory, or
//! the folder itself outside Git. The catalog a session advertises and the
//! prompt it expands come from that one registry. Everything here goes
//! through the real handle (`new_session`, `slash_command_catalog`, prompt)
//! on an agent without a workspace service, so these sessions are never
//! managed worktree sessions.

use super::worktree_slash_commands::{
    advertised_names, agent_level_registry, prompt, slash_builder, stored_user_text_in,
};
use super::*;

struct ProjectCommandsFixture {
    handle: Arc<LocalAgentHandle>,
    storage: std::sync::Arc<crate::session::sqlite_storage::SqliteStorage>,
    _plugin_dir: tempfile::TempDir,
}

fn project_slash_config() -> crate::config::SlashCommandsConfig {
    crate::config::SlashCommandsConfig {
        enabled: true,
        include_global: false,
        include_project: true,
        ..Default::default()
    }
}

/// An agent with the given agent-level registry and discovery config and no
/// workspace service.
async fn project_commands_fixture(
    agent_registry: crate::slash_commands::SlashCommandRegistry,
    slash_config: Option<crate::config::SlashCommandsConfig>,
) -> ProjectCommandsFixture {
    let (builder, storage, plugin_dir) = slash_builder(agent_registry, slash_config).await;
    let handle = Arc::new(LocalAgentHandle::from_config(Arc::new(builder.build())));
    ProjectCommandsFixture {
        handle,
        storage,
        _plugin_dir: plugin_dir,
    }
}

/// Write `<root>/.qmt/commands/<name>.md` with `template` as its body.
fn write_command(root: &Path, name: &str, template: &str) {
    let commands = root.join(".qmt/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(
        commands.join(format!("{name}.md")),
        format!("---\ndescription: {name} command\n---\n{template}\n"),
    )
    .unwrap();
}

fn git_init(root: &Path) {
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(root)
        .status()
        .expect("run git init");
    assert!(status.success());
}

async fn open_session(f: &ProjectCommandsFixture, cwd: &Path) -> String {
    f.handle
        .new_session(crate::acp::protocol::NewSessionRequest::new(
            cwd.to_path_buf(),
        ))
        .await
        .unwrap()
        .session_id
        .to_string()
}

async fn catalog_names(f: &ProjectCommandsFixture, session_id: &str) -> Vec<String> {
    advertised_names(&f.handle.slash_command_catalog(session_id).await)
}

/// The `source` the catalog reports for `name`, if it is advertised.
async fn catalog_source(
    f: &ProjectCommandsFixture,
    session_id: &str,
    name: &str,
) -> Option<String> {
    let value = serde_json::to_value(f.handle.slash_command_catalog(session_id).await).unwrap();
    value["update"]["availableCommands"]
        .as_array()
        .expect("availableCommands")
        .iter()
        .find(|command| command["name"] == name)
        .and_then(|command| command["_meta"]["querymt"]["source"].as_str())
        .map(str::to_owned)
}

/// What the actor stored for `text` after expansion.
async fn expanded(f: &ProjectCommandsFixture, session_id: &str, text: &str) -> String {
    prompt(&f.handle, session_id, text).await.unwrap();
    stored_user_text_in(&f.storage, session_id).await
}

#[tokio::test]
async fn session_in_a_plain_folder_sees_its_project_commands() {
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(project_slash_config()),
    )
    .await;
    let folder = tempfile::TempDir::new().unwrap();
    write_command(folder.path(), "explain", "EXPLAIN: $ARGUMENTS");

    let session = open_session(&f, folder.path()).await;

    assert!(
        catalog_names(&f, &session)
            .await
            .contains(&"explain".into())
    );
    assert_eq!(
        catalog_source(&f, &session, "explain").await.as_deref(),
        Some("project")
    );
    assert!(
        expanded(&f, &session, "/explain closures")
            .await
            .contains("EXPLAIN: closures")
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn session_in_a_subfolder_of_a_repository() {
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(project_slash_config()),
    )
    .await;
    let repo = tempfile::TempDir::new().unwrap();
    git_init(repo.path());
    write_command(repo.path(), "deploy", "ROOT-DEPLOY: $ARGUMENTS");
    let sub = repo.path().join("crates/api");
    std::fs::create_dir_all(&sub).unwrap();

    let session = open_session(&f, &sub).await;

    assert!(catalog_names(&f, &session).await.contains(&"deploy".into()));
    assert!(
        expanded(&f, &session, "/deploy now")
            .await
            .contains("ROOT-DEPLOY: now")
    );

    // Outside Git the folders above the session's own are not searched.
    let parent = tempfile::TempDir::new().unwrap();
    write_command(parent.path(), "lint", "LINT: $ARGUMENTS");
    let scratch = parent.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let scratch_session = open_session(&f, &scratch).await;
    assert!(
        !catalog_names(&f, &scratch_session)
            .await
            .contains(&"lint".into())
    );
    assert!(
        expanded(&f, &scratch_session, "/lint now")
            .await
            .contains("/lint now"),
        "a folder outside Git does not borrow its parent's commands"
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn sessions_in_different_folders_differ() {
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(project_slash_config()),
    )
    .await;
    let a = tempfile::TempDir::new().unwrap();
    let b = tempfile::TempDir::new().unwrap();
    write_command(a.path(), "deploy", "A-DEPLOY: $ARGUMENTS");
    write_command(b.path(), "deploy", "B-DEPLOY: $ARGUMENTS");

    let session_a = open_session(&f, a.path()).await;
    let session_b = open_session(&f, b.path()).await;

    assert!(
        expanded(&f, &session_a, "/deploy x")
            .await
            .contains("A-DEPLOY: x")
    );
    assert!(
        expanded(&f, &session_b, "/deploy x")
            .await
            .contains("B-DEPLOY: x")
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn idle_session_reports_the_same_commands_as_a_live_one() {
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(project_slash_config()),
    )
    .await;
    let folder = tempfile::TempDir::new().unwrap();
    write_command(folder.path(), "explain", "EXPLAIN: $ARGUMENTS");
    write_command(folder.path(), "deploy", "DEPLOY: $ARGUMENTS");
    let session = open_session(&f, folder.path()).await;
    let live = catalog_names(&f, &session).await;
    assert!(live.contains(&"explain".into()), "live: {live:?}");

    // No live actor any more: the catalog is re-resolved from the stored cwd.
    f.handle.registry.lock().await.remove(&session);
    let idle = catalog_names(&f, &session).await;

    let mut live_sorted = live;
    let mut idle_sorted = idle;
    live_sorted.sort();
    idle_sorted.sort();
    assert_eq!(idle_sorted, live_sorted);
    f.handle.shutdown().await;
}

#[tokio::test]
async fn agent_level_commands_do_not_leak_into_other_folders() {
    let f = project_commands_fixture(agent_level_registry(), Some(project_slash_config())).await;
    let folder = tempfile::TempDir::new().unwrap();

    let session = open_session(&f, folder.path()).await;

    let names = catalog_names(&f, &session).await;
    assert!(!names.contains(&"agent-only".into()), "names: {names:?}");
    assert!(
        expanded(&f, &session, "/agent-only x")
            .await
            .contains("/agent-only x"),
        "the agent-level command is not expanded in another folder"
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn session_without_a_working_directory_keeps_the_agent_level_commands() {
    let f = project_commands_fixture(agent_level_registry(), Some(project_slash_config())).await;

    // An empty cwd is "no working directory".
    let session = open_session(&f, Path::new("")).await;

    let names = catalog_names(&f, &session).await;
    assert!(names.contains(&"agent-only".into()), "names: {names:?}");
    assert!(
        expanded(&f, &session, "/agent-only x")
            .await
            .contains("AGENTLEVEL-ONLY: x")
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn discovery_switched_off() {
    let disabled = crate::config::SlashCommandsConfig {
        enabled: false,
        ..project_slash_config()
    };
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(disabled),
    )
    .await;
    let folder = tempfile::TempDir::new().unwrap();
    write_command(folder.path(), "explain", "EXPLAIN: $ARGUMENTS");

    let session = open_session(&f, folder.path()).await;

    let names = catalog_names(&f, &session).await;
    assert!(!names.contains(&"explain".into()), "names: {names:?}");
    assert!(
        expanded(&f, &session, "/explain closures")
            .await
            .contains("/explain closures")
    );
    f.handle.shutdown().await;
}

// ── Command files are read without a restart ─────────────────────────────

type Events = tokio::sync::broadcast::Receiver<Arc<querymt_agent_core::events::EventEnvelope>>;

/// How long a test waits for an update that must come.
const UPDATE_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a test waits to be sure an update does not come.
const NO_UPDATE_WITHIN: std::time::Duration = std::time::Duration::from_secs(2);

/// The `(name, source)` of every command in a `SlashCommandsChanged` update.
type CommandList = Vec<(String, Option<String>)>;

fn command_list(commands: &serde_json::Value) -> CommandList {
    commands
        .as_array()
        .expect("the update carries an array")
        .iter()
        .map(|command| {
            (
                command["name"].as_str().expect("command name").to_owned(),
                command["_meta"]["querymt"]["source"]
                    .as_str()
                    .map(str::to_owned),
            )
        })
        .collect()
}

fn has(list: &CommandList, name: &str) -> bool {
    list.iter().any(|(candidate, _)| candidate == name)
}

/// The next `SlashCommandsChanged` of `session_id` that `accept`s, within
/// `within`; `None` when none came.
async fn next_update(
    events: &mut Events,
    session_id: &str,
    within: std::time::Duration,
    accept: impl Fn(&CommandList) -> bool,
) -> Option<CommandList> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let received = tokio::time::timeout_at(deadline, events.recv())
            .await
            .ok()?;
        match received {
            Ok(event) if event.session_id() == session_id => {
                if let AgentEventKind::SlashCommandsChanged { commands } = event.kind() {
                    let list = command_list(commands);
                    if accept(&list) {
                        return Some(list);
                    }
                }
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

async fn expect_update(
    events: &mut Events,
    session_id: &str,
    accept: impl Fn(&CommandList) -> bool,
) -> CommandList {
    next_update(events, session_id, UPDATE_WITHIN, accept)
        .await
        .expect("the session's clients receive the command list update")
}

async fn expect_no_update(events: &mut Events, session_id: &str) {
    let update = next_update(events, session_id, NO_UPDATE_WITHIN, |_| true).await;
    assert!(update.is_none(), "unexpected update: {update:?}");
}

/// Wait until the watcher of `session_id` is live: a probe command written
/// to the project folder is announced.
async fn wait_for_watcher(events: &mut Events, session_id: &str, project: &Path) {
    write_command(project, "probe", "PROBE");
    expect_update(events, session_id, |list| has(list, "probe")).await;
}

/// A global folder with `include_global` on and `global_dir` pointing at it.
fn global_config(global: &Path) -> crate::config::SlashCommandsConfig {
    crate::config::SlashCommandsConfig {
        include_global: true,
        global_dir: Some(global.to_path_buf()),
        ..project_slash_config()
    }
}

/// The agent-level registry as `api/agent.rs` builds it at startup.
fn startup_registry(
    config: &crate::config::SlashCommandsConfig,
) -> crate::slash_commands::SlashCommandRegistry {
    let sources = crate::slash_commands::search_paths_with_global(
        config.global_dir.as_deref(),
        None,
        config.include_global,
        config.include_project,
        &config.paths,
    );
    crate::slash_commands::SlashCommandRegistry::from_sources(&sources, &config.scripts).0
}

/// A fixture whose agent watches `global` the way a built agent does.
async fn global_fixture(global: &Path) -> ProjectCommandsFixture {
    let config = global_config(global);
    let f = project_commands_fixture(startup_registry(&config), Some(config.clone())).await;
    crate::agent::slash_reload::watch_global_commands(&f.handle, global.to_path_buf(), move || {
        startup_registry(&config)
    })
    .await
    .expect("watch the global folder");
    f
}

#[tokio::test]
async fn new_global_command_appears_in_an_open_session() {
    let global = tempfile::TempDir::new().unwrap();
    let f = global_fixture(global.path()).await;
    let folder = tempfile::TempDir::new().unwrap();
    let session = open_session(&f, folder.path()).await;
    let mut events = f.handle.subscribe_events();

    std::fs::write(
        global.path().join("standup.md"),
        "---\ndescription: Daily standup\n---\nSTANDUP: $ARGUMENTS\n",
    )
    .unwrap();

    let list = expect_update(&mut events, &session, |list| has(list, "standup")).await;
    assert!(list.contains(&("standup".to_owned(), Some("global".to_owned()))));
    assert!(
        has(&list, "compact"),
        "built-ins stay in the update: {list:?}"
    );
    assert!(
        expanded(&f, &session, "/standup now")
            .await
            .contains("STANDUP: now")
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn new_project_command_appears_in_an_open_session() {
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(project_slash_config()),
    )
    .await;
    let folder = tempfile::TempDir::new().unwrap();
    let session = open_session(&f, folder.path()).await;
    let mut events = f.handle.subscribe_events();

    write_command(folder.path(), "explain", "EXPLAIN: $ARGUMENTS");

    let list = expect_update(&mut events, &session, |list| has(list, "explain")).await;
    assert!(list.contains(&("explain".to_owned(), Some("project".to_owned()))));
    assert!(
        has(&list, "compact"),
        "built-ins stay in the update: {list:?}"
    );
    assert!(
        expanded(&f, &session, "/explain closures")
            .await
            .contains("EXPLAIN: closures")
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn edited_template_is_used_by_the_next_prompt() {
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(project_slash_config()),
    )
    .await;
    let folder = tempfile::TempDir::new().unwrap();
    write_command(folder.path(), "explain", "FIRST: $ARGUMENTS");
    let session = open_session(&f, folder.path()).await;
    assert!(
        expanded(&f, &session, "/explain a")
            .await
            .contains("FIRST: a")
    );
    // The watcher is live once a change is seen; the edit alone announces
    // nothing, so the next prompt is how it shows.
    write_command(folder.path(), "explain", "SECOND: $ARGUMENTS");

    let deadline = tokio::time::Instant::now() + UPDATE_WITHIN;
    loop {
        if expanded(&f, &session, "/explain b")
            .await
            .contains("SECOND: b")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the edited template was never used"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    f.handle.shutdown().await;
}

#[tokio::test]
async fn deleted_command_disappears() {
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(project_slash_config()),
    )
    .await;
    let folder = tempfile::TempDir::new().unwrap();
    write_command(folder.path(), "explain", "EXPLAIN: $ARGUMENTS");
    let session = open_session(&f, folder.path()).await;
    assert!(
        catalog_names(&f, &session)
            .await
            .contains(&"explain".into())
    );
    let mut events = f.handle.subscribe_events();

    std::fs::remove_file(folder.path().join(".qmt/commands/explain.md")).unwrap();

    let list = expect_update(&mut events, &session, |list| !has(list, "explain")).await;
    assert!(
        has(&list, "compact"),
        "built-ins stay in the update: {list:?}"
    );
    assert!(
        expanded(&f, &session, "/explain x")
            .await
            .contains("/explain x"),
        "a removed command is sent to the model as ordinary text"
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn session_without_a_working_directory_sees_global_edits() {
    let global = tempfile::TempDir::new().unwrap();
    let f = global_fixture(global.path()).await;
    let session = open_session(&f, Path::new("")).await;
    let mut events = f.handle.subscribe_events();

    std::fs::write(
        global.path().join("standup2.md"),
        "---\ndescription: Daily standup\n---\nSTANDUP2: $ARGUMENTS\n",
    )
    .unwrap();

    expect_update(&mut events, &session, |list| has(list, "standup2")).await;
    assert!(
        catalog_names(&f, &session)
            .await
            .contains(&"standup2".into())
    );
    assert!(
        expanded(&f, &session, "/standup2 now")
            .await
            .contains("STANDUP2: now")
    );
    f.handle.shutdown().await;
}

#[tokio::test]
async fn unrelated_file_changes_send_no_update() {
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(project_slash_config()),
    )
    .await;
    let folder = tempfile::TempDir::new().unwrap();
    let session = open_session(&f, folder.path()).await;
    let mut events = f.handle.subscribe_events();
    wait_for_watcher(&mut events, &session, folder.path()).await;

    std::fs::write(folder.path().join("README.md"), "# readme\n").unwrap();
    std::fs::write(
        folder.path().join(".qmt/commands/notes.txt"),
        "not a command",
    )
    .unwrap();
    std::fs::create_dir_all(folder.path().join(".qmt/other")).unwrap();
    std::fs::write(folder.path().join(".qmt/other/explain.md"), "elsewhere").unwrap();

    expect_no_update(&mut events, &session).await;
    f.handle.shutdown().await;
}

#[tokio::test]
async fn configured_paths_are_read_at_startup_only() {
    let extra = tempfile::TempDir::new().unwrap();
    let config = crate::config::SlashCommandsConfig {
        paths: vec![extra.path().to_path_buf()],
        ..project_slash_config()
    };
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(config),
    )
    .await;
    let folder = tempfile::TempDir::new().unwrap();
    let session = open_session(&f, folder.path()).await;
    let mut events = f.handle.subscribe_events();
    wait_for_watcher(&mut events, &session, folder.path()).await;

    std::fs::write(
        extra.path().join("configured.md"),
        "---\ndescription: Configured\n---\nCONFIGURED\n",
    )
    .unwrap();
    expect_no_update(&mut events, &session).await;

    // The next reload of the session's commands lists it.
    write_command(folder.path(), "second", "SECOND");
    let list = expect_update(&mut events, &session, |list| has(list, "second")).await;
    assert!(has(&list, "configured"), "list: {list:?}");
    f.handle.shutdown().await;
}

#[tokio::test]
async fn ignored_commands_folder_is_read_on_open() {
    let f = project_commands_fixture(
        crate::slash_commands::SlashCommandRegistry::new(),
        Some(project_slash_config()),
    )
    .await;
    let repo = tempfile::TempDir::new().unwrap();
    git_init(repo.path());
    std::fs::write(repo.path().join(".gitignore"), ".qmt/\n").unwrap();
    let session = open_session(&f, repo.path()).await;
    let mut events = f.handle.subscribe_events();

    write_command(repo.path(), "explain", "EXPLAIN: $ARGUMENTS");
    expect_no_update(&mut events, &session).await;
    assert!(
        !catalog_names(&f, &session)
            .await
            .contains(&"explain".into())
    );

    // The next open reads the folder.
    f.handle.registry.lock().await.remove(&session);
    f.handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            SessionId::from(session.clone()),
            repo.path().to_path_buf(),
        ))
        .await
        .unwrap();
    assert!(
        catalog_names(&f, &session)
            .await
            .contains(&"explain".into())
    );
    assert!(
        expanded(&f, &session, "/explain now")
            .await
            .contains("EXPLAIN: now")
    );
    f.handle.shutdown().await;
}

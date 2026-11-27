//! Reading slash command files again while sessions are open.
//!
//! A session watches the workspace index of its project root for changes to
//! `<root>/.qmt/commands/*.md`; the agent watches the global command folder.
//! A matching change rebuilds the affected registries: the session's next
//! prompt expands from the new files, and the session's clients get a fresh
//! command list as an ephemeral [`AgentEventKind::SlashCommandsChanged`].

use crate::agent::agent_config::AgentConfig;
use crate::agent::core::SessionRuntime;
use crate::agent::handle::LocalAgentHandle;
use crate::slash_commands::SlashCommandRegistry;
use crate::slash_commands::builtin::BuiltinHost;
use querymt_agent_core::events::AgentEventKind;
use querymt_agent_index::{FileChangeKind, FileChangeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use tokio::sync::broadcast::error::RecvError;

/// Aborts the task it owns when dropped, so a watcher ends with its owner.
pub(crate) struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Whether `changes` touches a command file (`*.md`) under `commands_dir`.
///
/// Anything else, including other files of the same folder, is not a reason
/// to read the folder again.
fn touches_commands(changes: &FileChangeSet, commands_dir: &Path) -> bool {
    let is_command = |path: &Path| {
        path.starts_with(commands_dir) && path.extension().is_some_and(|ext| ext == "md")
    };
    changes.changes.iter().any(|change| {
        is_command(&change.path)
            || matches!(&change.kind, FileChangeKind::Renamed { from } if is_command(from))
    })
}

/// The complete command list a session of `config`'s host advertises for
/// `registry`: user commands followed by the built-ins the host can run.
fn catalog_commands(config: &AgentConfig, registry: &SlashCommandRegistry) -> serde_json::Value {
    let host = BuiltinHost {
        local: true,
        workspace_service: config.workspace_service().is_some(),
    };
    let update = crate::slash_commands::acp::catalog_update(registry, host);
    serde_json::to_value(&update.available_commands).unwrap_or_default()
}

/// Record the command list the session's clients know at materialization
/// (the catalog of the registry it expands with), so a later reload
/// announces only a real change.
pub(crate) fn record_announced(config: &AgentConfig, runtime: &SessionRuntime) {
    let registry = runtime.effective_slash_registry(&config.slash_command_registry);
    *runtime.announced_slash_commands.lock() = catalog_commands(config, &registry);
}

/// Rebuild a session's commands and announce them when the list changed.
///
/// A session with a project root rebuilds its own registry from that root
/// and stores it (so an edited template is used by the next prompt even
/// when the list is unchanged). A session without one follows the
/// agent-level registry. The event is sent only when the advertised list
/// differs from the one the session's clients last heard of.
pub(crate) async fn reload_session(
    config: &AgentConfig,
    runtime: &SessionRuntime,
    session_id: &str,
) {
    let slash_config = config
        .slash_commands_config
        .clone()
        .filter(|slash| slash.enabled);
    let after = match (runtime.slash_project_root.get().cloned(), slash_config) {
        (Some(root), Some(slash_config)) => {
            let rebuilt = tokio::task::spawn_blocking(move || {
                SlashCommandRegistry::registry_for_workspace(&slash_config, &root)
            })
            .await;
            let Ok(rebuilt) = rebuilt else {
                return;
            };
            let rebuilt = Arc::new(rebuilt);
            runtime.slash_registry.store(Some(rebuilt.clone()));
            rebuilt
        }
        _ => config.slash_command_registry.load_full(),
    };
    let commands = catalog_commands(config, &after);
    {
        let mut announced = runtime.announced_slash_commands.lock();
        if *announced == commands {
            return;
        }
        *announced = commands.clone();
    }
    config.emit_event(
        session_id,
        AgentEventKind::SlashCommandsChanged { commands },
    );
}

/// Watch the session's project root for command file changes until the
/// runtime is dropped. Does nothing for a session without a project root.
pub(crate) fn spawn_session_watch(
    config: Arc<AgentConfig>,
    runtime: &Arc<SessionRuntime>,
    session_id: String,
) {
    let Some(root) = runtime.slash_project_root.get().cloned() else {
        return;
    };
    let weak = Arc::downgrade(runtime);
    let task = tokio::spawn(watch_session(config, weak, session_id, root));
    *runtime.slash_watch.lock() = Some(AbortOnDrop(task));
}

async fn watch_session(
    config: Arc<AgentConfig>,
    runtime: Weak<SessionRuntime>,
    session_id: String,
    root: PathBuf,
) {
    // The index reports canonical paths, so it is keyed and filtered by the
    // canonical root.
    let Ok(watch_root) = std::fs::canonicalize(&root) else {
        return;
    };
    let handle = match querymt_agent_index::get_or_create_workspace_with_timeout(
        &config.workspace_manager_actor,
        watch_root,
    )
    .await
    {
        Ok(handle) => handle,
        Err(error) => {
            log::warn!("slash commands: cannot watch {}: {error}", root.display());
            return;
        }
    };
    let commands_dir = handle.file_watcher.root().join(".qmt/commands");
    let mut changes = handle.file_watcher.subscribe_changes();

    // Files may have changed before the subscription existed.
    match runtime.upgrade() {
        Some(runtime) => reload_session(&config, &runtime, &session_id).await,
        None => return,
    }

    loop {
        match changes.recv().await {
            Ok(set) if !touches_commands(&set, &commands_dir) => continue,
            // A lagged receiver lost changes, which may include ours.
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
        let Some(runtime) = runtime.upgrade() else {
            return;
        };
        reload_session(&config, &runtime, &session_id).await;
    }
}

/// Watch the global command folder for the lifetime of `handle`: a matching
/// change replaces the agent-level registry with `rebuild()`'s result and
/// reloads every live local session.
///
/// The folder must exist: it is not created, and one created later is read
/// at the next start.
pub async fn watch_global_commands<F>(
    handle: &Arc<LocalAgentHandle>,
    dir: PathBuf,
    rebuild: F,
) -> Result<(), querymt_agent_index::FileIndexError>
where
    F: Fn() -> SlashCommandRegistry + Send + Sync + 'static,
{
    // Gitignore rules belong to repositories, not to the global folder.
    let config = querymt_agent_index::FileIndexConfig {
        respect_gitignore: false,
        ignore_patterns: Vec::new(),
        ..Default::default()
    };
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let watcher = querymt_agent_index::FileIndexWatcher::with_config(dir, config).await?;
    let mut changes = watcher.subscribe_changes();
    let commands_dir = watcher.root().clone();
    let weak = Arc::downgrade(handle);
    let task = tokio::spawn(async move {
        // Keeps the watcher alive as long as this task runs.
        let _watcher = watcher;
        let rebuild = Arc::new(rebuild);
        loop {
            match changes.recv().await {
                Ok(set) if !touches_commands(&set, &commands_dir) => continue,
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            }
            let Some(handle) = weak.upgrade() else {
                return;
            };
            let rebuild = rebuild.clone();
            let Ok(registry) = tokio::task::spawn_blocking(move || rebuild()).await else {
                continue;
            };
            handle
                .config
                .slash_command_registry
                .store(Arc::new(registry));
            handle.reload_slash_commands().await;
        }
    });
    *handle.slash_global_watch.lock() = Some(AbortOnDrop(task));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt_agent_index::file_index::FileChange;

    fn change(path: &str, kind: FileChangeKind) -> FileChange {
        FileChange {
            path: PathBuf::from(path),
            kind,
            is_dir: false,
        }
    }

    fn set(changes: Vec<FileChange>) -> FileChangeSet {
        FileChangeSet {
            changes,
            timestamp: 0,
        }
    }

    #[test]
    fn only_markdown_files_in_the_commands_folder_count() {
        let dir = Path::new("/p/.qmt/commands");
        let hit =
            |path: &str| touches_commands(&set(vec![change(path, FileChangeKind::Modified)]), dir);
        assert!(hit("/p/.qmt/commands/explain.md"));
        assert!(hit("/p/.qmt/commands/sub/nested.md"));
        assert!(!hit("/p/.qmt/commands/notes.txt"));
        assert!(!hit("/p/.qmt/other/explain.md"));
        assert!(!hit("/p/src/main.rs"));
    }

    #[test]
    fn a_rename_out_of_the_commands_folder_counts() {
        let renamed = FileChangeKind::Renamed {
            from: PathBuf::from("/p/.qmt/commands/old.md"),
        };
        assert!(touches_commands(
            &set(vec![change("/p/elsewhere/old.md", renamed)]),
            Path::new("/p/.qmt/commands"),
        ));
    }
}

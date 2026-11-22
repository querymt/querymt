use anyhow::{Context, Result, anyhow};
use querymt_agent_core::config::workspace::WorkspaceSettings;
use std::path::{Path, PathBuf};

/// The effective, absolute storage root of managed worktrees: the configured
/// `storage_root` (with a leading `~/` expanded), or `<qmt config dir>/worktrees`.
pub fn resolved_storage_root(settings: &WorkspaceSettings) -> Result<PathBuf> {
    let root = match &settings.storage_root {
        Some(root) => expand_tilde(root)?,
        None => querymt_utils::providers::config_dir()?.join("worktrees"),
    };
    std::path::absolute(&root)
        .with_context(|| format!("cannot resolve workspace storage_root {}", root.display()))
}

fn expand_tilde(path: &Path) -> Result<PathBuf> {
    // A path that is not valid UTF-8 cannot be a `~` spelling.
    let Some(raw) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    querymt_agent_core::path::expand_tilde(raw)
        .map(std::borrow::Cow::into_owned)
        .map_err(|_| anyhow!("workspace storage_root uses `~` but no home directory exists"))
}

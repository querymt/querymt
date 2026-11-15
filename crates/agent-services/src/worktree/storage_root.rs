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
    let starts_with_tilde = path
        .components()
        .next()
        .is_some_and(|c| c.as_os_str() == "~");
    if !starts_with_tilde {
        return Ok(path.to_path_buf());
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow!("workspace storage_root uses `~` but no home directory exists"))?;
    Ok(home.join(path.components().skip(1).collect::<PathBuf>()))
}

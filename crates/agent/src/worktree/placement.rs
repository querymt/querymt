//! Local-filesystem placement validation for managed workspaces.
//!
//! This version supports isolated execution only on the local filesystem
//! host. Remote filesystem paths fail closed before any Git administration
//! or child dispatch — a local worktree path is never silently treated as
//! remote. Remote *model inference* with local filesystem execution is
//! unaffected: placement only concerns where checkouts live and which
//! filesystem tools and integrations operate against.

use crate::worktree::error::{WorktreeError, WorktreeResult};
use std::path::Path;

/// Validate that `path` can host isolated execution on this version.
///
/// Fails closed with an actionable diagnostic for remote filesystem paths
/// instead of dispatching against a host this process cannot reach.
pub fn ensure_local_filesystem_execution(path: &Path) -> WorktreeResult<()> {
    if is_local_execution_path(path) {
        Ok(())
    } else {
        Err(WorktreeError::InvalidOperation(format!(
            "isolated execution is only supported on the local filesystem host; \
             refusing to dispatch against the remote filesystem path {}",
            path.display()
        )))
    }
}

/// Whether `path` refers to this host's local filesystem.
///
/// On non-Windows platforms every path is treated as local; on Windows,
/// UNC device paths (`\\server\share`, `\\?\UNC\...`) refer to remote
/// hosts and are rejected.
pub fn is_local_execution_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        !path.as_os_str().to_string_lossy().starts_with("\\\\")
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_paths_pass_placement_validation() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(is_local_execution_path(dir.path()));
        ensure_local_filesystem_execution(dir.path()).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn unc_paths_fail_placement_validation() {
        let remote = Path::new(r"\\server\share\repo");
        assert!(!is_local_execution_path(remote));
        let err = ensure_local_filesystem_execution(remote).unwrap_err();
        assert!(err.to_string().contains("remote filesystem path"));
    }
}

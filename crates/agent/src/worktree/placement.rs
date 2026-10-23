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
/// UNC paths (`\\server\share`, `\\?\UNC\...`) refer to remote hosts and
/// are rejected. Verbatim and device-namespace paths that name a local
/// drive (`\\?\C:\...`, `\\.\C:\...`), which `canonicalize` produces for
/// every local path, are local.
pub fn is_local_execution_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        is_local_windows_spelling(&path.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        true
    }
}

/// Classify a Windows path spelling. Pure string logic so it is testable on
/// every platform.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_local_windows_spelling(path: &str) -> bool {
    let rest = if let Some(rest) = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix(r"\\.\"))
    {
        // `\\?\UNC\server\share` is remote; `\\?\C:\x` is a local drive.
        if rest
            .as_bytes()
            .get(..4)
            .is_some_and(|head| head.eq_ignore_ascii_case(br"UNC\"))
        {
            return false;
        }
        rest
    } else {
        // Plain UNC (`\\server\share`, `//server/share`) is remote;
        // anything else is an ordinary local spelling.
        return !(path.starts_with(r"\\") || path.starts_with("//"));
    };
    // A verbatim/device path is local only when it names a drive letter.
    let mut chars = rest.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(drive), Some(':')) if drive.is_ascii_alphabetic()
    )
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

    #[test]
    fn windows_path_spellings_are_classified_by_host() {
        // Local drives, including the verbatim form canonicalize produces.
        assert!(is_local_windows_spelling(r"C:\work\repo"));
        assert!(is_local_windows_spelling(r"\\?\C:\work\repo"));
        assert!(is_local_windows_spelling(r"\\.\D:\work"));
        // Remote hosts.
        assert!(!is_local_windows_spelling(r"\\server\share\repo"));
        assert!(!is_local_windows_spelling(r"\\?\UNC\server\share\repo"));
        assert!(!is_local_windows_spelling(r"\\?\unc\server\share"));
        assert!(!is_local_windows_spelling("//server/share/repo"));
        // Verbatim paths that are not a drive are not trusted as local.
        assert!(!is_local_windows_spelling(r"\\?\GLOBALROOT\Device\x"));
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

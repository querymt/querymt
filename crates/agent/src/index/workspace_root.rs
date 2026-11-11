use std::path::{Path, PathBuf};

const ROOT_MARKERS: [&str; 4] = [".git", "Cargo.toml", "package.json", "pyproject.toml"];

pub fn resolve_workspace_root(cwd: &Path) -> PathBuf {
    let mut current = normalize_cwd(cwd);

    loop {
        if has_root_marker(&current) {
            return current;
        }

        if !current.pop() {
            return normalize_cwd(cwd);
        }
    }
}

/// A working directory and its workspace root, both resolved through symlinks
/// and `..` so that aliases of one directory share one identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalWorkspace {
    pub root: PathBuf,
    pub cwd: PathBuf,
}

impl CanonicalWorkspace {
    /// Whether the resolved working directory is a filesystem root.
    pub fn cwd_is_filesystem_root(&self) -> bool {
        self.cwd.parent().is_none()
    }
}

/// Resolve `cwd` through the filesystem, then find its workspace root.
///
/// This is the one workspace identity shared by the index manager, the
/// `querymt/workspace/files` query and workspace-confined reads. Unlike
/// [`resolve_workspace_root`] it fails when `cwd` is not an existing
/// directory, and never returns a path that still contains symlinks or `..`.
/// Performs blocking filesystem calls; see [`canonical_workspace_async`].
pub fn canonical_workspace(cwd: &Path) -> std::io::Result<CanonicalWorkspace> {
    let cwd = dunce::canonicalize(normalize_cwd(cwd))?;
    if !std::fs::metadata(&cwd)?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("{} is not a directory", cwd.display()),
        ));
    }
    let root = resolve_workspace_root(&cwd);
    Ok(CanonicalWorkspace { root, cwd })
}

/// [`canonical_workspace`] off the async worker threads.
pub async fn canonical_workspace_async(cwd: PathBuf) -> std::io::Result<CanonicalWorkspace> {
    tokio::task::spawn_blocking(move || canonical_workspace(&cwd))
        .await
        .map_err(std::io::Error::other)?
}

pub fn normalize_cwd(cwd: &Path) -> PathBuf {
    if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|base| base.join(cwd))
            .unwrap_or_else(|_| cwd.to_path_buf())
    }
}

fn has_root_marker(dir: &Path) -> bool {
    ROOT_MARKERS.iter().any(|marker| dir.join(marker).exists())
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── normalize_cwd ────────────────────────────────────────────────────────

    #[test]
    fn test_normalize_cwd_absolute_path_returned_unchanged() {
        let path = std::path::Path::new("/absolute/path/to/project");
        // normalize_cwd just returns the path as-is when absolute
        let result = normalize_cwd(path);
        assert_eq!(result, path);
    }

    #[test]
    fn test_normalize_cwd_relative_path_resolved() {
        // A relative path is joined with current_dir()
        let rel = std::path::Path::new("some/relative");
        let result = normalize_cwd(rel);
        // The result must be absolute
        assert!(result.is_absolute(), "normalized path should be absolute");
        // It should end with the relative component
        assert!(
            result.ends_with("some/relative"),
            "normalized path should end with the relative part"
        );
    }

    // ── canonical_workspace ──────────────────────────────────────────────────

    #[test]
    fn canonical_workspace_resolves_dotdot_and_symlinked_roots() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        let aliased = tmp.path().join("a/b/../b");
        let resolved = canonical_workspace(&aliased).unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        assert_eq!(resolved.root, root);
        assert_eq!(resolved.cwd, root.join("a/b"));
        assert!(!resolved.cwd_is_filesystem_root());
    }

    #[test]
    fn canonical_workspace_detects_filesystem_root_aliases() {
        let tmp = TempDir::new().unwrap();
        let mut alias = dunce::canonicalize(tmp.path()).unwrap();
        let depth = alias.components().count();
        for _ in 0..depth {
            alias.push("..");
        }
        // `<tmp>/../..` style aliases resolve to the filesystem root.
        let resolved = canonical_workspace(&alias).unwrap();
        assert!(resolved.cwd_is_filesystem_root());
    }

    #[test]
    fn canonical_workspace_fails_for_missing_directory_and_files() {
        let tmp = TempDir::new().unwrap();
        assert!(canonical_workspace(&tmp.path().join("missing")).is_err());
        std::fs::write(tmp.path().join("file"), "").unwrap();
        assert!(canonical_workspace(&tmp.path().join("file")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn canonical_workspace_resolves_symlinked_cwd_to_one_identity() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join(".git")).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("alias")).unwrap();
        let via_alias = canonical_workspace(&tmp.path().join("alias")).unwrap();
        let direct = canonical_workspace(&real).unwrap();
        assert_eq!(via_alias, direct);
    }

    // ── resolve_workspace_root ───────────────────────────────────────────────

    #[test]
    fn test_resolve_finds_git_root() {
        let tmp = TempDir::new().unwrap();
        // Create a .git directory at the tmp root
        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        // Create a nested subdirectory as cwd
        let subdir = tmp.path().join("src").join("module");
        std::fs::create_dir_all(&subdir).unwrap();

        let root = resolve_workspace_root(&subdir);
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap(),
            "should find the .git parent as workspace root"
        );
    }

    #[test]
    fn test_resolve_finds_cargo_toml_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();

        let subdir = tmp.path().join("crates").join("mylib");
        std::fs::create_dir_all(&subdir).unwrap();

        let root = resolve_workspace_root(&subdir);
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap(),
            "should find Cargo.toml parent as workspace root"
        );
    }

    #[test]
    fn test_resolve_finds_package_json_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();

        let subdir = tmp.path().join("src");
        std::fs::create_dir_all(&subdir).unwrap();

        let root = resolve_workspace_root(&subdir);
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap(),
            "should find package.json parent as workspace root"
        );
    }

    #[test]
    fn test_resolve_no_marker_returns_cwd() {
        // Directory with no markers at all — resolve_workspace_root climbs
        // until it runs out of parent dirs and returns normalize_cwd(cwd)
        let tmp = TempDir::new().unwrap();
        // No markers placed — result should be tmp itself (or an ancestor that
        // happens to have a marker on the real filesystem, but in a freshly
        // created tmpdir that's unlikely; at minimum it's absolute)
        let root = resolve_workspace_root(tmp.path());
        assert!(root.is_absolute(), "root must be absolute");
    }

    #[test]
    fn test_resolve_cwd_itself_has_marker() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();

        // cwd IS the root
        let root = resolve_workspace_root(tmp.path());
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap(),
        );
    }

    #[test]
    fn test_resolve_pyproject_toml_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[tool.poetry]\n").unwrap();

        let subdir = tmp.path().join("mypackage");
        std::fs::create_dir_all(&subdir).unwrap();

        let root = resolve_workspace_root(&subdir);
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap(),
        );
    }

    #[test]
    fn test_resolve_prefers_closest_marker() {
        let tmp = TempDir::new().unwrap();
        // Outer .git
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        // Inner Cargo.toml (closer to cwd)
        let inner = tmp.path().join("crates").join("agent");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("Cargo.toml"), "[package]\n").unwrap();

        // Starting inside inner → should return inner (closest marker first)
        let subdir = inner.join("src");
        std::fs::create_dir_all(&subdir).unwrap();
        let root = resolve_workspace_root(&subdir);
        assert_eq!(
            root.canonicalize().unwrap(),
            inner.canonicalize().unwrap(),
            "should stop at the first (closest) marker found going up"
        );
    }
}

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

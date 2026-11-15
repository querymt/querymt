//! Path identity for tool calls.
//!
//! The tool-batch scheduler and the parallel-edit guard must agree on when two
//! tool calls refer to the same file, regardless of how the model spelled the
//! path (`a.md`, `./a.md`, an absolute path, or through a symlinked directory).

use std::path::{Path, PathBuf};

use super::context::FileAccess;
use super::read_scope::{ReadFilter, ReadScope};

pub use querymt_agent_core::path::{
    effective_root, normalize_tool_path, paths_overlap, resolve_arg_path,
};

/// Whether the optional `root` argument (or, when absent, the `cwd`) resolves.
///
/// When this is `false` the tool fails before touching any file, so it must
/// declare no file access.
pub fn root_arg_resolves(args: &serde_json::Value, cwd: Option<&Path>) -> bool {
    effective_root(args, cwd).is_some()
}

/// `FileAccess` for tools that write the single path in `args[key]`.
///
/// Returns [`FileAccess::None`] when the argument is missing, not a string, or
/// cannot be resolved: the call will fail its own validation.
pub fn single_path_write_access(
    args: &serde_json::Value,
    key: &str,
    cwd: Option<&Path>,
) -> FileAccess {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| resolve_arg_path(raw, cwd))
        .map_or(FileAccess::None, |path| FileAccess::writes([path]))
}

/// `FileAccess` for tools that read the single path in `args[key]`.
///
/// The path may be a file or a directory; readers of a directory conflict with
/// writers of anything beneath it.
pub fn single_path_read_access(
    args: &serde_json::Value,
    key: &str,
    cwd: Option<&Path>,
) -> FileAccess {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| resolve_arg_path(raw, cwd))
        .map_or(FileAccess::None, |path| FileAccess::reads([path]))
}

/// `FileAccess` for tools that read the optional path in `args[key]`, falling
/// back to the working directory when the argument is absent (e.g. a search
/// rooted at the workspace).
pub fn path_or_cwd_read_access(
    args: &serde_json::Value,
    key: &str,
    cwd: Option<&Path>,
) -> FileAccess {
    match args.get(key).and_then(serde_json::Value::as_str) {
        Some(raw) => {
            resolve_arg_path(raw, cwd).map_or(FileAccess::None, |p| FileAccess::reads([p]))
        }
        None => cwd.map_or(FileAccess::None, |cwd| {
            FileAccess::reads([normalize_tool_path(cwd, None)])
        }),
    }
}

/// Like [`path_or_cwd_read_access`], but the directory read is narrowed by
/// `filter` (the include/exclude/file-type filter the walker itself applies),
/// so only writes the walker could observe are ordered against it.
pub fn path_or_cwd_filtered_read_access(
    args: &serde_json::Value,
    key: &str,
    cwd: Option<&Path>,
    filter: ReadFilter,
) -> FileAccess {
    let root = match args.get(key).and_then(serde_json::Value::as_str) {
        Some(raw) => resolve_arg_path(raw, cwd),
        None => cwd.map(|cwd| normalize_tool_path(cwd, None)),
    };
    root.map_or(FileAccess::None, |root| {
        FileAccess::read_scopes([ReadScope::filtered(root, filter)])
    })
}

/// `FileAccess` for tools that read every string in the array `args[key]`.
///
/// Requires [`root_arg_resolves`], mirroring tools that call `resolve_root`
/// before resolving their targets. Non-string entries are ignored, like the
/// tools' own argument parsing does.
pub fn path_list_read_access(
    args: &serde_json::Value,
    key: &str,
    cwd: Option<&Path>,
) -> FileAccess {
    let Some(root) = effective_root(args, cwd) else {
        return FileAccess::None;
    };
    let Some(entries) = args.get(key).and_then(serde_json::Value::as_array) else {
        return FileAccess::None;
    };
    collect_read_paths(
        entries
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(|raw| resolve_arg_path(raw, Some(&root))),
    )
}

/// Build a read-only access from resolved paths.
///
/// If any entry failed to resolve the call will fail validation, so no access
/// is declared. Duplicates collapse; an empty list declares nothing.
pub fn collect_read_paths(paths: impl IntoIterator<Item = Option<PathBuf>>) -> FileAccess {
    let mut reads: Vec<PathBuf> = Vec::new();
    for path in paths {
        let Some(path) = path else {
            return FileAccess::None;
        };
        if !reads.contains(&path) {
            reads.push(path);
        }
    }
    if reads.is_empty() {
        FileAccess::None
    } else {
        FileAccess::reads(reads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(dir.path()).expect("canonical tempdir");
        (dir, root)
    }

    #[test]
    fn root_arg_resolves_mirrors_resolve_root() {
        let (_guard, root) = canonical_tmp();
        let none = serde_json::json!({});
        let abs = serde_json::json!({"root": root.to_str().unwrap()});
        let rel = serde_json::json!({"root": "sub"});

        assert!(root_arg_resolves(&none, Some(&root)));
        assert!(!root_arg_resolves(&none, None));
        assert!(root_arg_resolves(&abs, None));
        assert!(root_arg_resolves(&rel, Some(&root)));
        assert!(!root_arg_resolves(&rel, None));
    }

    #[test]
    fn path_list_read_access_joins_relative_paths_with_root_arg() {
        let (_guard, root) = canonical_tmp();
        let other = root.join("other");
        let args = serde_json::json!({
            "root": other.to_str().unwrap(),
            "paths": ["src/a.rs"]
        });

        // `root` differs from the cwd: the relative path names a file under `root`.
        assert_eq!(
            path_list_read_access(&args, "paths", Some(&root)),
            FileAccess::reads([other.join("src/a.rs")])
        );
        // An absolute root makes relative paths resolvable even without a cwd.
        assert_eq!(
            path_list_read_access(&args, "paths", None),
            FileAccess::reads([other.join("src/a.rs")])
        );
    }

    #[test]
    fn read_helpers_return_none_for_invalid_arguments() {
        let (_guard, root) = canonical_tmp();
        let bad = serde_json::json!({"path": 1, "paths": "x"});

        assert_eq!(
            single_path_read_access(&bad, "path", Some(&root)),
            FileAccess::None
        );
        assert_eq!(
            path_list_read_access(&bad, "paths", Some(&root)),
            FileAccess::None
        );
        assert_eq!(
            path_list_read_access(&serde_json::json!({"paths": []}), "paths", Some(&root)),
            FileAccess::None
        );
        // An unresolvable entry means the whole call fails validation.
        assert_eq!(
            path_list_read_access(&serde_json::json!({"paths": ["a.rs"]}), "paths", None),
            FileAccess::None
        );
    }

    #[test]
    fn path_list_read_access_collapses_duplicates() {
        let (_guard, root) = canonical_tmp();
        let args = serde_json::json!({"paths": ["a.rs", "./a.rs", "b.rs"]});

        assert_eq!(
            path_list_read_access(&args, "paths", Some(&root)),
            FileAccess::reads([root.join("a.rs"), root.join("b.rs")])
        );
    }

    #[test]
    fn path_or_cwd_read_access_defaults_to_cwd() {
        let (_guard, root) = canonical_tmp();

        assert_eq!(
            path_or_cwd_read_access(&serde_json::json!({}), "path", Some(&root)),
            FileAccess::reads([root.clone()])
        );
        assert_eq!(
            path_or_cwd_read_access(&serde_json::json!({"path": "src"}), "path", Some(&root)),
            FileAccess::reads([root.join("src")])
        );
        assert_eq!(
            path_or_cwd_read_access(&serde_json::json!({}), "path", None),
            FileAccess::None
        );
    }
}

//! Path identity for tool calls.
//!
//! The tool-batch scheduler and the parallel-edit guard must agree on when two
//! tool calls refer to the same file, regardless of how the model spelled the
//! path (`a.md`, `./a.md`, an absolute path, or through a symlinked directory).

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use super::context::FileAccess;

/// Normalise a tool-call path into a stable identity key.
///
/// 1. Resolve against `cwd` when the path is relative (mirrors
///    `ToolContext::resolve_path`). Without a `cwd` a relative path stays
///    relative.
/// 2. Lexically clean `.` and `..` components.
/// 3. Canonicalize the path if it exists; otherwise canonicalize the deepest
///    existing ancestor and append the remaining components.
///
/// This never fails. When canonicalization is impossible the cleaned absolute
/// path is returned, so the worst case is two spellings being treated as
/// different files (the pre-existing behaviour).
pub fn normalize_tool_path(raw: impl AsRef<Path>, cwd: Option<&Path>) -> PathBuf {
    let raw = raw.as_ref();
    let joined = match cwd {
        Some(cwd) if !raw.is_absolute() => cwd.join(raw),
        _ => raw.to_path_buf(),
    };
    let cleaned = lexical_clean(&joined);
    if !cleaned.is_absolute() {
        return cleaned;
    }
    canonicalize_deepest_ancestor(cleaned)
}

/// Build a normalised path from a string argument, mirroring
/// `ToolContext::resolve_path`: a relative path without a `cwd` cannot be
/// resolved, so the tool would fail and `None` is returned.
pub fn resolve_arg_path(raw: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if !Path::new(raw).is_absolute() && cwd.is_none() {
        return None;
    }
    Some(normalize_tool_path(raw, cwd))
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

/// Remove `.` components and resolve `..` against preceding normal components.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `..` at the root stays at the root.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                Some(Component::ParentDir) | None => out.push(component.as_os_str()),
                Some(Component::CurDir) => {}
            },
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Canonicalize the longest existing prefix of `path` and re-append the rest.
fn canonicalize_deepest_ancestor(path: PathBuf) -> PathBuf {
    let mut tail: Vec<OsString> = Vec::new();
    let mut current = path.clone();
    loop {
        match current.canonicalize() {
            Ok(mut resolved) => {
                resolved.extend(tail.iter().rev());
                return resolved;
            }
            Err(_) => match (current.file_name(), current.parent()) {
                (Some(name), Some(parent)) => {
                    tail.push(name.to_os_string());
                    current = parent.to_path_buf();
                }
                // Nothing along the path can be canonicalized.
                _ => return path,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical tempdir");
        (dir, root)
    }

    #[test]
    fn relative_dot_relative_and_absolute_are_equal() {
        let (_guard, root) = canonical_tmp();
        std::fs::write(root.join("a.md"), "x").unwrap();

        let plain = normalize_tool_path("a.md", Some(&root));
        let dotted = normalize_tool_path("./a.md", Some(&root));
        let absolute = normalize_tool_path(root.join("a.md"), Some(&root));

        assert_eq!(plain, root.join("a.md"));
        assert_eq!(plain, dotted);
        assert_eq!(plain, absolute);
    }

    #[test]
    fn parent_segments_are_cleaned() {
        let (_guard, root) = canonical_tmp();
        std::fs::create_dir_all(root.join("docs/sub")).unwrap();
        std::fs::write(root.join("docs/a.md"), "x").unwrap();

        let via_parent = normalize_tool_path("docs/sub/../a.md", Some(&root));
        assert_eq!(via_parent, root.join("docs/a.md"));
    }

    #[test]
    fn nonexistent_file_in_existing_dir_keeps_name() {
        let (_guard, root) = canonical_tmp();
        std::fs::create_dir_all(root.join("docs")).unwrap();

        let result = normalize_tool_path("./docs/new.md", Some(&root));
        assert_eq!(result, root.join("docs/new.md"));
    }

    #[test]
    fn nonexistent_nested_path_canonicalizes_deepest_ancestor() {
        let (_guard, root) = canonical_tmp();

        let result = normalize_tool_path("a/b/c.md", Some(&root));
        assert_eq!(result, root.join("a/b/c.md"));
    }

    #[test]
    fn parent_segments_cannot_escape_root() {
        let result = normalize_tool_path("/../../etc/../x", None);
        assert!(result.is_absolute());
        assert!(!result.components().any(|c| c == Component::ParentDir));
    }

    #[test]
    fn relative_path_without_cwd_stays_relative() {
        let result = normalize_tool_path("./a/../b.md", None);
        assert_eq!(result, PathBuf::from("b.md"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dir_resolves_to_same_key() {
        let (_guard, root) = canonical_tmp();
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/a.md"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();

        let direct = normalize_tool_path("real/a.md", Some(&root));
        let through_link = normalize_tool_path("link/a.md", Some(&root));
        assert_eq!(direct, through_link);

        // Also for a file that does not exist yet beneath the symlink.
        let new_direct = normalize_tool_path("real/new.md", Some(&root));
        let new_link = normalize_tool_path("link/new.md", Some(&root));
        assert_eq!(new_direct, new_link);
    }
}

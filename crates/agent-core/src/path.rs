//! Pure path identity helpers shared by the agent layers.
//!
//! The tool-batch scheduler and the parallel-edit guard must agree on when two
//! tool calls refer to the same file, regardless of how the model spelled the
//! path (`a.md`, `./a.md`, an absolute path, or through a symlinked directory).

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

/// A path argument starts with `~` but the process has no home directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HomeDirectoryUnknown;

/// Expand a leading `~` or `~/` of a path argument against `home`.
///
/// Only a value that is exactly `~` or starts with `~/` (also `~\` on Windows)
/// is expanded. Every other value is returned as written: `~alice/x` (a user
/// name), `a/~b` (a `~` that is not first) and plain paths. A value that needs
/// expanding while `home` is `None` is an error, so the caller can fail the call
/// naming the argument instead of reading a directory literally called `~`.
pub fn expand_leading_tilde<'a>(
    raw: &'a str,
    home: Option<&Path>,
) -> Result<Cow<'a, Path>, HomeDirectoryUnknown> {
    let rest = if raw == "~" {
        Some("")
    } else if let Some(rest) = raw.strip_prefix("~/") {
        Some(rest)
    } else if cfg!(windows) {
        raw.strip_prefix("~\\")
    } else {
        None
    };
    let Some(rest) = rest else {
        return Ok(Cow::Borrowed(Path::new(raw)));
    };
    let home = home.ok_or(HomeDirectoryUnknown)?;
    Ok(Cow::Owned(if rest.is_empty() {
        home.to_path_buf()
    } else {
        home.join(rest)
    }))
}

/// [`expand_leading_tilde`] against the home directory of the user running the
/// agent process.
pub fn expand_tilde(raw: &str) -> Result<Cow<'_, Path>, HomeDirectoryUnknown> {
    expand_leading_tilde(raw, dirs::home_dir().as_deref())
}

/// Normalise a tool-call path into a stable identity key.
///
/// 1. Resolve against `cwd` when the path is relative (mirrors
///    `ToolContext::resolve_path`). Without a `cwd` a relative path stays
///    relative and is only lexically cleaned.
/// 2. Canonicalize the longest existing prefix of the path. This is done on the
///    *un-cleaned* path so the operating system resolves `..` the way it will
///    when the tool opens the file (`link/../x` is relative to the symlink
///    target, not to the directory containing the link).
/// 3. Append the remaining (non-existent) components, resolving `.` and `..`
///    lexically.
///
/// Canonicalization uses [`dunce`], so on Windows the result never carries the
/// `\\?\` verbatim prefix and `C:\a` and `\\?\C:\a` map to the same key.
///
/// This never fails. When nothing can be canonicalized the lexically cleaned
/// path is returned, so the worst case is two spellings being treated as
/// different files (the pre-existing behaviour).
pub fn normalize_tool_path(raw: impl AsRef<Path>, cwd: Option<&Path>) -> PathBuf {
    let raw = raw.as_ref();
    let joined = match cwd {
        Some(cwd) if !raw.is_absolute() => cwd.join(raw),
        _ => raw.to_path_buf(),
    };
    if !joined.is_absolute() {
        return lexical_clean(&joined);
    }
    canonicalize_existing_prefix(&joined)
}

/// Build a normalised path from a string argument, mirroring
/// `ToolContext::resolve_path`: a relative path without a `cwd` cannot be
/// resolved, so the tool would fail and `None` is returned.
///
/// A leading `~` is expanded first ([`expand_leading_tilde`]), so scheduling and
/// approval see the path the tool will open. When the home directory is unknown
/// the call fails in the tool itself, so `None` is returned here too.
pub fn resolve_arg_path(raw: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    resolve_arg_path_with_home(raw, cwd, dirs::home_dir().as_deref())
}

fn resolve_arg_path_with_home(
    raw: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let expanded = expand_leading_tilde(raw, home).ok()?;
    if !expanded.is_absolute() && cwd.is_none() {
        return None;
    }
    Some(normalize_tool_path(expanded, cwd))
}

/// The normalised directory that relative target paths are resolved against:
/// the optional `root` argument (itself resolved against `cwd`), or `cwd` when
/// `root` is absent.
///
/// Mirrors `helpers::resolve_root`. `None` means the tool fails before touching
/// any file, so it must declare no file access.
pub fn effective_root(args: &serde_json::Value, cwd: Option<&Path>) -> Option<PathBuf> {
    match args.get("root").and_then(serde_json::Value::as_str) {
        Some(root) => resolve_arg_path(root, cwd),
        None => cwd.map(|cwd| normalize_tool_path(cwd, None)),
    }
}

/// Whether two normalised paths overlap: they are equal, or one contains the
/// other (so a directory conflicts with anything beneath it). Used for
/// write/write conflicts; reads go through [`ReadScope::observes_write`].
pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Remove `.` components and resolve `..` against preceding normal components.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => pop_parent(&mut out, true),
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Apply a `..` to `out`. `keep_relative` keeps leading `..` of relative paths.
fn pop_parent(out: &mut PathBuf, keep_relative: bool) {
    match out.components().next_back() {
        Some(Component::Normal(_)) => {
            out.pop();
        }
        // `..` at the root stays at the root.
        Some(Component::RootDir | Component::Prefix(_)) => {}
        Some(Component::ParentDir) | None => {
            if keep_relative {
                out.push("..");
            }
        }
        Some(Component::CurDir) => {}
    }
}

/// Canonicalize the longest existing prefix of an absolute `path` and apply the
/// remaining components lexically.
fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    let components: Vec<Component<'_>> = path
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect();
    // Never try to canonicalize a bare drive prefix such as `C:` (that means
    // "the current directory of drive C").
    let min_prefix = match components.as_slice() {
        [Component::Prefix(_), Component::RootDir, ..] => 2,
        _ => 1,
    };

    for split in (min_prefix..=components.len()).rev() {
        let prefix: PathBuf = components[..split]
            .iter()
            .map(|component| component.as_os_str())
            .collect();
        if let Ok(mut resolved) = dunce::canonicalize(&prefix) {
            for component in &components[split..] {
                match component {
                    Component::Normal(part) => resolved.push(part),
                    Component::ParentDir => pop_parent(&mut resolved, false),
                    _ => {}
                }
            }
            return resolved;
        }
    }
    lexical_clean(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(dir.path()).expect("canonical tempdir");
        (dir, root)
    }

    fn home() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\wiking")
        } else {
            PathBuf::from("/Users/wiking")
        }
    }

    #[test]
    fn tilde_directory_under_the_home_directory() {
        let home = home();
        let expanded = expand_leading_tilde("~/.local/share", Some(&home)).unwrap();
        assert_eq!(expanded, home.join(".local/share"));
    }

    #[test]
    fn bare_tilde_is_the_home_directory() {
        let home = home();
        assert_eq!(expand_leading_tilde("~", Some(&home)).unwrap(), home);
        assert_eq!(expand_leading_tilde("~/", Some(&home)).unwrap(), home);
    }

    #[test]
    fn tilde_with_a_user_name_is_not_expanded() {
        let home = home();
        for raw in ["~alice/notes.md", "~alice", "~.", "~~"] {
            assert_eq!(
                expand_leading_tilde(raw, Some(&home)).unwrap(),
                Path::new(raw),
                "{raw}"
            );
        }
    }

    #[test]
    fn tilde_inside_a_path_is_literal() {
        let home = home();
        for raw in ["backup/~old/notes.md", "./~", "a~/b", "/abs/~/x"] {
            assert_eq!(
                expand_leading_tilde(raw, Some(&home)).unwrap(),
                Path::new(raw),
                "{raw}"
            );
        }
    }

    #[test]
    fn home_directory_unknown_is_an_error_only_when_expansion_is_needed() {
        assert_eq!(
            expand_leading_tilde("~/data", None),
            Err(HomeDirectoryUnknown)
        );
        assert_eq!(expand_leading_tilde("~", None), Err(HomeDirectoryUnknown));
        assert_eq!(
            expand_leading_tilde("data/x", None).unwrap(),
            Path::new("data/x")
        );
        assert_eq!(
            expand_leading_tilde("~alice/x", None).unwrap(),
            Path::new("~alice/x")
        );
    }

    #[test]
    fn arg_path_with_tilde_is_the_same_file_as_the_absolute_spelling() {
        let (_guard, root) = canonical_tmp();
        // The home directory is a temp dir here, so nothing real is touched.
        let tilde = resolve_arg_path_with_home("~/x/y.md", Some(&root), Some(&root)).unwrap();
        let absolute = resolve_arg_path_with_home(
            root.join("x/y.md").to_str().unwrap(),
            Some(&root),
            Some(&root),
        )
        .unwrap();
        assert_eq!(tilde, absolute);
        assert_eq!(tilde, root.join("x/y.md"));
    }

    #[test]
    fn arg_path_with_tilde_needs_no_cwd_and_fails_without_home() {
        let (_guard, root) = canonical_tmp();
        assert_eq!(
            resolve_arg_path_with_home("~/a.md", None, Some(&root)),
            Some(root.join("a.md"))
        );
        assert_eq!(
            resolve_arg_path_with_home("~/a.md", Some(&root), None),
            None
        );
        // `~alice` is a relative path like any other.
        assert_eq!(
            resolve_arg_path_with_home("~alice/a.md", Some(&root), Some(&root)),
            Some(root.join("~alice/a.md"))
        );
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
    fn parent_segments_after_missing_dirs_are_cleaned() {
        let (_guard, root) = canonical_tmp();

        let result = normalize_tool_path("missing/../a.md", Some(&root));
        assert_eq!(result, root.join("a.md"));
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

    #[test]
    fn resolve_arg_path_requires_cwd_for_relative_paths() {
        let (_guard, root) = canonical_tmp();
        assert_eq!(resolve_arg_path("a.md", None), None);
        assert_eq!(
            resolve_arg_path("a.md", Some(&root)),
            Some(root.join("a.md"))
        );
    }

    #[test]
    fn effective_root_resolves_root_arg_against_cwd() {
        let (_guard, root) = canonical_tmp();
        let other = root.join("other");

        assert_eq!(
            effective_root(&serde_json::json!({}), Some(&root)),
            Some(root.clone())
        );
        assert_eq!(effective_root(&serde_json::json!({}), None), None);
        assert_eq!(
            effective_root(&serde_json::json!({"root": "sub"}), Some(&root)),
            Some(root.join("sub"))
        );
        assert_eq!(
            effective_root(
                &serde_json::json!({"root": other.to_str().unwrap()}),
                Some(&root)
            ),
            Some(other)
        );
    }

    #[test]
    fn paths_overlap_is_containment_not_prefix_of_name() {
        let dir = Path::new("/ws/src");
        assert!(paths_overlap(dir, Path::new("/ws/src")));
        assert!(paths_overlap(dir, Path::new("/ws/src/a.rs")));
        assert!(paths_overlap(Path::new("/ws/src/a.rs"), dir));
        assert!(!paths_overlap(dir, Path::new("/ws/src2/a.rs")));
        assert!(!paths_overlap(
            Path::new("/ws/src/a.rs"),
            Path::new("/ws/src/b.rs")
        ));
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

    #[cfg(unix)]
    #[test]
    fn parent_segment_after_symlink_follows_the_link_target() {
        let (_guard, root) = canonical_tmp();
        std::fs::create_dir_all(root.join("real/sub")).unwrap();
        std::os::unix::fs::symlink(root.join("real/sub"), root.join("link")).unwrap();

        // The OS resolves `link/..` to `real`, not to `root`.
        let through_link = normalize_tool_path("link/../x.md", Some(&root));
        assert_eq!(through_link, root.join("real/x.md"));
        assert_eq!(through_link, normalize_tool_path("real/x.md", Some(&root)));
    }
}

//! Cross-OS scoping of a workspace index to a working directory.
//!
//! Index entries are relative to the workspace root and use the owning node's
//! native separator. A relay node may answer for a peer on another OS, so all
//! path handling here goes through `typed-path` (never `std::path`) and file
//! URIs are built with `url` from path components.

use super::WorkspaceFileEntry;
use crate::index::FileIndexEntry;
use std::borrow::Cow;
use typed_path::{
    Utf8TypedComponent, Utf8TypedPath, Utf8TypedPathBuf, Utf8UnixComponent, Utf8WindowsComponent,
    Utf8WindowsPrefix,
};
use url::Url;

/// A workspace index narrowed to a working directory.
///
/// Borrows the index: candidate paths are slices of the original entries
/// unless a Windows peer's separators must be rewritten.
#[derive(Debug)]
pub struct ScopedIndex<'a> {
    root: &'a str,
    cwd: &'a str,
    windows: bool,
    /// Path segments of the cwd below the root (empty when cwd is the root).
    cwd_segments: Vec<String>,
    /// Paths relative to the cwd, `/`-separated.
    paths: Vec<Cow<'a, str>>,
    /// Index into the source entries, parallel to `paths`.
    source: Vec<usize>,
    files: &'a [FileIndexEntry],
}

impl<'a> ScopedIndex<'a> {
    /// Keep the entries at or below `cwd` (or the root when `cwd` is `None` or
    /// not inside the root). The cwd itself is never an entry.
    pub fn new(workspace_root: &'a str, cwd: Option<&'a str>, files: &'a [FileIndexEntry]) -> Self {
        let windows = Utf8TypedPath::derive(workspace_root).is_windows();
        let cwd_segments = cwd.and_then(|cwd| relative_components(workspace_root, cwd));
        let cwd_display = match (&cwd_segments, cwd) {
            (Some(_), Some(cwd)) => cwd,
            _ => workspace_root,
        };
        let cwd_segments = cwd_segments.unwrap_or_default();
        let prefix = if cwd_segments.is_empty() {
            String::new()
        } else {
            format!("{}/", cwd_segments.join("/"))
        };

        let mut paths = Vec::with_capacity(files.len());
        let mut source = Vec::with_capacity(files.len());
        for (index, entry) in files.iter().enumerate() {
            let normalized = if windows && entry.path.contains('\\') {
                Cow::Owned(entry.path.replace('\\', "/"))
            } else {
                Cow::Borrowed(entry.path.as_str())
            };
            let rest = match normalized {
                Cow::Borrowed(path) => path.strip_prefix(prefix.as_str()).map(Cow::Borrowed),
                Cow::Owned(path) => path
                    .strip_prefix(prefix.as_str())
                    .map(|rest| Cow::Owned(rest.to_owned())),
            };
            match rest {
                Some(rest) if !rest.is_empty() => {
                    paths.push(rest);
                    source.push(index);
                }
                _ => {}
            }
        }
        Self {
            root: workspace_root,
            cwd: cwd_display,
            windows,
            cwd_segments,
            paths,
            source,
            files,
        }
    }

    /// The effective working directory, in the owning node's native format.
    pub fn cwd(&self) -> &str {
        self.cwd
    }

    /// Candidate paths relative to the working directory.
    pub fn candidates(&self) -> &[Cow<'a, str>] {
        &self.paths
    }

    /// Build the wire entry for candidate `index`.
    pub fn entry(&self, index: usize) -> Option<WorkspaceFileEntry> {
        let path = self.paths.get(index)?;
        let is_dir = self.files.get(*self.source.get(index)?)?.is_dir;
        let mut abs = if self.windows {
            Utf8TypedPathBuf::from_windows(self.root)
        } else {
            Utf8TypedPathBuf::from_unix(self.root)
        };
        for segment in self
            .cwd_segments
            .iter()
            .map(String::as_str)
            .chain(path.split('/').filter(|s| !s.is_empty()))
        {
            abs.push(segment);
        }
        let uri = file_uri(abs.to_path())?;
        Some(WorkspaceFileEntry {
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            path: path.to_string(),
            abs_path: abs.as_str().to_owned(),
            uri,
            is_dir,
        })
    }
}

/// Components of `path` below `base`, or `None` when `path` is not inside it.
fn relative_components(base: &str, path: &str) -> Option<Vec<String>> {
    let full = Utf8TypedPath::derive(path);
    let rest = full.strip_prefix(base).ok()?;
    Some(
        rest.components()
            .filter(|component| component.is_normal())
            .map(|component| component.as_str().to_owned())
            .collect(),
    )
}

/// Build a `file://` URI for an absolute path of either OS flavor.
pub fn file_uri(path: Utf8TypedPath<'_>) -> Option<String> {
    let mut host: Option<&str> = None;
    let mut segments: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Utf8TypedComponent::Unix(Utf8UnixComponent::Normal(name))
            | Utf8TypedComponent::Windows(Utf8WindowsComponent::Normal(name)) => {
                segments.push(name.to_owned());
            }
            Utf8TypedComponent::Windows(Utf8WindowsComponent::Prefix(prefix)) => {
                match prefix.kind() {
                    Utf8WindowsPrefix::Disk(drive) | Utf8WindowsPrefix::VerbatimDisk(drive) => {
                        segments.push(format!("{drive}:"));
                    }
                    Utf8WindowsPrefix::UNC(server, share)
                    | Utf8WindowsPrefix::VerbatimUNC(server, share) => {
                        host = Some(server);
                        segments.push(share.to_owned());
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let mut url = Url::parse("file:///").ok()?;
    if let Some(host) = host {
        url.set_host(Some(host)).ok()?;
    }
    url.path_segments_mut().ok()?.clear().extend(segments);
    Some(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool) -> FileIndexEntry {
        FileIndexEntry {
            path: path.into(),
            is_dir,
        }
    }

    #[test]
    fn subdirectory_cwd_scopes_and_relativizes() {
        let files = [
            entry("crates", true),
            entry("crates/agent", true),
            entry("crates/agent/lib.rs", false),
            entry("crates/other/x.rs", false),
            entry("README.md", false),
        ];
        let scoped = ScopedIndex::new("/repo", Some("/repo/crates/agent"), &files);
        assert_eq!(scoped.candidates(), ["lib.rs"]);
        assert_eq!(scoped.cwd(), "/repo/crates/agent");
        let e = scoped.entry(0).unwrap();
        assert_eq!(e.abs_path, "/repo/crates/agent/lib.rs");
        assert_eq!(e.uri, "file:///repo/crates/agent/lib.rs");
    }

    #[test]
    fn cwd_outside_root_falls_back_to_root() {
        let files = [entry("a.rs", false)];
        let scoped = ScopedIndex::new("/repo", Some("/elsewhere"), &files);
        assert_eq!(scoped.candidates(), ["a.rs"]);
        assert_eq!(scoped.cwd(), "/repo");
    }

    #[test]
    fn sibling_with_common_prefix_is_not_inside_cwd() {
        let files = [entry("ab/x.rs", false), entry("a/y.rs", false)];
        let scoped = ScopedIndex::new("/repo", Some("/repo/a"), &files);
        assert_eq!(scoped.candidates(), ["y.rs"]);
    }

    #[test]
    fn windows_peer_paths_become_slash_relative_with_valid_uris_on_unix_host() {
        let files = [
            entry(r"src", true),
            entry(r"src\my file.rs", false),
            entry(r"docs\a.md", false),
        ];
        let scoped = ScopedIndex::new(r"C:\Users\dev\repo", Some(r"C:\Users\dev\repo\src"), &files);
        assert_eq!(scoped.candidates(), ["my file.rs"]);
        let e = scoped.entry(0).unwrap();
        assert_eq!(e.path, "my file.rs");
        assert_eq!(e.abs_path, r"C:\Users\dev\repo\src\my file.rs");
        assert_eq!(e.uri, "file:///C:/Users/dev/repo/src/my%20file.rs");
        assert_eq!(e.name, "my file.rs");
    }

    #[test]
    fn windows_root_without_cwd_lists_everything_slash_separated() {
        let files = [entry(r"docs\a.md", false)];
        let scoped = ScopedIndex::new(r"C:\repo", None, &files);
        assert_eq!(scoped.candidates(), ["docs/a.md"]);
        assert_eq!(scoped.entry(0).unwrap().abs_path, r"C:\repo\docs\a.md");
    }

    #[test]
    fn unix_uri_percent_encodes_segments() {
        let files = [entry("a b/#c.rs", false)];
        let scoped = ScopedIndex::new("/repo", None, &files);
        assert_eq!(scoped.entry(0).unwrap().uri, "file:///repo/a%20b/%23c.rs");
    }

    #[test]
    fn directory_entry_flags_is_dir() {
        let files = [entry("src", true)];
        let scoped = ScopedIndex::new("/repo", None, &files);
        let e = scoped.entry(0).unwrap();
        assert!(e.is_dir);
        assert_eq!(e.uri, "file:///repo/src");
    }
}

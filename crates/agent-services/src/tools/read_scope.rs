//! Read scopes: what part of the file system a read-only tool call observes.
//!
//! A plain read names a file or a directory and observes everything at or
//! beneath it. Directory walkers (`search_text`, `glob`, `ls`) usually look at
//! far less than that: only files matching an include pattern, minus excluded
//! patterns, optionally restricted to a ripgrep file type. A [`ReadFilter`]
//! records exactly the filter the tool itself applies, so the scheduler only
//! orders such a read against writes the read could actually observe.
//!
//! Filters are only ever used to *narrow* a scope. Whenever a filter cannot be
//! built the same way the tool builds it, the scope stays unfiltered, which is
//! the conservative (more serializing) choice.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use glob::Pattern;
use ignore::types::{Types, TypesBuilder};

/// A path read by a tool call, optionally narrowed by a filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadScope {
    /// Normalised file or directory read by the call.
    pub path: PathBuf,
    /// When set, only entries strictly beneath `path` that pass the filter are
    /// observed. `None` means everything at or beneath `path`.
    pub filter: Option<Arc<ReadFilter>>,
}

impl ReadScope {
    /// A scope observing everything at or beneath `path`.
    pub fn all(path: PathBuf) -> Self {
        Self { path, filter: None }
    }

    /// A scope observing only entries beneath `path` that pass `filter`.
    /// An empty filter is dropped, so the scope compares equal to [`Self::all`].
    pub fn filtered(path: PathBuf, filter: ReadFilter) -> Self {
        let filter = (!filter.is_empty()).then(|| Arc::new(filter));
        Self { path, filter }
    }

    /// Whether a write of `written` may change what this read observes.
    ///
    /// - Writing the scope path itself, or any ancestor of it (e.g. deleting a
    ///   directory that contains the read file), always conflicts.
    /// - Writing beneath the scope path conflicts unless the filter excludes
    ///   that entry.
    /// - Anything else is unrelated.
    pub fn observes_write(&self, written: &Path) -> bool {
        if self.path.starts_with(written) {
            return true;
        }
        let Ok(relative) = written.strip_prefix(&self.path) else {
            return false;
        };
        self.filter
            .as_ref()
            .is_none_or(|filter| filter.matches(relative))
    }
}

impl ReadScope {
    /// Whether removing or replacing the directory tree `root` may change what
    /// this read observes. The filter does not apply: the tree may contain
    /// matching entries whatever its own name is.
    pub fn observes_tree_write(&self, root: &Path) -> bool {
        self.path.starts_with(root) || root.starts_with(&self.path)
    }
}

/// The entry filter a directory walker applies, relative to its search root.
///
/// Mirrors the tools' own matching: `glob::Pattern::matches_path` on the path
/// relative to the scope root, and ripgrep file types on the file name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadFilter {
    /// When non-empty, an entry must match at least one of these.
    include: Vec<Pattern>,
    /// An entry matching any of these is not observed.
    exclude: Vec<Pattern>,
    /// When set, an entry must be of this ripgrep file type.
    file_type: Option<FileTypeFilter>,
}

impl ReadFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Require entries to match `pattern`. An invalid pattern is ignored
    /// (the filter stays broader than the tool's, never narrower).
    pub fn include(mut self, pattern: &str) -> Self {
        if let Ok(pattern) = Pattern::new(pattern) {
            self.include.push(pattern);
        }
        self
    }

    /// Skip entries matching `pattern`. An invalid pattern is ignored.
    pub fn exclude(mut self, pattern: &str) -> Self {
        if let Ok(pattern) = Pattern::new(pattern) {
            self.exclude.push(pattern);
        }
        self
    }

    /// Skip entries matching any of `patterns`. Invalid patterns are ignored.
    pub fn exclude_all<'a>(self, patterns: impl IntoIterator<Item = &'a str>) -> Self {
        patterns.into_iter().fold(self, Self::exclude)
    }

    /// Restrict entries to a ripgrep file type (e.g. `"rust"`). An unknown type
    /// is ignored.
    pub fn file_type(mut self, name: &str) -> Self {
        self.file_type = FileTypeFilter::new(name);
        self
    }

    fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty() && self.file_type.is_none()
    }

    /// Whether the walker would observe the entry at `relative` (relative to
    /// the scope root).
    pub fn matches(&self, relative: &Path) -> bool {
        if !self.include.is_empty() && !self.include.iter().any(|p| p.matches_path(relative)) {
            return false;
        }
        if self.exclude.iter().any(|p| p.matches_path(relative)) {
            return false;
        }
        self.file_type
            .as_ref()
            .is_none_or(|file_type| file_type.matches(relative))
    }
}

/// A compiled ripgrep file-type selection, compared by name.
#[derive(Clone)]
struct FileTypeFilter {
    name: String,
    types: Arc<Types>,
}

impl FileTypeFilter {
    fn new(name: &str) -> Option<Self> {
        let mut builder = TypesBuilder::new();
        builder.add_defaults();
        builder.select(name);
        let types = builder.build().ok()?;
        Some(Self {
            name: name.to_string(),
            types: Arc::new(types),
        })
    }

    fn matches(&self, relative: &Path) -> bool {
        // Same rule as the walker: a selected type whitelists matching files
        // and ignores everything else.
        !self.types.matched(relative, false).is_ignore()
    }
}

impl PartialEq for FileTypeFilter {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for FileTypeFilter {}

impl fmt::Debug for FileTypeFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FileTypeFilter").field(&self.name).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(filter: ReadFilter) -> ReadScope {
        ReadScope::filtered(PathBuf::from("/ws/src"), filter)
    }

    #[test]
    fn unfiltered_scope_observes_everything_beneath_and_ancestors() {
        let all = ReadScope::all(PathBuf::from("/ws/src"));
        assert!(all.observes_write(Path::new("/ws/src")));
        assert!(all.observes_write(Path::new("/ws/src/a/b.rs")));
        assert!(all.observes_write(Path::new("/ws")));
        assert!(!all.observes_write(Path::new("/ws/src2/a.rs")));
        assert!(!all.observes_write(Path::new("/ws/other.rs")));
    }

    #[test]
    fn include_narrows_the_scope() {
        let md = scope(ReadFilter::new().include("*.md"));
        assert!(md.observes_write(Path::new("/ws/src/a.md")));
        // `glob::Pattern` lets `*` cross separators, exactly like the tools.
        assert!(md.observes_write(Path::new("/ws/src/docs/a.md")));
        assert!(!md.observes_write(Path::new("/ws/src/a.rs")));
    }

    #[test]
    fn exclude_narrows_the_scope() {
        let no_target = scope(ReadFilter::new().exclude("target/**"));
        assert!(no_target.observes_write(Path::new("/ws/src/a.rs")));
        assert!(!no_target.observes_write(Path::new("/ws/src/target/debug/x")));
    }

    #[test]
    fn file_type_narrows_the_scope() {
        let rust = scope(ReadFilter::new().file_type("rust"));
        assert!(rust.observes_write(Path::new("/ws/src/lib.rs")));
        assert!(!rust.observes_write(Path::new("/ws/src/notes.md")));
    }

    #[test]
    fn writes_to_the_scope_root_or_ancestors_ignore_the_filter() {
        // e.g. `search_text` over a single file, or deleting the searched dir.
        let md = scope(ReadFilter::new().include("*.md"));
        assert!(md.observes_write(Path::new("/ws/src")));
        assert!(md.observes_write(Path::new("/ws")));
    }

    #[test]
    fn tree_writes_beneath_a_filtered_scope_are_always_observed() {
        let md = scope(ReadFilter::new().include("*.md"));
        // Removing `src/docs` may remove `src/docs/a.md` even though `docs`
        // itself does not match `*.md`.
        assert!(!md.observes_write(Path::new("/ws/src/docs")));
        assert!(md.observes_tree_write(Path::new("/ws/src/docs")));
        assert!(md.observes_tree_write(Path::new("/ws")));
        assert!(!md.observes_tree_write(Path::new("/ws/src2")));
    }

    #[test]
    fn invalid_or_unknown_filters_fall_back_to_broader_scopes() {
        let bad = ReadFilter::new()
            .include("[")
            .exclude("[")
            .file_type("no-such-type");
        assert_eq!(scope(bad), ReadScope::all(PathBuf::from("/ws/src")));
    }
}

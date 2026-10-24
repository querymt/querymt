//! Confined discovery of manifest-selected regular files.
//!
//! Discovery answers "which files does the manifest select under the
//! authorized root?" It reads **metadata only** (never file contents), so the
//! warm-refresh fast path can later compare stored `(size, mtime, file id)`
//! against the stamps captured here without opening any file.
//!
//! Safety properties:
//!
//! * The walk never follows symlinked directories, so traversal can only stay
//!   inside the canonical root. `..` is already rejected in manifest patterns.
//! * A selected symlink to a file is accepted only when its canonical target is
//!   inside the root, is a regular file, is not excluded by the manifest, is not
//!   under `.git`, and was itself visited by the walk (so a symlink cannot be
//!   used to reach a Git-ignored file such as `.env`). Anything else is
//!   rejected with a diagnostic that names only the repository-relative path.
//! * `.git` is never entered. Git-ignored content is excluded unless the host
//!   sets `allow_ignored`. Only portable ignore rules apply: `.gitignore` files
//!   inside the root. Machine-local rules (global excludes, `.git/info/exclude`,
//!   `.ignore`, and `.gitignore` files above the root) are deliberately not
//!   consulted, so two clones discover the same files.
//! * Hidden directories such as `.agents/memories` are walked when selected.
//! * File count, per-file bytes, and total bytes are bounded. Anything that is
//!   excluded or truncated is disclosed in [`Discovery::diagnostics`]; a
//!   non-empty diagnostic list means coverage is incomplete.
//!
//! Output is sorted by repository-relative path and truncation is applied in
//! that order, so results do not depend on directory iteration order.

use super::manifest::KnowledgeManifest;
use super::root::ResolvedRoot;
use crate::config::RepositoryKnowledgeSettings;
use glob::{MatchOptions, Pattern};
use ignore::WalkBuilder;
use std::collections::{BTreeMap, HashSet};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// Host-controlled discovery limits and policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryOptions {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    /// Include Git-ignored files that the manifest selects.
    pub allow_ignored: bool,
}

impl From<&RepositoryKnowledgeSettings> for DiscoveryOptions {
    fn from(settings: &RepositoryKnowledgeSettings) -> Self {
        Self {
            max_files: settings.max_files,
            max_file_bytes: settings.max_file_bytes,
            max_total_bytes: settings.max_total_bytes,
            allow_ignored: settings.allow_ignored,
        }
    }
}

/// Cheap change-detection metadata captured without reading content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch, when the
    /// platform reports one.
    pub mtime_ns: Option<i64>,
    /// `(device, inode)` where available. `None` means file identity is not
    /// reliable on this platform and callers must fall back to hashing.
    pub file_id: Option<(u64, u64)>,
}

impl FileStamp {
    pub(super) fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            size: metadata.len(),
            mtime_ns: metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_nanos()).ok()),
            file_id: file_id(metadata),
        }
    }
}

#[cfg(unix)]
fn file_id(metadata: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_id(_metadata: &Metadata) -> Option<(u64, u64)> {
    None
}

/// A selected, confined regular file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    /// Forward-slash path relative to the knowledge root. This is the portable
    /// identity used in citations; it never contains machine-local text.
    pub relative_path: String,
    /// Canonical absolute path to read from. Machine-local; never emit it.
    pub canonical_path: PathBuf,
    pub stamp: FileStamp,
    /// Selected by `[documents]`.
    pub is_document: bool,
    /// Selected by `[code]`.
    pub is_code: bool,
    /// Reached through a symlink inside the root.
    pub via_symlink: bool,
}

/// A source-specific or coverage diagnostic. Messages never contain absolute
/// paths.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DiscoveryDiagnostic {
    /// Repository-relative source path, when the diagnostic is about one file.
    pub path: Option<String>,
    /// Stable machine-readable code.
    pub code: &'static str,
    pub message: String,
}

impl DiscoveryDiagnostic {
    fn source(path: &str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            path: Some(path.to_string()),
            code,
            message: message.into(),
        }
    }
}

/// Diagnostic codes emitted by discovery.
pub mod codes {
    pub const ESCAPES_ROOT: &str = "source_escapes_root";
    pub const NOT_REGULAR: &str = "source_not_regular";
    pub const TOO_LARGE: &str = "source_too_large";
    pub const UNREADABLE: &str = "source_unreadable";
    pub const NOT_UTF8: &str = "source_path_not_utf8";
    pub const SYMLINK_UNAVAILABLE: &str = "source_symlink_unavailable";
    pub const FILE_LIMIT: &str = "file_limit_reached";
    pub const TOTAL_BYTES_LIMIT: &str = "total_bytes_limit_reached";
    pub const WALK_ERROR: &str = "walk_error";
}

/// Result of a discovery pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Discovery {
    /// Accepted files, sorted by `relative_path`, each exactly once.
    pub files: Vec<DiscoveredFile>,
    /// Sorted by `(path, code, message)`.
    pub diagnostics: Vec<DiscoveryDiagnostic>,
    /// Sum of accepted file sizes.
    pub total_bytes: u64,
}

impl Discovery {
    /// True when every selected file was accepted. Every diagnostic describes
    /// something that was excluded or truncated, so any diagnostic means the
    /// answer must not be presented as exhaustive.
    pub fn is_complete(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    relative_path: String,
    canonical_path: PathBuf,
    stamp: FileStamp,
    is_document: bool,
    is_code: bool,
    via_symlink: bool,
}

struct PendingSymlink {
    relative_path: String,
    path: PathBuf,
    is_document: bool,
    is_code: bool,
}

/// Discover selected files under `root`. Never reads file contents and never
/// writes anything.
pub fn discover(
    root: &ResolvedRoot,
    manifest: &KnowledgeManifest,
    options: &DiscoveryOptions,
) -> Discovery {
    let root_path = root.root();
    let prune = Arc::new(PruneIndex::new(
        manifest
            .documents
            .include_patterns()
            .chain(manifest.code.include_patterns()),
    ));
    if prune.is_empty() {
        // Nothing is selected: do not walk the tree at all.
        return Discovery::default();
    }

    let mut diagnostics: Vec<DiscoveryDiagnostic> = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut pending: Vec<PendingSymlink> = Vec::new();
    // Every non-ignored regular file the walk visited. Used to prove that a
    // symlink target was reachable without bypassing ignore rules.
    let mut seen: HashSet<String> = HashSet::new();

    let mut builder = WalkBuilder::new(root_path);
    builder
        .hidden(false)
        .follow_links(false)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .require_git(false)
        .git_ignore(!options.allow_ignored);
    {
        let prune = Arc::clone(&prune);
        let walk_root = root_path.to_path_buf();
        builder.filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_name() == ".git" {
                return false;
            }
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                return match relative_segments(&walk_root, entry.path()) {
                    Some(segments) => prune.may_contain(&segments),
                    // Non-UTF-8 directory names cannot match any pattern.
                    None => false,
                };
            }
            true
        });
    }

    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                let reason = err
                    .io_error()
                    .map(|e| e.kind().to_string())
                    .unwrap_or_else(|| "walk failure".to_string());
                diagnostics.push(DiscoveryDiagnostic {
                    path: None,
                    code: codes::WALK_ERROR,
                    message: format!("a directory could not be read ({reason})"),
                });
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            continue;
        }

        let Some(segments) = relative_segments(root_path, entry.path()) else {
            // Only report non-UTF-8 names that could plausibly have been
            // selected; there is no portable string to match against.
            diagnostics.push(DiscoveryDiagnostic {
                path: None,
                code: codes::NOT_UTF8,
                message: "a path with a non-UTF-8 name was skipped".to_string(),
            });
            continue;
        };
        let relative = segments.join("/");

        if file_type.is_file() {
            seen.insert(relative.clone());
        }
        let is_document = manifest.documents.is_selected(&relative);
        let is_code = manifest.code.is_selected(&relative);
        if !is_document && !is_code {
            continue;
        }

        if file_type.is_symlink() {
            pending.push(PendingSymlink {
                relative_path: relative,
                path: entry.path().to_path_buf(),
                is_document,
                is_code,
            });
            continue;
        }
        if !file_type.is_file() {
            diagnostics.push(DiscoveryDiagnostic::source(
                &relative,
                codes::NOT_REGULAR,
                "selected source is not a regular file",
            ));
            continue;
        }
        match entry.metadata() {
            Ok(metadata) => candidates.push(Candidate {
                relative_path: relative,
                canonical_path: entry.path().to_path_buf(),
                stamp: FileStamp::from_metadata(&metadata),
                is_document,
                is_code,
                via_symlink: false,
            }),
            Err(_) => diagnostics.push(DiscoveryDiagnostic::source(
                &relative,
                codes::UNREADABLE,
                "selected source metadata could not be read",
            )),
        }
    }

    for link in pending {
        resolve_symlink(
            root_path,
            manifest,
            &seen,
            link,
            &mut candidates,
            &mut diagnostics,
        );
    }

    let files = dedupe(candidates);
    let (files, total_bytes) = apply_budgets(files, options, &mut diagnostics);
    diagnostics.sort();
    diagnostics.dedup();
    Discovery {
        files,
        diagnostics,
        total_bytes,
    }
}

fn resolve_symlink(
    root: &Path,
    manifest: &KnowledgeManifest,
    seen: &HashSet<String>,
    link: PendingSymlink,
    candidates: &mut Vec<Candidate>,
    diagnostics: &mut Vec<DiscoveryDiagnostic>,
) {
    let PendingSymlink {
        relative_path,
        path,
        is_document,
        is_code,
    } = link;

    let target = match path.canonicalize() {
        Ok(target) => target,
        Err(_) => {
            diagnostics.push(DiscoveryDiagnostic::source(
                &relative_path,
                codes::UNREADABLE,
                "selected symlink could not be resolved",
            ));
            return;
        }
    };
    let Ok(target_relative) = target.strip_prefix(root) else {
        diagnostics.push(DiscoveryDiagnostic::source(
            &relative_path,
            codes::ESCAPES_ROOT,
            "selected symlink resolves outside the knowledge root",
        ));
        return;
    };
    let metadata = match std::fs::metadata(&target) {
        Ok(metadata) => metadata,
        Err(_) => {
            diagnostics.push(DiscoveryDiagnostic::source(
                &relative_path,
                codes::UNREADABLE,
                "selected symlink target could not be read",
            ));
            return;
        }
    };
    if metadata.is_dir() {
        // A symlinked directory is never descended into and is not a document.
        return;
    }
    if !metadata.is_file() {
        diagnostics.push(DiscoveryDiagnostic::source(
            &relative_path,
            codes::NOT_REGULAR,
            "selected symlink target is not a regular file",
        ));
        return;
    }

    let Some(target_rel) = target_relative
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.join("/"))
    else {
        diagnostics.push(DiscoveryDiagnostic::source(
            &relative_path,
            codes::NOT_UTF8,
            "selected symlink target has a non-UTF-8 path",
        ));
        return;
    };

    // The target must not be in `.git`, must not be excluded for the kind the
    // link was selected as, and must have been visited by the (ignore-aware)
    // walk. The last condition is what stops `docs/x.md -> ../.env`.
    let in_git = target_rel == ".git" || target_rel.starts_with(".git/");
    let is_document = is_document && !manifest.documents.is_excluded(&target_rel);
    let is_code = is_code && !manifest.code.is_excluded(&target_rel);
    if in_git || !seen.contains(&target_rel) || !(is_document || is_code) {
        diagnostics.push(DiscoveryDiagnostic::source(
            &relative_path,
            codes::SYMLINK_UNAVAILABLE,
            "selected symlink target is excluded, ignored, or not reachable by discovery",
        ));
        return;
    }

    candidates.push(Candidate {
        relative_path,
        canonical_path: target,
        stamp: FileStamp::from_metadata(&metadata),
        is_document,
        is_code,
        via_symlink: true,
    });
}

/// Collapse candidates that name the same file. A real file wins over a symlink
/// alias of it; otherwise the lexicographically smallest path wins. Selection
/// kinds are merged, so one file yields one repository projection.
fn dedupe(candidates: Vec<Candidate>) -> Vec<DiscoveredFile> {
    let mut by_file: BTreeMap<PathBuf, Candidate> = BTreeMap::new();
    for candidate in candidates {
        match by_file.get_mut(&candidate.canonical_path) {
            None => {
                by_file.insert(candidate.canonical_path.clone(), candidate);
            }
            Some(existing) => {
                let incoming_wins = (existing.via_symlink && !candidate.via_symlink)
                    || (existing.via_symlink == candidate.via_symlink
                        && candidate.relative_path < existing.relative_path);
                let is_document = existing.is_document || candidate.is_document;
                let is_code = existing.is_code || candidate.is_code;
                if incoming_wins {
                    *existing = candidate;
                }
                existing.is_document = is_document;
                existing.is_code = is_code;
            }
        }
    }
    let mut files: Vec<DiscoveredFile> = by_file
        .into_values()
        .map(|c| DiscoveredFile {
            relative_path: c.relative_path,
            canonical_path: c.canonical_path,
            stamp: c.stamp,
            is_document: c.is_document,
            is_code: c.is_code,
            via_symlink: c.via_symlink,
        })
        .collect();
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    files
}

/// Apply per-file, count, and total-byte budgets in path order. An over-sized
/// file is excluded individually. Once the count or total budget is reached,
/// every later file is skipped, so coverage is a deterministic prefix.
fn apply_budgets(
    files: Vec<DiscoveredFile>,
    options: &DiscoveryOptions,
    diagnostics: &mut Vec<DiscoveryDiagnostic>,
) -> (Vec<DiscoveredFile>, u64) {
    let mut accepted: Vec<DiscoveredFile> = Vec::new();
    let mut total: u64 = 0;
    let mut truncation: Option<&'static str> = None;
    let mut skipped: usize = 0;

    for file in files {
        if truncation.is_some() {
            skipped += 1;
            continue;
        }
        let size = file.stamp.size;
        if size > options.max_file_bytes {
            diagnostics.push(DiscoveryDiagnostic::source(
                &file.relative_path,
                codes::TOO_LARGE,
                format!(
                    "selected source is {size} bytes, over the {} byte per-file limit",
                    options.max_file_bytes
                ),
            ));
            continue;
        }
        if accepted.len() >= options.max_files {
            truncation = Some(codes::FILE_LIMIT);
            skipped += 1;
            continue;
        }
        if total.saturating_add(size) > options.max_total_bytes {
            truncation = Some(codes::TOTAL_BYTES_LIMIT);
            skipped += 1;
            continue;
        }
        total += size;
        accepted.push(file);
    }

    if let Some(code) = truncation {
        let limit = if code == codes::FILE_LIMIT {
            format!("{} file", options.max_files)
        } else {
            format!("{} byte total", options.max_total_bytes)
        };
        diagnostics.push(DiscoveryDiagnostic {
            path: None,
            code,
            message: format!(
                "{skipped} selected source(s) were not indexed after reaching the {limit} limit"
            ),
        });
    }
    (accepted, total)
}

/// Components of `path` below `root` as UTF-8 segments, or `None` when any
/// segment is not valid UTF-8 or the path is not below `root`.
fn relative_segments(root: &Path, path: &Path) -> Option<Vec<String>> {
    path.strip_prefix(root)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect()
}

// ─── Directory pruning ───────────────────────────────────────────────────────

const PRUNE_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

enum Segment {
    /// `**`: matches any number of directories.
    Recursive,
    Glob(Pattern),
    /// A segment that failed to compile; never prunes.
    Unknown,
}

/// Decides whether a directory can contain any file matched by an include
/// pattern, so the walk skips subtrees (`target/`, `node_modules/`, ...) that
/// cannot contribute. This is an optimization only: it must never prune a
/// directory that holds a selected file.
struct PruneIndex {
    patterns: Vec<Vec<Segment>>,
}

impl PruneIndex {
    fn new<'a>(includes: impl Iterator<Item = &'a str>) -> Self {
        let patterns = includes
            .map(|pattern| {
                pattern
                    .split('/')
                    .map(|segment| {
                        if segment == "**" {
                            Segment::Recursive
                        } else {
                            Pattern::new(segment)
                                .map(Segment::Glob)
                                .unwrap_or(Segment::Unknown)
                        }
                    })
                    .collect()
            })
            .collect();
        Self { patterns }
    }

    fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    fn may_contain(&self, dir: &[String]) -> bool {
        self.patterns.iter().any(|pattern| {
            for (index, name) in dir.iter().enumerate() {
                match pattern.get(index) {
                    // Pattern ended above this depth: nothing below can match.
                    None => return false,
                    Some(Segment::Recursive | Segment::Unknown) => return true,
                    Some(Segment::Glob(glob)) => {
                        if !glob.matches_with(name, PRUNE_OPTIONS) {
                            return false;
                        }
                    }
                }
            }
            // The directory matched the pattern's prefix. Files can only live
            // below it if the pattern has at least one more segment.
            pattern.len() > dir.len()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::root::{RootRequest, resolve_root};
    use std::fs;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        (tmp, repo)
    }

    fn manifest(toml: &str) -> KnowledgeManifest {
        KnowledgeManifest::parse(toml).unwrap()
    }

    fn options() -> DiscoveryOptions {
        DiscoveryOptions {
            max_files: 1_000,
            max_file_bytes: 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            allow_ignored: false,
        }
    }

    fn run(repo: &Path, manifest: &KnowledgeManifest, options: &DiscoveryOptions) -> Discovery {
        let root = resolve_root(&RootRequest {
            cwd: Some(repo),
            ..Default::default()
        })
        .unwrap();
        discover(&root, manifest, options)
    }

    fn paths(d: &Discovery) -> Vec<&str> {
        d.files.iter().map(|f| f.relative_path.as_str()).collect()
    }

    fn codes_of(d: &Discovery) -> Vec<&'static str> {
        d.diagnostics.iter().map(|d| d.code).collect()
    }

    #[test]
    fn discovers_selected_files_sorted_with_stamps_and_kinds() {
        let (_tmp, repo) = setup();
        write(&repo, "docs/b.md", "b");
        write(&repo, "docs/a.md", "aa");
        write(&repo, "docs/sub/c.md", "ccc");
        write(&repo, "docs/skip.txt", "no");
        write(&repo, "src/lib.rs", "fn x() {}");
        let m = manifest(
            "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n",
        );
        let d = run(&repo, &m, &options());
        assert_eq!(
            paths(&d),
            vec!["docs/a.md", "docs/b.md", "docs/sub/c.md", "src/lib.rs"]
        );
        assert!(d.is_complete());
        assert_eq!(d.total_bytes, 2 + 1 + 3 + 9);
        let a = &d.files[0];
        assert_eq!(a.stamp.size, 2);
        assert!(a.stamp.mtime_ns.is_some());
        assert!(a.is_document && !a.is_code && !a.via_symlink);
        assert!(d.files[3].is_code && !d.files[3].is_document);
        assert!(a.canonical_path.is_absolute());
    }

    #[test]
    fn empty_selection_walks_nothing_and_reports_nothing() {
        let (_tmp, repo) = setup();
        write(&repo, "docs/a.md", "a");
        let d = run(&repo, &manifest("version = 1\n"), &options());
        assert_eq!(d, Discovery::default());
        assert!(d.is_complete());
    }

    #[test]
    fn selected_hidden_agents_memories_are_discovered() {
        let (_tmp, repo) = setup();
        write(&repo, ".agents/memories/runtime.md", "mem");
        write(&repo, ".agents/memories/nested/deep.md", "deep");
        write(&repo, ".agents/other.md", "other");
        let m = manifest("version = 1\n[documents]\ninclude = [\".agents/memories/*.md\"]\n");
        let d = run(&repo, &m, &options());
        assert_eq!(paths(&d), vec![".agents/memories/runtime.md"]);
        // Discovery never writes under .agents/.
        let mut entries: Vec<_> = fs::read_dir(repo.join(".agents"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        entries.sort();
        assert_eq!(entries, vec!["memories", "other.md"]);
    }

    #[test]
    fn git_directory_is_never_entered() {
        let (_tmp, repo) = setup();
        write(&repo, ".git/config", "[core]");
        write(&repo, ".git/notes.md", "secret");
        write(&repo, "README.md", "hi");
        let m = manifest("version = 1\n[documents]\ninclude = [\"**/*\", \".git/**\"]\n");
        let d = run(&repo, &m, &options());
        assert_eq!(paths(&d), vec!["README.md"]);
    }

    #[test]
    fn gitignored_files_are_excluded_unless_the_host_allows_them() {
        let (_tmp, repo) = setup();
        write(&repo, ".gitignore", ".env\nsecrets/\n*.local.md\n");
        write(&repo, ".env", "TOKEN=1");
        write(&repo, "secrets/key.md", "key");
        write(&repo, "docs/notes.local.md", "local");
        write(&repo, "docs/ok.md", "ok");
        write(&repo, "docs/nested/.gitignore", "private.md\n");
        write(&repo, "docs/nested/private.md", "private");
        write(&repo, "docs/nested/public.md", "public");
        let m = manifest("version = 1\n[documents]\ninclude = [\"**/*\"]\n");

        let default = run(&repo, &m, &options());
        let found = paths(&default);
        assert!(found.contains(&"docs/ok.md"));
        assert!(found.contains(&"docs/nested/public.md"));
        for hidden in [
            ".env",
            "secrets/key.md",
            "docs/notes.local.md",
            "docs/nested/private.md",
        ] {
            assert!(!found.contains(&hidden), "{hidden} must be ignored");
        }

        let allowed = run(
            &repo,
            &m,
            &DiscoveryOptions {
                allow_ignored: true,
                ..options()
            },
        );
        let found = paths(&allowed);
        for visible in [".env", "secrets/key.md", "docs/notes.local.md"] {
            assert!(found.contains(&visible), "{visible} must be allowed");
        }
    }

    #[test]
    fn machine_local_ignore_rules_are_not_consulted() {
        let (_tmp, repo) = setup();
        write(&repo, ".git/info/exclude", "docs/local-only.md\n");
        write(&repo, ".ignore", "docs/dot-ignore.md\n");
        write(&repo, "docs/local-only.md", "x");
        write(&repo, "docs/dot-ignore.md", "x");
        let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
        let d = run(&repo, &m, &options());
        // A clone without this machine's private excludes must see the same set.
        assert_eq!(paths(&d), vec!["docs/dot-ignore.md", "docs/local-only.md"]);
    }

    #[test]
    fn excludes_win_and_overlapping_patterns_yield_one_file() {
        let (_tmp, repo) = setup();
        write(&repo, "docs/a.md", "a");
        write(&repo, "docs/archive/old.md", "old");
        let m = manifest(
            "version = 1\n[documents]\ninclude = [\"docs/**/*.md\", \"docs/a.md\", \"**/*.md\"]\nexclude = [\"docs/archive/**\"]\n",
        );
        let d = run(&repo, &m, &options());
        assert_eq!(paths(&d), vec!["docs/a.md"]);
    }

    #[test]
    fn file_selected_as_document_and_code_is_one_record_with_both_kinds() {
        let (_tmp, repo) = setup();
        write(&repo, "tools/run.md", "x");
        let m = manifest(
            "version = 1\n[documents]\ninclude = [\"tools/*\"]\n[code]\ninclude = [\"tools/*.md\"]\n",
        );
        let d = run(&repo, &m, &options());
        assert_eq!(d.files.len(), 1);
        assert!(d.files[0].is_document && d.files[0].is_code);
    }

    #[test]
    fn per_file_limit_excludes_only_that_file_and_reports_incomplete() {
        let (_tmp, repo) = setup();
        write(&repo, "docs/small.md", "tiny");
        write(&repo, "docs/big.md", &"x".repeat(100));
        let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
        let d = run(
            &repo,
            &m,
            &DiscoveryOptions {
                max_file_bytes: 10,
                ..options()
            },
        );
        assert_eq!(paths(&d), vec!["docs/small.md"]);
        assert!(!d.is_complete());
        assert_eq!(codes_of(&d), vec![codes::TOO_LARGE]);
        assert_eq!(d.diagnostics[0].path.as_deref(), Some("docs/big.md"));
    }

    #[test]
    fn file_count_limit_keeps_a_deterministic_prefix() {
        let (_tmp, repo) = setup();
        // Created out of order on purpose.
        for name in ["d", "b", "e", "a", "c"] {
            write(&repo, &format!("docs/{name}.md"), name);
        }
        let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
        let d = run(
            &repo,
            &m,
            &DiscoveryOptions {
                max_files: 3,
                ..options()
            },
        );
        assert_eq!(paths(&d), vec!["docs/a.md", "docs/b.md", "docs/c.md"]);
        assert_eq!(codes_of(&d), vec![codes::FILE_LIMIT]);
        assert!(d.diagnostics[0].message.starts_with("2 selected"));
        assert!(!d.is_complete());
    }

    #[test]
    fn total_byte_limit_truncates_in_path_order() {
        let (_tmp, repo) = setup();
        write(&repo, "docs/a.md", &"a".repeat(40));
        write(&repo, "docs/b.md", &"b".repeat(40));
        write(&repo, "docs/c.md", "c");
        let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
        let d = run(
            &repo,
            &m,
            &DiscoveryOptions {
                max_total_bytes: 60,
                ..options()
            },
        );
        // `c.md` would fit, but coverage is a prefix so results never depend on
        // which small files happen to follow a large one.
        assert_eq!(paths(&d), vec!["docs/a.md"]);
        assert_eq!(codes_of(&d), vec![codes::TOTAL_BYTES_LIMIT]);
        assert_eq!(d.total_bytes, 40);
    }

    #[test]
    fn zero_budgets_discover_nothing_and_say_so() {
        let (_tmp, repo) = setup();
        write(&repo, "docs/a.md", "a");
        let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
        let d = run(
            &repo,
            &m,
            &DiscoveryOptions {
                max_files: 0,
                ..options()
            },
        );
        assert!(d.files.is_empty());
        assert_eq!(codes_of(&d), vec![codes::FILE_LIMIT]);
    }

    #[test]
    fn results_are_independent_of_creation_order_and_checkout_location() {
        let m = manifest("version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n");
        let mut results = Vec::new();
        for order in [["a", "b", "c"], ["c", "a", "b"]] {
            let (_tmp, repo) = setup();
            for name in order {
                write(&repo, &format!("docs/{name}.md"), name);
            }
            let d = run(&repo, &m, &options());
            results.push(
                d.files
                    .iter()
                    .map(|f| (f.relative_path.clone(), f.stamp.size))
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(results[0], results[1]);
    }

    #[test]
    fn diagnostics_never_leak_absolute_paths() {
        let (tmp, repo) = setup();
        write(&repo, "docs/big.md", &"x".repeat(100));
        let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
        let d = run(
            &repo,
            &m,
            &DiscoveryOptions {
                max_file_bytes: 1,
                ..options()
            },
        );
        let rendered = format!("{:?}", d.diagnostics);
        assert!(
            !rendered.contains(tmp.path().to_str().unwrap()),
            "{rendered}"
        );
    }

    #[test]
    fn discovery_does_not_modify_the_checkout() {
        let (_tmp, repo) = setup();
        write(&repo, "docs/a.md", "a");
        let m = manifest("version = 1\n[documents]\ninclude = [\"**/*.md\"]\n");
        let before = tree(&repo);
        let _ = run(&repo, &m, &options());
        assert_eq!(tree(&repo), before);
        assert!(!repo.join(".qmt").exists());
    }

    fn tree(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            out.push(path.clone());
            if path.is_dir() {
                out.extend(tree(&path));
            }
        }
        out.sort();
        out
    }

    #[cfg(unix)]
    mod symlinks {
        use super::*;
        use std::os::unix::fs::symlink;

        #[test]
        fn external_file_symlink_is_rejected_with_a_relative_diagnostic() {
            let (tmp, repo) = setup();
            let outside = tmp.path().join("outside");
            write(&outside, "secret.md", "TOP SECRET");
            write(&repo, "docs/ok.md", "ok");
            fs::create_dir_all(repo.join("docs")).unwrap();
            symlink(outside.join("secret.md"), repo.join("docs/leak.md")).unwrap();

            let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
            let d = run(&repo, &m, &options());
            assert_eq!(paths(&d), vec!["docs/ok.md"]);
            assert_eq!(codes_of(&d), vec![codes::ESCAPES_ROOT]);
            assert_eq!(d.diagnostics[0].path.as_deref(), Some("docs/leak.md"));
            let rendered = format!("{:?}", d.diagnostics);
            assert!(!rendered.contains("secret.md"), "{rendered}");
            assert!(
                !rendered.contains(tmp.path().to_str().unwrap()),
                "{rendered}"
            );
            assert!(!d.is_complete());
        }

        #[test]
        fn symlinked_directories_are_not_traversed() {
            let (tmp, repo) = setup();
            let outside = tmp.path().join("outside");
            write(&outside, "secret.md", "TOP SECRET");
            write(&repo, "docs/ok.md", "ok");
            symlink(&outside, repo.join("docs/ext")).unwrap();
            // A link to a directory inside the root is not descended either, so
            // the same file is never reachable under two names.
            symlink(repo.join("docs"), repo.join("docs/self")).unwrap();

            let m = manifest("version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n");
            let d = run(&repo, &m, &options());
            assert_eq!(paths(&d), vec!["docs/ok.md"]);
            assert!(d.is_complete());
        }

        #[test]
        fn broken_symlink_is_reported_unreadable() {
            let (_tmp, repo) = setup();
            fs::create_dir_all(repo.join("docs")).unwrap();
            symlink(repo.join("docs/missing.md"), repo.join("docs/dangling.md")).unwrap();
            let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
            let d = run(&repo, &m, &options());
            assert!(d.files.is_empty());
            assert_eq!(codes_of(&d), vec![codes::UNREADABLE]);
        }

        #[test]
        fn internal_symlink_to_a_selected_file_is_deduplicated() {
            let (_tmp, repo) = setup();
            write(&repo, "docs/CLAUDE.md", "shared");
            symlink(repo.join("docs/CLAUDE.md"), repo.join("docs/AGENTS.md")).unwrap();
            let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
            let d = run(&repo, &m, &options());
            // One projection per source; the real file wins over its alias.
            assert_eq!(paths(&d), vec!["docs/CLAUDE.md"]);
            assert!(!d.files[0].via_symlink);
            assert!(d.is_complete());
        }

        #[test]
        fn symlink_alias_of_an_unselected_visible_file_is_accepted() {
            let (_tmp, repo) = setup();
            write(&repo, "notes/real.md", "real");
            fs::create_dir_all(repo.join("docs")).unwrap();
            symlink(repo.join("notes/real.md"), repo.join("docs/alias.md")).unwrap();
            let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
            // `notes/` is pruned from the walk, so the target was not visited.
            let pruned = run(&repo, &m, &options());
            assert!(pruned.files.is_empty());
            assert_eq!(codes_of(&pruned), vec![codes::SYMLINK_UNAVAILABLE]);

            // When the target's directory is walked, the alias is accepted.
            let m =
                manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\", \"notes/*.txt\"]\n");
            let walked = run(&repo, &m, &options());
            assert_eq!(paths(&walked), vec!["docs/alias.md"]);
            assert!(walked.files[0].via_symlink);
            assert!(walked.files[0].canonical_path.ends_with("notes/real.md"));
        }

        #[test]
        fn symlink_cannot_reach_a_gitignored_or_excluded_file() {
            let (_tmp, repo) = setup();
            write(&repo, ".gitignore", ".env\n");
            write(&repo, ".env", "TOKEN=1");
            write(&repo, "private/plan.md", "plan");
            fs::create_dir_all(repo.join("docs")).unwrap();
            symlink(repo.join(".env"), repo.join("docs/env.md")).unwrap();
            symlink(repo.join("private/plan.md"), repo.join("docs/plan.md")).unwrap();
            symlink(repo.join(".git"), repo.join("docs/git.md")).unwrap();

            let m = manifest(
                "version = 1\n[documents]\ninclude = [\"docs/*.md\", \"private/*.md\", \"*\"]\nexclude = [\"private/**\"]\n",
            );
            let d = run(&repo, &m, &options());
            assert!(
                d.files
                    .iter()
                    .all(|f| f.canonical_path.ends_with(".gitignore")
                        || !f.relative_path.starts_with("docs/")),
                "{:?}",
                paths(&d)
            );
            let rejected: Vec<_> = d
                .diagnostics
                .iter()
                .filter_map(|d| d.path.as_deref())
                .collect();
            assert!(rejected.contains(&"docs/env.md"), "{rejected:?}");
            assert!(rejected.contains(&"docs/plan.md"), "{rejected:?}");
            assert!(!d.files.iter().any(|f| f.relative_path == ".env"));
        }

        #[test]
        fn fifo_selected_by_pattern_is_not_a_regular_file() {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let (_tmp, repo) = setup();
            fs::create_dir_all(repo.join("docs")).unwrap();
            let fifo = repo.join("docs/pipe.md");
            let c_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
            // SAFETY: `c_path` is a valid NUL-terminated path for the call.
            let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
            assert_eq!(rc, 0);
            write(&repo, "docs/ok.md", "ok");
            let m = manifest("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n");
            let d = run(&repo, &m, &options());
            assert_eq!(paths(&d), vec!["docs/ok.md"]);
            assert_eq!(codes_of(&d), vec![codes::NOT_REGULAR]);
            assert_eq!(d.diagnostics[0].path.as_deref(), Some("docs/pipe.md"));
        }
    }

    mod pruning {
        use super::*;

        fn index(patterns: &[&str]) -> PruneIndex {
            PruneIndex::new(patterns.iter().copied())
        }

        fn dir(path: &str) -> Vec<String> {
            path.split('/').map(str::to_string).collect()
        }

        #[test]
        fn prunes_only_directories_that_cannot_hold_matches() {
            let idx = index(&["crates/*/src/**/*.rs", "docs/*.md", ".agents/memories/*.md"]);
            assert!(idx.may_contain(&dir("crates")));
            assert!(idx.may_contain(&dir("crates/agent")));
            assert!(idx.may_contain(&dir("crates/agent/src")));
            assert!(idx.may_contain(&dir("crates/agent/src/deep/er")));
            assert!(!idx.may_contain(&dir("crates/agent/tests")));
            assert!(idx.may_contain(&dir("docs")));
            // `docs/*.md` has no `**`: nothing below `docs/x` can match.
            assert!(!idx.may_contain(&dir("docs/nested")));
            assert!(idx.may_contain(&dir(".agents")));
            assert!(idx.may_contain(&dir(".agents/memories")));
            assert!(!idx.may_contain(&dir(".agents/memories/nested")));
            assert!(!idx.may_contain(&dir("target")));
            assert!(!idx.may_contain(&dir("node_modules")));
        }

        #[test]
        fn leading_double_star_never_prunes() {
            let idx = index(&["**/AGENTS.md"]);
            assert!(idx.may_contain(&dir("anything")));
            assert!(idx.may_contain(&dir("a/b/c/d")));
        }

        #[test]
        fn a_pattern_equal_to_the_directory_does_not_keep_it() {
            // `docs` as a pattern matches the path `docs` itself, not files in it.
            let idx = index(&["docs"]);
            assert!(!idx.may_contain(&dir("docs")));
        }

        #[test]
        fn pruning_does_not_change_what_is_discovered() {
            let (_tmp, repo) = setup();
            write(&repo, "crates/a/src/x/lib.rs", "1");
            write(&repo, "crates/a/tests/t.rs", "2");
            write(&repo, "crates/b/src/lib.rs", "3");
            write(&repo, "target/debug/junk.rs", "4");
            let m = manifest("version = 1\n[code]\ninclude = [\"crates/*/src/**/*.rs\"]\n");
            let d = run(&repo, &m, &options());
            assert_eq!(
                paths(&d),
                vec!["crates/a/src/x/lib.rs", "crates/b/src/lib.rs"]
            );
        }
    }
}

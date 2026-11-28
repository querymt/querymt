//! Stat-based helpers shared by baseline capture and its validation.
//!
//! - [`reuse_predicate`]: when a tracked file's real-index entry may stand in
//!   for reading the file (design D2).
//! - [`ConversionDetector`]: whether a path's attributes or configuration
//!   convert content between the index and the working tree.
//! - [`walk_untracked_and_ignored`]: the ignore-aware directory walk.
//! - [`fingerprint_source`] and [`verify_captured_tree`]: the stat-fingerprint
//!   manifest and the racy re-hash of capture validation (design D3).

use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::repo_ops::{
    AttributeSource, DirwalkDisposition, FlatTree, classify_dirwalk_entry, flatten_tree,
    gix_pathbuf, native_err, open_checkout,
};
use filetime::FileTime;
use gix::bstr::{BStr, BString};
use gix::index::entry::{Flags, Mode, Stat, stat::Options as StatOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

/// The snapshot store's racy window: a captured file modified this recently
/// before the attempt started is re-hashed after the capture.
pub(crate) const RACY_WINDOW_SECS: i64 = 2;

/// Crate-private marker for a capture attempt that observed the source
/// changing underneath it (a vanished file, a large file that changed
/// between its hash and its write). It travels through the public
/// `RepositoryOps` boundary inside `WorktreeError::Io`, so `WorktreeError`
/// gains no variant, and the validated capture recognises it by type rather
/// than by message text.
#[derive(Debug)]
struct ChangedDuringCapture(String);

impl std::fmt::Display for ChangedDuringCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "source changed during capture: {}", self.0)
    }
}

impl std::error::Error for ChangedDuringCapture {}

pub(crate) fn changed_during_capture(detail: impl std::fmt::Display) -> WorktreeError {
    WorktreeError::Io(std::io::Error::other(ChangedDuringCapture(
        detail.to_string(),
    )))
}

/// What changed, when `err` reports a change during capture; the validated
/// capture retries on it.
pub(crate) fn changed_during_capture_detail(err: &WorktreeError) -> Option<&str> {
    let WorktreeError::Io(io) = err else {
        return None;
    };
    io.get_ref()?
        .downcast_ref::<ChangedDuringCapture>()
        .map(|changed| changed.0.as_str())
}

/// Map an I/O error while reading a candidate file: a vanished file is a
/// change during capture, anything else a native error.
pub(crate) fn read_err(err: std::io::Error, path: &Path) -> WorktreeError {
    if err.kind() == std::io::ErrorKind::NotFound {
        changed_during_capture(format!("{} vanished", path.display()))
    } else {
        native_err(format!("{}: {err}", path.display()))
    }
}

/// Regular file or symbolic link: the two kinds baselines record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
    File,
    Symlink,
}

pub(crate) fn index_kind(mode: Mode) -> Option<NodeKind> {
    if mode == Mode::SYMLINK {
        Some(NodeKind::Symlink)
    } else if mode.contains(Mode::FILE) {
        Some(NodeKind::File)
    } else {
        None
    }
}

pub(crate) fn disk_kind(md: &gix::index::fs::Metadata) -> Option<NodeKind> {
    if md.is_symlink() {
        Some(NodeKind::Symlink)
    } else if md.is_file() {
        Some(NodeKind::File)
    } else {
        None
    }
}

/// Whether the real-index `entry` (`None`: the path is not in the real index)
/// may stand in for reading the file `md` describes. Pure function of its
/// inputs (design D2, rules 2-5). `blob_size_matches` says the size of the
/// entry's blob (from its object header) equals the file's size on disk; a
/// blob staged under earlier conversion settings keeps matching stat data
/// while its bytes differ from the file's.
pub(crate) fn reuse_predicate(
    entry: Option<&gix::index::Entry>,
    md: &gix::index::fs::Metadata,
    index_timestamp: FileTime,
    opts: StatOptions,
    has_conversion: bool,
    blob_size_matches: bool,
) -> bool {
    let Some(entry) = entry else {
        return false;
    };
    let hidden =
        Flags::SKIP_WORKTREE | Flags::ASSUME_VALID | Flags::FSMONITOR_VALID | Flags::INTENT_TO_ADD;
    if entry.flags.intersects(hidden) || has_conversion || !blob_size_matches {
        return false;
    }
    let Some(kind) = index_kind(entry.mode) else {
        return false;
    };
    if disk_kind(md) != Some(kind) {
        return false;
    }
    let Ok(disk_stat) = Stat::from_fs(md) else {
        return false;
    };
    entry.stat.matches(&disk_stat, opts) && !entry.stat.is_racy(index_timestamp, opts)
}

/// Whether the blob `id` has the size `disk_size`, by object header only (no
/// content read). A failed lookup or a non-blob object is `false`: the file
/// is then read.
pub(crate) fn blob_size_matches(
    repo: &gix::Repository,
    id: gix::hash::ObjectId,
    disk_size: u64,
) -> bool {
    repo.find_header(id)
        .is_ok_and(|header| header.kind() == gix::object::Kind::Blob && header.size() == disk_size)
}

/// Resolves whether a path's content is converted between the index and the
/// working tree: `text`/`crlf`/`eol`/`ident`/`working-tree-encoding`/`filter`
/// attributes, or `core.autocrlf`/`core.eol` applying to a path whose `text`
/// is not unset.
pub(crate) struct ConversionDetector<'repo> {
    stack: gix::AttributeStack<'repo>,
    outcome: gix::attrs::search::Outcome,
    config_converts: bool,
}

impl<'repo> ConversionDetector<'repo> {
    pub(crate) fn new(
        repo: &'repo gix::Repository,
        index: &gix::index::State,
    ) -> WorktreeResult<Self> {
        let stack = repo
            .attributes_only(index, AttributeSource::WorktreeThenIdMapping)
            .map_err(native_err)?;
        let mut outcome = gix::attrs::search::Outcome::default();
        outcome.initialize_with_selection(
            stack.attributes_collection(),
            [
                "text",
                "crlf",
                "eol",
                "ident",
                "working-tree-encoding",
                "filter",
            ],
        );
        let config = repo.config_snapshot();
        // `true` (including a bare `autocrlf` key) and `input` convert; a
        // value that is not a boolean is `input` or unknown, so assume it
        // converts.
        let autocrlf = config.try_boolean("core.autocrlf").unwrap_or(Some(true)) == Some(true);
        let config_converts = autocrlf || config.string("core.eol").is_some();
        Ok(Self {
            stack,
            outcome,
            config_converts,
        })
    }

    pub(crate) fn has_conversion(&mut self, path: &BStr) -> WorktreeResult<bool> {
        let platform = self
            .stack
            .at_path(gix_pathbuf(path), None)
            .map_err(native_err)?;
        self.outcome.reset();
        let mut converts = false;
        let mut text_unset = false;
        if platform.matching_attributes(&mut self.outcome) {
            for matched in self.outcome.iter_selected() {
                let assignment = &matched.assignment;
                match (assignment.name.as_str(), &assignment.state) {
                    ("text" | "crlf", gix::attrs::StateRef::Unset) => text_unset = true,
                    (
                        "text" | "crlf" | "eol" | "ident" | "working-tree-encoding" | "filter",
                        gix::attrs::StateRef::Set | gix::attrs::StateRef::Value(_),
                    ) => converts = true,
                    _ => {}
                }
            }
        }
        Ok(converts || (self.config_converts && !text_unset))
    }
}

/// Paths the ignore-aware directory walk found.
#[derive(Default)]
pub(crate) struct WalkResult {
    pub ignored: Vec<String>,
    pub untracked: Vec<BString>,
}

struct Collect(WalkResult);

impl gix::dir::walk::Delegate for Collect {
    fn emit(
        &mut self,
        entry: gix::dir::EntryRef<'_>,
        _collapsed_directory_status: Option<gix::dir::entry::Status>,
    ) -> gix::dir::walk::Action {
        match classify_dirwalk_entry(&entry.to_owned()) {
            Some(DirwalkDisposition::Ignored(path)) => self.0.ignored.push(path),
            Some(DirwalkDisposition::Untracked(path)) => {
                self.0.untracked.push(path.into_bytes().into())
            }
            None => {}
        }
        std::ops::ControlFlow::Continue(())
    }
}

/// Untracked and ignored files of the working tree, as the status pass found
/// them before. Untracked files are emitted one per file, set explicitly
/// rather than derived from `status.showUntrackedFiles`; ignored directories
/// are not descended, and, as before, only ignored files and symlinks are
/// reported (an ignored directory is not). `index` decides what is tracked; entries the
/// caller verified against the disk carry [`Flags::UPTODATE`].
pub(crate) fn walk_untracked_and_ignored(
    repo: &gix::Repository,
    index: &gix::index::State,
) -> WorktreeResult<WalkResult> {
    let mut options = repo.dirwalk_options().map_err(native_err)?;
    options.set_emit_untracked(gix::dir::walk::EmissionMode::Matching);
    options.set_emit_ignored(Some(gix::dir::walk::EmissionMode::Matching));
    let mut delegate = Collect(WalkResult::default());
    let interrupt = AtomicBool::new(false);
    repo.dirwalk(
        index,
        Vec::<BString>::new(),
        &interrupt,
        options,
        &mut delegate,
    )
    .map_err(native_err)?;
    Ok(delegate.0)
}

/// Open the real index, or `None` when the checkout has none yet.
pub(crate) fn open_real_index(repo: &gix::Repository) -> WorktreeResult<Option<gix::index::File>> {
    if repo.index_path().exists() {
        Ok(Some(repo.open_index().map_err(native_err)?))
    } else {
        Ok(None)
    }
}

/// Flag the entries at `positions` as verified against the disk, which the
/// directory walk needs to take an entry's recorded kind at face value.
pub(crate) fn mark_uptodate(index: &mut gix::index::File, positions: &[usize]) {
    let entries = index.entries_mut();
    for &position in positions {
        entries[position].flags.insert(Flags::UPTODATE);
    }
}

// -- validation manifest -----------------------------------------------------

/// Stat fingerprint of one path (design D3). Which fields take part follows
/// the repository's stat options, like Git's own comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathFingerprint {
    size: u64,
    mtime_secs: i64,
    mtime_nsecs: u32,
    mode: u32,
    ctime: Option<(i64, u32)>,
    ino: Option<u64>,
    attributes: u32,
}

impl PathFingerprint {
    #[cfg(unix)]
    fn from_metadata(md: &std::fs::Metadata, opts: StatOptions) -> Self {
        use std::os::unix::fs::MetadataExt;
        let nsec = opts.check_stat && opts.use_nsec;
        Self {
            size: md.size(),
            mtime_secs: md.mtime(),
            mtime_nsecs: if nsec { md.mtime_nsec() as u32 } else { 0 },
            mode: md.mode(),
            ctime: opts
                .trust_ctime
                .then(|| (md.ctime(), if nsec { md.ctime_nsec() as u32 } else { 0 })),
            ino: opts.check_stat.then(|| md.ino()),
            attributes: 0,
        }
    }

    #[cfg(not(unix))]
    fn from_metadata(md: &std::fs::Metadata, opts: StatOptions) -> Self {
        let modified = md
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .unwrap_or_default();
        #[cfg(windows)]
        let attributes = {
            use std::os::windows::fs::MetadataExt;
            md.file_attributes()
        };
        #[cfg(not(windows))]
        let attributes = 0;
        let nsec = opts.check_stat && opts.use_nsec;
        Self {
            size: md.len(),
            mtime_secs: modified.as_secs() as i64,
            mtime_nsecs: if nsec { modified.subsec_nanos() } else { 0 },
            mode: u32::from(md.permissions().readonly()) | (u32::from(md.is_symlink()) << 1),
            ctime: None,
            ino: None,
            attributes,
        }
    }

    /// Whether this file was modified within the racy window before
    /// `capture_start`.
    fn is_racy(&self, capture_start: SystemTime) -> bool {
        let start = capture_start
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.mtime_secs >= start - RACY_WINDOW_SECS
    }
}

/// `path -> fingerprint`, `None` for tracked paths absent from the disk.
pub(crate) type Fingerprints = BTreeMap<BString, Option<PathFingerprint>>;

/// Fingerprint the tracked union (HEAD and real-index paths) plus the
/// non-ignored untracked files found by the same ignore-aware walk capture
/// uses.
pub(crate) fn fingerprint_source(checkout: &Path) -> WorktreeResult<Fingerprints> {
    let repo = open_checkout(checkout)?;
    let worktree = repo
        .workdir()
        .ok_or_else(|| native_err("bare repository has no working tree"))?
        .to_path_buf();
    let stat_opts = repo.stat_options().map_err(native_err)?;
    let mut real_index = open_real_index(&repo)?;

    let mut tracked: BTreeSet<BString> = BTreeSet::new();
    if let Some(tree) = crate::worktree::repo_ops::head_tree_id(&repo)? {
        let mut flat = FlatTree::new();
        flatten_tree(&repo, tree, &mut flat, "")?;
        tracked.extend(flat.into_keys());
    }
    if let Some(index) = &real_index {
        for entry in index.entries() {
            if entry.stage() == gix::index::entry::Stage::Unconflicted {
                tracked.insert(entry.path(index).to_vec().into());
            }
        }
    }

    let mut fingerprints = Fingerprints::new();
    let mut uptodate = Vec::new();
    let empty = gix::index::State::new(repo.object_hash());
    {
        let real_state: &gix::index::State = real_index.as_deref().unwrap_or(&empty);
        for path in &tracked {
            let abs = worktree.join(gix_pathbuf(path.as_ref()));
            let fingerprint = match std::fs::symlink_metadata(&abs) {
                Ok(md) if !md.is_dir() => {
                    if let Ok(position) = real_state.entry_index_by_path(path.as_ref()) {
                        let kind = index_kind(real_state.entries()[position].mode);
                        let on_disk = if md.file_type().is_symlink() {
                            Some(NodeKind::Symlink)
                        } else if md.is_file() {
                            Some(NodeKind::File)
                        } else {
                            None
                        };
                        if kind.is_some() && kind == on_disk {
                            uptodate.push(position);
                        }
                    }
                    Some(PathFingerprint::from_metadata(&md, stat_opts))
                }
                _ => None,
            };
            fingerprints.insert(path.clone(), fingerprint);
        }
    }
    if let Some(index) = real_index.as_mut() {
        mark_uptodate(index, &uptodate);
    }
    let walk = walk_untracked_and_ignored(&repo, real_index.as_deref().unwrap_or(&empty))?;
    for path in walk.untracked {
        if fingerprints.contains_key(&path) {
            continue;
        }
        let abs = worktree.join(gix_pathbuf(path.as_ref()));
        let fingerprint = std::fs::symlink_metadata(&abs)
            .ok()
            .map(|md| PathFingerprint::from_metadata(&md, stat_opts));
        fingerprints.insert(path, fingerprint);
    }
    Ok(fingerprints)
}

/// Outcome of checking a captured tree against the after-manifest.
pub(crate) struct Verification {
    /// Captured paths re-hashed because they were modified in the racy window.
    pub rehashed: u64,
    /// Why the attempt is not acceptable, if it is not.
    pub mismatch: Option<String>,
}

/// Compare the captured tree's path set with the manifest's (minus paths
/// absent from disk) and re-hash, without writing, every captured path
/// modified within the racy window of `capture_start`.
pub(crate) fn verify_captured_tree(
    checkout: &Path,
    tree: &str,
    after: &Fingerprints,
    capture_start: SystemTime,
) -> WorktreeResult<Verification> {
    let repo = open_checkout(checkout)?;
    let tree_id = crate::worktree::repo_ops::parse_oid(tree)?;
    let mut captured = FlatTree::new();
    flatten_tree(&repo, tree_id, &mut captured, "")?;

    let expected: BTreeSet<&BString> = after
        .iter()
        .filter(|(_, fingerprint)| fingerprint.is_some())
        .map(|(path, _)| path)
        .collect();
    let actual: BTreeSet<&BString> = captured.keys().collect();
    if expected != actual {
        let differing = expected
            .symmetric_difference(&actual)
            .next()
            .map(|path| path.to_string())
            .unwrap_or_default();
        return Ok(Verification {
            rehashed: 0,
            mismatch: Some(format!(
                "the captured tree and the observed paths differ (for example `{differing}`)"
            )),
        });
    }

    let worktree = repo
        .workdir()
        .ok_or_else(|| native_err("bare repository has no working tree"))?
        .to_path_buf();
    let mut rehashed = 0;
    for (path, (_, id)) in &captured {
        let Some(Some(fingerprint)) = after.get(path) else {
            continue;
        };
        if !fingerprint.is_racy(capture_start) {
            continue;
        }
        rehashed += 1;
        let abs = worktree.join(gix_pathbuf(path.as_ref()));
        let current = match hash_path_without_writing(&repo, &abs) {
            Ok(current) => current,
            Err(err) if changed_during_capture_detail(&err).is_some() => {
                return Ok(Verification {
                    rehashed,
                    mismatch: Some(format!("`{path}` changed after it was read")),
                });
            }
            Err(err) => return Err(err),
        };
        if current != *id {
            return Ok(Verification {
                rehashed,
                mismatch: Some(format!("`{path}` changed after it was read")),
            });
        }
    }
    Ok(Verification {
        rehashed,
        mismatch: None,
    })
}

/// Blob id of the file or symlink at `abs` as capture would store it,
/// computed without writing anything.
fn hash_path_without_writing(
    repo: &gix::Repository,
    abs: &Path,
) -> WorktreeResult<gix::hash::ObjectId> {
    let md = std::fs::symlink_metadata(abs).map_err(|err| read_err(err, abs))?;
    if md.file_type().is_symlink() {
        let target = std::fs::read_link(abs).map_err(|err| read_err(err, abs))?;
        let target = gix::path::into_bstr(target.as_path()).map_err(native_err)?;
        let bytes: &[u8] = &target;
        return gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Blob, bytes)
            .map_err(native_err);
    }
    hash_file_stream(repo, abs).map(|(id, _)| id)
}

/// Stream the file at `abs` once to compute its blob id. Returns the id and
/// the byte count read.
pub(crate) fn hash_file_stream(
    repo: &gix::Repository,
    abs: &Path,
) -> WorktreeResult<(gix::hash::ObjectId, u64)> {
    let mut file = std::fs::File::open(abs).map_err(|err| read_err(err, abs))?;
    let size = file.metadata().map_err(|err| read_err(err, abs))?.len();
    let id = gix::objs::compute_stream_hash(
        repo.object_hash(),
        gix::objs::Kind::Blob,
        &mut file,
        size,
        &mut gix::progress::Discard,
        &AtomicBool::new(false),
    )
    // A short read means the file shrank after its size was taken.
    .map_err(|err| changed_during_capture(format!("{}: {err}", abs.display())))?;
    Ok((id, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::index::fs::Metadata;
    use std::fs;
    use std::fs::File;
    use std::time::Duration;
    use tempfile::TempDir;

    struct Case {
        _dir: TempDir,
        entry_state: gix::index::State,
        metadata: Metadata,
        timestamp: FileTime,
        opts: StatOptions,
        path: std::path::PathBuf,
    }

    /// A file whose index entry records its own stat data, so every rule can
    /// be flipped on its own. The index timestamp is an hour after the mtime
    /// unless a test moves it.
    fn case(mode: Mode, flags: Flags) -> Case {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "content").unwrap();
        let aged = SystemTime::now() - Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(aged)
            .unwrap();
        let metadata = Metadata::from_path_no_follow(&path).unwrap();
        let mut state = gix::index::State::new(gix::hash::Kind::Sha1);
        state.dangerously_push_entry(
            Stat::from_fs(&metadata).unwrap(),
            gix::hash::ObjectId::empty_blob(gix::hash::Kind::Sha1),
            flags,
            mode,
            "f".into(),
        );
        Case {
            _dir: dir,
            entry_state: state,
            metadata,
            timestamp: FileTime::from_system_time(aged + Duration::from_secs(3600)),
            opts: StatOptions::default(),
            path,
        }
    }

    impl Case {
        fn reusable(&self, has_conversion: bool) -> bool {
            self.reusable_with(has_conversion, true)
        }

        fn reusable_with(&self, has_conversion: bool, blob_size_matches: bool) -> bool {
            reuse_predicate(
                Some(&self.entry_state.entries()[0]),
                &self.metadata,
                self.timestamp,
                self.opts,
                has_conversion,
                blob_size_matches,
            )
        }
    }

    #[test]
    fn vanished_file_is_a_retryable_change() {
        let path = Path::new("gone.txt");
        let vanished = read_err(std::io::ErrorKind::NotFound.into(), path);
        assert_eq!(
            changed_during_capture_detail(&vanished),
            Some("gone.txt vanished")
        );
        // Other read failures, and look-alike messages, are not retried.
        let denied = read_err(std::io::ErrorKind::PermissionDenied.into(), path);
        assert!(changed_during_capture_detail(&denied).is_none());
        let look_alike =
            WorktreeError::InvalidOperation("source changed during capture: x".to_string());
        assert!(changed_during_capture_detail(&look_alike).is_none());
    }

    #[test]
    fn reuse_predicate_rules() {
        // Stat match, not racy, no flag, no conversion: reuse.
        let base = case(Mode::FILE, Flags::empty());
        assert!(base.reusable(false));

        // Conversion forces a read.
        assert!(!base.reusable(true));

        // So does a blob whose header size differs from the file's.
        assert!(!base.reusable_with(false, false));

        // Each flag that can hide working content forces a read.
        for flag in [
            Flags::SKIP_WORKTREE,
            Flags::ASSUME_VALID,
            Flags::FSMONITOR_VALID,
            Flags::INTENT_TO_ADD,
        ] {
            assert!(!case(Mode::FILE, flag).reusable(false), "{flag:?}");
        }

        // An executable-bit difference alone does not prevent reuse.
        assert!(case(Mode::FILE_EXECUTABLE, Flags::empty()).reusable(false));

        // Stat mismatch: the file changed size.
        let mut changed = case(Mode::FILE, Flags::empty());
        fs::write(&changed.path, "content, longer").unwrap();
        changed.metadata = Metadata::from_path_no_follow(&changed.path).unwrap();
        assert!(!changed.reusable(false));

        // Racily clean: the index was written no later than the mtime.
        let mut racy = case(Mode::FILE, Flags::empty());
        racy.timestamp =
            FileTime::from_system_time(racy.metadata.modified().unwrap() - Duration::from_secs(1));
        assert!(!racy.reusable(false));

        // Kind change: the index records a symlink, the disk holds a file.
        assert!(!case(Mode::SYMLINK, Flags::empty()).reusable(false));

        // Path missing from the real index.
        let missing = case(Mode::FILE, Flags::empty());
        assert!(!reuse_predicate(
            None,
            &missing.metadata,
            missing.timestamp,
            missing.opts,
            false,
            true
        ));
    }
}

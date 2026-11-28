//! Capture side of the git snapshot backend: walk, stat cache, blob writes
//! and tree construction.

use super::super::backend::{SnapshotError, SnapshotId, SnapshotResult, io_err};
use super::gc::note_captured;
use super::sink::ObjectSink;
use super::snapshot_id;
use super::{GitSnapshotBackend, SnapshotLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// File name of the persistent stat cache inside the snapshot repository.
pub(super) const STAT_CACHE_FILE: &str = "qmt-stat-cache.json";
pub(super) const STAT_CACHE_VERSION: u32 = 2;

/// A file whose mtime is within this window of the scan start may still be
/// modified without its mtime changing (coarse timestamp granularity), so its
/// cache entry is not trusted (same idea as git's "racily clean" handling).
pub(super) const RACY_WINDOW_NS: u64 = 2_000_000_000;

/// File identity used to decide whether a cached hash is still valid.
///
/// Mirrors git's index stat data: besides size and mtime it records the inode
/// and ctime (unix), so a rewrite that preserves size and mtime (`cp -p`,
/// `rsync -t`, `touch -r`, `tar x`) is still noticed, because ctime cannot be
/// set by user space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct FileStamp {
    pub(super) size: u64,
    pub(super) mtime_ns: u64,
    pub(super) ctime_s: i64,
    pub(super) ctime_ns: i64,
    pub(super) ino: u64,
}

impl FileStamp {
    /// `None` when the platform cannot report a modification time.
    pub(super) fn from_metadata(meta: &fs::Metadata) -> Option<Self> {
        let mtime_ns = system_time_ns(meta.modified().ok()?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Self {
                size: meta.len(),
                mtime_ns,
                ctime_s: meta.ctime(),
                ctime_ns: meta.ctime_nsec(),
                ino: meta.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Some(Self {
                size: meta.len(),
                mtime_ns,
                ctime_s: 0,
                ctime_ns: 0,
                ino: 0,
            })
        }
    }
}

/// Cached result of hashing a file, keyed by repo-relative path.
///
/// A file whose [`FileStamp`] is unchanged is assumed to have identical
/// content, so it is neither re-read nor re-hashed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StatCacheEntry {
    pub(super) stamp: FileStamp,
    pub(super) oid: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct StatCache {
    pub(super) version: u32,
    pub(super) entries: HashMap<String, StatCacheEntry>,
}

impl StatCache {
    pub(super) fn empty() -> Self {
        Self {
            version: STAT_CACHE_VERSION,
            entries: HashMap::new(),
        }
    }
}

/// Prefix of the temporary directory holding a point-in-time file copy.
pub(super) const COPY_DIR_PREFIX: &str = "qmt-copy-";

/// What the directory walk saw for a file before reading it.
#[derive(Debug, Clone, Copy)]
pub(super) struct ListedFile {
    pub(super) size: u64,
    pub(super) stamp: Option<FileStamp>,
}

/// Result of writing a file's blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CaptureOutcome {
    /// The blob holds the file's content; `bytes` is what was actually read.
    Stored { oid: gix::ObjectId, bytes: u64 },
    /// The file changed while it was read; the stored bytes are not trusted.
    Changed,
}

/// A captured file.
#[derive(Debug, Clone, Copy)]
pub(super) struct Captured {
    pub(super) oid: gix::ObjectId,
    /// Bytes read from disk.
    pub(super) bytes: u64,
    /// Whether the content came from a point-in-time copy.
    pub(super) copied: bool,
}

/// Counters describing the work one `track` call performed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SnapshotStats {
    /// Files included in the snapshot tree.
    pub files: usize,
    /// Files whose content was read and hashed.
    pub hashed_files: usize,
    /// Files reused from the stat cache without reading them.
    pub cached_files: usize,
    /// Files that changed while being read and were captured from a
    /// point-in-time copy instead.
    pub copied_files: usize,
    /// Files of `large_warn_bytes` or more that the previous snapshot did not
    /// already hold at that size.
    pub newly_large_files: usize,
    /// The workspace equalled the previous snapshot, which was reused.
    pub reused: bool,
    /// Total bytes read from disk and hashed.
    pub bytes_hashed: u64,
}

/// At most this many large files are named per snapshot.
const MAX_LARGE_FILE_WARNINGS: usize = 10;

/// Split newly large files into those named individually (at most
/// [`MAX_LARGE_FILE_WARNINGS`]) and the number summarised.
pub(super) fn large_file_warnings(newly: &[(String, u64)]) -> (&[(String, u64)], usize) {
    let named = newly.len().min(MAX_LARGE_FILE_WARNINGS);
    (&newly[..named], newly.len() - named)
}

/// A file is newly large when the previous snapshot has no copy of it, or a
/// copy below `threshold`.
fn is_newly_large(
    repo: &gix::Repository,
    head_tree: Option<&gix::Tree<'_>>,
    rel: &str,
    threshold: u64,
) -> bool {
    let previous = head_tree
        .and_then(|tree| tree.lookup_entry_by_path(rel).ok().flatten())
        .and_then(|entry| repo.find_header(entry.object_id()).ok())
        .map(|header| header.size());
    previous.is_none_or(|size| size < threshold)
}

/// Nanoseconds since the Unix epoch, or `None` for pre-epoch / overflowing times.
pub(super) fn system_time_ns(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
}

/// `true` when `meta` still matches what the directory listing reported.
fn matches_listing(meta: &fs::Metadata, listed: &ListedFile) -> bool {
    match listed.stamp {
        Some(stamp) => FileStamp::from_metadata(meta) == Some(stamp),
        None => meta.len() == listed.size,
    }
}

pub(super) fn is_vanished(err: &ignore::Error) -> bool {
    err.io_error()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

impl GitSnapshotBackend {
    pub(super) fn load_stat_cache(repo: &gix::Repository) -> StatCache {
        let path = repo.git_dir().join(STAT_CACHE_FILE);
        fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StatCache>(&bytes).ok())
            .filter(|cache| cache.version == STAT_CACHE_VERSION)
            .unwrap_or_else(StatCache::empty)
    }

    /// Persist the cache atomically. Failures only cost speed, never correctness.
    pub(super) fn save_stat_cache(repo: &gix::Repository, cache: &StatCache) {
        let path = repo.git_dir().join(STAT_CACHE_FILE);
        static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
        let tmp = repo.git_dir().join(format!(
            "{}.{}.{}.tmp",
            STAT_CACHE_FILE,
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = serde_json::to_vec(cache)
            .map_err(std::io::Error::other)
            .and_then(|bytes| fs::write(&tmp, bytes))
            .and_then(|_| fs::rename(&tmp, &path));
        if let Err(e) = result {
            // Cleanup of our own temp file; it may not exist if the write failed first.
            if let Err(cleanup) = fs::remove_file(&tmp)
                && cleanup.kind() != std::io::ErrorKind::NotFound
            {
                tracing::debug!(path = %tmp.display(), error = %cleanup, "failed to remove stat cache temp file");
            }
            tracing::warn!(error = %e, "failed to persist snapshot stat cache");
        }
    }

    /// Build a tree object from a sorted list of (relative_path, blob_oid, executable) entries.
    ///
    /// This constructs a `gix_object::Tree` with proper `Entry` objects and writes it to the ODB.
    pub(super) fn build_tree_from_entries(
        sink: &ObjectSink<'_>,
        entries: &[(String, gix::ObjectId, bool)],
    ) -> SnapshotResult<gix::ObjectId> {
        // Group entries by top-level directory component
        let mut blobs: Vec<(String, gix::ObjectId, bool)> = Vec::new();
        let mut subdirs: BTreeMap<String, Vec<(String, gix::ObjectId, bool)>> = BTreeMap::new();

        for (path, oid, exec) in entries {
            if let Some(slash_pos) = path.find('/') {
                let dir_name = &path[..slash_pos];
                let rest = &path[slash_pos + 1..];
                subdirs.entry(dir_name.to_string()).or_default().push((
                    rest.to_string(),
                    *oid,
                    *exec,
                ));
            } else {
                blobs.push((path.clone(), *oid, *exec));
            }
        }

        // Build gix_object::Tree entries
        let mut tree_entries: Vec<gix::objs::tree::Entry> = Vec::new();

        // Add subdirectory trees (recurse)
        for (dir_name, sub_entries) in &subdirs {
            let sub_tree_id = Self::build_tree_from_entries(sink, sub_entries)?;
            tree_entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Tree.into(),
                filename: dir_name.as_str().into(),
                oid: sub_tree_id,
            });
        }

        // Add blob entries
        for (name, oid, exec) in &blobs {
            let mode = if *exec {
                gix::objs::tree::EntryKind::BlobExecutable.into()
            } else {
                gix::objs::tree::EntryKind::Blob.into()
            };
            tree_entries.push(gix::objs::tree::Entry {
                mode,
                filename: name.as_str().into(),
                oid: *oid,
            });
        }

        // Sort entries (git requires specific ordering for trees)
        tree_entries.sort();

        let tree = gix::objs::Tree {
            entries: tree_entries,
        };

        sink.write_object(&tree)
    }

    /// Stage all files from the worktree and create a snapshot commit.
    ///
    /// This builds a tree from the worktree contents by:
    /// 1. Walking the worktree (respecting .gitignore, pruning dot-directories)
    /// 2. Writing each changed file as a blob. Files whose stat data matches
    ///    the persistent stat cache are not read or hashed again.
    /// 3. Building a tree from those blobs
    /// 4. Creating a commit pointing to that tree
    ///
    /// Every file in scope is captured whatever its size. A file that changes
    /// while it is being read is captured from a point-in-time copy.
    pub(super) fn create_snapshot(
        &self,
        worktree: &Path,
        message: &str,
    ) -> SnapshotResult<(SnapshotId, SnapshotStats)> {
        let repo = self.open_or_init(worktree)?;
        // Held until the commit is written: the GC sweep needs the exclusive
        // lock, so it cannot delete blobs this snapshot has not committed yet.
        let _lock = SnapshotLock::shared(repo.git_dir())?;
        let sink = ObjectSink::new(&repo, self.limits.stream_bytes);
        let mut stats = SnapshotStats::default();
        let head_tree = repo.head_commit().ok().and_then(|c| c.tree().ok());
        let mut newly_large: Vec<(String, u64)> = Vec::new();

        let previous_cache = Self::load_stat_cache(&repo);
        let mut next_cache = StatCache::empty();
        let scan_started_ns = system_time_ns(SystemTime::now()).unwrap_or(u64::MAX);

        // Collect all files from the worktree
        let mut entries: Vec<(String, gix::ObjectId, bool)> = Vec::new();

        let walker = ignore::WalkBuilder::new(worktree)
            .hidden(false)
            // Scope: .ignore, .gitignore (also outside a git repository) and
            // .git/info/exclude; never the user's global excludes.
            .ignore(true)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(false)
            .require_git(false)
            // Never descend into dot-directories (.git, .venv, ...): their
            // files were always filtered out, so skip the whole subtree.
            .filter_entry(|entry| {
                entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
            })
            .build();

        for entry in walker {
            let entry = entry.map_err(|e| {
                SnapshotError::Filesystem(format!("Failed to walk directory: {}", e))
            })?;
            if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                continue;
            }

            let path = entry.path();
            let rel_path = path.strip_prefix(worktree).map_err(|e| {
                SnapshotError::Filesystem(format!("Failed to compute relative path: {}", e))
            })?;
            let rel_str = rel_path.to_string_lossy().into_owned();

            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) if is_vanished(&e) => continue,
                Err(e) => {
                    return Err(SnapshotError::Filesystem(format!(
                        "Failed to stat file: {}",
                        e
                    )));
                }
            };
            let size = meta.len();

            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;

            let stamp = FileStamp::from_metadata(&meta);

            // Reuse the previous hash when the stat data is unchanged and the
            // object is still present.
            if let Some(stamp) = stamp
                && let Some(cached) = previous_cache.entries.get(&rel_str)
                && cached.stamp == stamp
                && let Ok(oid) = gix::ObjectId::from_hex(cached.oid.as_bytes())
                && sink.contains(oid, size)
            {
                stats.cached_files += 1;
                next_cache.entries.insert(rel_str.clone(), cached.clone());
                entries.push((rel_str, oid, executable));
                continue;
            }

            let captured = match self.capture_file(&sink, path, ListedFile { size, stamp }) {
                Ok(captured) => captured,
                Err(SnapshotError::Io { op, source, .. })
                    if matches!(op, "read" | "copy")
                        && source.kind() == std::io::ErrorKind::NotFound =>
                {
                    // The file vanished between listing and reading. Other
                    // NotFound errors (e.g. the snapshot store) are real.
                    continue;
                }
                Err(e) => return Err(e),
            };
            let oid = captured.oid;
            stats.hashed_files += 1;
            stats.bytes_hashed += captured.bytes;
            if captured.copied {
                stats.copied_files += 1;
            }
            if captured.bytes >= self.limits.large_warn_bytes
                && is_newly_large(
                    &repo,
                    head_tree.as_ref(),
                    &rel_str,
                    self.limits.large_warn_bytes,
                )
            {
                newly_large.push((rel_str.clone(), captured.bytes));
            }

            // Cache the hash only if the file was not racily clean (modified
            // right before the scan, so a later write could keep the same
            // mtime) and did not change while it was being read.
            if let Some(stamp) = stamp
                && !captured.copied
                && stamp.mtime_ns.saturating_add(RACY_WINDOW_NS) < scan_started_ns
                && fs::metadata(path)
                    .ok()
                    .and_then(|m| FileStamp::from_metadata(&m))
                    == Some(stamp)
            {
                next_cache.entries.insert(
                    rel_str.clone(),
                    StatCacheEntry {
                        stamp,
                        oid: oid.to_string(),
                    },
                );
            }
            entries.push((rel_str, oid, executable));
        }

        stats.files = entries.len();
        if previous_cache.entries != next_cache.entries {
            Self::save_stat_cache(&repo, &next_cache);
        }

        // Sort entries for deterministic tree creation
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        // Build tree from entries
        let tree_id = Self::build_tree_from_entries(&sink, &entries)?;

        stats.newly_large_files = newly_large.len();
        let (named, remaining) = large_file_warnings(&newly_large);
        for (path, size_bytes) in named {
            tracing::warn!(
                path = %path,
                size_bytes,
                "large file entering undo snapshots; add it to .ignore to leave it out of undo"
            );
        }
        if remaining > 0 {
            tracing::warn!(
                count = remaining,
                "more large files entering undo snapshots; add them to .ignore to leave them out of undo"
            );
        }
        note_captured(repo.git_dir(), stats.bytes_hashed, &self.limits);

        // Get HEAD as parent (if exists)
        let head = repo.head_commit().ok();
        // Nothing changed: reuse the previous snapshot instead of recording
        // an identical commit.
        if let Some(head) = &head
            && head.tree_id().ok().map(|id| id.detach()) == Some(tree_id)
        {
            stats.reused = true;
            return Ok((snapshot_id(head.id().detach()), stats));
        }
        let parent_ids: Vec<gix::ObjectId> = head.iter().map(|c| c.id().detach()).collect();

        let commit_id = Self::write_snapshot_commit(&repo, &sink, message, tree_id, parent_ids)?;

        Ok((snapshot_id(commit_id), stats))
    }

    /// Write the snapshot commit through the sink, then point HEAD at it with a
    /// separate ref edit (`Repository::commit` would write the object through
    /// the alternates-aware path). Author and committer come from the store's
    /// config (`user.name`/`user.email` are set during init).
    fn write_snapshot_commit(
        repo: &gix::Repository,
        sink: &ObjectSink<'_>,
        message: &str,
        tree: gix::ObjectId,
        parents: Vec<gix::ObjectId>,
    ) -> SnapshotResult<gix::ObjectId> {
        use gix::bstr::ByteSlice as _;
        use gix::refs::transaction::{PreviousValue, RefEdit};

        let author = repo
            .author()
            .ok_or_else(|| SnapshotError::Repository("Author identity is not configured".into()))?
            .map_err(|e| SnapshotError::Repository(format!("Invalid author identity: {e}")))?;
        let committer = repo
            .committer()
            .ok_or_else(|| {
                SnapshotError::Repository("Committer identity is not configured".into())
            })?
            .map_err(|e| SnapshotError::Repository(format!("Invalid committer identity: {e}")))?;
        let commit = gix::objs::Commit {
            tree,
            parents: parents.iter().copied().collect(),
            author: author.into(),
            committer: committer.into(),
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        let commit_id = sink.write_object(&commit)?;

        let expected = match parents.first() {
            Some(parent) => PreviousValue::MustExistAndMatch(gix::refs::Target::Object(*parent)),
            None => PreviousValue::MustNotExist,
        };
        let head: gix::refs::FullName = "HEAD"
            .try_into()
            .map_err(|e| SnapshotError::Repository(format!("Invalid reference name HEAD: {e}")))?;
        let log_message =
            gix::reference::log::message("commit", message.as_bytes().as_bstr(), parents.len());
        repo.edit_reference(
            RefEdit::update(head, commit_id, expected, log_message).with_deref(true),
        )
        .map_err(|e| SnapshotError::Repository(format!("Failed to update HEAD: {e}")))?;
        Ok(commit_id)
    }

    /// Capture the current content of `path` as a blob.
    ///
    /// The result is always complete: if the file changes while it is being
    /// streamed, it is captured from a point-in-time copy instead.
    pub(super) fn capture_file(
        &self,
        sink: &ObjectSink<'_>,
        path: &Path,
        listed: ListedFile,
    ) -> SnapshotResult<Captured> {
        match Self::write_file_blob(sink, path, listed, self.limits.stream_bytes)? {
            CaptureOutcome::Stored { oid, bytes } => Ok(Captured {
                oid,
                bytes,
                copied: false,
            }),
            CaptureOutcome::Changed => {
                Self::capture_from_copy(sink, path, self.limits.stream_bytes)
            }
        }
    }

    /// Copy `path` next to the snapshot repository and capture the copy, which
    /// cannot change underneath us. `fs::copy` clones on APFS and uses
    /// `copy_file_range` on Linux. The copy is removed when the directory
    /// guard drops, on success and on every error path.
    ///
    /// Only a clone is a true point-in-time copy. A byte copy of a file that is
    /// being written can be torn, so the source is stat-ed before and after:
    /// a copy made while the file changed is discarded and retried, and the
    /// capture fails if the file never holds still.
    fn capture_from_copy(
        sink: &ObjectSink<'_>,
        path: &Path,
        stream_bytes: u64,
    ) -> SnapshotResult<Captured> {
        const COPY_ATTEMPTS: usize = 3;
        let git_dir = sink.git_dir();
        let dir = tempfile::Builder::new()
            .prefix(COPY_DIR_PREFIX)
            .tempdir_in(git_dir)
            .map_err(io_err("create temporary directory in", git_dir))?;
        let copy = dir.path().join("content");

        let identity = |meta: &fs::Metadata| (FileStamp::from_metadata(meta), meta.len());
        let mut stable = false;
        for _ in 0..COPY_ATTEMPTS {
            let before = identity(&fs::metadata(path).map_err(io_err("read", path))?);
            fs::copy(path, &copy).map_err(io_err("copy", path))?;
            let after = identity(&fs::metadata(path).map_err(io_err("read", path))?);
            if before == after {
                stable = true;
                break;
            }
        }
        if !stable {
            return Err(SnapshotError::Other(format!(
                "{} kept changing while it was being copied for the snapshot",
                path.display()
            )));
        }
        let mut file = fs::File::open(&copy).map_err(io_err("read", &copy))?;
        let size = file.metadata().map_err(io_err("read", &copy))?.len();
        // The rule is by size, as for any other file: a large copy streams and
        // is always written, a small one follows the sink's normal rule.
        let oid = if size >= stream_bytes {
            sink.write_stream_blob(file, size)?
        } else {
            let mut content = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
            std::io::Read::read_to_end(&mut file, &mut content).map_err(io_err("read", &copy))?;
            (content.len() as u64 == size)
                .then(|| sink.write_blob(&content))
                .transpose()?
        };
        match oid {
            Some(oid) => Ok(Captured {
                oid,
                bytes: size,
                copied: true,
            }),
            None => Err(SnapshotError::Other(format!(
                "copy of {} shrank while being snapshotted",
                path.display()
            ))),
        }
    }

    /// Write `path` into the object database as a blob.
    ///
    /// Files listed below `stream_bytes` are read whole and captured exactly as
    /// read. Larger files are streamed so a multi-hundred-MB model never has to
    /// fit in memory; [`CaptureOutcome::Changed`] reports that the stream was
    /// short or the file no longer matches the listing, so the caller must not
    /// trust it.
    pub(super) fn write_file_blob(
        sink: &ObjectSink<'_>,
        path: &Path,
        listed: ListedFile,
        stream_bytes: u64,
    ) -> SnapshotResult<CaptureOutcome> {
        if listed.size < stream_bytes {
            let content = fs::read(path).map_err(io_err("read", path))?;
            // A write racing the read can yield torn content: only trust it
            // when the file still matches its listing.
            let after = fs::metadata(path).map_err(io_err("read", path))?;
            if !matches_listing(&after, &listed) || content.len() as u64 != listed.size {
                return Ok(CaptureOutcome::Changed);
            }
            return sink.write_blob(&content).map(|oid| CaptureOutcome::Stored {
                oid,
                bytes: content.len() as u64,
            });
        }

        let file = fs::File::open(path).map_err(io_err("read", path))?;
        // Declare the size of the opened file so the declared length matches
        // what is read; the object is only published on success.
        let size = file.metadata().map_err(io_err("read", path))?.len();
        let Some(oid) = sink.write_stream_blob(file, size)? else {
            return Ok(CaptureOutcome::Changed);
        };
        // A size or stamp change catches growth and in-place rewrites too.
        let after = fs::metadata(path).map_err(io_err("read", path))?;
        let unchanged = matches_listing(&after, &listed);
        Ok(if unchanged {
            CaptureOutcome::Stored { oid, bytes: size }
        } else {
            CaptureOutcome::Changed
        })
    }
}

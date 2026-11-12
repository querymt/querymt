//! Git-based snapshot backend using gitoxide (gix)
//!
//! This backend stores snapshots in a shadow git repository located at
//! `$HOME/.qmt/snapshots/<hash>/` where `<hash>` is derived from the
//! worktree path. This keeps the user's project directory untouched.

use super::backend::{
    GcConfig, GcResult, SnapshotBackend, SnapshotError, SnapshotId, SnapshotResult,
};
use async_trait::async_trait;
use gix::objs::Write as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

/// File name of the persistent stat cache inside the snapshot repository.
const STAT_CACHE_FILE: &str = "qmt-stat-cache.json";
const STAT_CACHE_VERSION: u32 = 2;

/// Commit-message trailer listing paths a snapshot could not capture (too
/// large, or changing while being read), as a JSON array of strings. Diff and
/// restore use it to tell "not captured" apart from "did not exist".
const SKIPPED_TRAILER: &str = "qmt-skipped: ";

/// At most this many newly-skipped files are warned about individually per
/// snapshot; the rest are summarised in one line.
const MAX_SKIP_WARNINGS: usize = 10;

/// Default per-file size limit for snapshots (50 MiB). Larger files are left
/// out of snapshots and are not covered by undo.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Files larger than this are streamed from disk into the object database
/// instead of being loaded into memory.
const STREAM_THRESHOLD_BYTES: u64 = 16 * 1024 * 1024;

/// A file whose mtime is within this window of the scan start may still be
/// modified without its mtime changing (coarse timestamp granularity), so its
/// cache entry is not trusted (same idea as git's "racily clean" handling).
const RACY_WINDOW_NS: u64 = 2_000_000_000;

/// File identity used to decide whether a cached hash is still valid.
///
/// Mirrors git's index stat data: besides size and mtime it records the inode
/// and ctime (unix), so a rewrite that preserves size and mtime (`cp -p`,
/// `rsync -t`, `touch -r`, `tar x`) is still noticed, because ctime cannot be
/// set by user space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileStamp {
    size: u64,
    mtime_ns: u64,
    ctime_s: i64,
    ctime_ns: i64,
    ino: u64,
}

impl FileStamp {
    /// `None` when the platform cannot report a modification time.
    fn from_metadata(meta: &fs::Metadata) -> Option<Self> {
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
struct StatCacheEntry {
    stamp: FileStamp,
    oid: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StatCache {
    version: u32,
    entries: HashMap<String, StatCacheEntry>,
}

impl StatCache {
    fn empty() -> Self {
        Self {
            version: STAT_CACHE_VERSION,
            entries: HashMap::new(),
        }
    }
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
    /// Files left out of the snapshot because they exceed `max_file_bytes`.
    pub skipped_large_files: usize,
    /// Files left out because they shrank or changed while being read.
    pub skipped_unstable_files: usize,
    /// Total bytes read from disk and hashed.
    pub bytes_hashed: u64,
}

/// Nanoseconds since the Unix epoch, or `None` for pre-epoch / overflowing times.
fn system_time_ns(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
}

fn is_vanished(err: &ignore::Error) -> bool {
    err.io_error()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// Reader that fails with `UnexpectedEof` (and remembers it) if the source
/// ends before the declared number of bytes was produced.
struct ExactReader<R> {
    inner: R,
    remaining: u64,
    shrank: bool,
}

impl<R: std::io::Read> ExactReader<R> {
    fn new(inner: R, expected: u64) -> Self {
        Self {
            inner,
            remaining: expected,
            shrank: false,
        }
    }
}

impl<R: std::io::Read> std::io::Read for ExactReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && self.remaining > 0 && !buf.is_empty() {
            self.shrank = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "file shrank while being snapshotted",
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Paths a snapshot commit recorded as not captured. Empty for snapshots
/// without the trailer (including ones made before it existed).
fn skipped_paths(repo: &gix::Repository, commit_id: gix::ObjectId) -> BTreeSet<String> {
    let Ok(commit) = repo.find_commit(commit_id) else {
        return BTreeSet::new();
    };
    let Ok(message) = commit.message_raw() else {
        return BTreeSet::new();
    };
    let bytes: &[u8] = message;
    String::from_utf8_lossy(bytes)
        .lines()
        .find_map(|line| line.strip_prefix(SKIPPED_TRAILER))
        .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
        .map(|paths| paths.into_iter().collect())
        .unwrap_or_default()
}

/// Git-based snapshot backend using gitoxide (pure Rust, no git CLI dependency)
#[derive(Clone)]
pub struct GitSnapshotBackend {
    /// Files larger than this many bytes are left out of snapshots (and are
    /// therefore not covered by undo). `None` means no limit.
    max_file_bytes: Option<u64>,
    #[cfg(test)]
    snapshot_base_override: Option<PathBuf>,
}

impl GitSnapshotBackend {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            max_file_bytes: Some(DEFAULT_MAX_FILE_BYTES),
            #[cfg(test)]
            snapshot_base_override: None,
        }
    }

    /// Leave files larger than `max_file_bytes` out of snapshots; `None`
    /// (or `Some(0)`) means no limit.
    pub fn with_max_file_bytes(mut self, max_file_bytes: Option<u64>) -> Self {
        self.max_file_bytes = max_file_bytes.filter(|limit| *limit > 0);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_snapshot_base(base: PathBuf) -> Self {
        Self {
            max_file_bytes: Some(DEFAULT_MAX_FILE_BYTES),
            snapshot_base_override: Some(base),
        }
    }

    fn load_stat_cache(repo: &gix::Repository) -> StatCache {
        let path = repo.git_dir().join(STAT_CACHE_FILE);
        fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StatCache>(&bytes).ok())
            .filter(|cache| cache.version == STAT_CACHE_VERSION)
            .unwrap_or_else(StatCache::empty)
    }

    /// Persist the cache atomically. Failures only cost speed, never correctness.
    fn save_stat_cache(repo: &gix::Repository, cache: &StatCache) {
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
            let _ = fs::remove_file(&tmp);
            log::warn!("Failed to persist snapshot stat cache: {}", e);
        }
    }

    /// Compute the snapshot repository directory for a given worktree
    fn snapshot_dir(&self, worktree: &Path) -> SnapshotResult<PathBuf> {
        let canonical = worktree
            .canonicalize()
            .unwrap_or_else(|_| worktree.to_path_buf());
        let hash = crate::hash::RapidHash::new(canonical.to_string_lossy().as_bytes());

        #[cfg(test)]
        if let Some(ref base) = self.snapshot_base_override {
            return Ok(base.join(format!("{:016x}", hash.as_u64())));
        }

        let cache_dir = dirs::cache_dir()
            .ok_or_else(|| SnapshotError::Filesystem("HOME directory must be set".to_string()))?;
        Ok(cache_dir
            .join("querymt")
            .join("snapshots")
            .join(format!("{:016x}", hash.as_u64())))
    }

    /// Initialize or open the snapshot git repository.
    ///
    /// We use a bare repo with `core.worktree` pointing to the project directory.
    /// This keeps the project directory pristine (no .git folder).
    fn open_or_init(&self, worktree: &Path) -> SnapshotResult<gix::Repository> {
        let git_dir = self.snapshot_dir(worktree)?;

        if git_dir.join("HEAD").exists() {
            // Open existing repository
            gix::open(&git_dir).map_err(|e| {
                SnapshotError::Repository(format!("Failed to open snapshot repository: {}", e))
            })
        } else {
            // Initialize new bare repository
            fs::create_dir_all(&git_dir).map_err(|e| {
                SnapshotError::Filesystem(format!("Failed to create snapshot directory: {}", e))
            })?;

            gix::init_bare(&git_dir).map_err(|e| {
                SnapshotError::Repository(format!(
                    "Failed to initialize snapshot repository: {}",
                    e
                ))
            })?;

            // Write worktree path as metadata file
            fs::write(
                git_dir.join("WORKTREE_PATH"),
                worktree.to_string_lossy().as_bytes(),
            )
            .map_err(|e| {
                SnapshotError::Filesystem(format!("Failed to write worktree metadata: {}", e))
            })?;

            // Set core.worktree and user identity in git config
            let config_path = git_dir.join("config");
            let config_content = fs::read_to_string(&config_path).unwrap_or_else(|_| String::new());
            let extra = format!(
                "\n[core]\n\tworktree = {}\n[user]\n\tname = qmt-snapshot\n\temail = snapshot@qmt.local\n",
                worktree.display()
            );
            fs::write(&config_path, format!("{}{}", config_content, extra)).map_err(|e| {
                SnapshotError::Filesystem(format!("Failed to write git config: {}", e))
            })?;

            // Re-open the repo so it picks up our config changes
            gix::open(&git_dir).map_err(|e| {
                SnapshotError::Repository(format!(
                    "Failed to re-open snapshot repository after config: {}",
                    e
                ))
            })
        }
    }

    /// Build a tree object from a sorted list of (relative_path, blob_oid, executable) entries.
    ///
    /// This constructs a `gix_object::Tree` with proper `Entry` objects and writes it to the ODB.
    fn build_tree_from_entries(
        repo: &gix::Repository,
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
            let sub_tree_id = Self::build_tree_from_entries(repo, sub_entries)?;
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

        let tree_id = repo.write_object(&tree).map_err(|e| {
            SnapshotError::Repository(format!("Failed to write tree object: {}", e))
        })?;

        Ok(tree_id.detach())
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
    /// Files that cannot be captured (over `max_file_bytes`, or changing while
    /// being read) are left out of the tree and listed in the commit message so
    /// diff and restore never mistake them for deleted files.
    fn create_snapshot(
        &self,
        worktree: &Path,
        message: &str,
    ) -> SnapshotResult<(SnapshotId, SnapshotStats)> {
        let repo = self.open_or_init(worktree)?;
        let mut stats = SnapshotStats::default();

        let previous_cache = Self::load_stat_cache(&repo);
        let mut next_cache = StatCache::empty();
        let scan_started_ns = system_time_ns(SystemTime::now()).unwrap_or(u64::MAX);

        // Collect all files from the worktree
        let mut entries: Vec<(String, gix::ObjectId, bool)> = Vec::new();
        // Paths left out of the tree, with the reason.
        let mut skipped: BTreeMap<String, String> = BTreeMap::new();

        let walker = ignore::WalkBuilder::new(worktree)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
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

            if let Some(limit) = self.max_file_bytes
                && size > limit
            {
                stats.skipped_large_files += 1;
                skipped.insert(
                    rel_str,
                    format!("{size} bytes exceeds max_file_bytes={limit}"),
                );
                continue;
            }

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
                && repo.has_object(oid)
            {
                stats.cached_files += 1;
                next_cache.entries.insert(rel_str.clone(), cached.clone());
                entries.push((rel_str, oid, executable));
                continue;
            }

            let oid = match Self::write_file_blob(&repo, path, size) {
                Ok(Some(oid)) => oid,
                Ok(None) => {
                    stats.skipped_unstable_files += 1;
                    skipped.insert(rel_str, "file shrank while being read".to_string());
                    continue;
                }
                Err(SnapshotError::Filesystem(_)) if !path.exists() => continue,
                Err(e) => return Err(e),
            };
            stats.hashed_files += 1;
            stats.bytes_hashed += size;

            // Cache the hash only if the file was not racily clean (modified
            // right before the scan, so a later write could keep the same
            // mtime) and did not change while it was being read.
            if let Some(stamp) = stamp
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
        let tree_id = Self::build_tree_from_entries(&repo, &entries)?;

        // Get HEAD as parent (if exists)
        let parent_ids: Vec<gix::ObjectId> = match repo.head_commit() {
            Ok(commit) => vec![commit.id().detach()],
            Err(_) => vec![], // Empty repo, no parents
        };

        // Tell the user once about each file that undo does not cover: only
        // paths not already skipped by the previous snapshot are reported.
        if !skipped.is_empty() {
            let previously_skipped = parent_ids
                .first()
                .map(|id| skipped_paths(&repo, *id))
                .unwrap_or_default();
            let newly: Vec<(&String, &String)> = skipped
                .iter()
                .filter(|(path, _)| !previously_skipped.contains(*path))
                .collect();
            for (path, reason) in newly.iter().take(MAX_SKIP_WARNINGS) {
                log::warn!("Snapshot: {path} is not covered by undo ({reason})");
            }
            if newly.len() > MAX_SKIP_WARNINGS {
                log::warn!(
                    "Snapshot: {} more files are not covered by undo",
                    newly.len() - MAX_SKIP_WARNINGS
                );
            }
        }

        let commit_message = if skipped.is_empty() {
            message.to_string()
        } else {
            let paths: Vec<&String> = skipped.keys().collect();
            let json = serde_json::to_string(&paths).map_err(|e| {
                SnapshotError::Repository(format!("Failed to encode skipped paths: {}", e))
            })?;
            format!("{message}\n\n{SKIPPED_TRAILER}{json}\n")
        };

        // Create commit using repo.commit() which reads author/committer from config
        // (we set user.name and user.email in the git config during init)
        let commit_id = repo
            .commit("HEAD", commit_message.as_str(), tree_id, parent_ids)
            .map_err(|e| SnapshotError::Repository(format!("Failed to create commit: {}", e)))?;

        Ok((commit_id.detach().to_string(), stats))
    }

    /// Write `path` into the object database as a blob and return its id.
    ///
    /// Small files are read into memory; large files are streamed so a
    /// multi-hundred-MB model never has to fit in memory. Returns `None` when
    /// the file shrank while it was being read: it has no stable content to
    /// capture, so the caller leaves it out instead of failing the snapshot.
    fn write_file_blob(
        repo: &gix::Repository,
        path: &Path,
        size: u64,
    ) -> SnapshotResult<Option<gix::ObjectId>> {
        let fs_err = |e: std::io::Error| {
            SnapshotError::Filesystem(format!("Failed to read file {}: {}", path.display(), e))
        };

        if size <= STREAM_THRESHOLD_BYTES {
            let content = fs::read(path).map_err(fs_err)?;
            return repo
                .write_blob(&content)
                .map(|id| Some(id.detach()))
                .map_err(|e| SnapshotError::Repository(format!("Failed to write blob: {e}")));
        }

        let file = fs::File::open(path).map_err(fs_err)?;
        // Use the size of the opened file so the declared length matches what
        // we read; the object is only published on success.
        let size = file.metadata().map_err(fs_err)?.len();
        Self::write_stream_blob(repo, file, size)
    }

    /// Stream exactly `size` bytes from `source` into a blob. `Ok(None)` if the
    /// source ended early.
    ///
    /// Unlike `write_blob`, the loose-object writer compresses before checking
    /// whether the object already exists, so this is only used for large files
    /// that missed the stat cache.
    fn write_stream_blob(
        repo: &gix::Repository,
        source: impl std::io::Read,
        size: u64,
    ) -> SnapshotResult<Option<gix::ObjectId>> {
        let mut reader = ExactReader::new(source.take(size), size);
        match repo
            .objects
            .write_stream(gix::objs::Kind::Blob, size, &mut reader)
        {
            Ok(oid) => Ok(Some(oid)),
            Err(_) if reader.shrank => Ok(None),
            Err(e) => Err(SnapshotError::Repository(format!(
                "Failed to write blob: {e}"
            ))),
        }
    }

    /// Get diff between two commits by comparing their trees using `diff_tree_to_tree`
    fn diff_commits(repo: &gix::Repository, from: &str, to: &str) -> SnapshotResult<Vec<PathBuf>> {
        let from_id = gix::ObjectId::from_hex(from.as_bytes())
            .map_err(|_| SnapshotError::InvalidSnapshotId(from.to_string()))?;
        let to_id = gix::ObjectId::from_hex(to.as_bytes())
            .map_err(|_| SnapshotError::InvalidSnapshotId(to.to_string()))?;

        let from_commit = repo
            .find_commit(from_id)
            .map_err(|e| SnapshotError::NotFound(format!("'from' commit {}: {}", from, e)))?;
        let to_commit = repo
            .find_commit(to_id)
            .map_err(|e| SnapshotError::NotFound(format!("'to' commit {}: {}", to, e)))?;

        let from_tree = from_commit
            .tree()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get 'from' tree: {}", e)))?;
        let to_tree = to_commit
            .tree()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get 'to' tree: {}", e)))?;

        // Use Repository::diff_tree_to_tree for convenience
        let changes = repo
            .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)
            .map_err(|e| {
                SnapshotError::Repository(format!("Failed to compute tree diff: {}", e))
            })?;

        let mut changed_files = Vec::new();
        for change in &changes {
            // Skip tree (directory) entries – we only care about file-level changes.
            // gix's diff_tree_to_tree emits tree modifications for every ancestor
            // directory whose hash changed; passing those paths to `restore_paths`
            // would cause it to try to write a raw git tree object to a directory
            // path on disk, which errors and aborts the entire restore.
            if change.entry_mode().is_tree() {
                continue;
            }
            let location = change.location();
            if let Ok(path_str) = std::str::from_utf8(location.as_ref()) {
                changed_files.push(PathBuf::from(path_str));
            }
        }

        // A path the `from` snapshot could not capture (too large, unstable)
        // has no copy to restore, and may not have appeared or disappeared at
        // all; reporting it would make undo delete files it never had a copy
        // of. A path skipped only in `to` is kept: `from` still holds a copy
        // that undo can restore, and `checkout_paths` leaves paths the target
        // snapshot did not capture alone.
        let skipped_from = skipped_paths(repo, from_id);
        if !skipped_from.is_empty() {
            changed_files.retain(|path| {
                let path: String = path.to_string_lossy().into_owned();
                !skipped_from.contains(&path)
            });
        }

        Ok(changed_files)
    }

    /// Restore files from a commit to the worktree
    fn checkout_paths(
        repo: &gix::Repository,
        worktree: &Path,
        commit_sha: &str,
        paths: &[PathBuf],
    ) -> SnapshotResult<()> {
        let commit_id = gix::ObjectId::from_hex(commit_sha.as_bytes())
            .map_err(|_| SnapshotError::InvalidSnapshotId(commit_sha.to_string()))?;
        let commit = repo
            .find_commit(commit_id)
            .map_err(|e| SnapshotError::NotFound(format!("commit {}: {}", commit_sha, e)))?;
        let tree = commit
            .tree()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get tree: {}", e)))?;

        let skipped = skipped_paths(repo, commit_id);

        for path in paths {
            let entry = tree.lookup_entry_by_path(path).map_err(|e| {
                SnapshotError::Repository(format!("Failed to look up tree entry: {}", e))
            })?;

            if let Some(entry) = entry {
                // Defense-in-depth: skip tree (directory) entries.
                // `diff_commits` already filters these out, but callers may pass
                // directory paths directly; trying to write a tree object to a
                // directory path on disk would fail and abort the whole restore.
                if entry.mode().is_tree() {
                    continue;
                }
                let object = entry.object().map_err(|e| {
                    SnapshotError::Repository(format!("Failed to get blob object: {}", e))
                })?;
                let data = object.data.clone();
                let full_path = worktree.join(path);

                // Create parent directories if needed
                if let Some(parent) = full_path.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        SnapshotError::Filesystem(format!(
                            "Failed to create parent directory: {}",
                            e
                        ))
                    })?;
                }

                fs::write(&full_path, &data).map_err(|e| {
                    SnapshotError::Filesystem(format!("Failed to write file: {}", e))
                })?;
            } else {
                // The snapshot has no copy of this file. If it simply could not
                // capture it (too large, unstable) the file is not "new" and
                // must be left alone rather than deleted.
                let path_key: String = path.to_string_lossy().into_owned();
                if skipped.contains(&path_key) {
                    continue;
                }
                // File doesn't exist in this snapshot - remove it if it exists
                let full_path = worktree.join(path);
                if full_path.exists() {
                    let _ = fs::remove_file(&full_path);
                }
            }
        }

        Ok(())
    }

    /// Restore entire worktree from a commit
    fn checkout_all(
        repo: &gix::Repository,
        worktree: &Path,
        commit_sha: &str,
    ) -> SnapshotResult<()> {
        let commit_id = gix::ObjectId::from_hex(commit_sha.as_bytes())
            .map_err(|_| SnapshotError::InvalidSnapshotId(commit_sha.to_string()))?;
        let commit = repo
            .find_commit(commit_id)
            .map_err(|e| SnapshotError::NotFound(format!("commit {}: {}", commit_sha, e)))?;
        let tree = commit
            .tree()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get tree: {}", e)))?;

        // Collect all files from the tree recursively
        Self::restore_tree_recursive(repo, worktree, &tree, &PathBuf::new())?;

        Ok(())
    }

    /// Recursively restore a tree to the worktree
    #[allow(clippy::only_used_in_recursion)]
    fn restore_tree_recursive(
        repo: &gix::Repository,
        worktree: &Path,
        tree: &gix::Tree<'_>,
        prefix: &Path,
    ) -> SnapshotResult<()> {
        for entry_result in tree.iter() {
            let entry_ref = entry_result.map_err(|e| {
                SnapshotError::Repository(format!("Failed to read tree entry: {}", e))
            })?;
            let name = std::str::from_utf8(entry_ref.filename().as_ref()).map_err(|e| {
                SnapshotError::Repository(format!("Invalid UTF-8 in filename: {}", e))
            })?;
            let entry_path = prefix.join(name);

            if entry_ref.mode().is_tree() {
                // Recurse into subtree
                let sub_object = entry_ref.object().map_err(|e| {
                    SnapshotError::Repository(format!("Failed to find subtree object: {}", e))
                })?;
                let sub_tree = sub_object
                    .try_into_tree()
                    .map_err(|_| SnapshotError::Repository("Expected tree object".to_string()))?;
                Self::restore_tree_recursive(repo, worktree, &sub_tree, &entry_path)?;
            } else if entry_ref.mode().is_blob() || entry_ref.mode().is_blob_or_symlink() {
                // Restore blob
                let object = entry_ref.object().map_err(|e| {
                    SnapshotError::Repository(format!("Failed to find blob object: {}", e))
                })?;
                let full_path = worktree.join(&entry_path);

                if let Some(parent) = full_path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&full_path, &object.data);
            }
        }

        Ok(())
    }

    /// List all commits with timestamps by walking from HEAD
    fn list_commits(repo: &gix::Repository) -> SnapshotResult<Vec<(String, i64)>> {
        let head = match repo.head_commit() {
            Ok(commit) => commit,
            Err(_) => return Ok(Vec::new()), // Empty repo
        };

        let mut commits = Vec::new();
        let mut current = Some(head.id().detach());

        while let Some(oid) = current {
            match repo.find_commit(oid) {
                Ok(commit) => {
                    let timestamp = commit.time().map(|t| t.seconds).unwrap_or(0);
                    commits.push((oid.to_string(), timestamp));

                    // Get first parent
                    current = commit.parent_ids().next().map(|id| id.detach());
                }
                Err(_) => break,
            }
        }

        Ok(commits)
    }
}

#[async_trait]
impl SnapshotBackend for GitSnapshotBackend {
    fn is_available(&self, worktree: &Path) -> bool {
        worktree.exists() && worktree.is_dir() && dirs::home_dir().is_some()
    }

    async fn track(&self, worktree: &Path) -> SnapshotResult<SnapshotId> {
        let worktree = worktree.to_path_buf();
        let span = tracing::info_span!(
            "snapshot.git.track",
            files = tracing::field::Empty,
            hashed_files = tracing::field::Empty,
            cached_files = tracing::field::Empty,
            skipped_large_files = tracing::field::Empty,
            skipped_unstable_files = tracing::field::Empty,
            bytes_hashed = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
        );
        let backend = self.clone();

        tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let started = Instant::now();
                let (id, stats) = backend.create_snapshot(&worktree, "snapshot")?;
                tracing::Span::current()
                    .record("files", stats.files)
                    .record("hashed_files", stats.hashed_files)
                    .record("cached_files", stats.cached_files)
                    .record("skipped_large_files", stats.skipped_large_files)
                    .record("skipped_unstable_files", stats.skipped_unstable_files)
                    .record("bytes_hashed", stats.bytes_hashed)
                    .record("elapsed_ms", started.elapsed().as_millis() as u64);
                Ok(id)
            })
        })
        .await
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn diff(
        &self,
        worktree: &Path,
        pre: &SnapshotId,
        post: &SnapshotId,
    ) -> SnapshotResult<Vec<PathBuf>> {
        let worktree = worktree.to_path_buf();
        let pre = pre.clone();
        let post = post.clone();
        #[cfg(test)]
        let snapshot_base = self.snapshot_base_override.clone();

        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let backend = if let Some(base) = snapshot_base {
                GitSnapshotBackend::with_snapshot_base(base)
            } else {
                GitSnapshotBackend::new()
            };
            #[cfg(not(test))]
            let backend = GitSnapshotBackend::new();

            let repo = backend.open_or_init(&worktree)?;
            Self::diff_commits(&repo, &pre, &post)
        })
        .await
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn restore_paths(
        &self,
        worktree: &Path,
        snapshot: &SnapshotId,
        paths: &[PathBuf],
    ) -> SnapshotResult<()> {
        let worktree = worktree.to_path_buf();
        let snapshot = snapshot.clone();
        let paths = paths.to_vec();
        #[cfg(test)]
        let snapshot_base = self.snapshot_base_override.clone();

        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let backend = if let Some(base) = snapshot_base {
                GitSnapshotBackend::with_snapshot_base(base)
            } else {
                GitSnapshotBackend::new()
            };
            #[cfg(not(test))]
            let backend = GitSnapshotBackend::new();

            let repo = backend.open_or_init(&worktree)?;
            Self::checkout_paths(&repo, &worktree, &snapshot, &paths)
        })
        .await
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn restore(&self, worktree: &Path, snapshot: &SnapshotId) -> SnapshotResult<()> {
        let worktree = worktree.to_path_buf();
        let snapshot = snapshot.clone();
        #[cfg(test)]
        let snapshot_base = self.snapshot_base_override.clone();

        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let backend = if let Some(base) = snapshot_base {
                GitSnapshotBackend::with_snapshot_base(base)
            } else {
                GitSnapshotBackend::new()
            };
            #[cfg(not(test))]
            let backend = GitSnapshotBackend::new();

            let repo = backend.open_or_init(&worktree)?;
            Self::checkout_all(&repo, &worktree, &snapshot)
        })
        .await
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn gc(&self, worktree: &Path, config: &GcConfig) -> SnapshotResult<GcResult> {
        let worktree = worktree.to_path_buf();
        let config = config.clone();
        #[cfg(test)]
        let snapshot_base = self.snapshot_base_override.clone();

        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let backend = if let Some(base) = snapshot_base {
                GitSnapshotBackend::with_snapshot_base(base)
            } else {
                GitSnapshotBackend::new()
            };
            #[cfg(not(test))]
            let backend = GitSnapshotBackend::new();

            let repo = backend.open_or_init(&worktree)?;
            let commits = Self::list_commits(&repo)?;

            let total_count = commits.len();
            let now = OffsetDateTime::now_utc().unix_timestamp();

            // Filter by age
            let mut kept: Vec<_> = if let Some(max_age_days) = config.max_age_days {
                let cutoff = now - (max_age_days as i64 * 86400);
                commits
                    .into_iter()
                    .filter(|(_, timestamp)| *timestamp >= cutoff)
                    .collect()
            } else {
                commits
            };

            // Limit by count (keep most recent)
            if let Some(max_snapshots) = config.max_snapshots {
                kept.truncate(max_snapshots);
            }

            let removed = total_count.saturating_sub(kept.len());

            Ok(GcResult {
                removed_count: removed,
                remaining_count: kept.len(),
            })
        })
        .await
        .map_err(|_| SnapshotError::TaskPanicked)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_snapshot_dir_deterministic() {
        let tmpbase = TempDir::new().unwrap();
        let path1 = PathBuf::from("/tmp/test-project");
        let path2 = PathBuf::from("/tmp/test-project");

        let backend1 = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let backend2 = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());

        let dir1 = backend1.snapshot_dir(&path1).unwrap();
        let dir2 = backend2.snapshot_dir(&path2).unwrap();

        assert_eq!(dir1, dir2, "Same path should produce same snapshot dir");
    }

    #[tokio::test]
    async fn test_track_creates_snapshot() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        fs::write(tmpdir.path().join("test.txt"), "hello").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot_id = backend.track(tmpdir.path()).await.unwrap();

        assert!(
            !snapshot_id.is_empty(),
            "Should return non-empty snapshot ID"
        );
        assert_eq!(snapshot_id.len(), 40, "Git SHA should be 40 hex characters");
    }

    #[tokio::test]
    async fn test_diff_detects_changes() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let file_path = tmpdir.path().join("test.txt");

        fs::write(&file_path, "initial").unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

        fs::write(&file_path, "modified").unwrap();
        let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

        let diff = backend
            .diff(tmpdir.path(), &snapshot1, &snapshot2)
            .await
            .unwrap();

        assert_eq!(diff.len(), 1, "Should detect one changed file");
        assert_eq!(
            diff[0],
            PathBuf::from("test.txt"),
            "Should identify test.txt as changed"
        );
    }

    #[tokio::test]
    async fn test_restore_paths_works() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let file_path = tmpdir.path().join("test.txt");

        fs::write(&file_path, "original").unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot = backend.track(tmpdir.path()).await.unwrap();

        fs::write(&file_path, "modified").unwrap();
        backend
            .restore_paths(tmpdir.path(), &snapshot, &[PathBuf::from("test.txt")])
            .await
            .unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "original", "File should be restored to original");
    }

    #[tokio::test]
    async fn test_restore_full() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
        fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot = backend.track(tmpdir.path()).await.unwrap();

        // Modify both files
        fs::write(tmpdir.path().join("a.txt"), "AAA").unwrap();
        fs::write(tmpdir.path().join("b.txt"), "BBB").unwrap();

        backend.restore(tmpdir.path(), &snapshot).await.unwrap();

        assert_eq!(
            fs::read_to_string(tmpdir.path().join("a.txt")).unwrap(),
            "aaa"
        );
        assert_eq!(
            fs::read_to_string(tmpdir.path().join("b.txt")).unwrap(),
            "bbb"
        );
    }

    #[tokio::test]
    async fn test_gc_by_count() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());

        // Create 5 snapshots
        for i in 0..5 {
            fs::write(tmpdir.path().join("test.txt"), format!("v{}", i)).unwrap();
            backend.track(tmpdir.path()).await.unwrap();
        }

        let config = GcConfig {
            max_snapshots: Some(3),
            max_age_days: None,
        };

        let result = backend.gc(tmpdir.path(), &config).await.unwrap();
        assert_eq!(result.remaining_count, 3);
        assert_eq!(result.removed_count, 2);
    }

    // ==================== Snapshot GC vs workspace references ====================

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git must be available");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Undo snapshot garbage collection must never remove the workspace
    /// baseline/result references the worktree service keeps in the real
    /// project repository: snapshot history and workspace refs are
    /// independent lifecycles.
    #[tokio::test]
    async fn test_gc_leaves_workspace_baseline_refs_reachable() {
        // A real project repository.
        let project = TempDir::new().unwrap();
        run_git(project.path(), &["init", "-q"]);
        run_git(project.path(), &["config", "user.email", "qmt@example.com"]);
        run_git(project.path(), &["config", "user.name", "qmt"]);
        fs::write(project.path().join("app.txt"), "app").unwrap();
        run_git(project.path(), &["add", "."]);
        run_git(project.path(), &["commit", "-q", "-m", "init"]);

        // The baseline ref and a retained result ref the worktree service
        // would create for a workspace.
        let workspace_id = crate::worktree::WorkspaceId::generate();
        let baseline_ref = crate::worktree::baseline_ref_for(&workspace_id);
        run_git(project.path(), &["update-ref", &baseline_ref, "HEAD"]);
        let result_ref = format!("refs/qmt/results/{workspace_id}");
        run_git(project.path(), &["branch", "ws-result", "HEAD"]);
        run_git(project.path(), &["update-ref", &result_ref, "ws-result"]);

        // A linked worktree of that project with undo snapshot history.
        let linked = TempDir::new().unwrap();
        run_git(
            project.path(),
            &[
                "worktree",
                "add",
                "-b",
                "qmt/ws-gc-test",
                linked.path().to_str().unwrap(),
            ],
        );

        let snapshot_base = TempDir::new().unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(snapshot_base.path().to_path_buf());
        for i in 0..3 {
            fs::write(linked.path().join("scratch.txt"), format!("v{i}")).unwrap();
            backend.track(linked.path()).await.unwrap();
        }

        // Collect almost all snapshot history.
        let config = GcConfig {
            max_snapshots: Some(1),
            max_age_days: None,
        };
        let result = backend.gc(linked.path(), &config).await.unwrap();
        assert_eq!(result.remaining_count, 1);
        assert_eq!(result.removed_count, 2);

        // The project's workspace baseline ref and retained result ref are
        // still reachable.
        run_git(project.path(), &["rev-parse", "--verify", &baseline_ref]);
        run_git(project.path(), &["rev-parse", "--verify", &result_ref]);

        // The linked worktree still works: its administrative marker and
        // the shared repository metadata are intact.
        assert!(linked.path().join(".git").exists());
        run_git(linked.path(), &["status", "--porcelain"]);
    }

    // ==================== Test Suite 1.1: Track Idempotency & Determinism ====================

    #[tokio::test]
    async fn test_track_idempotent_no_change() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        fs::write(tmpdir.path().join("test.txt"), "hello").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

        // Note: Due to git timestamps, snapshot IDs will differ even with same content.
        // Instead, verify that both snapshots succeed and detect no differences.
        let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

        // The snapshots will have different commit hashes due to timestamps,
        // but diff should show no changes
        let diff = backend
            .diff(tmpdir.path(), &snapshot1, &snapshot2)
            .await
            .unwrap();

        assert_eq!(
            diff.len(),
            0,
            "No modifications should result in empty diff"
        );
    }

    #[tokio::test]
    async fn test_track_changes_after_modify() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let file_path = tmpdir.path().join("test.txt");

        fs::write(&file_path, "initial").unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

        fs::write(&file_path, "modified").unwrap();
        let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

        assert_ne!(
            snapshot1, snapshot2,
            "Track after modifications should return different snapshot ID"
        );
    }

    #[tokio::test]
    async fn test_track_empty_directory() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot_id = backend.track(tmpdir.path()).await.unwrap();

        assert!(
            !snapshot_id.is_empty(),
            "Track empty directory should return valid snapshot ID"
        );
    }

    // ==================== Test Suite 1.2: Diff Detection ====================

    #[tokio::test]
    async fn test_diff_new_file_added() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

        fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();
        let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

        let diff = backend
            .diff(tmpdir.path(), &snapshot1, &snapshot2)
            .await
            .unwrap();

        assert_eq!(diff.len(), 1, "Should detect one new file");
        assert!(
            diff.contains(&PathBuf::from("b.txt")),
            "Diff should contain b.txt"
        );
    }

    #[tokio::test]
    async fn test_diff_file_deleted() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
        fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

        fs::remove_file(tmpdir.path().join("b.txt")).unwrap();
        let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

        let diff = backend
            .diff(tmpdir.path(), &snapshot1, &snapshot2)
            .await
            .unwrap();

        assert_eq!(diff.len(), 1, "Should detect one deleted file");
        assert!(
            diff.contains(&PathBuf::from("b.txt")),
            "Diff should contain b.txt"
        );
    }

    #[tokio::test]
    async fn test_diff_no_changes_empty() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        fs::write(tmpdir.path().join("test.txt"), "hello").unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot1 = backend.track(tmpdir.path()).await.unwrap();
        let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

        let diff = backend
            .diff(tmpdir.path(), &snapshot1, &snapshot2)
            .await
            .unwrap();

        assert_eq!(
            diff.len(),
            0,
            "No modifications should result in empty diff"
        );
    }

    #[tokio::test]
    async fn test_diff_nested_directory() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        let nested_dir = tmpdir.path().join("src/foo");
        fs::create_dir_all(&nested_dir).unwrap();
        let nested_file = nested_dir.join("bar.rs");
        fs::write(&nested_file, "initial").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

        fs::write(&nested_file, "modified").unwrap();
        let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

        let diff = backend
            .diff(tmpdir.path(), &snapshot1, &snapshot2)
            .await
            .unwrap();

        // Git's diff returns tree entries - may include directories
        // Verify the file is in the diff (it may report src, src/foo, and src/foo/bar.rs)
        assert!(
            diff.contains(&PathBuf::from("src/foo/bar.rs")),
            "Diff should return the modified file path"
        );
    }

    // ==================== Test Suite 1.3: Selective Restore (restore_paths) ====================

    #[tokio::test]
    async fn test_restore_paths_deletes_new_file() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot = backend.track(tmpdir.path()).await.unwrap();

        fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();
        assert!(tmpdir.path().join("b.txt").exists(), "b.txt should exist");

        backend
            .restore_paths(tmpdir.path(), &snapshot, &[PathBuf::from("b.txt")])
            .await
            .unwrap();

        assert!(
            !tmpdir.path().join("b.txt").exists(),
            "b.txt should be removed after restore"
        );
    }

    #[tokio::test]
    async fn test_restore_paths_selective() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
        fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();
        fs::write(tmpdir.path().join("c.txt"), "ccc").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot = backend.track(tmpdir.path()).await.unwrap();

        fs::write(tmpdir.path().join("a.txt"), "AAA").unwrap();
        fs::write(tmpdir.path().join("b.txt"), "BBB").unwrap();
        fs::write(tmpdir.path().join("c.txt"), "CCC").unwrap();

        backend
            .restore_paths(
                tmpdir.path(),
                &snapshot,
                &[PathBuf::from("a.txt"), PathBuf::from("b.txt")],
            )
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(tmpdir.path().join("a.txt")).unwrap(),
            "aaa",
            "a.txt should be restored"
        );
        assert_eq!(
            fs::read_to_string(tmpdir.path().join("b.txt")).unwrap(),
            "bbb",
            "b.txt should be restored"
        );
        assert_eq!(
            fs::read_to_string(tmpdir.path().join("c.txt")).unwrap(),
            "CCC",
            "c.txt should remain untouched"
        );
    }

    #[tokio::test]
    async fn test_restore_paths_nested() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        let nested_dir = tmpdir.path().join("src/deep/nested");
        fs::create_dir_all(&nested_dir).unwrap();
        let nested_file = nested_dir.join("file.rs");
        fs::write(&nested_file, "original").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot = backend.track(tmpdir.path()).await.unwrap();

        fs::write(&nested_file, "modified").unwrap();

        backend
            .restore_paths(
                tmpdir.path(),
                &snapshot,
                &[PathBuf::from("src/deep/nested/file.rs")],
            )
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(&nested_file).unwrap(),
            "original",
            "Nested file should be restored"
        );
    }

    #[tokio::test]
    async fn test_restore_paths_empty_list() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        fs::write(tmpdir.path().join("test.txt"), "hello").unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot = backend.track(tmpdir.path()).await.unwrap();

        fs::write(tmpdir.path().join("test.txt"), "modified").unwrap();

        backend
            .restore_paths(tmpdir.path(), &snapshot, &[])
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(tmpdir.path().join("test.txt")).unwrap(),
            "modified",
            "File should remain modified after empty restore"
        );
    }

    // ==================== Test Suite 1.4: Full Restore & Edge Cases ====================

    #[tokio::test]
    async fn test_restore_full_does_not_remove_new_files() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot = backend.track(tmpdir.path()).await.unwrap();

        fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();

        backend.restore(tmpdir.path(), &snapshot).await.unwrap();

        assert!(
            tmpdir.path().join("a.txt").exists(),
            "a.txt should still exist"
        );
        assert_eq!(
            fs::read_to_string(tmpdir.path().join("a.txt")).unwrap(),
            "aaa",
            "a.txt should be restored"
        );

        // Bug #5: This documents the current limitation - b.txt is NOT removed
        assert!(
            tmpdir.path().join("b.txt").exists(),
            "BUG: b.txt still exists after full restore (documents Bug #5)"
        );
    }

    #[tokio::test]
    async fn test_multi_snapshot_chain_restore() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());

        let file_path = tmpdir.path().join("test.txt");

        // Create 5 snapshots with progressive changes
        fs::write(&file_path, "v1").unwrap();
        let _snapshot1 = backend.track(tmpdir.path()).await.unwrap();

        fs::write(&file_path, "v2").unwrap();
        let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

        fs::write(&file_path, "v3").unwrap();
        let _snapshot3 = backend.track(tmpdir.path()).await.unwrap();

        fs::write(&file_path, "v4").unwrap();
        let _snapshot4 = backend.track(tmpdir.path()).await.unwrap();

        fs::write(&file_path, "v5").unwrap();
        let _snapshot5 = backend.track(tmpdir.path()).await.unwrap();

        // Restore to snapshot #2
        backend.restore(tmpdir.path(), &snapshot2).await.unwrap();

        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "v2",
            "Should restore to snapshot #2 state"
        );
    }

    #[tokio::test]
    async fn test_restore_binary_file() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        let binary_data: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let file_path = tmpdir.path().join("test.bin");
        fs::write(&file_path, &binary_data).unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot = backend.track(tmpdir.path()).await.unwrap();

        let modified_data: Vec<u8> = vec![0xFF, 0xFF, 0xFF, 0xFF];
        fs::write(&file_path, &modified_data).unwrap();

        backend
            .restore_paths(tmpdir.path(), &snapshot, &[PathBuf::from("test.bin")])
            .await
            .unwrap();

        let restored_data = fs::read(&file_path).unwrap();
        assert_eq!(
            restored_data, binary_data,
            "Binary content should roundtrip correctly"
        );
    }

    // ==================== Regression tests: directory path handling ====================

    /// Regression: diff() must return only file paths, not intermediate directory paths.
    ///
    /// gix's diff_tree_to_tree emits change entries for every ancestor tree whose
    /// hash changed (e.g. "src", "src/foo") in addition to the actual file
    /// ("src/foo/bar.rs"). Passing those directory paths to restore_paths causes it
    /// to try to write raw git tree object bytes to a directory path on disk, which
    /// fails and aborts the entire undo operation.
    #[tokio::test]
    async fn test_diff_nested_returns_only_files() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        let nested_dir = tmpdir.path().join("src/foo");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::write(nested_dir.join("bar.rs"), "initial").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

        fs::write(nested_dir.join("bar.rs"), "modified").unwrap();
        let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

        let diff = backend
            .diff(tmpdir.path(), &snapshot1, &snapshot2)
            .await
            .unwrap();

        // Must only contain the file path, not intermediate directories "src" or "src/foo"
        assert_eq!(
            diff.len(),
            1,
            "diff should return exactly one entry (the file), got: {:?}",
            diff
        );
        assert_eq!(
            diff[0],
            PathBuf::from("src/foo/bar.rs"),
            "diff should identify src/foo/bar.rs"
        );
    }

    /// Regression: restore_paths must not error when directory paths are included.
    ///
    /// Even after the fix to diff(), external callers could pass directory paths.
    /// The defensive check in checkout_paths must skip them gracefully so that
    /// actual file paths in the same batch are still restored.
    #[tokio::test]
    async fn test_restore_paths_with_directory_path_does_not_error() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();

        let src_dir = tmpdir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("main.rs"), "original").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot = backend.track(tmpdir.path()).await.unwrap();

        fs::write(src_dir.join("main.rs"), "modified").unwrap();

        // Pass both the directory path "src" AND the file path – previously this
        // caused restore_paths to error on "src" before reaching "src/main.rs".
        backend
            .restore_paths(
                tmpdir.path(),
                &snapshot,
                &[PathBuf::from("src"), PathBuf::from("src/main.rs")],
            )
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(src_dir.join("main.rs")).unwrap(),
            "original",
            "file should be restored even when directory paths are included in the list"
        );
    }

    // ==================== Stat cache, streaming, size limit ====================

    /// Backdate a file's mtime so it is outside the "racily clean" window and
    /// therefore eligible for the stat cache.
    fn backdate(path: &Path) {
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    fn track_stats(backend: &GitSnapshotBackend, worktree: &Path) -> SnapshotStats {
        backend.create_snapshot(worktree, "snapshot").unwrap().1
    }

    #[test]
    fn test_second_snapshot_reuses_stat_cache() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let a = tmpdir.path().join("a.txt");
        let b = tmpdir.path().join("b.txt");
        fs::write(&a, "aaa").unwrap();
        fs::write(&b, "bbb").unwrap();
        backdate(&a);
        backdate(&b);

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let first = track_stats(&backend, tmpdir.path());
        assert_eq!((first.hashed_files, first.cached_files), (2, 0));
        assert_eq!(first.bytes_hashed, 6);

        let second = track_stats(&backend, tmpdir.path());
        assert_eq!(second.files, 2);
        assert_eq!(
            second.cached_files, 2,
            "unchanged files must not be re-read"
        );
        assert_eq!(second.hashed_files, 0);
        assert_eq!(second.bytes_hashed, 0);
    }

    #[test]
    fn test_stat_cache_rehashes_modified_file() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let a = tmpdir.path().join("a.txt");
        let b = tmpdir.path().join("b.txt");
        fs::write(&a, "aaa").unwrap();
        fs::write(&b, "bbb").unwrap();
        backdate(&a);
        backdate(&b);

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let (id1, _) = backend.create_snapshot(tmpdir.path(), "s").unwrap();

        // Same length, new mtime: must be detected via mtime.
        fs::write(&a, "AAA").unwrap();
        let (id2, stats) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
        assert_eq!(stats.hashed_files, 1);
        assert_eq!(stats.cached_files, 1);

        let repo = gix::open(
            backend
                .snapshot_dir(tmpdir.path())
                .expect("snapshot dir resolves"),
        )
        .unwrap();
        let changed = GitSnapshotBackend::diff_commits(&repo, &id1, &id2).unwrap();
        assert_eq!(changed, vec![PathBuf::from("a.txt")]);
    }

    #[test]
    fn test_racily_clean_files_are_not_cached() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        // Freshly written: mtime is inside the racy window.
        fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        track_stats(&backend, tmpdir.path());
        let second = track_stats(&backend, tmpdir.path());
        assert_eq!(second.cached_files, 0);
        assert_eq!(second.hashed_files, 1);
    }

    #[test]
    fn test_corrupt_or_stale_stat_cache_is_harmless() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let a = tmpdir.path().join("a.txt");
        fs::write(&a, "aaa").unwrap();
        backdate(&a);

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        track_stats(&backend, tmpdir.path());

        let cache_path = backend
            .snapshot_dir(tmpdir.path())
            .unwrap()
            .join(STAT_CACHE_FILE);
        assert!(cache_path.exists(), "cache must be persisted");

        fs::write(&cache_path, b"{ not json").unwrap();
        let stats = track_stats(&backend, tmpdir.path());
        assert_eq!(stats.hashed_files, 1, "corrupt cache falls back to hashing");
        assert_eq!(stats.files, 1);
    }

    #[test]
    fn test_cache_entry_with_missing_object_is_rehashed() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let a = tmpdir.path().join("a.txt");
        fs::write(&a, "aaa").unwrap();
        backdate(&a);

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        track_stats(&backend, tmpdir.path());

        // Point the cache at an object that does not exist.
        let cache_path = backend
            .snapshot_dir(tmpdir.path())
            .unwrap()
            .join(STAT_CACHE_FILE);
        let mut cache: StatCache = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
        for entry in cache.entries.values_mut() {
            entry.oid = "1".repeat(40);
        }
        fs::write(&cache_path, serde_json::to_vec(&cache).unwrap()).unwrap();

        let stats = track_stats(&backend, tmpdir.path());
        assert_eq!(stats.hashed_files, 1);
        assert_eq!(stats.cached_files, 0);
    }

    #[test]
    fn test_files_over_max_file_bytes_are_skipped() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        fs::write(tmpdir.path().join("small.txt"), "ok").unwrap();
        fs::write(tmpdir.path().join("model.bin"), vec![7u8; 4096]).unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf())
            .with_max_file_bytes(Some(1024));
        let (id, stats) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
        assert_eq!(stats.files, 1);
        assert_eq!(stats.skipped_large_files, 1);
        assert_eq!(stats.bytes_hashed, 2, "skipped files are never read");

        let repo = gix::open(backend.snapshot_dir(tmpdir.path()).unwrap()).unwrap();
        let commit = repo
            .find_commit(gix::ObjectId::from_hex(id.as_bytes()).unwrap())
            .unwrap();
        let tree = commit.tree().unwrap();
        assert!(tree.lookup_entry_by_path("small.txt").unwrap().is_some());
        assert!(tree.lookup_entry_by_path("model.bin").unwrap().is_none());
    }

    #[test]
    fn test_zero_max_file_bytes_means_unlimited() {
        let backend = GitSnapshotBackend::new().with_max_file_bytes(Some(0));
        assert_eq!(backend.max_file_bytes, None);
        assert_eq!(
            GitSnapshotBackend::new().max_file_bytes,
            Some(DEFAULT_MAX_FILE_BYTES)
        );
    }

    #[test]
    fn test_dot_directories_are_pruned() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        fs::create_dir_all(tmpdir.path().join(".venv/lib")).unwrap();
        fs::write(tmpdir.path().join(".venv/lib/x.py"), "x").unwrap();
        fs::write(tmpdir.path().join(".hidden"), "h").unwrap();
        fs::create_dir_all(tmpdir.path().join("src/.cache")).unwrap();
        fs::write(tmpdir.path().join("src/.cache/y"), "y").unwrap();
        fs::write(tmpdir.path().join("src/lib.rs"), "fn main() {}").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let stats = track_stats(&backend, tmpdir.path());
        assert_eq!(stats.files, 1, "only src/lib.rs belongs in the snapshot");
    }

    #[test]
    fn test_streamed_large_file_round_trips() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let size = (STREAM_THRESHOLD_BYTES + 1024 * 1024) as usize;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let big = tmpdir.path().join("big.bin");
        fs::write(&big, &data).unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf())
            .with_max_file_bytes(None);
        let (id, stats) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
        assert_eq!(stats.bytes_hashed, size as u64);

        // Overwrite, then restore from the streamed blob.
        fs::write(&big, b"clobbered").unwrap();
        let repo = gix::open(backend.snapshot_dir(tmpdir.path()).unwrap()).unwrap();
        GitSnapshotBackend::checkout_paths(&repo, tmpdir.path(), &id, &[PathBuf::from("big.bin")])
            .unwrap();
        assert!(
            fs::read(&big).unwrap() == data,
            "streamed blob must round-trip byte for byte"
        );
    }

    #[test]
    fn test_streamed_blob_matches_in_memory_hash() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let repo = backend.open_or_init(tmpdir.path()).unwrap();

        let size = STREAM_THRESHOLD_BYTES + 17;
        let path = tmpdir.path().join("blob.bin");
        fs::write(&path, vec![42u8; size as usize]).unwrap();

        let streamed = GitSnapshotBackend::write_file_blob(&repo, &path, size)
            .unwrap()
            .expect("file is stable");
        let expected = gix::objs::compute_hash(
            repo.object_hash(),
            gix::objs::Kind::Blob,
            &fs::read(&path).unwrap(),
        )
        .unwrap();
        assert_eq!(streamed, expected);
        assert!(repo.has_object(streamed));
    }

    // ==================== Review fixes ====================

    fn repo_for(backend: &GitSnapshotBackend, worktree: &Path) -> gix::Repository {
        gix::open(backend.snapshot_dir(worktree).unwrap()).unwrap()
    }

    /// A file that grows past `max_file_bytes` during a turn is not a deletion:
    /// diff still reports it (the pre-turn copy is restorable by undo), and a
    /// restore from the snapshot that could not capture it leaves it alone.
    #[test]
    fn test_file_growing_past_limit_is_not_reported_deleted_or_removed() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let big = tmpdir.path().join("data.bin");
        fs::write(&big, vec![1u8; 512]).unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf())
            .with_max_file_bytes(Some(1024));
        let (pre, _) = backend.create_snapshot(tmpdir.path(), "s").unwrap();

        fs::write(&big, vec![2u8; 4096]).unwrap();
        let (post, stats) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
        assert_eq!(stats.skipped_large_files, 1);

        let repo = repo_for(&backend, tmpdir.path());
        let changed = GitSnapshotBackend::diff_commits(&repo, &pre, &post).unwrap();
        assert_eq!(
            changed,
            vec![PathBuf::from("data.bin")],
            "pre-turn copy must stay restorable by undo"
        );

        // Undo restores the pre-turn copy.
        GitSnapshotBackend::checkout_paths(&repo, tmpdir.path(), &pre, &changed).unwrap();
        assert_eq!(fs::read(&big).unwrap(), vec![1u8; 512]);
        fs::write(&big, vec![2u8; 4096]).unwrap();

        // Redo-style restore from the snapshot that could not capture it.
        GitSnapshotBackend::checkout_paths(
            &repo,
            tmpdir.path(),
            &post,
            &[PathBuf::from("data.bin")],
        )
        .unwrap();
        assert_eq!(
            fs::read(&big).unwrap(),
            vec![2u8; 4096],
            "file must survive"
        );
    }

    /// A file that shrinks below the limit during a turn must not be reported
    /// as added (which would make undo delete it).
    #[test]
    fn test_file_shrinking_below_limit_is_not_reported_added() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let f = tmpdir.path().join("data.bin");
        fs::write(&f, vec![1u8; 4096]).unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf())
            .with_max_file_bytes(Some(1024));
        let (pre, _) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
        fs::write(&f, vec![2u8; 100]).unwrap();
        let (post, _) = backend.create_snapshot(tmpdir.path(), "s").unwrap();

        let repo = repo_for(&backend, tmpdir.path());
        assert!(
            GitSnapshotBackend::diff_commits(&repo, &pre, &post)
                .unwrap()
                .is_empty()
        );
        // Undo to `pre` (which never had a copy) must leave the file alone.
        GitSnapshotBackend::checkout_paths(
            &repo,
            tmpdir.path(),
            &pre,
            &[PathBuf::from("data.bin")],
        )
        .unwrap();
        assert_eq!(fs::read(&f).unwrap(), vec![2u8; 100]);
    }

    /// Genuinely new files are still reported and removed on undo.
    #[test]
    fn test_skipped_paths_do_not_hide_real_changes() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        fs::write(tmpdir.path().join("model.bin"), vec![0u8; 4096]).unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf())
            .with_max_file_bytes(Some(1024));
        let (pre, _) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
        fs::write(tmpdir.path().join("new.txt"), "n").unwrap();
        let (post, _) = backend.create_snapshot(tmpdir.path(), "s").unwrap();

        let repo = repo_for(&backend, tmpdir.path());
        let changed = GitSnapshotBackend::diff_commits(&repo, &pre, &post).unwrap();
        assert_eq!(changed, vec![PathBuf::from("new.txt")]);
        GitSnapshotBackend::checkout_paths(&repo, tmpdir.path(), &pre, &changed).unwrap();
        assert!(!tmpdir.path().join("new.txt").exists());
        assert!(tmpdir.path().join("model.bin").exists());
    }

    #[test]
    fn test_skipped_trailer_round_trips() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        fs::write(tmpdir.path().join("big one.bin"), vec![0u8; 4096]).unwrap();
        fs::write(tmpdir.path().join("ok.txt"), "x").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf())
            .with_max_file_bytes(Some(1024));
        let (id, _) = backend.create_snapshot(tmpdir.path(), "snapshot").unwrap();
        let repo = repo_for(&backend, tmpdir.path());
        let skipped = skipped_paths(&repo, gix::ObjectId::from_hex(id.as_bytes()).unwrap());
        assert_eq!(
            skipped.into_iter().collect::<Vec<_>>(),
            vec!["big one.bin".to_string()]
        );

        // A snapshot without skips has no trailer.
        let tmp2 = TempDir::new().unwrap();
        fs::write(tmp2.path().join("a"), "a").unwrap();
        let (id2, _) = backend.create_snapshot(tmp2.path(), "snapshot").unwrap();
        let repo2 = repo_for(&backend, tmp2.path());
        assert!(skipped_paths(&repo2, gix::ObjectId::from_hex(id2.as_bytes()).unwrap()).is_empty());
    }

    /// Rewriting a file with the same size and mtime (`cp -p`, `touch -r`) is
    /// still noticed because ctime / inode change.
    #[cfg(unix)]
    #[test]
    fn test_same_size_and_mtime_rewrite_is_detected() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let f = tmpdir.path().join("a.txt");
        fs::write(&f, "aaa").unwrap();
        backdate(&f);
        let mtime = fs::metadata(&f).unwrap().modified().unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let (id1, first) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
        assert_eq!(first.hashed_files, 1);

        // Same length, mtime restored to the exact previous value.
        // Sleep so the ctime of the rewrite differs on coarse-grained filesystems.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&f, "bbb").unwrap();
        fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let (id2, second) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
        assert_eq!(
            second.hashed_files, 1,
            "ctime change must invalidate the cache"
        );
        let repo = repo_for(&backend, tmpdir.path());
        assert_eq!(
            GitSnapshotBackend::diff_commits(&repo, &id1, &id2).unwrap(),
            vec![PathBuf::from("a.txt")]
        );
    }

    /// A file that is shorter than declared while streaming is skipped, not an
    /// error that fails the whole snapshot.
    #[test]
    fn test_shrinking_stream_is_skipped_not_fatal() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let repo = backend.open_or_init(tmpdir.path()).unwrap();

        // Source yields 10 bytes but 100 were declared.
        let short = std::io::Cursor::new(vec![9u8; 10]);
        let result = GitSnapshotBackend::write_stream_blob(&repo, short, 100).unwrap();
        assert_eq!(result, None);

        // A complete source still works.
        let full = std::io::Cursor::new(vec![9u8; 100]);
        let oid = GitSnapshotBackend::write_stream_blob(&repo, full, 100)
            .unwrap()
            .expect("complete stream");
        assert!(repo.has_object(oid));
    }

    #[test]
    fn test_unchanged_cache_is_not_rewritten() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let a = tmpdir.path().join("a.txt");
        fs::write(&a, "aaa").unwrap();
        backdate(&a);

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        track_stats(&backend, tmpdir.path());
        let cache_path = backend
            .snapshot_dir(tmpdir.path())
            .unwrap()
            .join(STAT_CACHE_FILE);
        let before = fs::metadata(&cache_path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        track_stats(&backend, tmpdir.path());
        let after = fs::metadata(&cache_path).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "cache file must not be rewritten when unchanged"
        );
    }

    #[test]
    fn test_concurrent_snapshots_leave_valid_cache_and_no_temp_files() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        for i in 0..20 {
            let f = tmpdir.path().join(format!("f{i}.txt"));
            fs::write(&f, format!("{i}")).unwrap();
            backdate(&f);
        }
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        track_stats(&backend, tmpdir.path());
        let git_dir = backend.snapshot_dir(tmpdir.path()).unwrap();
        // Force rewrites: invalidate the cache so every run saves it.
        let repo = repo_for(&backend, tmpdir.path());
        let cache = GitSnapshotBackend::load_stat_cache(&repo);
        assert_eq!(cache.entries.len(), 20);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let git_dir = git_dir.clone();
            handles.push(std::thread::spawn(move || {
                let repo = gix::open(&git_dir).unwrap();
                for _ in 0..25 {
                    GitSnapshotBackend::save_stat_cache(&repo, &StatCache::empty());
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let leftovers = fs::read_dir(&git_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "no temp files may be left behind");
        let repo = repo_for(&backend, tmpdir.path());
        assert_eq!(
            GitSnapshotBackend::load_stat_cache(&repo).version,
            STAT_CACHE_VERSION
        );
    }
}

//! Git-based snapshot backend using gitoxide (gix)
//!
//! This backend stores snapshots in a shadow git repository located at
//! `<user cache dir>/querymt/snapshots/<hash>/` (or below the directory given
//! to [`GitSnapshotBackend::with_snapshot_base`]) where `<hash>` is derived
//! from the worktree path. This keeps the user's project directory untouched.

mod capture;
mod confined;
mod gc;
mod restore;
#[cfg(test)]
mod tests;

use super::backend::{
    GcConfig, GcResult, SnapshotBackend, SnapshotError, SnapshotId, SnapshotResult, io_err,
};
use async_trait::async_trait;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// Tunable thresholds of the git snapshot backend.
///
/// Production code uses [`Limits::default`]; tests shrink them so the
/// threshold logic can be exercised with KiB-sized files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// Blobs of at least this many bytes are stored uncompressed and streamed.
    pub(crate) stream_bytes: u64,
    /// A file of at least this many bytes entering snapshots is reported.
    pub(crate) large_warn_bytes: u64,
    /// A snapshot store of at least this many bytes is reported.
    pub(crate) store_warn_bytes: u64,
    /// The store is re-measured after this many captured bytes.
    pub(crate) store_measure_every_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        const MIB: u64 = 1024 * 1024;
        Self {
            stream_bytes: 16 * MIB,
            large_warn_bytes: 64 * MIB,
            store_warn_bytes: 5 * 1024 * MIB,
            store_measure_every_bytes: 64 * MIB,
        }
    }
}

/// Cross-process lock over one snapshot repository.
///
/// `track`, `restore_paths` and `restore` hold it shared; the garbage-collection
/// sweep needs it exclusive, so it can never delete an object that an
/// in-flight snapshot wrote but has not committed yet. The lock is released
/// when the guard drops.
#[derive(Debug)]
pub(crate) struct SnapshotLock {
    file: fs::File,
}

impl SnapshotLock {
    const FILE_NAME: &'static str = "qmt-snapshot.lock";

    fn open(git_dir: &Path) -> SnapshotResult<fs::File> {
        let path = git_dir.join(Self::FILE_NAME);
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_err("open lock", &path))
    }

    /// Wait for and take a shared lock.
    pub(crate) fn shared(git_dir: &Path) -> SnapshotResult<Self> {
        let file = Self::open(git_dir)?;
        file.lock_shared()
            .map_err(io_err("lock", &git_dir.join(Self::FILE_NAME)))?;
        Ok(Self { file })
    }

    /// Take the exclusive lock, or `None` when someone holds it.
    pub(crate) fn try_exclusive(git_dir: &Path) -> SnapshotResult<Option<Self>> {
        let file = Self::open(git_dir)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => {
                Err(io_err("lock", &git_dir.join(Self::FILE_NAME))(e))
            }
        }
    }
}

impl Drop for SnapshotLock {
    fn drop(&mut self) {
        // Closing the file releases the lock anyway; unlocking explicitly just
        // makes that prompt. A failure here cannot be acted on.
        if let Err(e) = self.file.unlock() {
            tracing::debug!(error = %e, "failed to release snapshot lock");
        }
    }
}

/// Git-based snapshot backend using gitoxide (pure Rust, no git CLI dependency)
#[derive(Debug, Clone, Default)]
pub struct GitSnapshotBackend {
    /// Directory that holds the per-worktree snapshot repositories. `None`
    /// means `<user cache dir>/querymt/snapshots`.
    snapshot_base: Option<PathBuf>,
    limits: Limits,
}

impl GitSnapshotBackend {
    /// A backend that stores snapshots under the user cache directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store snapshots under `base` instead of the user cache directory.
    ///
    /// Each worktree still gets its own repository below `base`.
    #[must_use]
    pub fn with_snapshot_base(mut self, base: impl Into<PathBuf>) -> Self {
        self.snapshot_base = Some(base.into());
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Compute the snapshot repository directory for a given worktree
    fn snapshot_dir(&self, worktree: &Path) -> SnapshotResult<PathBuf> {
        let canonical = worktree
            .canonicalize()
            .unwrap_or_else(|_| worktree.to_path_buf());
        let hash = crate::hash::RapidHash::new(canonical.to_string_lossy().as_bytes());

        if let Some(base) = &self.snapshot_base {
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
            copied_files = tracing::field::Empty,
            newly_large_files = tracing::field::Empty,
            reused = tracing::field::Empty,
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
                    .record("copied_files", stats.copied_files)
                    .record("newly_large_files", stats.newly_large_files)
                    .record("reused", stats.reused)
                    .record("bytes_hashed", stats.bytes_hashed)
                    .record(
                        "elapsed_ms",
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    );
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
        let backend = self.clone();

        tokio::task::spawn_blocking(move || {
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
        let backend = self.clone();

        tokio::task::spawn_blocking(move || {
            let repo = backend.open_or_init(&worktree)?;
            let _lock = SnapshotLock::shared(repo.git_dir())?;
            backend
                .checkout_paths(&repo, &worktree, &snapshot, &paths)
                .map(drop)
        })
        .await
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn restore_batch(
        &self,
        worktree: &Path,
        steps: &[(SnapshotId, Vec<PathBuf>)],
    ) -> SnapshotResult<()> {
        let worktree = worktree.to_path_buf();
        let steps = steps.to_vec();
        let backend = self.clone();

        tokio::task::spawn_blocking(move || {
            let repo = backend.open_or_init(&worktree)?;
            // One shared lock for the whole batch: the sweep needs the
            // exclusive lock, so no snapshot can be pruned between the
            // resolution below and the last restore.
            let _lock = SnapshotLock::shared(repo.git_dir())?;
            // Resolve every snapshot before the first file changes.
            for (snapshot, _) in &steps {
                restore::commit_tree(&repo, snapshot)?;
            }
            for (snapshot, paths) in &steps {
                backend.checkout_paths(&repo, &worktree, snapshot, paths)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn uncaptured_paths(
        &self,
        worktree: &Path,
        snapshot: &SnapshotId,
        paths: &[PathBuf],
    ) -> SnapshotResult<Vec<PathBuf>> {
        let worktree = worktree.to_path_buf();
        let snapshot = snapshot.clone();
        let paths = paths.to_vec();
        let backend = self.clone();

        tokio::task::spawn_blocking(move || {
            let repo = backend.open_or_init(&worktree)?;
            let _lock = SnapshotLock::shared(repo.git_dir())?;
            let tree = restore::commit_tree(&repo, &snapshot)?;
            let mut uncaptured = Vec::new();
            for path in paths {
                confined::validate(&path)?;
                let in_snapshot = tree
                    .lookup_entry_by_path(&path)
                    .map_err(|e| {
                        SnapshotError::Repository(format!("Failed to look up tree entry: {e}"))
                    })?
                    .is_some();
                let on_disk = fs::symlink_metadata(worktree.join(&path))
                    .map(|m| m.is_file())
                    .unwrap_or(false);
                if on_disk && !in_snapshot {
                    uncaptured.push(path);
                }
            }
            Ok(uncaptured)
        })
        .await
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn restore(&self, worktree: &Path, snapshot: &SnapshotId) -> SnapshotResult<()> {
        let worktree = worktree.to_path_buf();
        let snapshot = snapshot.clone();
        let backend = self.clone();

        tokio::task::spawn_blocking(move || {
            let repo = backend.open_or_init(&worktree)?;
            let _lock = SnapshotLock::shared(repo.git_dir())?;
            backend.checkout_all(&repo, &worktree, &snapshot).map(drop)
        })
        .await
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn gc(&self, worktree: &Path, config: &GcConfig) -> SnapshotResult<GcResult> {
        let worktree = worktree.to_path_buf();
        let config = config.clone();
        let backend = self.clone();

        tokio::task::spawn_blocking(move || {
            let repo = backend.open_or_init(&worktree)?;
            gc::collect_garbage(&repo, &config, SystemTime::now())
        })
        .await
        .map_err(|_| SnapshotError::TaskPanicked)?
    }
}

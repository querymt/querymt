//! Storage selection, epoch, retention and the open/fallback policy (design D5).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ajika::{Database, DatabaseOptions, ErrorKind, Inputs, PersistenceErrorKind, Retention};
use querymt_agent_core::hash::RapidHash;

use super::queries::{CacheServices, WorkspaceRoot};

/// Version of the stored value encodings and query semantics.
///
/// Bump this whenever extractor output or the persisted mirrors change; a different value
/// makes every existing cache be discarded on open.
pub const CODE_CACHE_SCHEMA_VERSION: u32 = 1;

/// Hash of the extractor sources and the resolved parser dependency versions, computed by
/// `build.rs`. Part of the epoch, so a grammar or extractor change never serves old results
/// even when the package version stays the same (nightlies, local builds).
const EXTRACTOR_FINGERPRINT: &str = env!("QMT_CODE_INDEX_FINGERPRINT");

const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_MAX_ENTRIES: u64 = 200_000;
const DEFAULT_MAX_IDLE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Where a workspace's code cache is kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CodeIndexStorage {
    /// `<platform cache dir>/querymt/code-index/<hash of the root>`; memory when the
    /// platform has no cache directory.
    #[default]
    Default,
    /// `<base>/<hash of the root>`, used by tests.
    Directory(PathBuf),
    /// Keep results in memory only.
    Memory,
}

/// Cache location, size limits and schema version for one workspace index.
#[derive(Debug, Clone)]
pub struct CodeIndexCacheOptions {
    /// Where results are kept.
    pub storage: CodeIndexStorage,
    /// Maximum bytes of stored results.
    pub max_bytes: u64,
    /// Maximum number of stored results.
    pub max_entries: u64,
    /// Results unused for this long are evicted.
    pub max_idle: Duration,
    /// Cache schema version; a different value discards the cache on open.
    pub schema_version: u32,
}

impl Default for CodeIndexCacheOptions {
    fn default() -> Self {
        Self {
            storage: CodeIndexStorage::Default,
            max_bytes: DEFAULT_MAX_BYTES,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_idle: DEFAULT_MAX_IDLE,
            schema_version: CODE_CACHE_SCHEMA_VERSION,
        }
    }
}

impl CodeIndexCacheOptions {
    /// Options that keep everything in memory.
    #[must_use]
    pub fn memory() -> Self {
        Self {
            storage: CodeIndexStorage::Memory,
            ..Self::default()
        }
    }

    /// Options that store under `<base>/<hash of the root>`.
    #[must_use]
    pub fn in_directory(base: impl Into<PathBuf>) -> Self {
        Self {
            storage: CodeIndexStorage::Directory(base.into()),
            ..Self::default()
        }
    }

    /// The directory this configuration uses for `root`, or `None` when it keeps results in
    /// memory.
    #[must_use]
    pub fn cache_directory(&self, root: &Path) -> Option<PathBuf> {
        let base = match &self.storage {
            CodeIndexStorage::Default => dirs::cache_dir()?.join("querymt").join("code-index"),
            CodeIndexStorage::Directory(base) => base.clone(),
            CodeIndexStorage::Memory => return None,
        };
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let hash = RapidHash::new(canonical.to_string_lossy().as_bytes());
        Some(base.join(format!("{:016x}", hash.as_u64())))
    }

    /// The cache epoch: package version, schema version and the build fingerprint of the
    /// extractor sources and parser dependencies (see `build.rs`). Any difference discards
    /// the cache on open, so stored results always come from the extractors in this binary.
    fn epoch(&self) -> String {
        format!(
            "qmt-code-index/{}/{}/{}",
            env!("CARGO_PKG_VERSION"),
            self.schema_version,
            EXTRACTOR_FINGERPRINT
        )
    }

    fn database_options(&self) -> DatabaseOptions {
        let retention = Retention {
            max_bytes: self.max_bytes,
            max_entries: usize::try_from(self.max_entries).unwrap_or(usize::MAX),
            max_idle: self.max_idle,
        };
        let workers = std::thread::available_parallelism().map_or(1, usize::from);
        DatabaseOptions::new(retention)
            .parallelism(workers)
            .max_physical_bytes(self.max_bytes.saturating_mul(2))
    }
}

/// Where a running cache keeps its results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheStorageKind {
    /// Persisted in the cache directory.
    Disk,
    /// Held in memory for this process only.
    Memory,
}

fn inputs(root: &Path) -> Inputs {
    Inputs::with_state(WorkspaceRoot(root.to_path_buf()))
}

fn persistence_kind(error: &ajika::Error) -> Option<PersistenceErrorKind> {
    match error.kind() {
        ErrorKind::Persistence(persistence) => Some(persistence.kind()),
        _ => None,
    }
}

fn open_memory(root: &Path, options: &CodeIndexCacheOptions) -> Database<CacheServices> {
    Database::memory(
        inputs(root),
        CacheServices::default(),
        options.database_options(),
    )
}

/// Lock file Ajika 0.2.1 (`ajika_storage`) holds for as long as an owner has the directory
/// open. The sweep below uses it to detect directories that are in use; Ajika is pinned, so
/// this name is checked by `stale_directories_are_swept` on every upgrade.
const OWNER_LOCK: &str = "owner.lock";

/// Create the cache base directory readable only by the current user: stored results
/// include function bodies and configuration values.
fn create_private_base(base: &Path) -> std::io::Result<()> {
    if let Some(parent) = base.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        match fs::DirBuilder::new().mode(0o700).create(base) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                fs::set_permissions(base, fs::Permissions::from_mode(0o700))?;
            }
            Err(error) => return Err(error),
        }
    }
    #[cfg(not(unix))]
    fs::create_dir_all(base)?;
    Ok(())
}

/// Record that the cache in `directory` was just used, for the stale-directory sweep.
///
/// Ajika never writes its lock file, so its modification time is ours to keep.
pub(crate) fn touch(directory: &Path) {
    let result = fs::OpenOptions::new()
        .write(true)
        .open(directory.join(OWNER_LOCK))
        .and_then(|file| file.set_modified(SystemTime::now()));
    if let Err(error) = result {
        log::debug!("code cache: cannot record use of {directory:?}: {error}");
    }
}

/// When the cache in `directory` was last opened or closed.
fn last_used(directory: &Path) -> Option<SystemTime> {
    fs::metadata(directory.join(OWNER_LOCK))
        .or_else(|_| fs::metadata(directory))
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn is_cache_directory_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.len() == 16 && name.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Remove sibling caches under `base` that nobody holds and that were last used longer than
/// `max_idle` ago, such as caches of deleted worktrees. Retention inside a cache only runs
/// while its root is open, so without this sweep such directories would never be reclaimed.
///
/// Returns the number of removed directories.
pub(crate) fn sweep_stale(base: &Path, keep: &Path, max_idle: Duration) -> usize {
    let Ok(entries) = fs::read_dir(base) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || !is_cache_directory_name(&entry.file_name()) || !path.is_dir() {
            continue;
        }
        let idle = last_used(&path)
            .and_then(|used| now.duration_since(used).ok())
            .unwrap_or_default();
        if idle < max_idle {
            continue;
        }
        // Only remove a directory no owner (in any process) holds.
        let lock = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join(OWNER_LOCK))
        {
            Ok(lock) => match lock.try_lock() {
                Ok(()) => Some(lock),
                Err(_) => continue,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => continue,
        };
        // Unix can remove a directory while its lock is held, which closes the window in
        // which another owner could open it; Windows cannot delete an open file.
        #[cfg(windows)]
        drop(lock);
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                removed += 1;
                log::info!("code cache: removed {path:?}, unused for {idle:?}");
            }
            Err(error) => log::debug!("code cache: cannot remove stale {path:?}: {error}"),
        }
        #[cfg(not(windows))]
        drop(lock);
    }
    removed
}

/// Open the database for `root`, falling back to memory when the directory cannot be used.
///
/// - A held directory (`InUse`) falls back to memory directly.
/// - Contents Ajika reports as invalid are discarded and opened again once. (Ajika already
///   resets damaged contents itself; this covers what it cannot repair.)
/// - Any other error (I/O, permissions, a full disk) falls back to memory without deleting
///   anything, so a transient failure never wipes a valid cache.
///
/// Every fallback logs a warning. Returns the directory when the database is on disk.
pub(crate) fn open_database(
    root: &Path,
    options: &CodeIndexCacheOptions,
) -> (Database<CacheServices>, CacheStorageKind, Option<PathBuf>) {
    let Some(directory) = options.cache_directory(root) else {
        if options.storage != CodeIndexStorage::Memory {
            log::warn!("code cache: no platform cache directory, keeping the index in memory");
        }
        return (open_memory(root, options), CacheStorageKind::Memory, None);
    };
    let memory = |reason: String| {
        log::warn!("code cache: {reason}, using memory for {root:?}");
        (open_memory(root, options), CacheStorageKind::Memory, None)
    };

    if let Some(base) = directory.parent()
        && let Err(error) = create_private_base(base)
    {
        return memory(format!("cannot create {base:?} ({error})"));
    }

    let epoch = options.epoch();
    let try_open = || {
        Database::open(
            &directory,
            &epoch,
            inputs(root),
            CacheServices::default(),
            options.database_options(),
        )
    };

    let database = match try_open() {
        Ok(database) => database,
        Err(error) => match persistence_kind(&error) {
            Some(PersistenceErrorKind::InUse) => {
                return memory(format!("{directory:?} is held by another owner"));
            }
            Some(PersistenceErrorKind::InvalidCache) => {
                log::warn!(
                    "code cache: {directory:?} is invalid ({error}); discarding it and retrying once"
                );
                if let Err(error) = fs::remove_dir_all(&directory)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    return memory(format!("cannot remove {directory:?} ({error})"));
                }
                match try_open() {
                    Ok(database) => database,
                    Err(error) => return memory(format!("cannot reopen {directory:?} ({error})")),
                }
            }
            _ => return memory(format!("cannot open {directory:?} ({error})")),
        },
    };

    touch(&directory);
    if let Some(base) = directory.parent() {
        sweep_stale(base, &directory, options.max_idle);
    }
    (database, CacheStorageKind::Disk, Some(directory))
}

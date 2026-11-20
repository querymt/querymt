//! Storage selection, epoch, retention and the open/fallback policy (design D5).

use std::path::{Path, PathBuf};
use std::time::Duration;

use ajika::{Database, DatabaseOptions, ErrorKind, Inputs, PersistenceErrorKind, Retention};
use querymt_agent_core::hash::RapidHash;

use super::queries::{CacheServices, WorkspaceRoot};

/// Version of the stored value encodings and query semantics.
///
/// Bump this whenever extractor output or the persisted mirrors change; a different value
/// makes every existing cache be discarded on open.
pub const CODE_CACHE_SCHEMA_VERSION: u32 = 1;

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

    fn epoch(&self) -> String {
        format!(
            "qmt-code-index/{}/{}",
            env!("CARGO_PKG_VERSION"),
            self.schema_version
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

fn is_in_use(error: &ajika::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::Persistence(persistence) if persistence.kind() == PersistenceErrorKind::InUse
    )
}

fn open_memory(root: &Path, options: &CodeIndexCacheOptions) -> Database<CacheServices> {
    Database::memory(
        inputs(root),
        CacheServices::default(),
        options.database_options(),
    )
}

/// Open the database for `root`, falling back to memory when the directory cannot be used.
///
/// A held directory (`InUse`) falls back to memory directly. Any other open error discards
/// the directory and retries once, then falls back to memory. Every fallback logs a warning.
pub(crate) fn open_database(
    root: &Path,
    options: &CodeIndexCacheOptions,
) -> (Database<CacheServices>, CacheStorageKind) {
    let Some(directory) = options.cache_directory(root) else {
        if options.storage != CodeIndexStorage::Memory {
            log::warn!("code cache: no platform cache directory, keeping the index in memory");
        }
        return (open_memory(root, options), CacheStorageKind::Memory);
    };

    let epoch = options.epoch();
    let try_open = || -> ajika::Result<Database<CacheServices>> {
        if let Some(parent) = directory.parent() {
            std::fs::create_dir_all(parent).map_err(ajika::Error::application)?;
        }
        Database::open(
            &directory,
            &epoch,
            inputs(root),
            CacheServices::default(),
            options.database_options(),
        )
    };

    match try_open() {
        Ok(database) => return (database, CacheStorageKind::Disk),
        Err(error) if is_in_use(&error) => {
            log::warn!(
                "code cache: {directory:?} is held by another owner, using memory for {root:?}"
            );
            return (open_memory(root, options), CacheStorageKind::Memory);
        }
        Err(error) => {
            log::warn!(
                "code cache: cannot open {directory:?} ({error}); discarding it and retrying once"
            );
        }
    }

    if let Err(error) = std::fs::remove_dir_all(&directory)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("code cache: cannot remove {directory:?}: {error}");
    }
    match try_open() {
        Ok(database) => (database, CacheStorageKind::Disk),
        Err(error) => {
            log::warn!(
                "code cache: cannot reopen {directory:?} ({error}), using memory for {root:?}"
            );
            (open_memory(root, options), CacheStorageKind::Memory)
        }
    }
}

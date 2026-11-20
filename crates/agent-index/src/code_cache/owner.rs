//! The cache owner thread and its client (design D1, D6, D7).
//!
//! Ajika is synchronous and needs `&mut Database`, so one dedicated thread per workspace owns
//! the database and serves commands from a channel. Replies travel back over `tokio` oneshot
//! channels, so async callers never block a runtime worker.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use ajika::Database;
use rayon::prelude::*;
use tokio::sync::oneshot;

use super::queries::{CacheServices, Db, IndexSettings, SourceFile, functions_for, symbols_for};
use super::storage::{CacheStorageKind, CodeIndexCacheOptions, open_database};
use super::values::{FunctionsResult, SymbolsResult};
use crate::function_index::{FunctionIndexConfig, IndexedFunctionEntry, parse_pool};
use crate::symbol_index::{SymbolError, SymbolIndex};

/// Failure talking to the cache owner.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CodeCacheError {
    /// The owner thread has stopped (or never started) and cannot answer.
    #[error("code cache owner has stopped")]
    Stopped,
    /// The path is not inside the workspace root the cache belongs to.
    #[error("path {0:?} is outside the workspace root")]
    OutsideRoot(PathBuf),
    /// The cache operation failed.
    #[error("code cache operation failed: {0}")]
    Operation(String),
}

/// Cumulative counts since the cache started (design D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeCacheStats {
    /// Per-file extractions that ran.
    pub parsed_files: u64,
    /// Per-file results answered without running.
    pub reused_files: u64,
    /// Where results are kept.
    pub storage: CacheStorageKind,
}

/// A file the cache is asked about: its root-relative key and its path as the caller knows it.
struct FileRef {
    key: String,
    path: PathBuf,
}

/// A file whose text the host already has.
struct Change {
    file: FileRef,
    text: Arc<str>,
}

/// Function entries per file, in request order.
pub type FileEntries = Vec<(PathBuf, Vec<IndexedFunctionEntry>)>;

enum Command {
    IndexAll {
        files: Vec<FileRef>,
        reply: oneshot::Sender<Result<FileEntries, CodeCacheError>>,
    },
    Apply {
        changed: Vec<Change>,
        removed: Vec<FileRef>,
        reply: oneshot::Sender<Result<FileEntries, CodeCacheError>>,
    },
    Symbols {
        files: Vec<Change>,
        reply: oneshot::Sender<Result<Vec<Result<SymbolIndex, SymbolError>>, CodeCacheError>>,
    },
    Stats {
        reply: oneshot::Sender<CodeCacheStats>,
    },
    Collect {
        reply: oneshot::Sender<Result<usize, CodeCacheError>>,
    },
    Stop {
        reply: oneshot::Sender<()>,
    },
}

/// The workspace root as given, plus its canonical form when that differs.
struct Roots {
    root: PathBuf,
    canonical: Option<PathBuf>,
}

impl Roots {
    fn key(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.root).ok().or_else(|| {
            self.canonical
                .as_deref()
                .and_then(|canonical| path.strip_prefix(canonical).ok())
        })?;
        let mut key = String::new();
        for component in relative.components() {
            let std::path::Component::Normal(part) = component else {
                return None;
            };
            if !key.is_empty() {
                key.push('/');
            }
            key.push_str(part.to_str()?);
        }
        (!key.is_empty()).then_some(key)
    }

    fn file(&self, path: &Path) -> Result<FileRef, CodeCacheError> {
        let key = self
            .key(path)
            .ok_or_else(|| CodeCacheError::OutsideRoot(path.to_path_buf()))?;
        Ok(FileRef {
            key,
            path: path.to_path_buf(),
        })
    }
}

/// Clonable handle to a workspace's cache owner thread.
///
/// The thread exits, after a final retention pass, when the last clone is dropped or after
/// [`CodeCacheClient::shutdown`].
#[derive(Clone)]
pub struct CodeCacheClient {
    tx: mpsc::Sender<Command>,
    roots: Arc<Roots>,
    #[cfg(any(test, feature = "test-support"))]
    _keepalive: Option<Arc<std::sync::Mutex<mpsc::Receiver<Command>>>>,
}

impl std::fmt::Debug for CodeCacheClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeCacheClient")
            .field("root", &self.roots.root)
            .finish_non_exhaustive()
    }
}

impl CodeCacheClient {
    /// Start the owner thread for `root`. The database is opened on that thread, so this
    /// returns immediately; commands queue until it is ready.
    ///
    /// # Errors
    ///
    /// Returns the I/O error if the thread cannot be spawned.
    pub fn spawn(
        root: &Path,
        options: CodeIndexCacheOptions,
        function_config: FunctionIndexConfig,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let roots = Arc::new(Roots {
            root: root.to_path_buf(),
            canonical: root.canonicalize().ok().filter(|c| c != root),
        });
        let thread_root = root.to_path_buf();
        std::thread::Builder::new()
            .name("qmt-code-cache".to_owned())
            .spawn(move || owner_main(&thread_root, &options, &function_config, &rx))?;
        Ok(Self {
            tx,
            roots,
            #[cfg(any(test, feature = "test-support"))]
            _keepalive: None,
        })
    }

    /// Root-relative key of `path`, or `None` when it lies outside the workspace root.
    #[must_use]
    pub fn key_for(&self, path: &Path) -> Option<String> {
        self.roots.key(path)
    }

    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, CodeCacheError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(build(reply))
            .map_err(|_| CodeCacheError::Stopped)?;
        response.await.map_err(|_| CodeCacheError::Stopped)
    }

    /// Function entries for every file, computing only those whose text changed since their
    /// stored result. Used for the initial build and rebuilds.
    ///
    /// # Errors
    ///
    /// Fails if a path is outside the root, the owner stopped, or the operation failed.
    pub async fn index_all(&self, paths: &[PathBuf]) -> Result<FileEntries, CodeCacheError> {
        let files = paths
            .iter()
            .map(|path| self.roots.file(path))
            .collect::<Result<Vec<_>, _>>()?;
        self.request(|reply| Command::IndexAll { files, reply })
            .await?
    }

    /// Apply one watcher change set: install `changed` texts, forget `removed` files, and
    /// return the new function entries of every changed file.
    ///
    /// # Errors
    ///
    /// Fails if a path is outside the root, the owner stopped, or the operation failed.
    pub async fn apply(
        &self,
        changed: Vec<(PathBuf, Arc<str>)>,
        removed: &[PathBuf],
    ) -> Result<FileEntries, CodeCacheError> {
        let changed = changed
            .into_iter()
            .map(|(path, text)| {
                Ok(Change {
                    file: self.roots.file(&path)?,
                    text,
                })
            })
            .collect::<Result<Vec<_>, CodeCacheError>>()?;
        let removed = removed
            .iter()
            .map(|path| self.roots.file(path))
            .collect::<Result<Vec<_>, _>>()?;
        self.request(|reply| Command::Apply {
            changed,
            removed,
            reply,
        })
        .await?
    }

    /// Symbols of each file for the text the caller supplies, in order.
    ///
    /// Each entry equals `SymbolIndex::from_source_for_path` on the same text.
    ///
    /// # Errors
    ///
    /// Fails if a path is outside the root, the owner stopped, or the operation failed.
    pub async fn symbols(
        &self,
        files: Vec<(PathBuf, Arc<str>)>,
    ) -> Result<Vec<Result<SymbolIndex, SymbolError>>, CodeCacheError> {
        let files = files
            .into_iter()
            .map(|(path, text)| {
                Ok(Change {
                    file: self.roots.file(&path)?,
                    text,
                })
            })
            .collect::<Result<Vec<_>, CodeCacheError>>()?;
        self.request(|reply| Command::Symbols { files, reply })
            .await?
    }

    /// Cumulative parse/reuse counts.
    ///
    /// # Errors
    ///
    /// Fails if the owner stopped.
    pub async fn stats(&self) -> Result<CodeCacheStats, CodeCacheError> {
        self.request(|reply| Command::Stats { reply }).await
    }

    /// Apply retention limits now; returns the number of evicted records.
    ///
    /// # Errors
    ///
    /// Fails if the owner stopped or the collection failed.
    pub async fn collect(&self) -> Result<usize, CodeCacheError> {
        self.request(|reply| Command::Collect { reply }).await?
    }

    /// Stop the owner after a final retention pass and wait until it has released the cache
    /// directory. Later calls on any clone fail with [`CodeCacheError::Stopped`].
    pub async fn shutdown(&self) {
        // A stopped owner has nothing left to release.
        let _ = self.request(|reply| Command::Stop { reply }).await;
    }
}

#[cfg(any(test, feature = "test-support"))]
impl CodeCacheClient {
    fn detached(root: &Path) -> (Self, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel();
        let roots = Arc::new(Roots {
            root: root.to_path_buf(),
            canonical: root.canonicalize().ok().filter(|c| c != root),
        });
        (
            Self {
                tx,
                roots,
                _keepalive: None,
            },
            rx,
        )
    }

    /// A client that accepts commands but never replies (holds the receiver).
    #[must_use]
    pub fn unresponsive(root: &Path) -> Self {
        let (mut client, rx) = Self::detached(root);
        client._keepalive = Some(Arc::new(std::sync::Mutex::new(rx)));
        client
    }

    /// A client whose owner is gone: every call fails with [`CodeCacheError::Stopped`].
    #[must_use]
    pub fn disconnected(root: &Path) -> Self {
        let (client, rx) = Self::detached(root);
        drop(rx);
        client
    }
}

struct Owner {
    db: Database<CacheServices>,
    storage: CacheStorageKind,
    parsed: u64,
    reused: u64,
    executions_seen: u64,
}

fn operation_error(error: &ajika::Error) -> CodeCacheError {
    CodeCacheError::Operation(error.to_string())
}

fn owner_main(
    root: &Path,
    options: &CodeIndexCacheOptions,
    function_config: &FunctionIndexConfig,
    rx: &mpsc::Receiver<Command>,
) {
    let (db, storage) = open_database(root, options);
    let mut owner = Owner {
        db,
        storage,
        parsed: 0,
        reused: 0,
        executions_seen: 0,
    };
    let settings = IndexSettings::at(());
    let max_file_bytes = u64::try_from(function_config.max_file_bytes).unwrap_or(u64::MAX);
    if let Err(error) = owner.db.update_inputs(|update| {
        settings.set_min_function_lines(update, function_config.min_function_lines)?;
        settings.set_max_file_bytes(update, max_file_bytes)
    }) {
        log::error!("code cache: cannot install index settings for {root:?}: {error}");
        return;
    }

    while let Ok(command) = rx.recv() {
        match command {
            Command::IndexAll { files, reply } => {
                if !reply.is_closed() {
                    let _ = reply.send(owner.index_all(&files));
                }
            }
            Command::Apply {
                changed,
                removed,
                reply,
            } => {
                let _ = reply.send(owner.apply(&changed, &removed));
            }
            Command::Symbols { files, reply } => {
                // A lookup whose caller already timed out is not worth running.
                if !reply.is_closed() {
                    let _ = reply.send(owner.symbols(&files));
                }
            }
            Command::Stats { reply } => {
                let _ = reply.send(owner.stats());
            }
            Command::Collect { reply } => {
                let _ = reply.send(owner.db.collect().map_err(|e| operation_error(&e)));
            }
            Command::Stop { reply } => {
                owner.finish(root);
                drop(owner);
                let _ = reply.send(());
                return;
            }
        }
    }
    owner.finish(root);
}

impl Owner {
    fn finish(&mut self, root: &Path) {
        if let Err(error) = self.db.collect() {
            log::warn!("code cache: final collection for {root:?} failed: {error}");
        }
    }

    fn stats(&self) -> CodeCacheStats {
        CodeCacheStats {
            parsed_files: self.parsed,
            reused_files: self.reused,
            storage: self.storage,
        }
    }

    /// Install texts the host already has, unless the captured text is equal (design D6).
    fn install(&mut self, installed: &[(&str, &Arc<str>)]) -> ajika::Result<()> {
        let stale: Vec<_> = installed
            .iter()
            .filter(|(key, text)| {
                self.db
                    .inputs()
                    .peek(SourceFile::TEXT, &(*key).to_owned())
                    .is_none_or(|captured| captured.as_str() != &***text)
            })
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        self.db.update_inputs(|update| {
            for (key, text) in stale {
                let file = SourceFile::at((*key).to_owned());
                file.set_text(update, text.to_string())?;
                file.set_readable(update, true)?;
            }
            Ok(())
        })
    }

    fn forget(&mut self, keys: &[&str]) {
        let result = self.db.update_inputs(|update| {
            for key in keys {
                SourceFile::at((*key).to_owned()).forget_all(update);
            }
            Ok(())
        });
        if let Err(error) = result {
            log::warn!("code cache: cannot release file text: {error}");
        }
    }

    /// One Ajika operation: install, run, release the text of every file touched, and update
    /// the statistics from the real execution count.
    fn run_batch<R>(
        &mut self,
        installed: &[(&str, &Arc<str>)],
        touched: &[&str],
        requested: usize,
        work: impl FnOnce(&Db<'_>) -> ajika::Result<R>,
    ) -> Result<R, CodeCacheError> {
        let result = self.install(installed).and_then(|()| self.db.run(work));
        let executions = self.db.services().executions();
        let ran = executions.saturating_sub(self.executions_seen);
        self.executions_seen = executions;
        self.parsed += ran;
        self.reused += u64::try_from(requested)
            .unwrap_or(u64::MAX)
            .saturating_sub(ran);
        self.forget(touched);
        result.map_err(|error| operation_error(&error))
    }

    fn index_all(&mut self, files: &[FileRef]) -> Result<FileEntries, CodeCacheError> {
        let keys: Vec<String> = files.iter().map(|f| f.key.clone()).collect();
        let touched: Vec<&str> = keys.iter().map(String::as_str).collect();
        let results = self.run_batch(&[], &touched, keys.len(), |db| functions_for(db, &keys))?;
        function_entries(files, &results)
    }

    fn apply(
        &mut self,
        changed: &[Change],
        removed: &[FileRef],
    ) -> Result<FileEntries, CodeCacheError> {
        let removed_keys: Vec<&str> = removed.iter().map(|f| f.key.as_str()).collect();
        self.forget(&removed_keys);
        if changed.is_empty() {
            return Ok(Vec::new());
        }
        let installed: Vec<(&str, &Arc<str>)> = changed
            .iter()
            .map(|c| (c.file.key.as_str(), &c.text))
            .collect();
        let keys: Vec<String> = changed.iter().map(|c| c.file.key.clone()).collect();
        let touched: Vec<&str> = keys.iter().map(String::as_str).collect();
        let results = self.run_batch(&installed, &touched, keys.len(), |db| {
            functions_for(db, &keys)
        })?;
        let files: Vec<&FileRef> = changed.iter().map(|c| &c.file).collect();
        function_entries_ref(&files, &results)
    }

    fn symbols(
        &mut self,
        files: &[Change],
    ) -> Result<Vec<Result<SymbolIndex, SymbolError>>, CodeCacheError> {
        let installed: Vec<(&str, &Arc<str>)> = files
            .iter()
            .map(|c| (c.file.key.as_str(), &c.text))
            .collect();
        let keys: Vec<String> = files.iter().map(|c| c.file.key.clone()).collect();
        let touched: Vec<&str> = keys.iter().map(String::as_str).collect();
        let results = self.run_batch(&installed, &touched, keys.len(), |db| {
            symbols_for(db, &keys)
        })?;
        Ok(results
            .iter()
            .map(|result| SymbolsResult::to_index(result))
            .collect())
    }
}

fn function_entries(
    files: &[FileRef],
    results: &[Arc<FunctionsResult>],
) -> Result<FileEntries, CodeCacheError> {
    let refs: Vec<&FileRef> = files.iter().collect();
    function_entries_ref(&refs, results)
}

/// Rebuild public entries (recomputing TypeScript fingerprints) on the large-stack pool.
fn function_entries_ref(
    files: &[&FileRef],
    results: &[Arc<FunctionsResult>],
) -> Result<FileEntries, CodeCacheError> {
    let pool = parse_pool().map_err(|e| CodeCacheError::Operation(e.to_string()))?;
    Ok(pool.install(|| {
        files
            .par_iter()
            .zip(results.par_iter())
            .map(|(file, result)| (file.path.clone(), result.to_entries(&file.path)))
            .collect()
    }))
}

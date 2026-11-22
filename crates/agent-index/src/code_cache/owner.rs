//! The cache owner thread and its client (design D1, D6, D7).
//!
//! Ajika is synchronous and needs `&mut Database`, so one dedicated thread per cache directory
//! owns the database and serves commands from a channel. Replies travel back over `tokio`
//! oneshot channels, so async callers never block a runtime worker.
//!
//! Owners of on-disk caches are shared through a process-wide registry: a workspace index
//! that is recreated for a root (for example after the manager pruned an idle index while a
//! session still held the old handle) reuses the live owner instead of opening the directory
//! a second time, which would fail on the directory lock and fall back to memory.

use std::any::Any;
use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak, mpsc};
use std::time::Duration;

use ajika::Database;
use rayon::prelude::*;
use tokio::sync::oneshot;

use super::queries::{CacheServices, Db, IndexSettings, SourceFile, functions_for, symbols_for};
use super::storage::{
    CacheStorageKind, CodeIndexCacheOptions, CodeIndexStorage, open_database, touch,
};
use super::values::{FunctionsResult, SymbolsResult};
use crate::function_index::{FunctionIndexConfig, IndexedFunctionEntry, parse_pool};
use crate::symbol_index::{SymbolError, SymbolIndex};

/// How long a new owner waits for the previous owner of the same directory to release it.
const RELEASE_WAIT: Duration = Duration::from_secs(10);

/// Symbol lookups that may wait in the owner's queue at once. Each holds the texts of up to a
/// directory batch, so a longer queue would retain them while the owner is busy; further
/// lookups fail with [`CodeCacheError::Busy`] and the caller parses directly.
pub(super) const MAX_PENDING_SYMBOLS: usize = 4;

/// Files computed per Ajika operation during a full index. Source text is released after
/// each chunk, so a large workspace never holds every file's text at once.
pub(super) const INDEX_CHUNK_FILES: usize = 512;

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
    Operation(#[source] ajika::Error),
    /// The parse thread pool could not be created.
    #[error("failed to build the parse thread pool: {0}")]
    ParsePool(#[source] rayon::ThreadPoolBuildError),
    /// Too many symbol lookups are already waiting for the owner.
    #[error("code cache owner is busy")]
    Busy,
}

/// Cumulative counts since the cache owner started (design D7).
///
/// Every workspace index sharing one owner sees the same counts.
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
        /// Counts this command against [`MAX_PENDING_SYMBOLS`] until it is dropped.
        _permit: SymbolPermit,
        reply: oneshot::Sender<Result<Vec<Result<SymbolIndex, SymbolError>>, CodeCacheError>>,
    },
    /// Waits until the database is open, then replies.
    Ready {
        reply: oneshot::Sender<()>,
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
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            canonical: root.canonicalize().ok().filter(|c| c != root),
        }
    }

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

/// State shared between an owner thread and its clients.
#[derive(Default)]
struct OwnerState {
    parsed: AtomicU64,
    reused: AtomicU64,
    /// Set once the database is open.
    storage: OnceLock<CacheStorageKind>,
    /// Set once the owner was asked to stop or has stopped; such an owner is never shared.
    stopped: AtomicBool,
    /// Symbol lookups queued or running.
    pending_symbols: AtomicUsize,
}

/// Admission of one symbol lookup; releases its place when the command is dropped, whether
/// it was served, skipped or never delivered.
struct SymbolPermit(Arc<OwnerState>);

impl SymbolPermit {
    fn acquire(state: &Arc<OwnerState>) -> Result<Self, CodeCacheError> {
        let previous = state.pending_symbols.fetch_add(1, Ordering::AcqRel);
        if previous >= MAX_PENDING_SYMBOLS {
            state.pending_symbols.fetch_sub(1, Ordering::AcqRel);
            return Err(CodeCacheError::Busy);
        }
        Ok(Self(Arc::clone(state)))
    }
}

impl Drop for SymbolPermit {
    fn drop(&mut self) {
        self.0.pending_symbols.fetch_sub(1, Ordering::AcqRel);
    }
}

impl OwnerState {
    fn snapshot(&self) -> Option<CodeCacheStats> {
        Some(CodeCacheStats {
            parsed_files: self.parsed.load(Ordering::Acquire),
            reused_files: self.reused.load(Ordering::Acquire),
            storage: *self.storage.get()?,
        })
    }
}

/// Signalled when an owner thread has dropped its database (and released its directory).
#[derive(Default)]
struct Released {
    done: Mutex<bool>,
    signal: Condvar,
}

impl Released {
    fn notify(&self) {
        *self.done.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.signal.notify_all();
    }

    fn is_done(&self) -> bool {
        *self.done.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait(&self, timeout: Duration) -> bool {
        let done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        let (done, _) = self
            .signal
            .wait_timeout_while(done, timeout, |done| !*done)
            .unwrap_or_else(PoisonError::into_inner);
        *done
    }
}

/// One owner thread, held by every client of it. The thread exits when the last link drops.
struct OwnerLink {
    tx: mpsc::Sender<Command>,
    state: Arc<OwnerState>,
    /// Test clients keep their command receiver here so it lives as long as the client.
    _keepalive: Option<Box<dyn Any + Send + Sync>>,
}

/// Registry entry for an on-disk cache directory.
struct Slot {
    link: Weak<OwnerLink>,
    settings: SharedSettings,
    released: Arc<Released>,
}

/// Everything that must match for two clients to share one owner: the index settings and
/// the cache's epoch and retention, which the owner fixed when it opened the directory.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SharedSettings {
    min_function_lines: u32,
    max_file_bytes: usize,
    schema_version: u32,
    max_bytes: u64,
    max_entries: u64,
    max_idle: Duration,
}

impl SharedSettings {
    fn new(options: &CodeIndexCacheOptions, function_config: &FunctionIndexConfig) -> Self {
        Self {
            min_function_lines: function_config.min_function_lines,
            max_file_bytes: function_config.max_file_bytes,
            schema_version: options.schema_version,
            max_bytes: options.max_bytes,
            max_entries: options.max_entries,
            max_idle: options.max_idle,
        }
    }
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Slot>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Slot>>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// Clonable handle to a workspace's cache owner thread.
///
/// The thread exits, after a final retention pass, when the last client of it is dropped or
/// after [`CodeCacheClient::shutdown`].
#[derive(Clone)]
pub struct CodeCacheClient {
    link: Arc<OwnerLink>,
    roots: Arc<Roots>,
}

impl std::fmt::Debug for CodeCacheClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeCacheClient")
            .field("root", &self.roots.root)
            .finish_non_exhaustive()
    }
}

fn start_owner(
    root: &Path,
    options: CodeIndexCacheOptions,
    function_config: FunctionIndexConfig,
    previous: Option<Arc<Released>>,
    released: Arc<Released>,
) -> std::io::Result<Arc<OwnerLink>> {
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(OwnerState::default());
    let thread_state = Arc::clone(&state);
    let thread_root = root.to_path_buf();
    std::thread::Builder::new()
        .name("qmt-code-cache".to_owned())
        .spawn(move || {
            if let Some(previous) = previous
                && !previous.wait(RELEASE_WAIT)
            {
                log::debug!("code cache: previous owner of {thread_root:?} is still closing");
            }
            owner_main(&thread_root, &options, &function_config, &rx, &thread_state);
            thread_state.stopped.store(true, Ordering::Release);
            released.notify();
        })?;
    Ok(Arc::new(OwnerLink {
        tx,
        state,
        _keepalive: None,
    }))
}

impl CodeCacheClient {
    /// Get a client for `root`'s cache.
    ///
    /// When a live owner already serves the same cache directory with the same settings, the
    /// client shares it. Otherwise a new owner thread starts; the database is opened on that
    /// thread, so this returns immediately and commands queue until it is ready.
    ///
    /// # Errors
    ///
    /// Returns the I/O error if the thread cannot be spawned.
    pub fn spawn(
        root: &Path,
        options: CodeIndexCacheOptions,
        function_config: FunctionIndexConfig,
    ) -> std::io::Result<Self> {
        let roots = Arc::new(Roots::new(root));
        let Some(directory) = options.cache_directory(root) else {
            let link = start_owner(root, options, function_config, None, Arc::default())?;
            return Ok(Self { link, roots });
        };

        let settings = SharedSettings::new(&options, &function_config);
        let mut slots = registry().lock().unwrap_or_else(PoisonError::into_inner);
        // Drop entries whose owner is gone and has released its directory.
        slots.retain(|_, slot| slot.link.strong_count() > 0 || !slot.released.is_done());

        let previous = match slots.get(&directory) {
            Some(slot) => match slot.link.upgrade() {
                Some(link) if !link.state.stopped.load(Ordering::Acquire) => {
                    if slot.settings == settings {
                        return Ok(Self { link, roots });
                    }
                    // A live owner with other settings holds the directory: this owner stays
                    // in memory (it never opens the directory, so it cannot win it from the
                    // registered owner) and does not replace the registered one.
                    let options = CodeIndexCacheOptions {
                        storage: CodeIndexStorage::Memory,
                        ..options
                    };
                    let link = start_owner(root, options, function_config, None, Arc::default())?;
                    return Ok(Self { link, roots });
                }
                _ => Some(Arc::clone(&slot.released)),
            },
            None => None,
        };

        let released = Arc::new(Released::default());
        let link = start_owner(
            root,
            options,
            function_config,
            previous,
            Arc::clone(&released),
        )?;
        slots.insert(
            directory,
            Slot {
                link: Arc::downgrade(&link),
                settings,
                released,
            },
        );
        Ok(Self { link, roots })
    }

    /// Root-relative key of `path`, or `None` when it lies outside the workspace root or
    /// cannot be keyed (a non-UTF-8 path component).
    #[must_use]
    pub fn key_for(&self, path: &Path) -> Option<String> {
        self.roots.key(path)
    }

    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, CodeCacheError> {
        let (reply, response) = oneshot::channel();
        self.link
            .tx
            .send(build(reply))
            .map_err(|_| CodeCacheError::Stopped)?;
        response.await.map_err(|_| CodeCacheError::Stopped)
    }

    /// Function entries for every file, computing only those whose text changed since their
    /// stored result. Used for the initial build and rebuilds.
    ///
    /// # Errors
    ///
    /// Fails if a path cannot be keyed (see [`Self::key_for`]), the owner stopped, or the
    /// operation failed.
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
    /// Fails if a path cannot be keyed, the owner stopped, or the operation failed.
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
    /// Each entry equals `SymbolIndex::from_source_for_path` on that entry's own text, also
    /// when several entries name the same path with different texts.
    ///
    /// # Errors
    ///
    /// Fails if a path cannot be keyed, the owner stopped, the operation failed, or too many
    /// lookups are already waiting ([`CodeCacheError::Busy`]).
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
        let permit = SymbolPermit::acquire(&self.link.state)?;
        self.request(|reply| Command::Symbols {
            files,
            _permit: permit,
            reply,
        })
        .await?
    }

    /// Cumulative parse/reuse counts.
    ///
    /// Once the database is open this reads shared counters and never waits behind a running
    /// operation; before that, it waits until the owner has opened its database.
    ///
    /// # Errors
    ///
    /// Fails if the owner stopped before opening its database.
    pub async fn stats(&self) -> Result<CodeCacheStats, CodeCacheError> {
        if let Some(stats) = self.link.state.snapshot() {
            return Ok(stats);
        }
        self.request(|reply| Command::Ready { reply }).await?;
        self.link.state.snapshot().ok_or(CodeCacheError::Stopped)
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
    /// directory. Later calls on any client sharing this owner fail with
    /// [`CodeCacheError::Stopped`], so those clients fall back to direct parsing; a client
    /// spawned afterwards starts a new owner.
    pub async fn shutdown(&self) {
        self.link.state.stopped.store(true, Ordering::Release);
        // A stopped owner has nothing left to release.
        let _ = self.request(|reply| Command::Stop { reply }).await;
    }
}

#[cfg(any(test, feature = "test-support"))]
impl CodeCacheClient {
    fn detached(root: &Path, keep_receiver: bool) -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        let keepalive: Option<Box<dyn Any + Send + Sync>> =
            keep_receiver.then(|| Box::new(Mutex::new(rx)) as Box<dyn Any + Send + Sync>);
        Self {
            link: Arc::new(OwnerLink {
                tx,
                state: Arc::default(),
                _keepalive: keepalive,
            }),
            roots: Arc::new(Roots::new(root)),
        }
    }

    /// A client that accepts commands but never replies (holds the receiver).
    #[must_use]
    pub fn unresponsive(root: &Path) -> Self {
        Self::detached(root, true)
    }

    /// A client whose owner is gone: every call fails with [`CodeCacheError::Stopped`].
    #[must_use]
    pub fn disconnected(root: &Path) -> Self {
        Self::detached(root, false)
    }
}

struct Owner {
    db: Database<CacheServices>,
    directory: Option<PathBuf>,
    state: Arc<OwnerState>,
    executions_seen: u64,
}

fn owner_main(
    root: &Path,
    options: &CodeIndexCacheOptions,
    function_config: &FunctionIndexConfig,
    rx: &mpsc::Receiver<Command>,
    state: &Arc<OwnerState>,
) {
    let (db, storage, directory) = open_database(root, options);
    let mut owner = Owner {
        db,
        directory,
        state: Arc::clone(state),
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
    let _ = state.storage.set(storage);

    while let Ok(command) = rx.recv() {
        // A panic inside an operation (for example in a parser) only loses that command:
        // its reply is dropped, so the caller falls back to direct parsing. Ajika resets its
        // operation state after a panic, so the owner keeps serving later commands.
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| owner.handle(command)));
        match outcome {
            Ok(None) => {}
            Ok(Some(reply)) => {
                owner.finish(root);
                // Dropping the database releases the directory before the caller resumes.
                drop(owner);
                let _ = reply.send(());
                return;
            }
            Err(_) => log::warn!(
                "code cache: an operation for {root:?} panicked; the caller parses directly"
            ),
        }
    }
    owner.finish(root);
}

impl Owner {
    /// Serve one command. Returns the reply channel of `Stop`, which the caller answers once
    /// the database is released.
    fn handle(&mut self, command: Command) -> Option<oneshot::Sender<()>> {
        match command {
            Command::IndexAll { files, reply } => {
                if !reply.is_closed() {
                    let _ = reply.send(self.index_all(&files));
                }
            }
            Command::Apply {
                changed,
                removed,
                reply,
            } => {
                let _ = reply.send(self.apply(&changed, &removed));
            }
            Command::Symbols { files, reply, .. } => {
                // A lookup whose caller already timed out is not worth running.
                if !reply.is_closed() {
                    let _ = reply.send(self.symbols(&files));
                }
            }
            Command::Ready { reply } => {
                let _ = reply.send(());
            }
            Command::Collect { reply } => {
                let _ = reply.send(self.db.collect().map_err(CodeCacheError::Operation));
            }
            Command::Stop { reply } => return Some(reply),
        }
        None
    }

    fn finish(&mut self, root: &Path) {
        log::debug!("code cache: closing the cache of {root:?}");
        if let Err(error) = self.db.collect() {
            log::warn!("code cache: final collection failed: {error}");
        }
        if let Some(directory) = &self.directory {
            touch(directory);
        }
    }

    /// Install texts the host already has.
    ///
    /// Text is forgotten after every operation (design D6), so there is no captured value to
    /// compare with; Ajika keeps the stored result valid when the new text equals the text
    /// behind it, so an unchanged file is still answered without parsing.
    fn install(&mut self, installed: &[(&str, &Arc<str>)]) -> ajika::Result<()> {
        if installed.is_empty() {
            return Ok(());
        }
        self.db.update_inputs(|update| {
            for (key, text) in installed {
                SourceFile::at((*key).to_owned()).set_text(update, Some(text.to_string()))?;
            }
            Ok(())
        })
    }

    fn forget(&mut self, keys: &[&str]) {
        if keys.is_empty() {
            return;
        }
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
    /// the statistics from the real execution count. Text is released even if the operation
    /// panics.
    fn run_batch<R>(
        &mut self,
        installed: &[(&str, &Arc<str>)],
        touched: &[&str],
        requested: usize,
        work: impl FnOnce(&Db<'_>) -> ajika::Result<R>,
    ) -> Result<R, CodeCacheError> {
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            self.install(installed).and_then(|()| self.db.run(work))
        }));
        let executions = self.db.services().executions();
        let ran = executions.saturating_sub(self.executions_seen);
        self.executions_seen = executions;
        self.state.parsed.fetch_add(ran, Ordering::AcqRel);
        if matches!(outcome, Ok(Ok(_))) {
            let requested = u64::try_from(requested).unwrap_or(u64::MAX);
            self.state
                .reused
                .fetch_add(requested.saturating_sub(ran), Ordering::AcqRel);
        }
        self.forget(touched);
        match outcome {
            Ok(result) => result.map_err(CodeCacheError::Operation),
            Err(panic) => {
                // Entries of an abandoned operation must not leak into the next one.
                self.db.services().take_fresh_functions();
                panic::resume_unwind(panic)
            }
        }
    }

    /// Processes `files` in chunks of [`INDEX_CHUNK_FILES`]: each chunk is one Ajika
    /// operation whose source texts are released before the next chunk starts. The result
    /// keeps request order.
    fn index_all(&mut self, files: &[FileRef]) -> Result<FileEntries, CodeCacheError> {
        let mut entries = Vec::with_capacity(files.len());
        for chunk in files.chunks(INDEX_CHUNK_FILES) {
            let keys: Vec<String> = chunk.iter().map(|f| f.key.clone()).collect();
            let touched: Vec<&str> = keys.iter().map(String::as_str).collect();
            let results = self.run_batch(&[], &touched, keys.len(), |db| functions_for(db, &keys));
            let fresh = self.db.services().take_fresh_functions();
            let refs: Vec<&FileRef> = chunk.iter().collect();
            entries.extend(function_entries(&refs, &results?, fresh)?);
        }
        Ok(entries)
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
        });
        let fresh = self.db.services().take_fresh_functions();
        let files: Vec<&FileRef> = changed.iter().map(|c| &c.file).collect();
        function_entries(&files, &results?, fresh)
    }

    fn symbols(
        &mut self,
        files: &[Change],
    ) -> Result<Vec<Result<SymbolIndex, SymbolError>>, CodeCacheError> {
        // One stored result exists per key, so only the first text given for a key goes
        // through the cache. An entry repeating that text shares its result; an entry with a
        // different text for the same key is parsed directly, so no entry ever receives
        // symbols computed from another entry's text.
        let mut first: HashMap<&str, &Arc<str>> = HashMap::new();
        let mut cached: Vec<&Change> = Vec::new();
        let mut conflicting = vec![false; files.len()];
        for (position, change) in files.iter().enumerate() {
            match first.get(change.file.key.as_str()) {
                None => {
                    first.insert(change.file.key.as_str(), &change.text);
                    cached.push(change);
                }
                Some(text) if ***text == *change.text => {}
                Some(_) => conflicting[position] = true,
            }
        }

        let installed: Vec<(&str, &Arc<str>)> = cached
            .iter()
            .map(|c| (c.file.key.as_str(), &c.text))
            .collect();
        let keys: Vec<String> = cached.iter().map(|c| c.file.key.clone()).collect();
        let touched: Vec<&str> = keys.iter().map(String::as_str).collect();
        let results = self.run_batch(&installed, &touched, keys.len(), |db| {
            symbols_for(db, &keys)
        })?;
        let by_key: HashMap<&str, &Arc<SymbolsResult>> =
            keys.iter().map(String::as_str).zip(&results).collect();

        let pool = if conflicting.contains(&true) {
            Some(parse_pool().map_err(CodeCacheError::ParsePool)?)
        } else {
            None
        };
        Ok(files
            .iter()
            .zip(&conflicting)
            .map(|(change, &conflict)| match pool {
                Some(pool) if conflict => pool
                    .install(|| SymbolIndex::from_source_for_path(&change.file.path, &change.text)),
                _ => by_key[change.file.key.as_str()].to_index(),
            })
            .collect())
    }
}

/// Public entries for each file, in order.
///
/// Files computed in this operation use the entries their query just produced, which still
/// carry their TypeScript fingerprints. Reused files are rebuilt from the stored mirror,
/// recomputing those fingerprints, on the large-stack pool.
fn function_entries(
    files: &[&FileRef],
    results: &[Arc<FunctionsResult>],
    mut fresh: HashMap<String, Vec<IndexedFunctionEntry>>,
) -> Result<FileEntries, CodeCacheError> {
    let mut entries: Vec<Option<Vec<IndexedFunctionEntry>>> = files
        .iter()
        .map(|file| {
            fresh.remove(&file.key).map(|mut list| {
                for entry in &mut list {
                    entry.file_path.clone_from(&file.path);
                }
                list
            })
        })
        .collect();
    let pool = parse_pool().map_err(CodeCacheError::ParsePool)?;
    pool.install(|| {
        entries
            .par_iter_mut()
            .zip(files.par_iter().zip(results.par_iter()))
            .filter(|(entry, _)| entry.is_none())
            .for_each(|(entry, (file, result))| *entry = Some(result.to_entries(&file.path)));
    });
    Ok(files
        .iter()
        .zip(entries)
        .map(|(file, list)| (file.path.clone(), list.unwrap_or_default()))
        .collect())
}

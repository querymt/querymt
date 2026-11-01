//! The checkout-local repository service: serialized refresh and source
//! revalidation on top of [`reconcile`](super::reconcile).
//!
//! # Refresh
//!
//! [`RepositoryService::refresh`] reconciles the index with the files on disk.
//!
//! * Refreshes through one service are serialized by an in-process lock, so
//!   two sessions on the same checkout never reconcile concurrently.
//! * Other connections and processes are handled by SQLite itself: every
//!   commit is an immediate transaction guarded by the generation the
//!   reconciliation was based on. Another writer holding the lock
//!   ([`StoreError::Busy`], after the connection's busy timeout) or having
//!   committed in between ([`StoreError::Stale`]) is retried a bounded number
//!   of times with a short back-off. Retrying re-reads the state, so work based
//!   on an old generation is never applied.
//! * When the attempts are exhausted, or the index cannot be used at all
//!   (corruption, I/O), the result is [`ServiceError::Unavailable`]. The
//!   caller is told to fall back to normal code search; stale cached content is
//!   never served as if it were current.
//! * A failed refresh rolls back completely: readers keep seeing the previous
//!   complete generation. Readers never take the writer lock (WAL), so a long
//!   refresh does not block them and they cannot observe a half-applied one.
//!
//! # Pre-emission revalidation
//!
//! [`RepositoryService::fresh_document`] is what retrieval uses before it
//! emits a body or line coordinates. It applies the reconciliation rule to the
//! one source (metadata first, hash on any doubt) and, when the file moved
//! under the index, refreshes and tries again a bounded number of times before
//! reporting [`ServiceError::Updating`]. This is not a filesystem snapshot
//! against arbitrary external writers; it narrows the window and discloses
//! the remainder.
//!
//! The service is synchronous; async callers run it on a blocking thread.

use super::discovery::FileStamp;
use super::reconcile::{
    ReconcileError, ReconcileOptions, ReconcileReport, fast_path_valid, read_source,
    reconcile_with, system_now_ns,
};
use super::root::ResolvedRoot;
use super::store::{RepositoryStore, SourceTouch, StoreError, StoredDocument, StoredSource};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use thiserror::Error;

/// How long a connection waits for another writer before a refresh attempt
/// counts as busy.
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// Retry behavior for refresh and revalidation. All bounds are finite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshPolicy {
    /// Refresh attempts (and revalidation rounds) before giving up. At least 1.
    pub max_attempts: u32,
    /// Base delay between attempts; attempt `n` waits `n * retry_delay`.
    pub retry_delay: Duration,
    /// SQLite busy timeout for this service's connection.
    pub busy_timeout: Duration,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            retry_delay: Duration::from_millis(25),
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
        }
    }
}

/// Why the service could not answer. Every variant is a disclosed condition,
/// never silently outdated data.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// The index cannot be refreshed or read right now (busy past the retry
    /// bound, corrupt, I/O). Use normal code search.
    #[error("repository knowledge is unavailable: {reason}")]
    Unavailable { reason: String },
    /// The source kept changing while it was being revalidated. Retry shortly.
    #[error("repository source `{path}` is being updated; retry shortly")]
    Updating { path: String },
    /// The path is not a live indexed document (not selected, excluded,
    /// malformed, disabled, or deleted).
    #[error("`{path}` is not an indexed repository document")]
    NotIndexed { path: String },
}

impl ServiceError {
    /// Stable machine-readable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "index_unavailable",
            Self::Updating { .. } => "index_updating",
            Self::NotIndexed { .. } => "source_not_indexed",
        }
    }

    pub(super) fn unavailable(error: impl std::fmt::Display) -> Self {
        Self::Unavailable {
            reason: error.to_string(),
        }
    }
}

/// A document whose source was revalidated against the file on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrentDocument {
    /// Generation the document was read from.
    pub generation: u64,
    pub document: StoredDocument,
}

pub(super) enum SourceCheck {
    /// The stored record matches the file.
    Current,
    /// The file differs from the record, or can no longer be read as one.
    Changed,
    /// No record for this path.
    Absent,
}

/// A repository index bound to one checkout.
#[derive(Debug)]
pub struct RepositoryService {
    root: ResolvedRoot,
    store: RepositoryStore,
    options: ReconcileOptions,
    policy: RefreshPolicy,
    refresh_lock: Mutex<()>,
    /// A workspace change was reported since the last hinted refresh began.
    hinted: AtomicBool,
    /// A hinted refresh is scheduled or running; further hints only set
    /// `hinted`, so bursts of events coalesce into few refreshes.
    hint_active: AtomicBool,
}

fn retryable(error: &StoreError) -> bool {
    matches!(error, StoreError::Busy | StoreError::Stale { .. })
}

impl RepositoryService {
    /// Open (or create, or rebuild) the index at `index_path` for `root`.
    /// Reads no repository files: the first refresh does that.
    pub fn open(
        root: ResolvedRoot,
        index_path: &Path,
        options: ReconcileOptions,
        policy: RefreshPolicy,
    ) -> Result<Self, ServiceError> {
        // Creating the schema and switching to WAL can report `Busy` without
        // waiting when another process creates the same index at the same
        // moment, so opening gets the same bounded retry as refreshing.
        let attempts = policy.max_attempts.max(1);
        let mut attempt = 0;
        let store = loop {
            match RepositoryStore::open(index_path) {
                Ok(store) => break store,
                Err(StoreError::Busy) if attempt + 1 < attempts => {
                    attempt += 1;
                    std::thread::sleep(policy.retry_delay * attempt);
                }
                Err(error) => return Err(ServiceError::unavailable(error)),
            }
        };
        store
            .set_busy_timeout(policy.busy_timeout)
            .map_err(ServiceError::unavailable)?;
        Ok(Self {
            root,
            store,
            options,
            policy,
            refresh_lock: Mutex::new(()),
            hinted: AtomicBool::new(false),
            hint_active: AtomicBool::new(false),
        })
    }

    /// Record that the workspace reported a change. Returns `true` when the
    /// caller should schedule [`run_hinted_refresh`](Self::run_hinted_refresh)
    /// (no hinted refresh is already pending).
    ///
    /// A hint is only an acceleration: it reads nothing itself, and retrieval
    /// reconciles with the files regardless of whether any hint arrived, so a
    /// missed event, an external write or a branch switch is still observed.
    pub fn hint_changed(&self) -> bool {
        self.hinted.store(true, Ordering::SeqCst);
        !self.hint_active.swap(true, Ordering::SeqCst)
    }

    /// Refresh for every hint recorded so far, coalescing hints that arrive
    /// meanwhile. Call only after [`hint_changed`](Self::hint_changed) returned
    /// `true`. Returns the number of refreshes performed. A failed refresh is
    /// not retried here: the next retrieval reports it.
    pub fn run_hinted_refresh(&self) -> u32 {
        let mut refreshes = 0;
        loop {
            while self.hinted.swap(false, Ordering::SeqCst) {
                refreshes += 1;
                if self.refresh().is_err() {
                    break;
                }
            }
            self.hint_active.store(false, Ordering::SeqCst);
            // A hint that landed between the last check and releasing the
            // slot must not be lost; take the slot back if nobody else did.
            if !self.hinted.load(Ordering::SeqCst) || self.hint_active.swap(true, Ordering::SeqCst)
            {
                return refreshes;
            }
        }
    }

    /// Attempts bounding refresh, revalidation and retrieval retries.
    pub(super) fn attempts(&self) -> u32 {
        self.policy.max_attempts.max(1)
    }

    pub fn root(&self) -> &ResolvedRoot {
        &self.root
    }

    pub fn store(&self) -> &RepositoryStore {
        &self.store
    }

    /// Reconcile the index with the files on disk.
    pub fn refresh(&self) -> Result<ReconcileReport, ServiceError> {
        self.refresh_with(&mut || Ok(()))
    }

    /// As [`refresh`](Self::refresh), calling `before_commit` inside each
    /// attempt's write transaction (fault injection and concurrency tests). An
    /// error it returns is treated like a failing commit: retried when busy or
    /// stale, otherwise fatal.
    pub(crate) fn refresh_with(
        &self,
        before_commit: &mut dyn FnMut() -> Result<(), StoreError>,
    ) -> Result<ReconcileReport, ServiceError> {
        // The guard protects no data, so a poisoned lock is still a valid lock.
        let _serialized = self
            .refresh_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let attempts = self.policy.max_attempts.max(1);
        let mut last: Option<StoreError> = None;
        for attempt in 0..attempts {
            if attempt > 0 {
                std::thread::sleep(self.policy.retry_delay * attempt);
            }
            match reconcile_with(&self.root, &self.store, &self.options, before_commit) {
                Ok(report) => return Ok(report),
                Err(ReconcileError::Store(error)) if retryable(&error) => last = Some(error),
                Err(ReconcileError::Store(error)) => return Err(ServiceError::unavailable(error)),
            }
        }
        let reason = last.map_or_else(
            || "refresh did not complete".to_string(),
            |error| error.to_string(),
        );
        Err(ServiceError::Unavailable {
            reason: format!("{reason} (gave up after {attempts} attempts)"),
        })
    }

    /// The indexed document at `path`, with its source revalidated against the
    /// file on disk. Refreshes and retries (bounded) if the file changed; the
    /// returned body is never one known to be outdated.
    pub fn fresh_document(&self, path: &str) -> Result<CurrentDocument, ServiceError> {
        self.fresh_document_with(path, &mut |_| {})
    }

    /// As [`fresh_document`](Self::fresh_document), calling `on_round` with the
    /// number of changes seen so far at the start of every round (tests).
    pub(crate) fn fresh_document_with(
        &self,
        path: &str,
        on_round: &mut dyn FnMut(u32),
    ) -> Result<CurrentDocument, ServiceError> {
        let limit = self.policy.max_attempts.max(1);
        let mut changes = 0u32;
        let mut refreshed = false;
        loop {
            on_round(changes);
            match self.check_source(path)? {
                SourceCheck::Current => {
                    let (generation, document) = self
                        .store
                        .document(path)
                        .map_err(ServiceError::unavailable)?;
                    return match document {
                        Some(document) => Ok(CurrentDocument {
                            generation,
                            document,
                        }),
                        None => Err(ServiceError::NotIndexed {
                            path: path.to_string(),
                        }),
                    };
                }
                SourceCheck::Absent => {
                    if refreshed {
                        return Err(ServiceError::NotIndexed {
                            path: path.to_string(),
                        });
                    }
                }
                SourceCheck::Changed => {
                    changes += 1;
                    if changes >= limit {
                        return Err(ServiceError::Updating {
                            path: path.to_string(),
                        });
                    }
                }
            }
            self.refresh()?;
            refreshed = true;
        }
    }

    /// Apply the reconciliation freshness rule to one source.
    pub(super) fn check_source(&self, path: &str) -> Result<SourceCheck, ServiceError> {
        let (_, stored) = self.store.source(path).map_err(ServiceError::unavailable)?;
        // Only paths the index itself recorded are ever resolved against the
        // filesystem, so a caller cannot make this read an arbitrary file.
        let Some(stored) = stored else {
            return Ok(SourceCheck::Absent);
        };
        let root = self.root.root();
        let Ok(canonical) = root.join(path).canonicalize() else {
            return Ok(SourceCheck::Changed);
        };
        if !canonical.starts_with(root) {
            return Ok(SourceCheck::Changed);
        }
        let Ok(metadata) = std::fs::metadata(&canonical) else {
            return Ok(SourceCheck::Changed);
        };
        if !metadata.is_file() {
            return Ok(SourceCheck::Changed);
        }
        let stamp = FileStamp::from_metadata(&metadata);
        let window_ns = i64::try_from(self.options.racy_window.as_nanos()).unwrap_or(i64::MAX);
        if !self.options.force_hash && fast_path_valid(&stored, &stamp, window_ns) {
            return Ok(SourceCheck::Current);
        }
        let Ok(read) = read_source(path, &canonical, self.options.discovery.max_file_bytes) else {
            return Ok(SourceCheck::Changed);
        };
        if !same_content(&stored, &read.hash) {
            return Ok(SourceCheck::Changed);
        }
        // Content is unchanged; remember the new stamp. Bookkeeping only, so a
        // failure here must not fail the read.
        let _ = self.store.touch_sources(&[SourceTouch {
            path: path.to_string(),
            stamp: read.stamp,
            content_hash: Some(read.hash),
            indexed_at_ns: self.options.now_ns.unwrap_or_else(system_now_ns),
        }]);
        Ok(SourceCheck::Current)
    }
}

fn same_content(stored: &StoredSource, hash: &str) -> bool {
    stored.content_hash.as_deref() == Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepositoryKnowledgeSettings;
    use crate::knowledge::repository::reconcile::ReconcileStatus;
    use crate::knowledge::repository::root::{RootRequest, resolve_root};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::SystemTime;

    const MANIFEST: &str = "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n";
    const DOCS: usize = 20;

    fn quick() -> RefreshPolicy {
        RefreshPolicy {
            max_attempts: 60,
            retry_delay: Duration::from_millis(1),
            busy_timeout: Duration::from_secs(10),
        }
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        index: PathBuf,
        root: ResolvedRoot,
        service: RepositoryService,
    }

    impl Fx {
        fn new() -> Self {
            Self::with(quick())
        }

        fn with(policy: RefreshPolicy) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            fs::create_dir_all(repo.join(".git")).unwrap();
            let root = resolve_root(&RootRequest {
                cwd: Some(&repo),
                ..Default::default()
            })
            .unwrap();
            let index = tmp.path().join("cache/knowledge/k/index.sqlite");
            let service = RepositoryService::open(
                root.clone(),
                &index,
                ReconcileOptions::from(&RepositoryKnowledgeSettings::default()),
                policy,
            )
            .unwrap();
            let fx = Self {
                _tmp: tmp,
                repo,
                index,
                root,
                service,
            };
            fx.put(".qmt/knowledge.toml", MANIFEST);
            fx
        }

        /// Another connection to the same index, as a second process would have.
        fn peer(&self) -> RepositoryService {
            RepositoryService::open(
                self.root.clone(),
                &self.index,
                ReconcileOptions::from(&RepositoryKnowledgeSettings::default()),
                quick(),
            )
            .unwrap()
        }

        fn put(&self, rel: &str, content: &str) {
            let path = self.repo.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn put_old(&self, rel: &str, content: &str) {
            self.put(rel, content);
            let file = fs::OpenOptions::new()
                .write(true)
                .open(self.repo.join(rel))
                .unwrap();
            file.set_modified(SystemTime::now() - Duration::from_secs(3600))
                .unwrap();
        }

        fn write_all(&self, version: u32) {
            for n in 0..DOCS {
                self.put(
                    &format!("docs/d{n:02}.md"),
                    &format!("# Doc {n:02}\nversion {version:03}\n"),
                );
            }
        }
    }

    fn versions(docs: &[StoredDocument]) -> Vec<String> {
        let mut found: Vec<String> = docs
            .iter()
            .flat_map(|doc| doc.sections.iter())
            .filter_map(|section| section.body.lines().find(|l| l.starts_with("version ")))
            .map(str::to_string)
            .collect();
        found.sort();
        found.dedup();
        found
    }

    #[test]
    fn refresh_builds_and_a_second_refresh_is_a_noop() {
        let fx = Fx::new();
        fx.write_all(0);
        let first = fx.service.refresh().unwrap();
        assert_eq!(first.status, ReconcileStatus::Built);
        assert_eq!(fx.service.store().counts().unwrap().documents, DOCS);
        let again = fx.service.refresh().unwrap();
        assert_eq!(again.generation, first.generation);
    }

    #[test]
    fn change_hints_coalesce_and_prewarm_the_index() {
        let fx = Fx::new();
        fx.write_all(0);
        assert!(fx.service.hint_changed(), "first hint schedules a refresh");
        assert!(
            !fx.service.hint_changed(),
            "hints while one is pending coalesce into it"
        );
        assert_eq!(fx.service.run_hinted_refresh(), 1);
        assert_eq!(fx.service.store().counts().unwrap().documents, DOCS);
        // The slot is released, so later changes schedule again.
        assert!(fx.service.hint_changed());
        assert_eq!(fx.service.run_hinted_refresh(), 1);
    }

    #[test]
    fn writes_without_any_hint_are_still_reconciled_on_retrieval() {
        let fx = Fx::new();
        fx.write_all(0);
        fx.service.refresh().unwrap();
        // An external edit and an addition that no watcher reported.
        fx.put("docs/d00.md", "# Doc 00\nversion 999\n");
        fx.put("docs/new.md", "# New\nversion 999\n");
        let current = fx.service.fresh_document("docs/d00.md").unwrap();
        assert!(current.document.sections[0].body.contains("version 999"));
        fx.service.refresh().unwrap();
        assert_eq!(fx.service.store().counts().unwrap().documents, DOCS + 1);
    }

    #[test]
    fn concurrent_readers_only_ever_see_complete_generations() {
        let fx = Fx::new();
        let reader = fx.peer();
        let stop = AtomicBool::new(false);
        let observed = AtomicU32::new(0);

        std::thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let state = reader.store().state().unwrap();
                        assert!(
                            state.sources.is_empty() || state.sources.len() == DOCS,
                            "partial generation: {} sources",
                            state.sources.len()
                        );
                        let (_, docs) = reader.store().export_logical().unwrap();
                        assert!(docs.is_empty() || docs.len() == DOCS, "{} docs", docs.len());
                        assert!(
                            versions(&docs).len() <= 1,
                            "mixed generations: {:?}",
                            versions(&docs)
                        );
                        reader.store().check_fts_integrity().unwrap();
                        observed.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            for version in 0..6 {
                fx.write_all(version);
                fx.service.refresh().unwrap();
            }
            stop.store(true, Ordering::Relaxed);
        });

        assert!(observed.load(Ordering::Relaxed) > 0);
        let (_, docs) = fx.service.store().export_logical().unwrap();
        assert_eq!(docs.len(), DOCS);
        assert_eq!(versions(&docs), vec!["version 005"]);
    }

    #[test]
    fn refreshes_from_many_threads_and_connections_converge() {
        let fx = Fx::new();
        fx.write_all(7);
        let peer = fx.peer();
        let services = [&fx.service, &peer];

        std::thread::scope(|scope| {
            for service in services {
                for _ in 0..2 {
                    scope.spawn(move || {
                        for _ in 0..5 {
                            service
                                .refresh()
                                .expect("bounded retries absorb contention");
                        }
                    });
                }
            }
        });

        let (_, docs) = fx.service.store().export_logical().unwrap();
        assert_eq!(docs.len(), DOCS);
        assert_eq!(versions(&docs), vec!["version 007"]);
        fx.service.store().check_fts_integrity().unwrap();

        // The converged corpus equals a fresh build.
        let fresh = Fx::new();
        fresh.write_all(7);
        fresh.service.refresh().unwrap();
        assert_eq!(docs, fresh.service.store().export_logical().unwrap().1);
    }

    const CHILD_TEST: &str = "knowledge::repository::service::tests::process_child";

    /// Body of the child processes spawned by the test below. Does nothing
    /// unless launched with the environment it sets.
    #[test]
    fn process_child() {
        let (Ok(repo), Ok(index)) = (
            std::env::var("QMT_RK_CHILD_REPO"),
            std::env::var("QMT_RK_CHILD_INDEX"),
        ) else {
            return;
        };
        let root = resolve_root(&RootRequest {
            cwd: Some(Path::new(&repo)),
            ..Default::default()
        })
        .unwrap();
        let service = RepositoryService::open(
            root,
            Path::new(&index),
            ReconcileOptions::from(&RepositoryKnowledgeSettings::default()),
            quick(),
        )
        .unwrap();
        for _ in 0..8 {
            service.refresh().expect("child refresh");
        }
    }

    #[test]
    fn refreshing_processes_and_a_reading_process_never_see_partial_state() {
        let fx = Fx::new();
        fx.write_all(3);
        let exe = std::env::current_exe().unwrap();
        let mut children: Vec<_> = (0..3)
            .map(|_| {
                std::process::Command::new(&exe)
                    .args(["--exact", CHILD_TEST, "--test-threads=1"])
                    .env("QMT_RK_CHILD_REPO", &fx.repo)
                    .env("QMT_RK_CHILD_INDEX", &fx.index)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();

        // This process reads (and refreshes) while the children write.
        let mut running = true;
        while running {
            if let Ok(state) = fx.service.store().state() {
                assert!(
                    state.sources.is_empty() || state.sources.len() == DOCS,
                    "partial generation: {} sources",
                    state.sources.len()
                );
            }
            let (_, docs) = fx.service.store().export_logical().unwrap();
            assert!(docs.is_empty() || docs.len() == DOCS, "{} docs", docs.len());
            fx.service.refresh().expect("parent refresh");
            running = children
                .iter_mut()
                .any(|child| child.try_wait().unwrap().is_none());
        }
        for mut child in children {
            assert!(child.wait().unwrap().success(), "child refresh failed");
        }

        let (_, docs) = fx.service.store().export_logical().unwrap();
        assert_eq!(docs.len(), DOCS);
        assert_eq!(versions(&docs), vec!["version 003"]);
        fx.service.store().check_fts_integrity().unwrap();
    }

    #[test]
    fn an_injected_failure_never_exposes_a_partial_generation() {
        let fx = Fx::new();
        fx.write_all(0);
        let built = fx.service.refresh().unwrap();
        let before = fx.service.store().export_logical().unwrap().1;

        fx.write_all(1);
        let error = fx
            .service
            .refresh_with(&mut || Err(StoreError::Unreadable("injected".to_string())))
            .unwrap_err();
        assert_eq!(error.code(), "index_unavailable");
        assert!(error.to_string().contains("injected"));

        // Nothing leaked: same generation, same corpus, consistent search index.
        assert_eq!(fx.service.store().generation().unwrap(), built.generation);
        assert_eq!(fx.service.store().export_logical().unwrap().1, before);
        fx.service.store().check_fts_integrity().unwrap();

        let recovered = fx.service.refresh().unwrap();
        assert!(recovered.generation > built.generation);
        assert_eq!(
            versions(&fx.service.store().export_logical().unwrap().1),
            vec!["version 001"]
        );
    }

    #[test]
    fn transient_commit_conflicts_are_retried_then_succeed() {
        let fx = Fx::new();
        fx.write_all(0);
        let calls = AtomicU32::new(0);
        let report = fx
            .service
            .refresh_with(&mut || {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(StoreError::Busy)
                } else {
                    Ok(())
                }
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(report.status, ReconcileStatus::Built);
        assert_eq!(fx.service.store().counts().unwrap().documents, DOCS);
    }

    #[test]
    fn persistent_contention_is_bounded_and_reported_unavailable() {
        let fx = Fx::with(RefreshPolicy {
            max_attempts: 3,
            retry_delay: Duration::from_millis(1),
            busy_timeout: Duration::from_millis(20),
        });
        fx.write_all(0);
        // Another connection (process) holds the write lock.
        let other = rusqlite::Connection::open(&fx.index).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let error = fx.service.refresh().unwrap_err();
        assert_eq!(error.code(), "index_unavailable");
        assert!(error.to_string().contains("busy"), "{error}");
        assert!(error.to_string().contains("3 attempts"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "retries are bounded"
        );

        // Readers are not blocked by the writer, and nothing was committed.
        assert!(!fx.service.store().is_built().unwrap());

        other.execute_batch("ROLLBACK").unwrap();
        assert_eq!(fx.service.refresh().unwrap().status, ReconcileStatus::Built);
    }

    #[test]
    fn a_corrupted_cache_is_rebuilt_from_the_sources() {
        let fx = Fx::new();
        fx.write_all(0);
        fx.service.refresh().unwrap();
        let expected = fx.service.store().export_logical().unwrap().1;
        drop(fx.service);

        fs::write(&fx.index, b"this is not a database").unwrap();
        let service = RepositoryService::open(
            fx.root.clone(),
            &fx.index,
            ReconcileOptions::from(&RepositoryKnowledgeSettings::default()),
            quick(),
        )
        .unwrap();
        assert!(!service.store().is_built().unwrap());
        service.refresh().unwrap();
        assert_eq!(service.store().export_logical().unwrap().1, expected);
    }

    #[test]
    fn revalidation_returns_the_current_document_without_reading_unchanged_content() {
        let fx = Fx::new();
        fx.put_old("docs/a.md", "# A\nalpha\n");
        fx.service.refresh().unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = fx.repo.join("docs/a.md");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            let current = fx.service.fresh_document("docs/a.md");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            let current = current.expect("metadata alone proves the record current");
            assert!(current.document.sections[0].body.contains("alpha"));
            assert_eq!(current.generation, fx.service.store().generation().unwrap());
        }
    }

    #[test]
    fn an_edit_between_index_and_emission_is_never_served_stale() {
        let fx = Fx::new();
        fx.put("docs/a.md", "# A\nalpha\n");
        fx.service.refresh().unwrap();
        let indexed = fx.service.store().generation().unwrap();

        // Same size, same mtime: only the racy rule can notice.
        let path = fx.repo.join("docs/a.md");
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "# A\nbeta!\n").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let current = fx.service.fresh_document("docs/a.md").unwrap();
        assert!(current.document.sections[0].body.contains("beta!"));
        assert!(current.generation > indexed);
    }

    #[test]
    fn deleted_or_invalid_sources_are_not_emitted() {
        let fx = Fx::new();
        fx.put_old("docs/a.md", "# A\nalpha\n");
        fx.put_old("docs/b.md", "# B\nbravo\n");
        fx.service.refresh().unwrap();

        fs::remove_file(fx.repo.join("docs/a.md")).unwrap();
        let gone = fx.service.fresh_document("docs/a.md").unwrap_err();
        assert_eq!(gone.code(), "source_not_indexed");

        fx.put_old("docs/b.md", "---\ntitle: never closed\n");
        let invalid = fx.service.fresh_document("docs/b.md").unwrap_err();
        assert_eq!(invalid.code(), "source_not_indexed");

        // Never selected, and never resolved against the filesystem.
        for path in ["docs/unknown.md", "../outside.md", "/etc/passwd"] {
            let error = fx.service.fresh_document(path).unwrap_err();
            assert_eq!(error.code(), "source_not_indexed", "{path}");
        }
    }

    #[test]
    fn a_source_that_keeps_changing_is_disclosed_as_updating() {
        let fx = Fx::with(RefreshPolicy {
            max_attempts: 3,
            ..quick()
        });
        fx.put("docs/a.md", "# A\nvers 0\n");
        fx.service.refresh().unwrap();

        let rounds = AtomicU32::new(0);
        let error = fx
            .service
            .fresh_document_with("docs/a.md", &mut |_| {
                // An external writer lands a same-size edit before every check.
                let n = rounds.fetch_add(1, Ordering::SeqCst) + 1;
                fx.put("docs/a.md", &format!("# A\nvers {n}\n"));
            })
            .unwrap_err();
        assert_eq!(error.code(), "index_updating");
        assert_eq!(rounds.load(Ordering::SeqCst), 3, "bounded by max_attempts");

        // Once the writer stops, the next read converges.
        let current = fx.service.fresh_document("docs/a.md").unwrap();
        assert!(current.document.sections[0].body.contains("vers 3"));
    }

    #[test]
    fn revalidation_reports_unavailable_when_the_index_cannot_refresh() {
        let fx = Fx::with(RefreshPolicy {
            max_attempts: 2,
            retry_delay: Duration::from_millis(1),
            busy_timeout: Duration::from_millis(20),
        });
        fx.put_old("docs/a.md", "# A\nalpha\n");
        fx.service.refresh().unwrap();
        fx.put_old("docs/a.md", "# A\nalpha edited\n");

        let other = rusqlite::Connection::open(&fx.index).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let error = fx.service.fresh_document("docs/a.md").unwrap_err();
        assert_eq!(error.code(), "index_unavailable");
        other.execute_batch("ROLLBACK").unwrap();

        let current = fx.service.fresh_document("docs/a.md").unwrap();
        assert!(current.document.sections[0].body.contains("edited"));
    }
}

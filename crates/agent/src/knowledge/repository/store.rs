//! Rebuildable, checkout-local repository index.
//!
//! The store is a **disposable cache** in its own SQLite file (see
//! [`ResolvedRoot::index_path`](super::root::ResolvedRoot::index_path)). It is
//! never part of the session database: this module opens exactly the path it is
//! given, runs no session migrations, and shares no tables with the legacy
//! knowledge store. Deleting the file loses nothing that cannot be rebuilt from
//! the configured source files.
//!
//! # Generations
//!
//! Every change to the indexed corpus is applied in **one write transaction**
//! that also bumps a monotonically increasing generation number. Readers use a
//! read transaction (WAL snapshot), so they observe either the complete previous
//! generation or the complete next one — never a half-reconciled mix of
//! documents, sections and full-text rows. A failed or injected-failure update
//! rolls back entirely and leaves the generation unchanged.
//!
//! Generation `0` means *nothing has been built yet*. That is deliberately
//! distinct from generation `>= 1` with zero documents, which is a valid empty
//! corpus.
//!
//! # Recovery
//!
//! A missing, corrupt, or incompatible cache is replaced by an empty store
//! ([`OpenOutcome::Rebuilt`]); callers then reconcile it from the source files.
//! Only unambiguous corruption (`NOT A DATABASE`, `CORRUPT`, failed
//! `quick_check`) or a foreign/other-version schema triggers a rebuild. Transient
//! conditions such as a busy or read-only database are reported as errors and
//! never delete anything.
//!
//! # Portability
//!
//! Logical keys (document IDs, section keys, relative paths) are stored exactly
//! as the identity pass renders them and contain nothing machine-local.
//! Machine-local facts (file stamps, index time) live only in `sources` and are
//! excluded from [`RepositoryStore::export_logical`], which is what clone/rebuild
//! equivalence compares.

use super::discovery::FileStamp;
use super::document::{DiagnosticSeverity, DocumentDiagnostic, DocumentKind};
use super::identity::NormalizedDocument;
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use thiserror::Error;

/// Identifies a file as a repository knowledge index.
pub const STORE_FORMAT: &str = "qmt-repository-knowledge";

/// Layout version. Any other value found on disk is rebuilt, never migrated:
/// the file is a cache.
pub const SCHEMA_VERSION: u32 = 1;

/// How long a connection waits on another process's write lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA_SQL: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) WITHOUT ROWID;

-- Every discovered, selected file (documents and code). Machine-local change
-- detection data lives here and nowhere else.
CREATE TABLE sources (
    path          TEXT PRIMARY KEY NOT NULL,
    is_document   INTEGER NOT NULL,
    is_code       INTEGER NOT NULL,
    size          INTEGER NOT NULL,
    mtime_ns      INTEGER,
    file_dev      INTEGER,
    file_ino      INTEGER,
    content_hash  TEXT,
    indexed_at_ns INTEGER NOT NULL,
    -- 'code' | 'parsed' | 'excluded'. A parsed source may have no `documents`
    -- row when its explicit ID is claimed by another source.
    state         TEXT NOT NULL,
    explicit_id   TEXT,
    diagnostics_json TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE documents (
    path                  TEXT PRIMARY KEY NOT NULL
                          REFERENCES sources(path) ON DELETE CASCADE,
    doc_id                TEXT NOT NULL UNIQUE,
    kind                  TEXT NOT NULL,
    title                 TEXT NOT NULL,
    description           TEXT,
    status                TEXT,
    tags_json             TEXT NOT NULL,
    knowledge_sources_json TEXT NOT NULL,
    knowledge_paths_json  TEXT NOT NULL,
    extensions_json       TEXT NOT NULL,
    had_frontmatter       INTEGER NOT NULL,
    line_count            INTEGER NOT NULL
) WITHOUT ROWID;

-- Rows are only ever inserted or deleted (a changed document is replaced), so
-- the external-content index below needs no UPDATE trigger.
CREATE TABLE sections (
    id                 INTEGER PRIMARY KEY,
    doc_path           TEXT NOT NULL REFERENCES documents(path) ON DELETE CASCADE,
    ordinal            INTEGER NOT NULL,
    section_key        TEXT NOT NULL UNIQUE,
    level              INTEGER NOT NULL,
    title              TEXT,
    breadcrumb_json    TEXT NOT NULL,
    occurrence         INTEGER NOT NULL,
    start_line         INTEGER NOT NULL,
    end_line           INTEGER NOT NULL,
    body               TEXT NOT NULL,
    from_metadata      INTEGER NOT NULL,
    search_title       TEXT NOT NULL,
    search_breadcrumb  TEXT NOT NULL,
    search_tags        TEXT NOT NULL,
    search_description TEXT NOT NULL,
    UNIQUE (doc_path, ordinal)
);

CREATE VIRTUAL TABLE sections_fts USING fts5(
    search_title, search_breadcrumb, search_tags, search_description, body,
    content = 'sections',
    content_rowid = 'id',
    tokenize = 'porter unicode61'
);

CREATE TRIGGER sections_ai AFTER INSERT ON sections BEGIN
    INSERT INTO sections_fts (rowid, search_title, search_breadcrumb, search_tags, search_description, body)
    VALUES (new.id, new.search_title, new.search_breadcrumb, new.search_tags, new.search_description, new.body);
END;

CREATE TRIGGER sections_ad AFTER DELETE ON sections BEGIN
    INSERT INTO sections_fts (sections_fts, rowid, search_title, search_breadcrumb, search_tags, search_description, body)
    VALUES ('delete', old.id, old.search_title, old.search_breadcrumb, old.search_tags, old.search_description, old.body);
END;
"#;

/// Why the store could not be used.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("repository index is busy; retry shortly")]
    Busy,
    #[error("repository index conflict: {0}")]
    Conflict(String),
    #[error(
        "repository index changed during refresh (expected generation {expected}, found {actual})"
    )]
    Stale { expected: u64, actual: u64 },
    #[error("invalid repository index record: {0}")]
    InvalidRecord(String),
    #[error("repository index contains unreadable data: {0}")]
    Unreadable(String),
    #[error("repository index lock was poisoned")]
    Poisoned,
    #[error("repository index I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("repository index database error: {0}")]
    Sqlite(rusqlite::Error),
}

impl StoreError {
    /// Stable machine-readable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Busy => "index_busy",
            Self::Conflict(_) => "index_conflict",
            Self::Stale { .. } => "index_stale",
            Self::InvalidRecord(_) => "index_invalid_record",
            Self::Unreadable(_) => "index_unreadable",
            Self::Poisoned => "index_poisoned",
            Self::Io(_) => "index_io",
            Self::Sqlite(_) => "index_database",
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        match &error {
            rusqlite::Error::SqliteFailure(failure, _) => match failure.code {
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Self::Busy,
                ErrorCode::ConstraintViolation => Self::Conflict(error.to_string()),
                _ => Self::Sqlite(error),
            },
            _ => Self::Sqlite(error),
        }
    }
}

/// Why an existing cache was discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildReason {
    /// Not a database, or failed an integrity check.
    Corrupt,
    /// A database written by a different format or layout version.
    Incompatible,
}

/// What [`RepositoryStore::open`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    /// No usable file existed; an empty store was created.
    Created,
    /// An existing compatible store was opened as-is.
    Reopened,
    /// An unusable cache was removed and replaced with an empty store. The
    /// corpus must be rebuilt from the source files.
    Rebuilt(RebuildReason),
}

/// How a source was handled when it was last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    /// Path-level record; content is not parsed.
    Code,
    /// Parsed as a valid document, which is stored.
    Parsed,
    /// Unusable (malformed, disabled, or its explicit ID is claimed by another
    /// source); no document is stored. See the diagnostics. `explicit_id` is
    /// kept for duplicate-ID exclusions so the claim is not forgotten.
    Excluded,
}

impl SourceState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Parsed => "parsed",
            Self::Excluded => "excluded",
        }
    }

    fn parse(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "code" => Ok(Self::Code),
            "parsed" => Ok(Self::Parsed),
            "excluded" => Ok(Self::Excluded),
            other => Err(StoreError::Unreadable(format!(
                "unknown source state `{other}`"
            ))),
        }
    }
}

/// A persisted, source-specific diagnostic. Never contains absolute paths.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IndexDiagnostic {
    /// Repository-relative source path, when the diagnostic is about one file.
    pub path: Option<String>,
    pub severity: DiagnosticSeverity,
    /// Stable machine-readable code.
    pub code: String,
    pub message: String,
}

impl IndexDiagnostic {
    pub fn new(
        path: Option<&str>,
        severity: DiagnosticSeverity,
        code: &str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.map(str::to_string),
            severity,
            code: code.to_string(),
            message: message.into(),
        }
    }

    fn to_json(&self) -> Value {
        let severity = match self.severity {
            DiagnosticSeverity::Info => "info",
            DiagnosticSeverity::Warning => "warning",
            DiagnosticSeverity::Error => "error",
        };
        serde_json::json!({
            "path": self.path,
            "severity": severity,
            "code": self.code,
            "message": self.message,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let severity = match value.get("severity")?.as_str()? {
            "info" => DiagnosticSeverity::Info,
            "warning" => DiagnosticSeverity::Warning,
            "error" => DiagnosticSeverity::Error,
            _ => return None,
        };
        Some(Self {
            path: value.get("path")?.as_str().map(str::to_string),
            severity,
            code: value.get("code")?.as_str()?.to_string(),
            message: value.get("message")?.as_str()?.to_string(),
        })
    }
}

impl From<&DocumentDiagnostic> for IndexDiagnostic {
    fn from(diagnostic: &DocumentDiagnostic) -> Self {
        Self {
            path: Some(diagnostic.path.clone()),
            severity: diagnostic.severity,
            code: diagnostic.code.to_string(),
            message: diagnostic.message.clone(),
        }
    }
}

fn diagnostics_to_json(diagnostics: &[IndexDiagnostic]) -> String {
    Value::Array(diagnostics.iter().map(IndexDiagnostic::to_json).collect()).to_string()
}

fn diagnostics_from_json(raw: &str) -> Vec<IndexDiagnostic> {
    serde_json::from_str::<Vec<Value>>(raw)
        .unwrap_or_default()
        .iter()
        .filter_map(IndexDiagnostic::from_json)
        .collect()
}

/// A selected file to record. `document` is present for files parsed as
/// documents; code files are recorded as path-level records only.
#[derive(Debug, Clone)]
pub struct SourceRecord {
    /// Repository-relative, forward-slash path.
    pub path: String,
    pub is_document: bool,
    pub is_code: bool,
    pub stamp: FileStamp,
    /// Content hash recorded at index time (none until the file was hashed).
    pub content_hash: Option<String>,
    pub indexed_at_ns: i64,
    pub document: Option<NormalizedDocument>,
    pub state: SourceState,
    /// Normalized explicit document ID claimed by this source, if any.
    pub explicit_id: Option<String>,
    pub diagnostics: Vec<IndexDiagnostic>,
}

impl SourceRecord {
    fn validate(&self) -> Result<(), StoreError> {
        if self.path.is_empty()
            || self.path.starts_with('/')
            || self.path.split('/').any(|segment| segment == "..")
        {
            return Err(StoreError::InvalidRecord(format!(
                "source path `{}` is not a repository-relative path",
                self.path
            )));
        }
        if let Some(document) = &self.document {
            if !self.is_document {
                return Err(StoreError::InvalidRecord(format!(
                    "`{}` carries a document but is not marked as one",
                    self.path
                )));
            }
            if document.relative_path() != self.path {
                return Err(StoreError::InvalidRecord(format!(
                    "document path `{}` does not match source `{}`",
                    document.relative_path(),
                    self.path
                )));
            }
        }
        Ok(())
    }
}

/// An atomic change to the corpus.
#[derive(Debug, Clone, Default)]
pub struct GenerationUpdate {
    /// Sources to add or replace (a replaced document's sections and full-text
    /// rows are replaced with it).
    pub upserts: Vec<SourceRecord>,
    /// Source paths to delete along with everything derived from them.
    pub removals: Vec<String>,
    /// Opaque fingerprint of the manifest/limits/extractor configuration the
    /// corpus was built under. `None` leaves the stored value untouched.
    pub config_fingerprint: Option<String>,
    /// Coverage diagnostics (skipped, over-limit, or escaping sources) for this
    /// generation. `None` leaves the stored value untouched.
    pub coverage: Option<Vec<IndexDiagnostic>>,
    /// Apply only if the store is still at this generation. A concurrent writer
    /// that committed in between makes the update fail with
    /// [`StoreError::Stale`] instead of overwriting work based on old state.
    pub expected_generation: Option<u64>,
    /// Refreshed stamps for sources whose content did not change. Applied in
    /// the same transaction as the rest, but never advances the generation by
    /// itself: it is cache bookkeeping, not a corpus change.
    pub touches: Vec<SourceTouch>,
}

/// Machine-local change-detection data for one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSource {
    pub path: String,
    pub is_document: bool,
    pub is_code: bool,
    pub stamp: FileStamp,
    pub content_hash: Option<String>,
    pub indexed_at_ns: i64,
    pub state: SourceState,
    pub explicit_id: Option<String>,
    pub diagnostics: Vec<IndexDiagnostic>,
    /// A stored document exists for this source.
    pub has_document: bool,
}

/// Refreshed change-detection data for a source whose content did not change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTouch {
    pub path: String,
    pub stamp: FileStamp,
    pub content_hash: Option<String>,
    pub indexed_at_ns: i64,
}

/// A stored section, as rendered by the identity pass.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSection {
    pub ordinal: usize,
    /// Portable section key (`<document-id>#<breadcrumb>[~n]`).
    pub key: String,
    pub level: u8,
    pub title: Option<String>,
    pub breadcrumb: Vec<String>,
    pub occurrence: u32,
    pub start_line: usize,
    pub end_line: usize,
    pub body: String,
    pub from_metadata: bool,
}

/// A stored document. Holds no stamps, hashes or times, so two stores built
/// from the same content compare equal wherever they live.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredDocument {
    pub path: String,
    /// Portable document ID (`id:<id>` or `path:<relative-path>`).
    pub id: String,
    pub kind: DocumentKind,
    pub title: String,
    pub description: Option<String>,
    pub status: Option<String>,
    pub tags: Vec<String>,
    pub knowledge_sources: Vec<String>,
    pub knowledge_paths: Vec<String>,
    pub extensions: BTreeMap<String, Value>,
    pub had_frontmatter: bool,
    pub line_count: usize,
    pub sections: Vec<StoredSection>,
}

/// Row counts for one generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreCounts {
    pub generation: u64,
    pub sources: usize,
    pub documents: usize,
    pub sections: usize,
}

/// Everything reconciliation needs, read from one consistent snapshot.
#[derive(Debug, Clone)]
pub struct StoreState {
    pub generation: u64,
    pub fingerprint: Option<String>,
    /// Coverage diagnostics recorded for this generation, sorted.
    pub coverage: Vec<IndexDiagnostic>,
    pub sources: Vec<StoredSource>,
}

/// A checkout-local SQLite repository index.
pub struct RepositoryStore {
    conn: Mutex<Connection>,
    path: PathBuf,
    outcome: OpenOutcome,
}

impl std::fmt::Debug for RepositoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepositoryStore")
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

enum OpenFailure {
    Rebuild(RebuildReason),
    Fatal(StoreError),
}

/// Corruption is rebuilt; every other error (busy, read-only, I/O) is fatal so
/// a transient condition can never delete a healthy cache.
fn classify(error: rusqlite::Error) -> OpenFailure {
    match &error {
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt
            ) =>
        {
            OpenFailure::Rebuild(RebuildReason::Corrupt)
        }
        _ => OpenFailure::Fatal(error.into()),
    }
}

fn classify_all<T>(result: rusqlite::Result<T>) -> Result<T, OpenFailure> {
    result.map_err(classify)
}

impl RepositoryStore {
    /// Open or create the index at `path`, creating missing parent directories.
    /// Recovers from a corrupt or incompatible cache by replacing it.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match Self::try_open(path) {
            Ok((conn, outcome)) => Ok(Self::new(conn, path, outcome)),
            Err(OpenFailure::Fatal(error)) => Err(error),
            Err(OpenFailure::Rebuild(reason)) => {
                remove_database_files(path)?;
                match Self::try_open(path) {
                    Ok((conn, _)) => Ok(Self::new(conn, path, OpenOutcome::Rebuilt(reason))),
                    Err(OpenFailure::Fatal(error)) => Err(error),
                    Err(OpenFailure::Rebuild(_)) => Err(StoreError::Unreadable(
                        "a freshly created index was rejected".to_string(),
                    )),
                }
            }
        }
    }

    fn new(conn: Connection, path: &Path, outcome: OpenOutcome) -> Self {
        Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
            outcome,
        }
    }

    fn try_open(path: &Path) -> Result<(Connection, OpenOutcome), OpenFailure> {
        let mut conn = classify_all(Connection::open(path))?;
        classify_all(conn.busy_timeout(BUSY_TIMEOUT))?;
        classify_all(conn.pragma_update(None, "foreign_keys", true))?;
        classify_all(conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0)))?;
        classify_all(conn.pragma_update(None, "synchronous", "NORMAL"))?;

        let objects: i64 =
            classify_all(
                conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0)),
            )?;
        if objects == 0 {
            // Two processes can both see an empty file; the loser re-checks
            // under the write lock and just reopens what the winner made.
            let created = Self::create_schema(&mut conn)?;
            return Ok((
                conn,
                if created {
                    OpenOutcome::Created
                } else {
                    OpenOutcome::Reopened
                },
            ));
        }

        let format = classify_all(Self::meta_value(&conn, "format")).unwrap_or(None);
        let version = classify_all(Self::meta_value(&conn, "schema_version")).unwrap_or(None);
        if format.as_deref() != Some(STORE_FORMAT)
            || version.as_deref() != Some(&SCHEMA_VERSION.to_string())
        {
            return Err(OpenFailure::Rebuild(RebuildReason::Incompatible));
        }

        let check: String =
            classify_all(conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0)))?;
        if check != "ok" {
            return Err(OpenFailure::Rebuild(RebuildReason::Corrupt));
        }
        Ok((conn, OpenOutcome::Reopened))
    }

    /// Returns whether this call created the schema.
    fn create_schema(conn: &mut Connection) -> Result<bool, OpenFailure> {
        let tx = classify_all(conn.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let objects: i64 =
            classify_all(tx.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0)))?;
        if objects != 0 {
            return Ok(false);
        }
        classify_all(tx.execute_batch(SCHEMA_SQL))?;
        for (key, value) in [
            ("format", STORE_FORMAT.to_string()),
            ("schema_version", SCHEMA_VERSION.to_string()),
            ("generation", "0".to_string()),
        ] {
            classify_all(tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            ))?;
        }
        classify_all(tx.commit())?;
        Ok(true)
    }

    fn meta_value(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
    }

    fn meta_set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn generation_of(conn: &Connection) -> Result<u64, StoreError> {
        let raw = Self::meta_value(conn, "generation")?;
        raw.as_deref()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| StoreError::Unreadable("missing or invalid generation".to_string()))
    }

    /// Location of the database file.
    /// How long this connection waits on another connection's write lock
    /// before reporting [`StoreError::Busy`].
    pub fn set_busy_timeout(&self, timeout: Duration) -> Result<(), StoreError> {
        self.lock()?.busy_timeout(timeout)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What opening this store did.
    pub fn open_outcome(&self) -> OpenOutcome {
        self.outcome
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn.lock().map_err(|_| StoreError::Poisoned)
    }

    /// Run `f` against one consistent snapshot and return it with that
    /// snapshot's generation.
    pub(super) fn read<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<(u64, T), StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let generation = Self::generation_of(&tx)?;
        let value = f(&tx)?;
        Ok((generation, value))
    }

    // ── generation state ─────────────────────────────────────────────────

    /// Current generation. `0` means nothing has been built yet.
    pub fn generation(&self) -> Result<u64, StoreError> {
        self.read(|_| Ok(())).map(|(generation, ())| generation)
    }

    /// Whether any generation has been committed.
    pub fn is_built(&self) -> Result<bool, StoreError> {
        Ok(self.generation()? > 0)
    }

    /// The configuration fingerprint the current corpus was built under.
    pub fn config_fingerprint(&self) -> Result<Option<String>, StoreError> {
        self.read(|conn| Ok(Self::meta_value(conn, "config_fingerprint")?))
            .map(|(_, value)| value)
    }

    pub fn counts(&self) -> Result<StoreCounts, StoreError> {
        let (generation, (sources, documents, sections)) = self.read(|conn| {
            let count = |sql: &str| -> Result<usize, StoreError> {
                let n: i64 = conn.query_row(sql, [], |row| row.get(0))?;
                Ok(usize::try_from(n).unwrap_or_default())
            };
            Ok((
                count("SELECT count(*) FROM sources")?,
                count("SELECT count(*) FROM documents")?,
                count("SELECT count(*) FROM sections")?,
            ))
        })?;
        Ok(StoreCounts {
            generation,
            sources,
            documents,
            sections,
        })
    }

    // ── writes ───────────────────────────────────────────────────────────

    /// Apply `update` atomically and return the resulting generation.
    ///
    /// The generation advances only when something actually changed: removing a
    /// path that is not indexed, or restating the stored fingerprint, is a
    /// no-op. Any failure rolls the whole update back.
    pub fn apply_update(&self, update: &GenerationUpdate) -> Result<u64, StoreError> {
        self.apply_update_with(update, &mut || Ok(()))
    }

    /// As [`apply_update`](Self::apply_update), calling `before_commit` after
    /// all writes and before the commit (fault injection and concurrency tests).
    pub(crate) fn apply_update_with(
        &self,
        update: &GenerationUpdate,
        before_commit: &mut dyn FnMut() -> Result<(), StoreError>,
    ) -> Result<u64, StoreError> {
        validate_update(update)?;
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let generation = Self::generation_of(&tx)?;
        if let Some(expected) = update.expected_generation
            && expected != generation
        {
            return Err(StoreError::Stale {
                expected,
                actual: generation,
            });
        }
        let mut changed = false;
        let mut touched = false;

        // Delete everything being removed or replaced before inserting anything,
        // so an ID or key released by one source can be taken by another in the
        // same generation regardless of path order.
        for path in &update.removals {
            changed |= tx.execute("DELETE FROM sources WHERE path = ?1", [path])? > 0;
        }
        for record in &update.upserts {
            tx.execute("DELETE FROM sources WHERE path = ?1", [&record.path])?;
        }
        for record in &update.upserts {
            insert_source(&tx, record)?;
            changed = true;
        }
        for touch in &update.touches {
            touched |= apply_touch(&tx, touch)? > 0;
        }
        if let Some(fingerprint) = &update.config_fingerprint
            && Self::meta_value(&tx, "config_fingerprint")?.as_deref() != Some(fingerprint)
        {
            Self::meta_set(&tx, "config_fingerprint", fingerprint)?;
            changed = true;
        }
        if let Some(coverage) = &update.coverage {
            let encoded = diagnostics_to_json(coverage);
            if Self::meta_value(&tx, "coverage_json")?.as_deref() != Some(encoded.as_str()) {
                Self::meta_set(&tx, "coverage_json", &encoded)?;
                changed = true;
            }
        }

        before_commit()?;
        if !changed {
            if touched {
                tx.commit()?;
            }
            return Ok(generation);
        }
        let next = generation + 1;
        Self::meta_set(&tx, "generation", &next.to_string())?;
        tx.commit()?;
        Ok(next)
    }

    /// Replace the entire corpus in one generation. Always advances the
    /// generation, so a rebuild that finds nothing yields a built, empty corpus.
    pub fn replace_all(
        &self,
        records: &[SourceRecord],
        config_fingerprint: Option<&str>,
    ) -> Result<u64, StoreError> {
        validate_unique(records.iter().map(|record| record.path.as_str()), "upserts")?;
        for record in records {
            record.validate()?;
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM sources", [])?;
        for record in records {
            insert_source(&tx, record)?;
        }
        match config_fingerprint {
            Some(fingerprint) => Self::meta_set(&tx, "config_fingerprint", fingerprint)?,
            None => {
                tx.execute("DELETE FROM meta WHERE key = 'config_fingerprint'", [])?;
            }
        }
        tx.execute("DELETE FROM meta WHERE key = 'coverage_json'", [])?;
        let next = Self::generation_of(&tx)? + 1;
        Self::meta_set(&tx, "generation", &next.to_string())?;
        tx.commit()?;
        Ok(next)
    }

    // ── reads ────────────────────────────────────────────────────────────

    /// Generation, configuration fingerprint and all sources from a single
    /// snapshot, so a concurrent commit cannot split them.
    pub fn state(&self) -> Result<StoreState, StoreError> {
        let (generation, (fingerprint, coverage, sources)) = self.read(|conn| {
            let fingerprint = Self::meta_value(conn, "config_fingerprint")?;
            let coverage = Self::meta_value(conn, "coverage_json")?
                .map(|raw| diagnostics_from_json(&raw))
                .unwrap_or_default();
            let mut statement = conn.prepare(&format!("{SOURCE_SELECT} ORDER BY s.path"))?;
            let rows = statement
                .query_map([], source_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            let sources = rows.into_iter().collect::<Result<Vec<_>, _>>()?;
            Ok((fingerprint, coverage, sources))
        })?;
        Ok(StoreState {
            generation,
            fingerprint,
            coverage,
            sources,
        })
    }

    /// Change-detection data for every source, sorted by path.
    pub fn sources(&self) -> Result<(u64, Vec<StoredSource>), StoreError> {
        self.read(|conn| {
            let mut statement = conn.prepare(&format!("{SOURCE_SELECT} ORDER BY s.path"))?;
            let rows = statement.query_map([], source_from_row)?;
            let rows = rows.collect::<Result<Vec<_>, _>>()?;
            rows.into_iter().collect::<Result<Vec<_>, _>>()
        })
    }

    pub fn source(&self, path: &str) -> Result<(u64, Option<StoredSource>), StoreError> {
        self.read(|conn| {
            conn.query_row(
                &format!("{SOURCE_SELECT} WHERE s.path = ?1"),
                [path],
                source_from_row,
            )
            .optional()?
            .transpose()
        })
    }

    /// Refresh stamps, hashes and index times of sources whose content did not
    /// change. This is cache bookkeeping, not a corpus change: the generation
    /// does not advance, so readers and delivered citations are unaffected.
    pub fn touch_sources(&self, touches: &[SourceTouch]) -> Result<usize, StoreError> {
        if touches.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut updated = 0;
        for touch in touches {
            updated += apply_touch(&tx, touch)?;
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Coverage diagnostics recorded for the current generation.
    pub fn coverage(&self) -> Result<(u64, Vec<IndexDiagnostic>), StoreError> {
        self.read(|conn| {
            Ok(Self::meta_value(conn, "coverage_json")?
                .map(|raw| diagnostics_from_json(&raw))
                .unwrap_or_default())
        })
    }

    /// Every persisted diagnostic (coverage plus per-source), sorted. A
    /// non-empty list means the corpus is incomplete or partly excluded.
    pub fn index_diagnostics(&self) -> Result<(u64, Vec<IndexDiagnostic>), StoreError> {
        let (generation, sources) = self.sources()?;
        let (_, coverage) = self.coverage()?;
        let mut all = coverage;
        for source in sources {
            all.extend(source.diagnostics);
        }
        all.sort();
        all.dedup();
        Ok((generation, all))
    }

    pub fn document(&self, path: &str) -> Result<(u64, Option<StoredDocument>), StoreError> {
        self.read(|conn| load_document(conn, "path", path))
    }

    pub fn document_by_id(&self, id: &str) -> Result<(u64, Option<StoredDocument>), StoreError> {
        self.read(|conn| load_document(conn, "doc_id", id))
    }

    /// Look a section up by its portable key. A key that no longer exists (a
    /// renamed heading, a removed document) is `None` — never a different
    /// section.
    pub fn section(&self, key: &str) -> Result<(u64, Option<StoredSection>), StoreError> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    &format!("{SECTION_SELECT} WHERE section_key = ?1"),
                    [key],
                    section_from_row,
                )
                .optional()?)
        })
    }

    /// Every document with its sections, sorted by path, with no machine-local
    /// data. Equal output means equal logical corpora.
    pub fn export_logical(&self) -> Result<(u64, Vec<StoredDocument>), StoreError> {
        self.read(|conn| {
            let paths: Vec<String> = {
                let mut statement = conn.prepare("SELECT path FROM documents ORDER BY path")?;
                statement
                    .query_map([], |row| row.get(0))?
                    .collect::<Result<_, _>>()?
            };
            let mut documents = Vec::with_capacity(paths.len());
            for path in paths {
                if let Some(document) = load_document(conn, "path", &path)? {
                    documents.push(document);
                }
            }
            Ok(documents)
        })
    }

    /// Verify the full-text index agrees with the section rows.
    pub fn check_fts_integrity(&self) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sections_fts (sections_fts, rank) VALUES ('integrity-check', 1)",
            [],
        )
        .map(|_| ())
        .map_err(|error| StoreError::Unreadable(format!("full-text index mismatch: {error}")))
    }

    /// Section keys matching a raw FTS5 expression. Tests only: production
    /// retrieval treats user text as text and never passes raw FTS syntax.
    #[cfg(test)]
    pub(crate) fn fts_keys(&self, expression: &str) -> Vec<String> {
        let conn = self.lock().unwrap();
        let mut statement = conn
            .prepare(
                "SELECT s.section_key FROM sections_fts f JOIN sections s ON s.id = f.rowid \
                 WHERE sections_fts MATCH ?1 ORDER BY s.section_key",
            )
            .unwrap();
        statement
            .query_map([expression], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
}

// ─── Search ──────────────────────────────────────────────────────────────────

/// What one retrieval pass reads from a single generation.
#[derive(Debug, Clone, Default)]
pub struct SearchPlan {
    /// A complete, already-quoted FTS5 expression built by the caller from
    /// tokens. Never user text.
    pub fts_expression: Option<String>,
    /// Normalized repository-relative file or directory the results are
    /// confined to.
    pub scope: Option<String>,
    /// Normalized candidate paths named by the question; documents among them
    /// contribute all their sections as exact matches.
    pub exact_paths: Vec<String>,
    /// Maximum ranked full-text rows to return.
    pub fts_limit: usize,
    /// Also list path-level code records in scope.
    pub include_files: bool,
}

/// A section that matched, with the document facts retrieval needs.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionMatch {
    pub doc_path: String,
    pub doc_id: String,
    pub doc_title: String,
    pub doc_status: Option<String>,
    pub section: StoredSection,
    /// The section belongs to a document the question named by path.
    pub exact: bool,
}

/// A path-level code record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub path: String,
    pub size: u64,
}

/// Everything one [`RepositoryStore::search`] read, from one generation.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchSnapshot {
    pub generation: u64,
    /// Exact-path sections (path, ordinal order) followed by ranked full-text
    /// sections (best first, ties by path then ordinal).
    pub sections: Vec<SectionMatch>,
    /// Whether the full-text query returned as many rows as it was allowed to.
    pub fts_saturated: bool,
    /// Code records in scope, sorted by path.
    pub files: Vec<FileRecord>,
}

/// Whether `path` is `scope` itself or lies below it. `None` is everything.
pub fn path_in_scope(path: &str, scope: Option<&str>) -> bool {
    match scope {
        None => true,
        Some(scope) => {
            path == scope
                || path
                    .strip_prefix(scope)
                    .is_some_and(|rest| rest.starts_with('/'))
        }
    }
}

fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

const MATCH_COLUMNS: &str = "s.ordinal, s.section_key, s.level, s.title, s.breadcrumb_json, \
     s.occurrence, s.start_line, s.end_line, s.body, s.from_metadata, \
     s.doc_path, d.doc_id, d.title, d.status";

fn match_from_row(row: &rusqlite::Row<'_>, exact: bool) -> rusqlite::Result<SectionMatch> {
    Ok(SectionMatch {
        section: section_from_row(row)?,
        doc_path: row.get(10)?,
        doc_id: row.get(11)?,
        doc_title: row.get(12)?,
        doc_status: row.get(13)?,
        exact,
    })
}

impl RepositoryStore {
    /// Read the matching sections and code records from one snapshot, so a
    /// commit cannot split a result across generations.
    pub fn search(&self, plan: &SearchPlan) -> Result<SearchSnapshot, StoreError> {
        let scope = plan.scope.as_deref();
        let like = scope.map(|scope| format!("{}/%", escape_like(scope)));
        let (generation, (sections, fts_saturated, files)) = self.read(|conn| {
            let mut sections: Vec<SectionMatch> = Vec::new();
            let mut seen = BTreeSet::new();

            for path in &plan.exact_paths {
                if !path_in_scope(path, scope) {
                    continue;
                }
                let mut statement = conn.prepare(&format!(
                    "SELECT {MATCH_COLUMNS} FROM sections s JOIN documents d ON d.path = s.doc_path \
                     WHERE s.doc_path = ?1 ORDER BY s.ordinal"
                ))?;
                let rows = statement
                    .query_map([path], |row| match_from_row(row, true))?
                    .collect::<Result<Vec<_>, _>>()?;
                for row in rows {
                    if seen.insert(row.section.key.clone()) {
                        sections.push(row);
                    }
                }
            }

            let mut saturated = false;
            if let Some(expression) = &plan.fts_expression {
                let limit = to_i64(plan.fts_limit.max(1));
                let mut statement = conn.prepare(&format!(
                    "SELECT {MATCH_COLUMNS} FROM sections_fts f \
                     JOIN sections s ON s.id = f.rowid \
                     JOIN documents d ON d.path = s.doc_path \
                     WHERE sections_fts MATCH ?1 \
                       AND (?2 IS NULL OR s.doc_path = ?2 OR s.doc_path LIKE ?3 ESCAPE '\\') \
                     ORDER BY bm25(sections_fts, 10.0, 6.0, 4.0, 4.0, 1.0), s.doc_path, s.ordinal \
                     LIMIT ?4"
                ))?;
                let rows = statement
                    .query_map(params![expression, scope, like, limit], |row| {
                        match_from_row(row, false)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                saturated = i64::try_from(rows.len()).unwrap_or(i64::MAX) >= limit;
                for row in rows {
                    if seen.insert(row.section.key.clone()) {
                        sections.push(row);
                    }
                }
            }

            let mut files = Vec::new();
            if plan.include_files {
                let mut statement = conn.prepare(
                    "SELECT path, size FROM sources \
                     WHERE is_code = 1 \
                       AND (?1 IS NULL OR path = ?1 OR path LIKE ?2 ESCAPE '\\') \
                     ORDER BY path",
                )?;
                files = statement
                    .query_map(params![scope, like], |row| {
                        Ok(FileRecord {
                            path: row.get(0)?,
                            size: u64::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
            }
            Ok((sections, saturated, files))
        })?;
        Ok(SearchSnapshot {
            generation,
            sections,
            fts_saturated,
            files,
        })
    }
}

// ─── Writes ──────────────────────────────────────────────────────────────────

fn validate_unique<'a>(paths: impl Iterator<Item = &'a str>, what: &str) -> Result<(), StoreError> {
    let mut seen = BTreeSet::new();
    for path in paths {
        if !seen.insert(path) {
            return Err(StoreError::InvalidRecord(format!(
                "`{path}` appears more than once in {what}"
            )));
        }
    }
    Ok(())
}

fn validate_update(update: &GenerationUpdate) -> Result<(), StoreError> {
    validate_unique(
        update.upserts.iter().map(|record| record.path.as_str()),
        "upserts",
    )?;
    validate_unique(update.removals.iter().map(String::as_str), "removals")?;
    for record in &update.upserts {
        record.validate()?;
        if update.removals.contains(&record.path) {
            return Err(StoreError::InvalidRecord(format!(
                "`{}` is both upserted and removed",
                record.path
            )));
        }
    }
    Ok(())
}

fn id_bits(value: u64) -> i64 {
    i64::from_ne_bytes(value.to_ne_bytes())
}

fn id_from_bits(value: i64) -> u64 {
    u64::from_ne_bytes(value.to_ne_bytes())
}

fn to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::InvalidRecord(error.to_string()))
}

fn apply_touch(conn: &Connection, touch: &SourceTouch) -> Result<usize, StoreError> {
    let (dev, ino) = match touch.stamp.file_id {
        Some((dev, ino)) => (Some(id_bits(dev)), Some(id_bits(ino))),
        None => (None, None),
    };
    Ok(conn.execute(
        "UPDATE sources SET size = ?2, mtime_ns = ?3, file_dev = ?4, file_ino = ?5, \
         content_hash = ?6, indexed_at_ns = ?7 WHERE path = ?1",
        params![
            touch.path,
            i64::try_from(touch.stamp.size).unwrap_or(i64::MAX),
            touch.stamp.mtime_ns,
            dev,
            ino,
            touch.content_hash,
            touch.indexed_at_ns,
        ],
    )?)
}

fn insert_source(conn: &Connection, record: &SourceRecord) -> Result<(), StoreError> {
    let (dev, ino) = match record.stamp.file_id {
        Some((dev, ino)) => (Some(id_bits(dev)), Some(id_bits(ino))),
        None => (None, None),
    };
    conn.execute(
        "INSERT INTO sources (path, is_document, is_code, size, mtime_ns, file_dev, file_ino, \
         content_hash, indexed_at_ns, state, explicit_id, diagnostics_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            record.path,
            record.is_document,
            record.is_code,
            i64::try_from(record.stamp.size).unwrap_or(i64::MAX),
            record.stamp.mtime_ns,
            dev,
            ino,
            record.content_hash,
            record.indexed_at_ns,
            record.state.as_str(),
            record.explicit_id,
            diagnostics_to_json(&record.diagnostics),
        ],
    )?;
    if let Some(document) = &record.document {
        insert_document(conn, document)?;
    }
    Ok(())
}

fn kind_name(kind: DocumentKind) -> &'static str {
    match kind {
        DocumentKind::Markdown => "markdown",
        DocumentKind::WorkspaceMemory => "workspace_memory",
    }
}

fn kind_from_name(name: &str) -> Result<DocumentKind, StoreError> {
    match name {
        "markdown" => Ok(DocumentKind::Markdown),
        "workspace_memory" => Ok(DocumentKind::WorkspaceMemory),
        other => Err(StoreError::Unreadable(format!(
            "unknown document kind `{other}`"
        ))),
    }
}

fn insert_document(conn: &Connection, document: &NormalizedDocument) -> Result<(), StoreError> {
    let parsed = &document.parsed;
    conn.execute(
        "INSERT INTO documents (path, doc_id, kind, title, description, status, tags_json, \
         knowledge_sources_json, knowledge_paths_json, extensions_json, had_frontmatter, \
         line_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            parsed.relative_path,
            document.id.as_str(),
            kind_name(parsed.kind),
            document.title,
            parsed.description,
            parsed.status,
            json(&parsed.tags)?,
            json(&parsed.knowledge_sources)?,
            json(&parsed.knowledge_paths)?,
            json(&parsed.extensions)?,
            parsed.had_frontmatter,
            to_i64(parsed.line_count),
        ],
    )?;

    let tags = parsed.tags.join(" ");
    let description = parsed.description.clone().unwrap_or_default();
    for section in &document.sections {
        let search_title = section
            .title
            .clone()
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| document.title.clone());
        conn.execute(
            "INSERT INTO sections (doc_path, ordinal, section_key, level, title, breadcrumb_json, \
             occurrence, start_line, end_line, body, from_metadata, search_title, \
             search_breadcrumb, search_tags, search_description) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                parsed.relative_path,
                to_i64(section.ordinal),
                section.key.to_string(),
                i64::from(section.level),
                section.title,
                json(&section.key.breadcrumb)?,
                i64::from(section.key.occurrence),
                to_i64(section.start_line),
                to_i64(section.end_line),
                section.body,
                section.from_metadata,
                search_title,
                section.key.breadcrumb.join(" > "),
                tags,
                description,
            ],
        )?;
    }
    Ok(())
}

// ─── Reads ───────────────────────────────────────────────────────────────────

const SOURCE_SELECT: &str = "SELECT s.path, s.is_document, s.is_code, s.size, s.mtime_ns, \
     s.file_dev, s.file_ino, s.content_hash, s.indexed_at_ns, s.state, s.explicit_id, \
     s.diagnostics_json, (d.path IS NOT NULL) \
     FROM sources s LEFT JOIN documents d ON d.path = s.path";

fn source_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<StoredSource, StoreError>> {
    let dev: Option<i64> = row.get(5)?;
    let ino: Option<i64> = row.get(6)?;
    let state: String = row.get(9)?;
    let diagnostics: String = row.get(11)?;
    let state = match SourceState::parse(&state) {
        Ok(state) => state,
        Err(error) => return Ok(Err(error)),
    };
    Ok(Ok(StoredSource {
        path: row.get(0)?,
        is_document: row.get(1)?,
        is_code: row.get(2)?,
        stamp: FileStamp {
            size: u64::try_from(row.get::<_, i64>(3)?).unwrap_or_default(),
            mtime_ns: row.get(4)?,
            file_id: dev
                .zip(ino)
                .map(|(d, i)| (id_from_bits(d), id_from_bits(i))),
        },
        content_hash: row.get(7)?,
        indexed_at_ns: row.get(8)?,
        state,
        explicit_id: row.get(10)?,
        diagnostics: diagnostics_from_json(&diagnostics),
        has_document: row.get(12)?,
    }))
}

const SECTION_SELECT: &str = "SELECT ordinal, section_key, level, title, breadcrumb_json, \
     occurrence, start_line, end_line, body, from_metadata FROM sections";

fn section_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSection> {
    let breadcrumb_json: String = row.get(4)?;
    Ok(StoredSection {
        ordinal: usize::try_from(row.get::<_, i64>(0)?).unwrap_or_default(),
        key: row.get(1)?,
        level: u8::try_from(row.get::<_, i64>(2)?).unwrap_or_default(),
        title: row.get(3)?,
        breadcrumb: serde_json::from_str(&breadcrumb_json).unwrap_or_default(),
        occurrence: u32::try_from(row.get::<_, i64>(5)?).unwrap_or_default(),
        start_line: usize::try_from(row.get::<_, i64>(6)?).unwrap_or_default(),
        end_line: usize::try_from(row.get::<_, i64>(7)?).unwrap_or_default(),
        body: row.get(8)?,
        from_metadata: row.get(9)?,
    })
}

fn parse_json<T: serde::de::DeserializeOwned + Default>(raw: &str) -> T {
    serde_json::from_str(raw).unwrap_or_default()
}

/// `column` is a fixed identifier chosen by this module, never user input.
fn load_document(
    conn: &Connection,
    column: &'static str,
    value: &str,
) -> Result<Option<StoredDocument>, StoreError> {
    let sql = format!(
        "SELECT path, doc_id, kind, title, description, status, tags_json, \
         knowledge_sources_json, knowledge_paths_json, extensions_json, had_frontmatter, \
         line_count FROM documents WHERE {column} = ?1"
    );
    let row = conn
        .query_row(&sql, [value], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, bool>(10)?,
                row.get::<_, i64>(11)?,
            ))
        })
        .optional()?;
    let Some((path, id, kind, title, description, status, tags, sources, paths, ext, fm, lines)) =
        row
    else {
        return Ok(None);
    };

    let mut statement = conn.prepare(&format!(
        "{SECTION_SELECT} WHERE doc_path = ?1 ORDER BY ordinal"
    ))?;
    let sections = statement
        .query_map([&path], section_from_row)?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Some(StoredDocument {
        path,
        id,
        kind: kind_from_name(&kind)?,
        title,
        description,
        status,
        tags: parse_json(&tags),
        knowledge_sources: parse_json(&sources),
        knowledge_paths: parse_json(&paths),
        extensions: parse_json(&ext),
        had_frontmatter: fm,
        line_count: usize::try_from(lines).unwrap_or_default(),
        sections,
    }))
}

/// Delete a database and its sidecar files. Missing files are fine.
fn remove_database_files(path: &Path) -> Result<(), StoreError> {
    let mut names = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        names.push(PathBuf::from(sidecar));
    }
    for file in names {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::document::parse_documents;
    use crate::knowledge::repository::identity::normalize_documents;
    use std::fs;

    fn stamp(n: u64) -> FileStamp {
        FileStamp {
            size: 100 + n,
            mtime_ns: Some(1_000 + n as i64),
            file_id: Some((7, n)),
        }
    }

    fn doc_record(path: &str, text: &str, n: u64) -> SourceRecord {
        let mut batch = normalize_documents(parse_documents([(path, text.as_bytes())]));
        assert!(batch.diagnostics.is_empty(), "{:?}", batch.diagnostics);
        SourceRecord {
            path: path.to_string(),
            is_document: true,
            is_code: false,
            stamp: stamp(n),
            content_hash: Some(format!("hash-{n}")),
            indexed_at_ns: 5_000 + n as i64,
            document: Some(batch.documents.remove(0)),
            state: SourceState::Parsed,
            explicit_id: None,
            diagnostics: Vec::new(),
        }
    }

    fn code_record(path: &str, n: u64) -> SourceRecord {
        SourceRecord {
            path: path.to_string(),
            is_document: false,
            is_code: true,
            stamp: stamp(n),
            content_hash: None,
            indexed_at_ns: 5_000,
            document: None,
            state: SourceState::Code,
            explicit_id: None,
            diagnostics: Vec::new(),
        }
    }

    fn update(upserts: Vec<SourceRecord>) -> GenerationUpdate {
        GenerationUpdate {
            upserts,
            ..Default::default()
        }
    }

    fn store_in(dir: &Path) -> RepositoryStore {
        RepositoryStore::open(&dir.join("cache/knowledge/abc/index.sqlite")).unwrap()
    }

    const RUNTIME: &str = "---\nid: runtime\ntitle: Runtime\ntags: [sessions, lifecycle]\ndescription: How the runtime schedules work\n---\n# Overview\nThe scheduler dispatches tasks.\n## Configuration\nConfigured through the builder.\n";
    const TOOLS: &str = "# Tools\nTools are registered per session.\n";

    // ── open / create / reopen ───────────────────────────────────────────

    #[test]
    fn creates_then_reopens_with_data_and_generation_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache/knowledge/k/index.sqlite");
        {
            let store = RepositoryStore::open(&path).unwrap();
            assert_eq!(store.open_outcome(), OpenOutcome::Created);
            assert_eq!(store.generation().unwrap(), 0);
            let generation = store
                .apply_update(&update(vec![doc_record("docs/runtime.md", RUNTIME, 1)]))
                .unwrap();
            assert_eq!(generation, 1);
        }
        let store = RepositoryStore::open(&path).unwrap();
        assert_eq!(store.open_outcome(), OpenOutcome::Reopened);
        assert_eq!(store.generation().unwrap(), 1);
        let (_, document) = store.document("docs/runtime.md").unwrap();
        let document = document.unwrap();
        assert_eq!(document.id, "id:runtime");
        assert_eq!(document.sections.len(), 2);
    }

    #[test]
    fn unbuilt_store_is_distinct_from_a_built_empty_corpus() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        assert!(!store.is_built().unwrap());
        assert_eq!(store.counts().unwrap().documents, 0);

        store.replace_all(&[], Some("fp")).unwrap();
        assert!(store.is_built().unwrap());
        assert_eq!(store.generation().unwrap(), 1);
        assert_eq!(store.counts().unwrap().documents, 0);
    }

    #[test]
    fn zero_byte_file_is_created_not_rebuilt() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.sqlite");
        fs::write(&path, b"").unwrap();
        let store = RepositoryStore::open(&path).unwrap();
        assert_eq!(store.open_outcome(), OpenOutcome::Created);
    }

    #[test]
    fn corrupt_cache_is_replaced_with_an_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.sqlite");
        fs::write(&path, vec![0xAB; 4096]).unwrap();
        fs::write(tmp.path().join("index.sqlite-wal"), b"stale wal").unwrap();

        let store = RepositoryStore::open(&path).unwrap();
        assert_eq!(
            store.open_outcome(),
            OpenOutcome::Rebuilt(RebuildReason::Corrupt)
        );
        assert_eq!(store.generation().unwrap(), 0);
        store
            .apply_update(&update(vec![doc_record("docs/tools.md", TOOLS, 1)]))
            .unwrap();
        assert_eq!(store.counts().unwrap().documents, 1);
    }

    #[test]
    fn damaged_database_pages_are_detected_and_rebuilt() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.sqlite");
        {
            let store = RepositoryStore::open(&path).unwrap();
            store
                .apply_update(&update(vec![doc_record("docs/runtime.md", RUNTIME, 1)]))
                .unwrap();
            // Fold the WAL into the main file so the damage lands in it.
            store
                .lock()
                .unwrap()
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .unwrap();
        }
        let mut bytes = fs::read(&path).unwrap();
        assert!(bytes.len() > 4096);
        // Smash the header's magic string so it no longer looks like SQLite.
        bytes[..16].copy_from_slice(&[0xFF; 16]);
        fs::write(&path, bytes).unwrap();

        let store = RepositoryStore::open(&path).unwrap();
        assert!(matches!(
            store.open_outcome(),
            OpenOutcome::Rebuilt(RebuildReason::Corrupt)
        ));
        assert_eq!(store.counts().unwrap().documents, 0);
    }

    #[test]
    fn other_schema_versions_and_foreign_databases_are_rebuilt() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.sqlite");
        RepositoryStore::open(&path).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        }
        let store = RepositoryStore::open(&path).unwrap();
        assert_eq!(
            store.open_outcome(),
            OpenOutcome::Rebuilt(RebuildReason::Incompatible)
        );
        assert_eq!(store.generation().unwrap(), 0);
        drop(store);

        // A SQLite file that is not ours at all.
        let foreign = tmp.path().join("foreign.sqlite");
        Connection::open(&foreign)
            .unwrap()
            .execute_batch("CREATE TABLE sessions (id TEXT); INSERT INTO sessions VALUES ('x');")
            .unwrap();
        let store = RepositoryStore::open(&foreign).unwrap();
        assert_eq!(
            store.open_outcome(),
            OpenOutcome::Rebuilt(RebuildReason::Incompatible)
        );
    }

    #[test]
    fn only_corruption_triggers_a_rebuild_never_transient_errors() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        assert!(matches!(
            classify(busy),
            OpenFailure::Fatal(StoreError::Busy)
        ));
        let readonly = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_READONLY),
            None,
        );
        assert!(matches!(classify(readonly), OpenFailure::Fatal(_)));
        let not_db = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
            None,
        );
        assert!(matches!(
            classify(not_db),
            OpenFailure::Rebuild(RebuildReason::Corrupt)
        ));
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        );
        assert!(matches!(
            classify(corrupt),
            OpenFailure::Rebuild(RebuildReason::Corrupt)
        ));
    }

    #[test]
    fn open_touches_only_its_own_files_and_no_session_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".agents")).unwrap();
        let before: Vec<_> = walk(&repo);

        let store = store_in(tmp.path());
        store
            .apply_update(&update(vec![doc_record("docs/runtime.md", RUNTIME, 1)]))
            .unwrap();

        assert_eq!(walk(&repo), before, "repository must be untouched");
        let files: Vec<String> = walk(tmp.path())
            .into_iter()
            .filter(|p| p.is_file())
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            files.iter().all(|n| n.starts_with("index.sqlite")),
            "{files:?}"
        );

        let conn = store.lock().unwrap();
        let tables: BTreeSet<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sections_fts%'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            tables,
            ["documents", "meta", "sections", "sources"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<_>>()
        );
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                out.push(path.clone());
                if path.is_dir() {
                    out.extend(walk(&path));
                }
            }
        }
        out.sort();
        out
    }

    // ── updates, removals, full-text ─────────────────────────────────────

    #[test]
    fn stores_documents_sections_and_path_only_code_records() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        store
            .apply_update(&update(vec![
                doc_record("docs/runtime.md", RUNTIME, 1),
                doc_record("docs/tools.md", TOOLS, 2),
                code_record("src/lib.rs", 3),
            ]))
            .unwrap();

        let counts = store.counts().unwrap();
        assert_eq!(
            (counts.sources, counts.documents, counts.sections),
            (3, 2, 3)
        );
        let (_, code) = store.source("src/lib.rs").unwrap();
        let code = code.unwrap();
        assert!(code.is_code && !code.is_document && code.content_hash.is_none());
        assert!(store.document("src/lib.rs").unwrap().1.is_none());

        let (_, runtime) = store.document_by_id("id:runtime").unwrap();
        let runtime = runtime.unwrap();
        assert_eq!(runtime.path, "docs/runtime.md");
        assert_eq!(runtime.title, "Runtime");
        assert_eq!(runtime.tags, vec!["sessions", "lifecycle"]);
        assert_eq!(
            runtime.description.as_deref(),
            Some("How the runtime schedules work")
        );
        let keys: Vec<_> = runtime.sections.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["id:runtime#Overview", "id:runtime#Overview>Configuration"]
        );
        // The preamble is the frontmatter-free section list's first entry only
        // when text precedes the first heading; here there is none.
        assert_eq!(runtime.sections[0].level, 1);
        assert_eq!(
            runtime.sections[1].breadcrumb,
            vec!["Overview", "Configuration"]
        );
    }

    #[test]
    fn replacing_a_document_replaces_its_sections_and_fts_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        store
            .apply_update(&update(vec![doc_record("docs/runtime.md", RUNTIME, 1)]))
            .unwrap();
        // `tasks` appears only in the Overview body; `scheduler` would also hit
        // the stemmed description on every section.
        assert_eq!(store.fts_keys("tasks"), vec!["id:runtime#Overview"]);

        let edited =
            "---\nid: runtime\ntitle: Runtime\n---\n# Overview\nThe dispatcher runs jobs.\n";
        let generation = store
            .apply_update(&update(vec![doc_record("docs/runtime.md", edited, 2)]))
            .unwrap();
        assert_eq!(generation, 2);
        assert!(store.fts_keys("tasks").is_empty(), "old text must be gone");
        assert_eq!(store.fts_keys("dispatcher"), vec!["id:runtime#Overview"]);
        assert_eq!(store.counts().unwrap().sections, 1);
        store.check_fts_integrity().unwrap();
    }

    #[test]
    fn removals_delete_everything_derived_and_are_noops_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        store
            .apply_update(&update(vec![
                doc_record("docs/runtime.md", RUNTIME, 1),
                doc_record("docs/tools.md", TOOLS, 2),
            ]))
            .unwrap();
        let generation = store
            .apply_update(&GenerationUpdate {
                removals: vec!["docs/runtime.md".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(generation, 2);
        assert!(store.document("docs/runtime.md").unwrap().1.is_none());
        assert!(store.section("id:runtime#Overview").unwrap().1.is_none());
        assert!(store.fts_keys("scheduler").is_empty());
        assert_eq!(store.counts().unwrap().sections, 1);

        // Removing something that is not indexed does not advance the generation.
        let again = store
            .apply_update(&GenerationUpdate {
                removals: vec!["docs/runtime.md".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(again, 2);
        store.check_fts_integrity().unwrap();
    }

    #[test]
    fn unchanged_fingerprint_is_a_noop_and_changed_one_advances() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let fp = |value: &str| GenerationUpdate {
            config_fingerprint: Some(value.into()),
            ..Default::default()
        };
        assert_eq!(store.apply_update(&fp("a")).unwrap(), 1);
        assert_eq!(store.apply_update(&fp("a")).unwrap(), 1);
        assert_eq!(store.apply_update(&fp("b")).unwrap(), 2);
        assert_eq!(store.config_fingerprint().unwrap().as_deref(), Some("b"));
    }

    #[test]
    fn a_failed_update_rolls_back_completely() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        store
            .apply_update(&update(vec![doc_record("docs/tools.md", TOOLS, 1)]))
            .unwrap();
        let before = store.export_logical().unwrap();

        // Injected failure after every write has been issued.
        let result = store.apply_update_with(
            &update(vec![
                doc_record("docs/runtime.md", RUNTIME, 2),
                code_record("src/lib.rs", 3),
            ]),
            &mut || Err(StoreError::Unreadable("injected".into())),
        );
        assert!(result.is_err());
        assert_eq!(store.generation().unwrap(), 1);
        assert_eq!(store.export_logical().unwrap(), before);
        assert!(store.source("src/lib.rs").unwrap().1.is_none());
        assert!(store.fts_keys("scheduler").is_empty());
        store.check_fts_integrity().unwrap();

        // A constraint violation mid-update (duplicate explicit ID across two
        // documents) also leaves the previous generation untouched.
        let clash = update(vec![
            doc_record("docs/a.md", "---\nid: same\n---\nA\n", 4),
            doc_record("docs/b.md", "---\nid: same\n---\nB\n", 5),
        ]);
        let err = store.apply_update(&clash).unwrap_err();
        assert_eq!(err.code(), "index_conflict");
        assert_eq!(store.generation().unwrap(), 1);
        assert!(store.document("docs/a.md").unwrap().1.is_none());
    }

    #[test]
    fn invalid_updates_are_rejected_before_any_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        for bad in [
            update(vec![code_record("/abs/path.rs", 1)]),
            update(vec![code_record("../escape.rs", 1)]),
            update(vec![code_record("", 1)]),
            update(vec![code_record("a.rs", 1), code_record("a.rs", 2)]),
            GenerationUpdate {
                upserts: vec![code_record("a.rs", 1)],
                removals: vec!["a.rs".into()],
                ..Default::default()
            },
        ] {
            assert_eq!(
                store.apply_update(&bad).unwrap_err().code(),
                "index_invalid_record"
            );
        }
        assert_eq!(store.generation().unwrap(), 0);
    }

    #[test]
    fn replace_all_swaps_the_corpus_in_one_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        store
            .apply_update(&update(vec![doc_record("docs/runtime.md", RUNTIME, 1)]))
            .unwrap();
        let generation = store
            .replace_all(&[doc_record("docs/tools.md", TOOLS, 2)], Some("fp2"))
            .unwrap();
        assert_eq!(generation, 2);
        assert!(store.document("docs/runtime.md").unwrap().1.is_none());
        assert!(store.document("docs/tools.md").unwrap().1.is_some());
        assert_eq!(store.config_fingerprint().unwrap().as_deref(), Some("fp2"));
        store.check_fts_integrity().unwrap();
    }

    // ── full text, snapshots, concurrency ────────────────────────────────

    #[test]
    fn full_text_searches_titles_tags_description_and_body_with_stemming() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        store
            .apply_update(&update(vec![doc_record("docs/runtime.md", RUNTIME, 1)]))
            .unwrap();
        // body, stemmed ("Configured" matches "configuration")
        assert!(!store.fts_keys("configuration").is_empty());
        // tags and description are indexed on every section
        assert_eq!(store.fts_keys("lifecycle").len(), 2);
        assert_eq!(store.fts_keys("schedules").len(), 2);
        // heading titles
        assert_eq!(store.fts_keys("search_title: overview").len(), 1);
        assert!(store.fts_keys("nonexistentterm").is_empty());
    }

    #[test]
    fn readers_see_one_generation_while_a_writer_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.sqlite");
        let writer = RepositoryStore::open(&path).unwrap();
        writer
            .apply_update(&update(vec![doc_record("docs/tools.md", TOOLS, 1)]))
            .unwrap();

        // A separate connection (as another process would hold) starts a read
        // snapshot, the writer commits, and the reader still sees generation 1
        // in full until its transaction ends.
        let mut reader = Connection::open(&path).unwrap();
        let snapshot = reader.transaction().unwrap();
        let count = |tx: &rusqlite::Transaction<'_>| -> (String, i64) {
            (
                tx.query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
                    r.get(0)
                })
                .unwrap(),
                tx.query_row("SELECT count(*) FROM sections", [], |r| r.get(0))
                    .unwrap(),
            )
        };
        assert_eq!(count(&snapshot), ("1".to_string(), 1));

        writer
            .apply_update(&update(vec![doc_record("docs/runtime.md", RUNTIME, 2)]))
            .unwrap();
        assert_eq!(
            count(&snapshot),
            ("1".to_string(), 1),
            "snapshot must not tear"
        );
        drop(snapshot);

        let after = reader.transaction().unwrap();
        assert_eq!(count(&after), ("2".to_string(), 3));
    }

    #[test]
    fn concurrent_writers_serialize_and_every_generation_is_consistent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.sqlite");
        RepositoryStore::open(&path).unwrap();

        let handles: Vec<_> = (0..4u64)
            .map(|worker| {
                let path = path.clone();
                std::thread::spawn(move || {
                    // Each thread has its own connection, like separate processes.
                    let store = RepositoryStore::open(&path).unwrap();
                    for round in 0..5u64 {
                        let n = worker * 10 + round;
                        let text = format!("# T{n}\nbody for worker {worker} round {round}\n");
                        store
                            .apply_update(&update(vec![doc_record(
                                &format!("docs/w{worker}.md"),
                                &text,
                                n,
                            )]))
                            .unwrap();
                        let counts = store.counts().unwrap();
                        assert_eq!(counts.documents, counts.sections, "torn generation");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let store = RepositoryStore::open(&path).unwrap();
        let counts = store.counts().unwrap();
        assert_eq!(counts.generation, 20, "every commit advances exactly once");
        assert_eq!(counts.documents, 4);
        store.check_fts_integrity().unwrap();
    }

    // ── portability ──────────────────────────────────────────────────────

    #[test]
    fn logical_export_is_identical_across_stores_with_different_machine_data() {
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let a = store_in(tmp_a.path());
        let b = store_in(tmp_b.path());

        // Same content, different stamps/hashes/times/insert order.
        a.apply_update(&update(vec![
            doc_record("docs/runtime.md", RUNTIME, 1),
            doc_record("docs/tools.md", TOOLS, 2),
        ]))
        .unwrap();
        b.apply_update(&update(vec![doc_record("docs/tools.md", TOOLS, 90)]))
            .unwrap();
        b.apply_update(&update(vec![doc_record("docs/runtime.md", RUNTIME, 91)]))
            .unwrap();

        assert_ne!(a.sources().unwrap().1, b.sources().unwrap().1);
        assert_eq!(a.export_logical().unwrap().1, b.export_logical().unwrap().1);

        let rendered = format!("{:?}", a.export_logical().unwrap().1);
        let machine_local = [tmp_a.path().to_string_lossy().into_owned(), "/Users".into()];
        for fragment in machine_local {
            assert!(!rendered.contains(&fragment), "{fragment}");
        }
    }

    #[test]
    fn cache_loss_rebuild_from_the_same_content_is_logically_equivalent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.sqlite");
        let original = {
            let store = RepositoryStore::open(&path).unwrap();
            store
                .apply_update(&update(vec![
                    doc_record("docs/runtime.md", RUNTIME, 1),
                    doc_record("docs/tools.md", TOOLS, 2),
                ]))
                .unwrap();
            store.export_logical().unwrap().1
        };
        remove_database_files(&path).unwrap();

        let store = RepositoryStore::open(&path).unwrap();
        assert_eq!(store.open_outcome(), OpenOutcome::Created);
        assert!(!store.is_built().unwrap(), "a lost cache reports unbuilt");
        store
            .replace_all(
                &[
                    doc_record("docs/tools.md", TOOLS, 7),
                    doc_record("docs/runtime.md", RUNTIME, 8),
                ],
                None,
            )
            .unwrap();
        assert_eq!(store.export_logical().unwrap().1, original);
    }

    #[test]
    fn stale_section_keys_resolve_to_nothing_never_to_another_section() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        store
            .apply_update(&update(vec![doc_record(
                "docs/a.md",
                "# Old name\nbody\n",
                1,
            )]))
            .unwrap();
        let old_key = "path:docs/a.md#Old name";
        assert!(store.section(old_key).unwrap().1.is_some());
        store
            .apply_update(&update(vec![doc_record(
                "docs/a.md",
                "# New name\nbody\n",
                2,
            )]))
            .unwrap();
        assert!(store.section(old_key).unwrap().1.is_none());
        assert!(
            store
                .section("path:docs/a.md#New name")
                .unwrap()
                .1
                .is_some()
        );
    }

    #[test]
    fn stamps_round_trip_including_large_file_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let mut record = code_record("src/lib.rs", 1);
        record.stamp = FileStamp {
            size: u64::from(u32::MAX) + 5,
            mtime_ns: Some(1_700_000_000_123_456_789),
            file_id: Some((u64::MAX, u64::MAX - 3)),
        };
        store.apply_update(&update(vec![record.clone()])).unwrap();
        let stored = store.source("src/lib.rs").unwrap().1.unwrap();
        assert_eq!(stored.stamp, record.stamp);
        assert_eq!(stored.indexed_at_ns, record.indexed_at_ns);

        let mut no_id = code_record("src/other.rs", 2);
        no_id.stamp.file_id = None;
        no_id.stamp.mtime_ns = None;
        store.apply_update(&update(vec![no_id.clone()])).unwrap();
        assert_eq!(
            store.source("src/other.rs").unwrap().1.unwrap().stamp,
            no_id.stamp
        );
    }
}

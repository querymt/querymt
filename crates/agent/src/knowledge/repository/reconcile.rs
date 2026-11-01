//! Reconciling the checkout-local index with the files on disk.
//!
//! [`reconcile`] makes the store reflect what the manifest selects *right now*.
//! It never trusts that a watcher saw every write: each call enumerates the
//! selected files (metadata only) and decides per file whether its stored
//! record is still valid.
//!
//! # Metadata fast path
//!
//! A stored record is reused **without opening the file** only when all of the
//! following hold:
//!
//! * `(size, mtime_ns, device/inode)` equal the stored stamp, and the platform
//!   reports both an mtime and a file identity (otherwise metadata is not
//!   reliable and the file is hashed);
//! * the record is not *racily clean*: the file's mtime is not within
//!   [`ReconcileOptions::racy_window`] of the moment the record was indexed
//!   (Git's rule: an edit made shortly after indexing can leave size and mtime
//!   unchanged). Such files are hashed until the window has passed;
//! * the manifest-derived configuration fingerprint is unchanged and the file
//!   is still selected the same way (document and/or code);
//! * the host has not set `force_hash`.
//!
//! Anything else reads the file and compares a SHA-256 of its bytes with the
//! stored hash. An equal hash only refreshes the stored stamp (a
//! bookkeeping "touch" that never advances the generation); a different hash
//! re-parses the document. Unchanged files are therefore never re-parsed, and
//! a warm refresh with nothing to do performs no read of file content and no
//! write transaction.
//!
//! # Corpus-level rules
//!
//! * Additions, deletions, disabled/malformed sources and manifest changes are
//!   always reconciled. A missing or invalid manifest empties the corpus and
//!   records a coverage diagnostic, so a previous body is never presented as
//!   current.
//! * Explicit document IDs must be unique across the *whole* corpus, not just
//!   the files re-parsed in this pass. When a changed, added or removed source
//!   touches an ID, every other source claiming that ID is re-read so
//!   duplicates are excluded (and later restored) correctly.
//! * A file that cannot be read, grew past its limit, or vanished after
//!   discovery is dropped from the corpus with a coverage diagnostic.
//! * All changes are applied as one generation, guarded by the generation that
//!   was observed, so a concurrent writer surfaces as
//!   [`StoreError::Stale`] rather than being overwritten.
//!
//! Serialized refresh, retries and pre-emission revalidation are layered on
//! top of this by the service; this module is the pure reconciliation step.

use super::code::{CODE_EXTRACTOR_VERSION, extract_code};
use super::discovery::{
    DiscoveredFile, DiscoveryOptions, FileStamp, codes as discovery_codes, discover,
};
use super::document::{DiagnosticSeverity, ParsedBatch, parse_documents};
use super::identity::{
    NormalizedBatch, NormalizedDocument, normalize_documents, normalize_explicit_id,
};
use super::manifest::{KnowledgeManifest, MANIFEST_RELATIVE_PATH, ManifestOutcome, load_manifest};
use super::purpose::parse_purpose_rows;
use super::root::ResolvedRoot;
use super::store::{
    GenerationUpdate, IndexDiagnostic, RepositoryStore, SCHEMA_VERSION, SourceRecord, SourceState,
    SourceTouch, StoreError, StoredSource,
};
use crate::config::RepositoryKnowledgeSettings;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Version of the document parsing/normalization pipeline. Bumping it
/// re-parses every source on the next refresh. Code extraction has its own
/// version ([`CODE_EXTRACTOR_VERSION`]) that invalidates only code records.
const EXTRACTOR_VERSION: u32 = 2;

/// Default racy-timestamp window. Two seconds covers the coarsest common
/// filesystem timestamp granularity.
pub const DEFAULT_RACY_WINDOW: Duration = Duration::from_secs(2);

/// Why a reconciliation could not complete. Nothing is partially applied.
#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Inputs to [`reconcile`].
#[derive(Debug, Clone)]
pub struct ReconcileOptions {
    pub discovery: DiscoveryOptions,
    /// Hash every selected file, bypassing the metadata fast path.
    pub force_hash: bool,
    /// Files whose mtime lies within this window of their index time are
    /// hashed rather than trusted.
    pub racy_window: Duration,
    /// Override "now" (nanoseconds since the Unix epoch). `None` uses the
    /// system clock; tests pin it.
    pub now_ns: Option<i64>,
}

impl From<&RepositoryKnowledgeSettings> for ReconcileOptions {
    fn from(settings: &RepositoryKnowledgeSettings) -> Self {
        Self {
            discovery: DiscoveryOptions::from(settings),
            force_hash: settings.force_hash,
            racy_window: DEFAULT_RACY_WINDOW,
            now_ns: None,
        }
    }
}

/// What a reconciliation concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileStatus {
    /// The first generation was committed.
    Built,
    /// The corpus or its coverage changed; the generation advanced.
    Updated,
    /// The corpus already matched the files.
    Unchanged,
    /// No `.qmt/knowledge.toml`: nothing is selected or crawled.
    NotConfigured,
    /// The manifest could not be used; the corpus is empty until it is fixed.
    ManifestInvalid,
}

/// Outcome and cost of one reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub status: ReconcileStatus,
    /// Generation after the reconciliation.
    pub generation: u64,
    /// Sources in the corpus.
    pub selected: usize,
    /// Sources new to the index.
    pub added: usize,
    /// Previously indexed sources whose record was rebuilt.
    pub changed: usize,
    /// Sources dropped from the index.
    pub removed: usize,
    /// Sources trusted from metadata alone (content not opened).
    pub reused: usize,
    /// Sources whose content was read and hashed.
    pub hashed: usize,
    /// Documents parsed.
    pub reparsed: usize,
    /// Sources whose content was unchanged and whose stamp was refreshed.
    pub touched: usize,
    /// Coverage diagnostics for this generation (sorted). Empty means every
    /// selected file is indexed.
    pub diagnostics: Vec<IndexDiagnostic>,
}

impl ReconcileReport {
    /// True when every selected file is indexed and the manifest was usable.
    pub fn is_complete(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// A source whose record is being rebuilt.
struct Fresh {
    is_document: bool,
    is_code: bool,
    stamp: FileStamp,
    hash: String,
    /// Bytes awaiting parsing (documents) or symbol extraction (code).
    bytes: Vec<u8>,
}

pub(super) struct ReadSource {
    pub(super) stamp: FileStamp,
    pub(super) hash: String,
    pub(super) bytes: Vec<u8>,
}

/// Reconcile `store` with the files `root`'s manifest selects.
///
/// Reads file metadata for every selected file and file content only where the
/// stored record cannot be trusted. Never writes under the repository.
pub fn reconcile(
    root: &ResolvedRoot,
    store: &RepositoryStore,
    options: &ReconcileOptions,
) -> Result<ReconcileReport, ReconcileError> {
    reconcile_with(root, store, options, &mut || Ok(()))
}

/// As [`reconcile`], calling `before_commit` after all writes and before the
/// commit (fault injection and concurrency tests).
pub(crate) fn reconcile_with(
    root: &ResolvedRoot,
    store: &RepositoryStore,
    options: &ReconcileOptions,
    before_commit: &mut dyn FnMut() -> Result<(), StoreError>,
) -> Result<ReconcileReport, ReconcileError> {
    // Captured before any read so a file modified during this pass still looks
    // racy on the next one.
    let now_ns = options.now_ns.unwrap_or_else(system_now_ns);
    let state = store.state()?;

    let mut coverage: Vec<IndexDiagnostic> = Vec::new();
    let (manifest, fingerprint, override_status) = match load_manifest(root.root()) {
        Ok(ManifestOutcome::Loaded(manifest)) => {
            let fingerprint = fingerprint("manifest", Some(&manifest));
            (Some(manifest), fingerprint, None)
        }
        Ok(ManifestOutcome::NotConfigured) => {
            coverage.push(IndexDiagnostic::new(
                None,
                DiagnosticSeverity::Info,
                ManifestOutcome::NOT_CONFIGURED_CODE,
                format!("no {MANIFEST_RELATIVE_PATH}; repository knowledge is not configured"),
            ));
            (
                None,
                fingerprint("not-configured", None),
                Some(ReconcileStatus::NotConfigured),
            )
        }
        Err(error) => {
            coverage.push(IndexDiagnostic::new(
                Some(MANIFEST_RELATIVE_PATH),
                DiagnosticSeverity::Error,
                error.code(),
                error.to_string(),
            ));
            (
                None,
                fingerprint(error.code(), None),
                Some(ReconcileStatus::ManifestInvalid),
            )
        }
    };

    let discovery = manifest
        .as_ref()
        .map(|manifest| discover(root, manifest, &options.discovery))
        .unwrap_or_default();
    for diagnostic in &discovery.diagnostics {
        coverage.push(IndexDiagnostic::new(
            diagnostic.path.as_deref(),
            DiagnosticSeverity::Warning,
            diagnostic.code,
            diagnostic.message.clone(),
        ));
    }

    let stored: BTreeMap<&str, &StoredSource> = state
        .sources
        .iter()
        .map(|source| (source.path.as_str(), source))
        .collect();
    let rebuild_all = state.fingerprint.as_deref() != Some(fingerprint.as_str());
    let window_ns = i64::try_from(options.racy_window.as_nanos()).unwrap_or(i64::MAX);
    let max_bytes = options.discovery.max_file_bytes;

    // ── Decide, per file, whether the stored record can be trusted ──────────
    let mut desired: BTreeMap<&str, &DiscoveredFile> = BTreeMap::new();
    let mut touches: BTreeMap<String, SourceTouch> = BTreeMap::new();
    // Bytes of unchanged documents that claim an explicit ID, in case a
    // duplicate-ID change forces them to be re-parsed.
    let mut held: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut fresh: BTreeMap<String, Fresh> = BTreeMap::new();
    let (mut reused, mut hashed) = (0usize, 0usize);

    for file in &discovery.files {
        let path = file.relative_path.as_str();
        let reusable = stored.get(path).copied().filter(|source| {
            !rebuild_all
                && source.is_document == file.is_document
                && source.is_code == file.is_code
                && source.content_hash.is_some()
                // A code record built by another extractor format is never
                // reused, even when its bytes are unchanged.
                && (!file.is_code || source.code_version == Some(CODE_EXTRACTOR_VERSION))
        });
        if let Some(source) = reusable
            && !options.force_hash
            && fast_path_valid(source, &file.stamp, window_ns)
        {
            reused += 1;
            desired.insert(path, file);
            continue;
        }
        match read_source(&file.relative_path, &file.canonical_path, max_bytes) {
            Err(diagnostic) => coverage.push(diagnostic),
            Ok(read) => {
                hashed += 1;
                desired.insert(path, file);
                match reusable {
                    Some(source) if source.content_hash.as_deref() == Some(read.hash.as_str()) => {
                        if file.is_document && source.explicit_id.is_some() {
                            held.insert(path.to_string(), read.bytes);
                        }
                        touches.insert(
                            path.to_string(),
                            SourceTouch {
                                path: path.to_string(),
                                stamp: read.stamp,
                                content_hash: Some(read.hash),
                                indexed_at_ns: now_ns,
                            },
                        );
                    }
                    _ => {
                        fresh.insert(
                            path.to_string(),
                            Fresh {
                                is_document: file.is_document,
                                is_code: file.is_code,
                                stamp: read.stamp,
                                hash: read.hash,
                                bytes: read.bytes,
                            },
                        );
                    }
                }
            }
        }
    }

    // ── Explicit-ID claims are corpus-wide ──────────────────────────────────
    let mut affected: BTreeSet<String> = BTreeSet::new();
    for source in &state.sources {
        let path = source.path.as_str();
        if (!desired.contains_key(path) || fresh.contains_key(path))
            && let Some(id) = &source.explicit_id
        {
            affected.insert(id.clone());
        }
    }

    let mut batch = parse_documents(
        fresh
            .iter()
            .filter(|(_, source)| source.is_document)
            .map(|(path, source)| (path.as_str(), source.bytes.as_slice())),
    );
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    collect_ids(&batch, &mut ids);
    affected.extend(ids.values().cloned());

    // Unchanged sources claiming an affected ID must be re-evaluated against
    // the new claim set, even though their own bytes did not change.
    let extra_paths: Vec<&str> = state
        .sources
        .iter()
        .filter(|source| {
            source.is_document
                && !fresh.contains_key(source.path.as_str())
                && source
                    .explicit_id
                    .as_ref()
                    .is_some_and(|id| affected.contains(id))
        })
        .map(|source| source.path.as_str())
        .collect();
    let mut extras: BTreeMap<String, Fresh> = BTreeMap::new();
    for path in extra_paths {
        let Some(file) = desired.get(path).copied() else {
            continue;
        };
        let built = match (touches.remove(path), held.remove(path)) {
            (Some(touch), Some(bytes)) => Some(Fresh {
                is_document: file.is_document,
                is_code: file.is_code,
                stamp: touch.stamp,
                hash: touch.content_hash.unwrap_or_default(),
                bytes,
            }),
            _ => match read_source(&file.relative_path, &file.canonical_path, max_bytes) {
                Ok(read) => {
                    hashed += 1;
                    Some(Fresh {
                        is_document: file.is_document,
                        is_code: file.is_code,
                        stamp: read.stamp,
                        hash: read.hash,
                        bytes: read.bytes,
                    })
                }
                Err(diagnostic) => {
                    coverage.push(diagnostic);
                    desired.remove(path);
                    None
                }
            },
        };
        if let Some(built) = built {
            extras.insert(path.to_string(), built);
        }
    }
    if !extras.is_empty() {
        let more = parse_documents(
            extras
                .iter()
                .map(|(path, source)| (path.as_str(), source.bytes.as_slice())),
        );
        collect_ids(&more, &mut ids);
        batch.documents.extend(more.documents);
        batch.diagnostics.extend(more.diagnostics);
        fresh.extend(extras);
    }
    let reparsed = fresh.values().filter(|source| source.is_document).count();

    // ── Identity pass over everything parsed, then build records ────────────
    let NormalizedBatch {
        documents,
        diagnostics: document_diagnostics,
    } = normalize_documents(batch);
    let mut docs: BTreeMap<String, NormalizedDocument> = documents
        .into_iter()
        .map(|document| (document.relative_path().to_string(), document))
        .collect();
    let mut by_path: BTreeMap<String, Vec<IndexDiagnostic>> = BTreeMap::new();
    for diagnostic in &document_diagnostics {
        by_path
            .entry(diagnostic.path.clone())
            .or_default()
            .push(IndexDiagnostic::from(diagnostic));
    }

    let added = fresh
        .keys()
        .filter(|path| !stored.contains_key(path.as_str()))
        .count();
    let changed = fresh.len() - added;

    let mut upserts: Vec<SourceRecord> = Vec::with_capacity(fresh.len());
    for (path, source) in fresh {
        let diagnostics = by_path.remove(&path).unwrap_or_default();
        let mut diagnostics = diagnostics;
        let (state, document, explicit_id) = if source.is_document {
            let explicit_id = ids.get(&path).cloned();
            match docs.remove(&path) {
                Some(document) => (SourceState::Parsed, Some(document), explicit_id),
                None => (SourceState::Excluded, None, explicit_id),
            }
        } else {
            (SourceState::Code, None, None)
        };
        let purposes = document
            .as_ref()
            .filter(|document| !document.parsed.body_from_metadata)
            .map(|document| {
                parse_purpose_rows(
                    &path,
                    &document.parsed.body,
                    document.parsed.body_start_line,
                )
            })
            .unwrap_or_default();
        let code = source.is_code.then(|| {
            let extraction = extract_code(&path, &source.bytes);
            diagnostics.extend(extraction.diagnostic);
            extraction.record
        });
        upserts.push(SourceRecord {
            path,
            is_document: source.is_document,
            is_code: source.is_code,
            stamp: source.stamp,
            content_hash: Some(source.hash),
            indexed_at_ns: now_ns,
            document,
            state,
            explicit_id,
            diagnostics,
            code,
            purposes,
        });
    }

    let removals: Vec<String> = state
        .sources
        .iter()
        .filter(|source| !desired.contains_key(source.path.as_str()))
        .map(|source| source.path.clone())
        .collect();
    let touched = touches.len();

    coverage.sort();
    coverage.dedup();

    // ── Apply as one generation, only if something actually differs ─────────
    let nothing_to_do = upserts.is_empty()
        && removals.is_empty()
        && touches.is_empty()
        && state.fingerprint.as_deref() == Some(fingerprint.as_str())
        && state.coverage == coverage;
    let removed = removals.len();
    let selected = desired.len();
    let generation = if nothing_to_do {
        state.generation
    } else {
        let update = GenerationUpdate {
            upserts,
            removals,
            config_fingerprint: Some(fingerprint),
            coverage: Some(coverage.clone()),
            expected_generation: Some(state.generation),
            touches: touches.into_values().collect(),
        };
        store.apply_update_with(&update, before_commit)?
    };

    let status = override_status.unwrap_or(if generation == state.generation {
        ReconcileStatus::Unchanged
    } else if state.generation == 0 {
        ReconcileStatus::Built
    } else {
        ReconcileStatus::Updated
    });

    Ok(ReconcileReport {
        status,
        generation,
        selected,
        added,
        changed,
        removed,
        reused,
        hashed,
        reparsed,
        touched,
        diagnostics: coverage,
    })
}

/// Whether `stored` can be trusted for a file that now has `current`.
pub(super) fn fast_path_valid(stored: &StoredSource, current: &FileStamp, window_ns: i64) -> bool {
    if stored.stamp != *current || current.file_id.is_none() {
        return false;
    }
    let Some(mtime) = current.mtime_ns else {
        return false;
    };
    // Racily clean: the file was (or may have been) modified so close to the
    // time it was indexed that an edit could have left the stamp unchanged.
    stored.indexed_at_ns.saturating_sub(mtime) > window_ns
}

/// Open, validate and hash one selected file. The stamp is taken from the open
/// handle before reading, so an edit that lands mid-read leaves a stamp that no
/// longer matches and is picked up by the next pass.
pub(super) fn read_source(
    path: &str,
    canonical_path: &std::path::Path,
    max_bytes: u64,
) -> Result<ReadSource, IndexDiagnostic> {
    let fail = |code: &str, message: String| {
        IndexDiagnostic::new(Some(path), DiagnosticSeverity::Warning, code, message)
    };
    let mut handle = File::open(canonical_path).map_err(|error| {
        fail(
            discovery_codes::UNREADABLE,
            format!("source could not be opened: {error}"),
        )
    })?;
    let metadata = handle.metadata().map_err(|error| {
        fail(
            discovery_codes::UNREADABLE,
            format!("source metadata could not be read: {error}"),
        )
    })?;
    if !metadata.is_file() {
        return Err(fail(
            discovery_codes::NOT_REGULAR,
            "source is no longer a regular file".to_string(),
        ));
    }
    let stamp = FileStamp::from_metadata(&metadata);
    let mut bytes = Vec::new();
    (&mut handle)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            fail(
                discovery_codes::UNREADABLE,
                format!("source could not be read: {error}"),
            )
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(fail(
            discovery_codes::TOO_LARGE,
            format!("source exceeds the {max_bytes} byte per-file limit"),
        ));
    }
    let hash = hex::encode(Sha256::digest(&bytes));
    Ok(ReadSource { stamp, hash, bytes })
}

/// Normalized explicit IDs claimed by successfully parsed documents.
fn collect_ids(batch: &ParsedBatch, ids: &mut BTreeMap<String, String>) {
    for document in &batch.documents {
        if let Some(raw) = document.explicit_id.as_deref()
            && let Ok(normalized) = normalize_explicit_id(raw)
        {
            ids.insert(document.relative_path.clone(), normalized);
        }
    }
}

/// Fingerprint of everything besides file content that decides what the stored
/// records mean: the parsing pipeline, the store layout and the manifest's
/// selection patterns. Host limits are deliberately excluded; they change which
/// files are selected, which reconciliation already diffs.
fn fingerprint(tag: &str, manifest: Option<&KnowledgeManifest>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"qmt-repository-reconcile\0");
    hasher.update(EXTRACTOR_VERSION.to_le_bytes());
    hasher.update(SCHEMA_VERSION.to_le_bytes());
    hasher.update(tag.as_bytes());
    hasher.update([0u8]);
    if let Some(manifest) = manifest {
        for (label, selection) in [("documents", &manifest.documents), ("code", &manifest.code)] {
            hasher.update(label.as_bytes());
            hasher.update([0u8]);
            for pattern in selection.include_patterns() {
                hasher.update(b"+");
                hasher.update(pattern.as_bytes());
                hasher.update([0u8]);
            }
            for pattern in selection.exclude_patterns() {
                hasher.update(b"-");
                hasher.update(pattern.as_bytes());
                hasher.update([0u8]);
            }
        }
    }
    hex::encode(hasher.finalize())
}

pub(super) fn system_now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::root::{RootRequest, resolve_root};
    use crate::knowledge::repository::store::StoredDocument;
    use std::fs;
    use std::path::{Path, PathBuf};

    const MANIFEST: &str = "version = 1\n\
        [documents]\n\
        include = [\"docs/**/*.md\", \".agents/memories/*.md\"]\n\
        [code]\n\
        include = [\"src/**/*.rs\"]\n";

    fn opts() -> ReconcileOptions {
        ReconcileOptions::from(&RepositoryKnowledgeSettings::default())
    }

    fn ago(secs: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(secs)
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        root: ResolvedRoot,
        store: RepositoryStore,
    }

    impl Fx {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            fs::create_dir_all(repo.join(".git")).unwrap();
            let root = resolve_root(&RootRequest {
                cwd: Some(&repo),
                ..Default::default()
            })
            .unwrap();
            let store =
                RepositoryStore::open(&tmp.path().join("cache/knowledge/k/index.sqlite")).unwrap();
            let fx = Self {
                _tmp: tmp,
                repo,
                root,
                store,
            };
            fx.put(".qmt/knowledge.toml", MANIFEST);
            fx
        }

        /// Write a file, leaving a fresh mtime (inside the racy window).
        fn put(&self, rel: &str, content: &str) {
            let path = self.repo.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        /// Write a file with an hour-old mtime, safely outside the racy window.
        fn put_old(&self, rel: &str, content: &str) {
            self.put(rel, content);
            set_mtime(&self.repo.join(rel), ago(3600));
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.repo.join(rel)
        }

        fn rec(&self) -> ReconcileReport {
            self.rec_with(&opts())
        }

        fn rec_with(&self, options: &ReconcileOptions) -> ReconcileReport {
            reconcile(&self.root, &self.store, options).unwrap()
        }

        fn doc(&self, path: &str) -> Option<StoredDocument> {
            self.store.document(path).unwrap().1
        }

        fn source(&self, path: &str) -> Option<StoredSource> {
            self.store.source(path).unwrap().1
        }

        fn documents(&self) -> usize {
            self.store.counts().unwrap().documents
        }
    }

    fn standard() -> Fx {
        let fx = Fx::new();
        fx.put_old("docs/a.md", "# A\nalpha\n");
        fx.put_old("docs/b.md", "# B\nbravo\n");
        fx.put_old("src/lib.rs", "pub fn f() {}\n");
        fx
    }

    fn codes(report: &ReconcileReport) -> Vec<&str> {
        report.diagnostics.iter().map(|d| d.code.as_str()).collect()
    }

    #[test]
    fn cold_build_indexes_documents_and_code_paths() {
        let fx = standard();
        let report = fx.rec();
        assert_eq!(report.status, ReconcileStatus::Built);
        assert_eq!(report.selected, 3);
        assert_eq!((report.added, report.changed, report.removed), (3, 0, 0));
        assert_eq!((report.hashed, report.reparsed, report.reused), (3, 2, 0));
        assert!(report.is_complete());
        assert_eq!(fx.documents(), 2);
        assert_eq!(fx.source("src/lib.rs").unwrap().state, SourceState::Code);
        assert!(
            fx.doc("docs/a.md").unwrap().sections[0]
                .body
                .contains("alpha")
        );
        assert!(fx.store.is_built().unwrap());
    }

    #[test]
    fn warm_reconcile_opens_no_content_and_commits_nothing() {
        let fx = standard();
        let first = fx.rec();
        let second = fx.rec();
        assert_eq!(second.status, ReconcileStatus::Unchanged);
        assert_eq!(second.generation, first.generation);
        assert_eq!((second.hashed, second.reparsed, second.touched), (0, 0, 0));
        assert_eq!(second.reused, 3);

        // Even with content unreadable, the fast path never opens it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let a = fx.path("docs/a.md");
            fs::set_permissions(&a, fs::Permissions::from_mode(0o000)).unwrap();
            let third = fx.rec();
            fs::set_permissions(&a, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(third.status, ReconcileStatus::Unchanged);
            assert_eq!(third.hashed, 0);
            assert!(third.is_complete());
        }
    }

    #[test]
    fn same_size_edit_in_the_racy_window_is_detected() {
        let fx = Fx::new();
        fx.put("docs/a.md", "# A\nalpha\n");
        fx.rec();
        let before = fx.source("docs/a.md").unwrap();

        // Same size, same mtime, same inode: only the racy rule can see this.
        let path = fx.path("docs/a.md");
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "# A\nbeta!\n").unwrap();
        set_mtime(&path, mtime);
        assert_eq!(fs::metadata(&path).unwrap().len(), before.stamp.size);

        let report = fx.rec();
        assert_eq!(report.status, ReconcileStatus::Updated);
        assert_eq!((report.hashed, report.changed), (1, 1));
        assert!(
            fx.doc("docs/a.md").unwrap().sections[0]
                .body
                .contains("beta!")
        );
    }

    #[test]
    fn same_size_edit_outside_the_window_needs_forced_hashing() {
        let fx = standard();
        fx.rec();
        let path = fx.path("docs/a.md");
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "# A\nbeta!\n").unwrap();
        set_mtime(&path, mtime);

        // The metadata rule trusts an identical stamp that is not racy...
        let trusted = fx.rec();
        assert_eq!(trusted.status, ReconcileStatus::Unchanged);
        assert_eq!(trusted.hashed, 0);
        assert!(
            fx.doc("docs/a.md").unwrap().sections[0]
                .body
                .contains("alpha")
        );

        // ...and the host's force-hash option is the escape hatch.
        let forced = fx.rec_with(&ReconcileOptions {
            force_hash: true,
            ..opts()
        });
        assert_eq!(forced.status, ReconcileStatus::Updated);
        assert_eq!(forced.hashed, 3);
        assert_eq!(forced.reparsed, 1);
        assert!(
            fx.doc("docs/a.md").unwrap().sections[0]
                .body
                .contains("beta!")
        );
    }

    #[test]
    fn forced_hashing_reads_everything_but_reparses_nothing_unchanged() {
        let fx = standard();
        let first = fx.rec();
        let forced = fx.rec_with(&ReconcileOptions {
            force_hash: true,
            ..opts()
        });
        assert_eq!(forced.status, ReconcileStatus::Unchanged);
        assert_eq!(forced.generation, first.generation);
        assert_eq!((forced.hashed, forced.reparsed, forced.touched), (3, 0, 3));
    }

    #[test]
    fn mtime_only_change_reuses_the_record_without_a_new_generation() {
        let fx = standard();
        let first = fx.rec();
        let path = fx.path("docs/a.md");
        let old = fx.source("docs/a.md").unwrap();

        set_mtime(&path, ago(1800));
        let report = fx.rec();
        assert_eq!(report.status, ReconcileStatus::Unchanged);
        assert_eq!(report.generation, first.generation);
        assert_eq!((report.hashed, report.reparsed, report.touched), (1, 0, 1));
        let new = fx.source("docs/a.md").unwrap();
        assert_ne!(new.stamp.mtime_ns, old.stamp.mtime_ns);
        assert_eq!(new.content_hash, old.content_hash);

        // The refreshed stamp makes the next pass a pure fast path.
        let warm = fx.rec();
        assert_eq!((warm.hashed, warm.touched, warm.reused), (0, 0, 3));
    }

    #[test]
    fn external_edits_additions_and_deletions_reconcile_without_events() {
        let fx = standard();
        let first = fx.rec();

        fx.put_old("docs/a.md", "# A\nalpha changed\n");
        fx.put_old("docs/new.md", "# New\nnovel\n");
        fs::remove_file(fx.path("docs/b.md")).unwrap();
        fx.put_old("src/extra.rs", "pub fn g() {}\n");

        let report = fx.rec();
        assert_eq!(report.status, ReconcileStatus::Updated);
        assert!(report.generation > first.generation);
        assert_eq!((report.added, report.changed, report.removed), (2, 1, 1));
        assert_eq!(report.reused, 1);
        assert!(fx.doc("docs/b.md").is_none());
        assert!(fx.source("docs/b.md").is_none());
        assert!(fx.doc("docs/new.md").is_some());
        assert!(
            fx.doc("docs/a.md").unwrap().sections[0]
                .body
                .contains("changed")
        );
        assert!(fx.source("src/extra.rs").is_some());
        fx.store.check_fts_integrity().unwrap();
        assert!(fx.store.fts_keys("novel").len() == 1);
        assert!(fx.store.fts_keys("bravo").is_empty());
    }

    #[test]
    fn manifest_changes_reconcile_the_selection() {
        let fx = standard();
        fx.rec();

        fx.put(
            ".qmt/knowledge.toml",
            "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\nexclude = [\"docs/b.md\"]\n",
        );
        let report = fx.rec();
        assert_eq!(report.status, ReconcileStatus::Updated);
        assert_eq!(report.removed, 2); // docs/b.md and src/lib.rs
        assert_eq!(report.hashed, 1, "a changed manifest re-reads what remains");
        assert!(fx.doc("docs/b.md").is_none());
        assert!(fx.source("src/lib.rs").is_none());
        assert!(fx.doc("docs/a.md").is_some());

        // Back to the original selection restores everything.
        fx.put(".qmt/knowledge.toml", MANIFEST);
        let restored = fx.rec();
        assert_eq!(restored.status, ReconcileStatus::Updated);
        assert_eq!(restored.added, 2);
        assert_eq!(fx.documents(), 2);
    }

    #[test]
    fn missing_or_invalid_manifest_empties_the_corpus_with_a_diagnostic() {
        let fx = standard();
        fx.rec();

        fs::remove_file(fx.path(".qmt/knowledge.toml")).unwrap();
        let none = fx.rec();
        assert_eq!(none.status, ReconcileStatus::NotConfigured);
        assert_eq!(codes(&none), vec!["not_configured"]);
        assert_eq!(fx.documents(), 0);
        assert_eq!(fx.store.counts().unwrap().sources, 0);
        assert!(fx.store.is_built().unwrap());
        // Stable: asking again changes nothing.
        let again = fx.rec();
        assert_eq!(again.generation, none.generation);

        fx.put(".qmt/knowledge.toml", "version = 2\n");
        let bad = fx.rec();
        assert_eq!(bad.status, ReconcileStatus::ManifestInvalid);
        assert_eq!(codes(&bad), vec!["manifest_unsupported_version"]);
        assert_eq!(fx.documents(), 0);

        fx.put(".qmt/knowledge.toml", MANIFEST);
        let fixed = fx.rec();
        assert_eq!(fixed.status, ReconcileStatus::Updated);
        assert!(fixed.is_complete());
        assert_eq!(fx.documents(), 2);
    }

    #[test]
    fn malformed_and_disabled_sources_are_isolated_and_recover() {
        let fx = Fx::new();
        fx.put_old("docs/ok.md", "# Fine\nusable\n");
        fx.put_old("docs/broken.md", "---\ntitle: never closed\n");
        fx.put_old(
            ".agents/memories/off.md",
            "---\nenabled: false\n---\nbody\n",
        );
        let report = fx.rec();
        assert!(report.is_complete(), "exclusions are not coverage gaps");

        assert!(fx.doc("docs/ok.md").is_some());
        let broken = fx.source("docs/broken.md").unwrap();
        assert_eq!(broken.state, SourceState::Excluded);
        assert_eq!(broken.diagnostics[0].code, "frontmatter_malformed");
        assert!(fx.doc("docs/broken.md").is_none());
        let off = fx.source(".agents/memories/off.md").unwrap();
        assert_eq!(off.state, SourceState::Excluded);
        assert_eq!(off.diagnostics[0].code, "memory_disabled");

        // Unchanged exclusions are trusted from metadata, not re-parsed.
        let warm = fx.rec();
        assert_eq!((warm.hashed, warm.reparsed), (0, 0));

        fx.put_old("docs/broken.md", "# Fixed\nnow valid\n");
        fx.put_old(".agents/memories/off.md", "---\nenabled: true\n---\nbody\n");
        let fixed = fx.rec();
        assert_eq!(fixed.changed, 2);
        assert_eq!(fx.documents(), 3);
        assert_eq!(
            fx.source("docs/broken.md").unwrap().state,
            SourceState::Parsed
        );
        assert!(fx.source("docs/broken.md").unwrap().diagnostics.is_empty());
    }

    #[test]
    fn duplicate_ids_are_resolved_across_changed_and_unchanged_sources() {
        let fx = Fx::new();
        fx.put_old("docs/a.md", "---\nid: shared\n---\n# A\n");
        fx.put_old("docs/b.md", "# B\n");
        fx.rec();
        assert!(fx.doc("docs/a.md").is_some());

        // b claims a's ID: a did not change, yet both must be excluded.
        fx.put_old("docs/b.md", "---\nid: shared\n---\n# B\n");
        let clash = fx.rec();
        assert_eq!(clash.hashed, 2, "the unchanged claimant is re-read");
        assert!(fx.doc("docs/a.md").is_none());
        assert!(fx.doc("docs/b.md").is_none());
        let a = fx.source("docs/a.md").unwrap();
        assert_eq!(a.state, SourceState::Excluded);
        assert_eq!(a.explicit_id.as_deref(), Some("shared"));
        assert_eq!(a.diagnostics[0].code, "document_id_duplicate");

        // b moves to a different ID: a is restored though its bytes never changed.
        fx.put_old("docs/b.md", "---\nid: other\n---\n# B\n");
        fx.rec();
        assert!(fx.doc("docs/a.md").is_some());
        assert!(fx.doc("docs/b.md").is_some());
        assert!(fx.source("docs/a.md").unwrap().diagnostics.is_empty());

        // Deleting a claimant releases the ID for the other.
        fx.put_old("docs/b.md", "---\nid: shared\n---\n# B\n");
        fx.rec();
        assert_eq!(fx.documents(), 0);
        fs::remove_file(fx.path("docs/b.md")).unwrap();
        fx.rec();
        assert!(fx.doc("docs/a.md").is_some());

        // A warm pass afterwards is again free.
        let warm = fx.rec();
        assert_eq!((warm.hashed, warm.reparsed), (0, 0));
    }

    #[test]
    fn branch_switch_converges_to_the_same_corpus_as_a_fresh_build() {
        let switched = standard();
        switched.put_old("docs/c.md", "# C\ncharlie\n");
        switched.rec();

        // "Check out" another branch: edits, a rewrite of identical content with
        // a new mtime, a deletion and additions.
        switched.put_old("docs/a.md", "# A\nalpha on the other branch\n");
        switched.put_old("docs/b.md", "# B\nbravo\n");
        set_mtime(&switched.path("docs/b.md"), ago(900));
        fs::remove_file(switched.path("docs/c.md")).unwrap();
        switched.put_old("docs/d.md", "# D\ndelta\n");
        switched.put_old("src/lib.rs", "pub fn changed() {}\n");
        switched.put_old("src/new.rs", "pub fn n() {}\n");
        let report = switched.rec();
        assert_eq!(report.reparsed, 2, "only a.md and d.md are parsed again");
        assert_eq!(report.touched, 1, "b.md only needs its stamp refreshed");
        assert_eq!(report.removed, 1);

        let fresh = Fx::new();
        fresh.put_old("docs/a.md", "# A\nalpha on the other branch\n");
        fresh.put_old("docs/b.md", "# B\nbravo\n");
        fresh.put_old("docs/d.md", "# D\ndelta\n");
        fresh.put_old("src/lib.rs", "pub fn changed() {}\n");
        fresh.put_old("src/new.rs", "pub fn n() {}\n");
        fresh.rec();

        assert_eq!(
            switched.store.export_logical().unwrap().1,
            fresh.store.export_logical().unwrap().1
        );
        let paths = |fx: &Fx| -> Vec<String> {
            fx.store
                .sources()
                .unwrap()
                .1
                .into_iter()
                .map(|s| s.path)
                .collect()
        };
        assert_eq!(paths(&switched), paths(&fresh));
    }

    #[test]
    fn a_source_that_outgrows_its_limit_is_dropped_not_served_stale() {
        let fx = Fx::new();
        let mut options = opts();
        options.discovery.max_file_bytes = 64;
        fx.put_old("docs/big.md", "# Big\nsmall\n");
        fx.put_old("docs/ok.md", "# Ok\n");
        assert!(fx.rec_with(&options).is_complete());
        assert!(fx.doc("docs/big.md").is_some());

        fx.put_old("docs/big.md", &format!("# Big\n{}\n", "x".repeat(200)));
        let report = fx.rec_with(&options);
        assert_eq!(report.status, ReconcileStatus::Updated);
        assert_eq!(report.removed, 1);
        assert_eq!(codes(&report), vec!["source_too_large"]);
        assert_eq!(report.diagnostics[0].path.as_deref(), Some("docs/big.md"));
        assert!(fx.doc("docs/big.md").is_none());
        assert!(fx.doc("docs/ok.md").is_some());
        assert_eq!(fx.store.coverage().unwrap().1, report.diagnostics);

        // Shrinking it again recovers and clears the coverage gap.
        fx.put_old("docs/big.md", "# Big\nsmall again\n");
        let back = fx.rec_with(&options);
        assert!(back.is_complete());
        assert!(fx.doc("docs/big.md").is_some());
    }

    #[test]
    fn reconcile_never_writes_into_the_repository() {
        let fx = standard();
        fx.put_old(".agents/memories/m.md", "# Memory\nremember\n");
        let snapshot = |dir: &Path| -> Vec<(PathBuf, u64)> {
            let mut out = Vec::new();
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                for entry in fs::read_dir(d).unwrap() {
                    let entry = entry.unwrap();
                    let meta = entry.metadata().unwrap();
                    if meta.is_dir() {
                        stack.push(entry.path());
                    } else {
                        out.push((entry.path(), meta.len()));
                    }
                }
            }
            out.sort();
            out
        };
        let before = snapshot(&fx.repo);
        fx.rec();
        fx.rec();
        assert_eq!(before, snapshot(&fx.repo));
    }

    // ── code extraction cache ────────────────────────────────────────────

    fn symbols_of(fx: &Fx, path: &str) -> Vec<String> {
        fx.store
            .code_record(path)
            .unwrap()
            .1
            .map(|(_, record)| {
                record
                    .symbols
                    .into_iter()
                    .map(|symbol| symbol.qualified_name)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn code_files_get_symbol_records_on_the_first_build() {
        let fx = standard();
        fx.rec();
        assert_eq!(symbols_of(&fx, "src/lib.rs"), ["f"]);
        let counts = fx.store.counts().unwrap();
        assert_eq!((counts.symbols, counts.partial_code_files), (1, 0));
        assert_eq!(
            fx.source("src/lib.rs").unwrap().code_version,
            Some(CODE_EXTRACTOR_VERSION)
        );
    }

    #[test]
    fn only_the_changed_file_is_re_extracted() {
        let fx = standard();
        fx.put_old("src/other.rs", "pub fn other() {}\n");
        fx.rec();
        let before = fx.source("src/other.rs").unwrap();

        fx.put_old("src/lib.rs", "pub fn f() {}\npub fn g() {}\n");
        let report = fx.rec();
        assert_eq!(report.status, ReconcileStatus::Updated);
        assert_eq!((report.changed, report.added), (1, 0));
        assert_eq!(
            report.reused, 3,
            "unchanged files are trusted from metadata"
        );
        assert_eq!(symbols_of(&fx, "src/lib.rs"), ["f", "g"]);
        assert_eq!(fx.source("src/other.rs").unwrap(), before);
        assert_eq!(symbols_of(&fx, "src/other.rs"), ["other"]);
    }

    #[test]
    fn removed_symbols_and_deleted_files_leave_active_navigation() {
        let fx = standard();
        fx.put_old("src/lib.rs", "pub fn f() {}\npub fn gone() {}\n");
        fx.rec();
        assert_eq!(symbols_of(&fx, "src/lib.rs"), ["f", "gone"]);

        fx.put_old("src/lib.rs", "pub fn f() {}\n");
        fx.rec();
        assert_eq!(symbols_of(&fx, "src/lib.rs"), ["f"]);

        fs::remove_file(fx.path("src/lib.rs")).unwrap();
        let report = fx.rec();
        assert_eq!(report.removed, 1);
        assert!(fx.store.code_record("src/lib.rs").unwrap().1.is_none());
        assert_eq!(fx.store.counts().unwrap().symbols, 0);
    }

    #[test]
    fn a_new_extractor_format_re_extracts_code_but_not_documents() {
        let fx = standard();
        let first = fx.rec();
        assert_eq!(fx.rec().status, ReconcileStatus::Unchanged);

        fx.store.invalidate_code_versions();
        let report = fx.rec();
        assert_eq!(report.status, ReconcileStatus::Updated);
        assert_eq!(report.changed, 1, "only the code file is rebuilt");
        assert_eq!(report.reparsed, 0, "documents are not re-parsed");
        assert!(report.generation > first.generation);
        assert_eq!(
            fx.source("src/lib.rs").unwrap().code_version,
            Some(CODE_EXTRACTOR_VERSION)
        );
        assert_eq!(symbols_of(&fx, "src/lib.rs"), ["f"]);
        assert_eq!(fx.rec().status, ReconcileStatus::Unchanged);
    }

    #[test]
    fn unsupported_and_unparseable_sources_stay_visible_with_diagnostics() {
        let fx = Fx::new();
        fx.put(
            ".qmt/knowledge.toml",
            "version = 1\n[code]\ninclude = [\"src/**/*.rs\", \"data/*.xyz\"]\n",
        );
        fx.put_old("src/ok.rs", "pub fn ok() {}\n");
        fx.put_old("data/blob.xyz", "opaque");
        fs::write(fx.path("src/bad.rs"), [0xff, 0xfe, 0x00]).unwrap();
        set_mtime(&fx.path("src/bad.rs"), ago(3600));

        let report = fx.rec();
        assert_eq!(report.selected, 3);
        assert!(
            report.is_complete(),
            "coverage is about selection, not symbols"
        );

        let counts = fx.store.counts().unwrap();
        assert_eq!(counts.partial_code_files, 2);
        assert_eq!(symbols_of(&fx, "src/ok.rs"), ["ok"]);
        assert!(symbols_of(&fx, "data/blob.xyz").is_empty());

        let blob = fx.source("data/blob.xyz").unwrap();
        assert_eq!(blob.diagnostics.len(), 1);
        assert_eq!(blob.diagnostics[0].code, "code_unsupported_language");
        let bad = fx.source("src/bad.rs").unwrap();
        assert_eq!(bad.diagnostics[0].code, "code_not_utf8");
        assert!(bad.is_code, "a failed file keeps its path-level record");

        let (_, all) = fx.store.index_diagnostics().unwrap();
        assert_eq!(all.len(), 2);
        // Diagnostics are part of the cached record: a warm pass changes nothing.
        assert_eq!(fx.rec().status, ReconcileStatus::Unchanged);
    }

    #[test]
    fn purpose_rows_are_indexed_with_their_document_and_replaced_with_it() {
        let fx = standard();
        fx.put_old(
            "docs/map.md",
            "# Map\n\n| File | Purpose |\n|--|--|\n| `../src/lib.rs` | Crate root. |\n",
        );
        fx.rec();
        let (_, found) = fx.store.purpose("purpose:docs/map.md#src/lib.rs").unwrap();
        let found = found.unwrap();
        assert_eq!(found.row.purpose, "Crate root.");
        assert_eq!(found.row.line, 5);

        fx.put_old("docs/map.md", "# Map\nno table any more\n");
        fx.rec();
        assert!(
            fx.store
                .purpose("purpose:docs/map.md#src/lib.rs")
                .unwrap()
                .1
                .is_none()
        );
        assert_eq!(fx.store.counts().unwrap().purposes, 0);
    }
}

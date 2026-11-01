//! Evidence: hash-bound acknowledgements of authored claims.
//!
//! *Index* freshness asks whether indexed bytes match the file. *Evidence*
//! freshness asks something different: does the source an authored statement
//! relies on still match what a person last **explicitly acknowledged**? Equal
//! hashes never prove a statement true, and lifecycle `status` never changes an
//! evidence state.
//!
//! # Records
//!
//! A document is a set of acknowledgeable *records*:
//!
//! * every **section** (reviewed text: its body; dependencies: the document's
//!   `knowledge_sources`), and
//! * every **File/Purpose row** (reviewed text: the purpose; dependency: the
//!   file the row names).
//!
//! Markdown links and URLs are never dependencies, and neither is anything
//! `knowledge_paths` matches.
//!
//! # Files
//!
//! One Git-trackable file per acknowledged document, mirroring the document's
//! path: `.qmt/knowledge/evidence/<document-path>.json`. Branches that
//! acknowledge different notes therefore touch different files. Serialization
//! is deterministic (sorted keys, two-space indent, trailing newline) and holds
//! only repository-relative identities and hashes: no absolute paths, session
//! IDs, timestamps or private text.
//!
//! ```json
//! {
//!   "document": "docs/arch.md",
//!   "entries": {
//!     "section:[\"Architecture\"]": {
//!       "dependencies": { "src/lib.rs": "sha256-v1:…" },
//!       "text": "sha256-v1:…"
//!     }
//!   },
//!   "version": 1
//! }
//! ```
//!
//! # Hash normalization (`sha256-v1`)
//!
//! Text hashes are taken after stripping a UTF-8 byte-order mark, converting
//! CRLF and lone CR to LF, and removing trailing whitespace from every line.
//! The normalization is part of the hash version, so a CRLF checkout or an
//! editor that trims whitespace does not cause false drift. Symbol dependencies
//! (`path#qualified::name`) use the extractor's own digest, prefixed
//! `symbol-v1:`.
//!
//! # States
//!
//! | state        | meaning                                                              |
//! |--------------|----------------------------------------------------------------------|
//! | `unverified` | no acknowledgement, or the note or its dependency set changed since  |
//! | `unchanged`  | note and every dependency match the acknowledgement                  |
//! | `changed`    | a dependency's bytes differ from the acknowledgement                 |
//! | `missing`    | a dependency is gone, or its selector can no longer be resolved      |
//!
//! A document reports its most alarming record: `missing`, then `changed`, then
//! `unverified`; it is `unchanged` only when every record is.
//!
//! Nothing here writes. Acknowledging is the explicit, previewed operation in
//! [`acknowledge`](super::acknowledge).

use super::code::{CodeStatus, extract_code};
use super::export::{confine_target, read_target};
use super::maintenance::{Finding, MaintenanceError};
use super::query::normalize_relative_path;
use super::service::{RepositoryService, ServiceError};
use super::signals::EvidenceState;
use super::store::{CorpusEntry, StoredSection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Version of the evidence file format.
pub const EVIDENCE_VERSION: u32 = 1;
/// Directory (relative to the knowledge root) holding evidence files.
pub const EVIDENCE_DIR: &str = ".qmt/knowledge/evidence";
/// Prefix of normalized-text hashes; names the normalization.
pub const TEXT_HASH_PREFIX: &str = "sha256-v1:";
/// Prefix of symbol hashes; the extractor's digest follows.
pub const SYMBOL_HASH_PREFIX: &str = "symbol-v1:";

/// Largest dependency file that is read to hash.
const MAX_DEPENDENCY_BYTES: u64 = 8 * 1024 * 1024;
/// Most evidence files listed when looking for orphans.
const MAX_EVIDENCE_FILES: usize = 50_000;

pub mod codes {
    pub const FILE_INVALID: &str = "evidence_file_invalid";
    pub const VERSION_UNSUPPORTED: &str = "evidence_version_unsupported";
}

// ─── Hashing ─────────────────────────────────────────────────────────────────

/// Strip a BOM, unify line endings to LF and drop trailing whitespace on each
/// line.
pub fn normalize_text(input: &str) -> String {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let unified: Cow<'_, str> = if input.contains('\r') {
        Cow::Owned(input.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(input)
    };
    let mut out = String::with_capacity(unified.len());
    for (index, line) in unified.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    out
}

/// The versioned hash of `text` after [`normalize_text`].
pub fn text_hash(text: &str) -> String {
    let digest = Sha256::digest(normalize_text(text).as_bytes());
    format!("{TEXT_HASH_PREFIX}{}", hex::encode(digest))
}

// ─── Dependencies ────────────────────────────────────────────────────────────

/// Split `path` or `path#qualified::symbol`, confining the path.
///
/// # Errors
///
/// A static reason when the path could leave the checkout or the symbol is
/// empty.
pub fn parse_selector(selector: &str) -> Result<(String, Option<String>), &'static str> {
    let (path, symbol) = match selector.split_once('#') {
        Some((path, symbol)) => (path, Some(symbol.trim())),
        None => (selector, None),
    };
    let path = normalize_relative_path(path)?;
    match symbol {
        Some("") => Err("symbol selector is empty"),
        Some(symbol) => Ok((path, Some(symbol.to_owned()))),
        None => Ok((path, None)),
    }
}

/// What a dependency selector resolves to right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dependency {
    /// The versioned hash of the current bytes or symbol.
    Hash(String),
    /// The file, or the symbol within it, no longer exists.
    Missing,
    /// The selector cannot be resolved safely or unambiguously. Never guessed.
    Unresolved(String),
}

/// A repository file read for hashing.
pub(super) enum SourceBytes {
    Bytes(Vec<u8>),
    Missing,
    Unreadable(&'static str),
}

/// Read one regular file confined to `root`. Symlinks are followed only if they
/// stay inside the root.
pub(super) fn read_source_file(root: &Path, relative: &str) -> SourceBytes {
    let canonical = match root.join(relative).canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return SourceBytes::Missing,
        Err(_) => return SourceBytes::Unreadable("cannot be resolved"),
    };
    if !canonical.starts_with(root) {
        return SourceBytes::Unreadable("resolves outside the repository");
    }
    let Ok(metadata) = fs::metadata(&canonical) else {
        return SourceBytes::Missing;
    };
    if !metadata.is_file() {
        return SourceBytes::Unreadable("is not a regular file");
    }
    if metadata.len() > MAX_DEPENDENCY_BYTES {
        return SourceBytes::Unreadable("is too large to hash");
    }
    match fs::read(&canonical) {
        Ok(bytes) => SourceBytes::Bytes(bytes),
        Err(_) => SourceBytes::Unreadable("cannot be read"),
    }
}

/// Resolve one `knowledge_sources` selector against the checkout.
pub fn resolve_dependency(root: &Path, selector: &str) -> Dependency {
    let (path, symbol) = match parse_selector(selector) {
        Ok(parsed) => parsed,
        Err(reason) => return Dependency::Unresolved(format!("unsafe selector: {reason}")),
    };
    let bytes = match read_source_file(root, &path) {
        SourceBytes::Bytes(bytes) => bytes,
        SourceBytes::Missing => return Dependency::Missing,
        SourceBytes::Unreadable(reason) => return Dependency::Unresolved(reason.to_owned()),
    };
    let Some(symbol) = symbol else {
        return match std::str::from_utf8(&bytes) {
            Ok(text) => Dependency::Hash(text_hash(text)),
            Err(_) => Dependency::Unresolved("is not UTF-8 text".to_owned()),
        };
    };
    let extraction = extract_code(&path, &bytes);
    if extraction.record.status != CodeStatus::Extracted {
        return Dependency::Unresolved("has no symbol extractor for this file".to_owned());
    }
    let mut found = extraction
        .record
        .symbols
        .iter()
        .filter(|candidate| candidate.qualified_name == symbol);
    match (found.next(), found.next()) {
        (Some(only), None) => Dependency::Hash(format!("{SYMBOL_HASH_PREFIX}{}", only.digest)),
        (None, _) => Dependency::Missing,
        (Some(_), Some(_)) => Dependency::Unresolved("symbol selector is ambiguous".to_owned()),
    }
}

/// Memoizes [`resolve_dependency`] within one pass, since many records share
/// the same dependency.
#[derive(Debug, Default)]
pub struct DependencyCache(BTreeMap<String, Dependency>);

impl DependencyCache {
    pub fn resolve(&mut self, root: &Path, selector: &str) -> &Dependency {
        self.0
            .entry(selector.to_owned())
            .or_insert_with(|| resolve_dependency(root, selector))
    }
}

// ─── Files ───────────────────────────────────────────────────────────────────

/// What was acknowledged for one record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceEntry {
    /// Selector to versioned hash.
    pub dependencies: BTreeMap<String, String>,
    /// Versioned hash of the reviewed text.
    pub text: String,
}

/// The acknowledgements of one document. Fields are declared in key order so
/// serialization is sorted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceFile {
    pub document: String,
    pub entries: BTreeMap<String, EvidenceEntry>,
    pub version: u32,
}

impl EvidenceFile {
    pub fn new(document: &str) -> Self {
        Self {
            document: document.to_owned(),
            entries: BTreeMap::new(),
            version: EVIDENCE_VERSION,
        }
    }

    /// Deterministic text: sorted keys, two-space indent, trailing newline.
    pub fn to_text(&self) -> String {
        let mut text = serde_json::to_string_pretty(self)
            .expect("an evidence file holds only strings and string maps");
        text.push('\n');
        text
    }
}

/// The evidence file path for a document path.
pub fn evidence_path(document: &str) -> String {
    format!("{EVIDENCE_DIR}/{document}.json")
}

/// The result of reading a document's evidence file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceLoad {
    Absent,
    Loaded(EvidenceFile),
    Invalid { code: &'static str, reason: String },
}

/// Read and validate a document's evidence file.
pub fn load_evidence(root: &Path, document: &str) -> EvidenceLoad {
    let invalid = |code, reason: String| EvidenceLoad::Invalid { code, reason };
    let path = evidence_path(document);
    let bytes = match confine_target(root, &path)
        .and_then(|(relative, absolute)| read_target(&absolute, &relative))
    {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return EvidenceLoad::Absent,
        Err(error) => return invalid(codes::FILE_INVALID, error.to_string()),
    };
    parse_evidence(&bytes, document)
}

fn parse_evidence(bytes: &[u8], document: &str) -> EvidenceLoad {
    let invalid = |code, reason: String| EvidenceLoad::Invalid { code, reason };
    let value: Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(error) => return invalid(codes::FILE_INVALID, format!("not valid JSON: {error}")),
    };
    match value.get("version").and_then(Value::as_u64) {
        Some(version) if version == u64::from(EVIDENCE_VERSION) => {}
        Some(version) => {
            return invalid(
                codes::VERSION_UNSUPPORTED,
                format!("format version {version} is not supported (expected {EVIDENCE_VERSION})"),
            );
        }
        None => return invalid(codes::FILE_INVALID, "no numeric `version`".to_owned()),
    }
    match serde_json::from_value::<EvidenceFile>(value) {
        Ok(file) if file.document == document => EvidenceLoad::Loaded(file),
        Ok(file) => invalid(
            codes::FILE_INVALID,
            format!("names document `{}`, not `{document}`", file.document),
        ),
        Err(error) => invalid(codes::FILE_INVALID, error.to_string()),
    }
}

/// Documents that have an evidence file, sorted, without following symlinks.
/// Entries that cannot be listed are reported rather than skipped silently.
pub fn list_evidence_documents(root: &Path) -> (Vec<String>, Vec<Finding>) {
    use super::document::DiagnosticSeverity::Warning;

    let mut documents = Vec::new();
    let mut findings = Vec::new();
    // No component of the evidence directory may be a symlink.
    let mut base = root.to_path_buf();
    for segment in EVIDENCE_DIR.split('/') {
        base.push(segment);
        match fs::symlink_metadata(&base) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                findings.push(Finding::new(
                    Some(EVIDENCE_DIR),
                    codes::FILE_INVALID,
                    Warning,
                    "evidence location is not a plain directory and was not read",
                ));
                return (documents, findings);
            }
            Err(_) => return (documents, findings),
        }
    }

    let mut stack: Vec<(PathBuf, String)> = vec![(base, String::new())];
    while let Some((directory, prefix)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            findings.push(Finding::new(
                Some(&format!("{EVIDENCE_DIR}/{prefix}")),
                codes::FILE_INVALID,
                Warning,
                "directory could not be listed",
            ));
            continue;
        };
        for entry in entries.flatten() {
            let (Some(name), Ok(kind)) = (
                entry.file_name().to_str().map(str::to_owned),
                entry.file_type(),
            ) else {
                findings.push(Finding::new(
                    Some(EVIDENCE_DIR),
                    codes::FILE_INVALID,
                    Warning,
                    "an entry has a non-UTF-8 name or unreadable type and was skipped",
                ));
                continue;
            };
            let relative = format!("{prefix}{name}");
            if kind.is_dir() {
                stack.push((entry.path(), format!("{relative}/")));
            } else if kind.is_file() {
                if let Some(document) = relative.strip_suffix(".json")
                    && documents.len() < MAX_EVIDENCE_FILES
                {
                    documents.push(document.to_owned());
                }
            } else {
                findings.push(Finding::new(
                    Some(&format!("{EVIDENCE_DIR}/{relative}")),
                    codes::FILE_INVALID,
                    Warning,
                    "symlinks and special files are not followed",
                ));
            }
        }
    }
    documents.sort();
    (documents, findings)
}

// ─── Records ─────────────────────────────────────────────────────────────────

/// One acknowledgeable unit of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordSpec {
    /// Document-local, move-stable key: `section:["A","B"]~1` or `row:file~1`.
    pub key: String,
    /// 1-based line in the document where the record starts.
    pub line: usize,
    /// The text a reviewer vouches for.
    pub text: String,
    /// Sorted, distinct dependency selectors.
    pub selectors: Vec<String>,
}

/// Move-stable key of a section: its heading breadcrumb and occurrence.
pub fn section_record_key(section: &StoredSection) -> String {
    let crumbs = serde_json::to_string(&section.breadcrumb).unwrap_or_default();
    let mut key = format!("section:{crumbs}");
    if section.occurrence > 0 {
        key.push_str(&format!("~{}", section.occurrence));
    }
    key
}

/// Key of a File/Purpose row.
pub fn row_record_key(target: &str, occurrence: u32) -> String {
    let mut key = format!("row:{target}");
    if occurrence > 0 {
        key.push_str(&format!("~{occurrence}"));
    }
    key
}

/// Every acknowledgeable record of `entry`, in document order.
pub fn record_specs(entry: &CorpusEntry) -> Vec<RecordSpec> {
    let sources: Vec<String> = entry
        .document
        .knowledge_sources
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let sections = entry.document.sections.iter().map(|section| RecordSpec {
        key: section_record_key(section),
        line: section.start_line,
        text: section.body.clone(),
        selectors: sources.clone(),
    });
    let rows = entry.purposes.iter().map(|row| RecordSpec {
        key: row_record_key(&row.target_path, row.occurrence),
        line: row.line,
        text: row.purpose.clone(),
        selectors: vec![row.target_path.clone()],
    });
    sections.chain(rows).collect()
}

// ─── Evaluation ──────────────────────────────────────────────────────────────

/// How one dependency compares with its acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyOutcome {
    Unchanged,
    Changed,
    Missing,
    Unresolved(String),
    /// Declared now but not part of the acknowledgement.
    Unacknowledged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyStatus {
    pub selector: String,
    pub outcome: DependencyOutcome,
}

/// The evidence state of one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordEvidence {
    pub key: String,
    pub line: usize,
    /// An acknowledgement exists for this key (whether or not it still holds).
    pub acknowledged: bool,
    pub state: EvidenceState,
    pub dependencies: Vec<DependencyStatus>,
}

/// The evidence state of a document and of each of its records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentEvidence {
    pub path: String,
    /// An evidence file exists and was readable.
    pub has_file: bool,
    /// Why an existing evidence file could not be used.
    pub file_problem: Option<(&'static str, String)>,
    pub state: EvidenceState,
    pub records: Vec<RecordEvidence>,
}

/// Higher is more alarming.
fn alarm(state: EvidenceState) -> u8 {
    match state {
        EvidenceState::Unchanged => 0,
        EvidenceState::Unverified => 1,
        EvidenceState::Changed => 2,
        EvidenceState::Missing => 3,
    }
}

/// The most alarming of `states`; `Unverified` for an empty document.
pub fn worst(states: impl IntoIterator<Item = EvidenceState>) -> EvidenceState {
    states
        .into_iter()
        .max_by_key(|state| alarm(*state))
        .unwrap_or(EvidenceState::Unverified)
}

fn evaluate_record(
    root: &Path,
    spec: &RecordSpec,
    file: Option<&EvidenceFile>,
    cache: &mut DependencyCache,
) -> RecordEvidence {
    let acknowledgement = file.and_then(|file| file.entries.get(&spec.key));
    let mut outcomes = Vec::with_capacity(spec.selectors.len());
    for selector in &spec.selectors {
        let outcome = match (cache.resolve(root, selector), acknowledgement) {
            (_, None) => continue,
            (Dependency::Missing, Some(_)) => DependencyOutcome::Missing,
            (Dependency::Unresolved(reason), Some(_)) => {
                DependencyOutcome::Unresolved(reason.clone())
            }
            (Dependency::Hash(current), Some(entry)) => match entry.dependencies.get(selector) {
                None => DependencyOutcome::Unacknowledged,
                Some(acknowledged) if acknowledged == current => DependencyOutcome::Unchanged,
                Some(_) => DependencyOutcome::Changed,
            },
        };
        outcomes.push(DependencyStatus {
            selector: selector.clone(),
            outcome,
        });
    }

    let state = match acknowledgement {
        None => EvidenceState::Unverified,
        Some(entry) => {
            let same_note = entry.text == text_hash(&spec.text);
            // Both sides are sorted, so equality of the sequences is set equality.
            let same_set = entry
                .dependencies
                .keys()
                .map(String::as_str)
                .eq(spec.selectors.iter().map(String::as_str));
            if !same_note || !same_set {
                EvidenceState::Unverified
            } else if outcomes.iter().any(|status| {
                matches!(
                    status.outcome,
                    DependencyOutcome::Missing | DependencyOutcome::Unresolved(_)
                )
            }) {
                EvidenceState::Missing
            } else if outcomes
                .iter()
                .any(|status| status.outcome == DependencyOutcome::Changed)
            {
                EvidenceState::Changed
            } else {
                EvidenceState::Unchanged
            }
        }
    };
    RecordEvidence {
        key: spec.key.clone(),
        line: spec.line,
        acknowledged: acknowledgement.is_some(),
        state,
        dependencies: outcomes,
    }
}

/// Evaluate every record of `entry` against its evidence file and the current
/// dependencies. Reads only; never acknowledges anything.
pub fn evaluate_document(
    root: &Path,
    entry: &CorpusEntry,
    cache: &mut DependencyCache,
) -> DocumentEvidence {
    let path = entry.document.path.clone();
    let (file, file_problem) = match load_evidence(root, &path) {
        EvidenceLoad::Absent => (None, None),
        EvidenceLoad::Loaded(file) => (Some(file), None),
        EvidenceLoad::Invalid { code, reason } => (None, Some((code, reason))),
    };
    let records: Vec<RecordEvidence> = record_specs(entry)
        .iter()
        .map(|spec| evaluate_record(root, spec, file.as_ref(), cache))
        .collect();
    DocumentEvidence {
        state: worst(records.iter().map(|record| record.state)),
        has_file: file.is_some(),
        file_problem,
        path,
        records,
    }
}

impl RepositoryService {
    /// The evidence state of the indexed document at `path`, with its source
    /// revalidated first.
    ///
    /// # Errors
    ///
    /// [`MaintenanceError::NotFound`] when `path` is not a live indexed
    /// document; service errors when the index is unavailable.
    pub fn document_evidence(&self, path: &str) -> Result<DocumentEvidence, MaintenanceError> {
        self.fresh_document(path).map_err(|error| match error {
            ServiceError::NotIndexed { path } => MaintenanceError::NotFound { path },
            other => other.into(),
        })?;
        let (_, entry) = self.store().corpus_entry(path)?;
        let entry = entry.ok_or_else(|| MaintenanceError::NotFound {
            path: path.to_owned(),
        })?;
        Ok(evaluate_document(
            self.root().root(),
            &entry,
            &mut DependencyCache::default(),
        ))
    }

    /// Best-effort evidence state for labelling retrieval results. Any failure
    /// reads as [`EvidenceState::Unverified`], the state that never overclaims.
    pub fn evidence_state(&self, path: &str) -> EvidenceState {
        self.document_evidence(path)
            .map_or(EvidenceState::Unverified, |evidence| evidence.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::browse::tests::Fx;

    const NOTE: &str = "# Architecture\nThe scheduler dispatches tasks.\n";

    #[test]
    fn crlf_cr_bom_and_trailing_whitespace_hash_identically() {
        let lf = text_hash("a\nb\n\nc  \n");
        assert_eq!(lf, text_hash("a\r\nb\r\n\r\nc\r\n"));
        assert_eq!(lf, text_hash("a\rb\r\rc\r"));
        assert_eq!(lf, text_hash("\u{feff}a  \nb\t\n\nc\n"));
        assert!(lf.starts_with(TEXT_HASH_PREFIX));
    }

    #[test]
    fn a_substantive_edit_changes_the_hash() {
        assert_ne!(text_hash("a b\n"), text_hash("a  b\n"));
        assert_ne!(text_hash("a\n"), text_hash("b\n"));
        assert_ne!(
            text_hash(" a\n"),
            text_hash("a\n"),
            "leading space is content"
        );
    }

    #[test]
    fn selectors_reject_unsafe_paths_and_empty_symbols() {
        assert_eq!(
            parse_selector("src/lib.rs#Queue::push").unwrap(),
            ("src/lib.rs".to_owned(), Some("Queue::push".to_owned()))
        );
        assert_eq!(parse_selector("./src/lib.rs").unwrap().0, "src/lib.rs");
        for bad in [
            "../x",
            "/etc/passwd",
            "a/../../b",
            "a\\b",
            "",
            "src/lib.rs#",
        ] {
            assert!(parse_selector(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn dependencies_resolve_to_hashes_missing_or_unresolved() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct Q;\nimpl Q {\n    pub fn push(&self) {}\n    pub fn len(&self) {}\n}\nimpl Q {\n    pub fn len(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(root.join("blob.bin"), [0xff, 0xfe, 0x00]).unwrap();

        assert!(matches!(
            resolve_dependency(&root, "src/lib.rs"),
            Dependency::Hash(h) if h.starts_with(TEXT_HASH_PREFIX)
        ));
        assert!(matches!(
            resolve_dependency(&root, "src/lib.rs#Q::push"),
            Dependency::Hash(h) if h.starts_with(SYMBOL_HASH_PREFIX)
        ));
        assert_eq!(
            resolve_dependency(&root, "src/gone.rs"),
            Dependency::Missing
        );
        assert_eq!(
            resolve_dependency(&root, "src/lib.rs#Q::vanished"),
            Dependency::Missing
        );
        for unresolved in ["src/lib.rs#Q::len", "blob.bin", "../outside.md", "src"] {
            assert!(
                matches!(
                    resolve_dependency(&root, unresolved),
                    Dependency::Unresolved(_)
                ),
                "{unresolved}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_repository_is_unresolved_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(tmp.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("secret.txt"), root.join("link.txt")).unwrap();
        let root = root.canonicalize().unwrap();
        assert!(matches!(
            resolve_dependency(&root, "link.txt"),
            Dependency::Unresolved(_)
        ));
    }

    #[test]
    fn evidence_files_serialize_deterministically_with_sorted_keys() {
        let mut file = EvidenceFile::new("docs/a.md");
        for (key, dep) in [("section:[\"B\"]", "z.rs"), ("section:[\"A\"]", "a.rs")] {
            file.entries.insert(
                key.to_owned(),
                EvidenceEntry {
                    dependencies: BTreeMap::from([(dep.to_owned(), "sha256-v1:00".to_owned())]),
                    text: "sha256-v1:11".to_owned(),
                },
            );
        }
        let text = file.to_text();
        assert!(text.ends_with("}\n"));
        let document = text.find("\"document\"").unwrap();
        let entries = text.find("\"entries\"").unwrap();
        let version = text.find("\"version\"").unwrap();
        assert!(document < entries && entries < version);
        assert!(text.find("[\\\"A\\\"]").unwrap() < text.find("[\\\"B\\\"]").unwrap());
        assert_eq!(text, file.clone().to_text());
        assert!(
            !text.contains('/') || !text.contains(":\\"),
            "no absolute paths"
        );
    }

    #[test]
    fn unsupported_and_malformed_evidence_files_are_reported() {
        let load = |text: &str| parse_evidence(text.as_bytes(), "docs/a.md");
        assert!(matches!(
            load("{\"version\": 99}"),
            EvidenceLoad::Invalid {
                code: codes::VERSION_UNSUPPORTED,
                ..
            }
        ));
        for bad in [
            "not json",
            "{}",
            "{\"version\":1,\"document\":\"other.md\",\"entries\":{}}",
            "{\"version\":1,\"document\":\"docs/a.md\",\"entries\":{},\"extra\":1}",
        ] {
            assert!(
                matches!(
                    load(bad),
                    EvidenceLoad::Invalid {
                        code: codes::FILE_INVALID,
                        ..
                    }
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            load("{\"version\":1,\"document\":\"docs/a.md\",\"entries\":{}}"),
            EvidenceLoad::Loaded(_)
        ));
    }

    /// Acknowledge by hand so these tests exercise evaluation alone.
    fn acknowledge_by_hand(fx: &Fx, document: &str, deps: &[&str]) {
        let root = fx.service.root().root().to_path_buf();
        let (_, entry) = fx.service.store().corpus_entry(document).unwrap();
        let mut file = EvidenceFile::new(document);
        for spec in record_specs(&entry.unwrap()) {
            let dependencies = deps
                .iter()
                .map(|selector| {
                    let Dependency::Hash(hash) = resolve_dependency(&root, selector) else {
                        panic!("{selector} must resolve");
                    };
                    ((*selector).to_owned(), hash)
                })
                .collect();
            file.entries.insert(
                spec.key,
                EvidenceEntry {
                    dependencies,
                    text: text_hash(&spec.text),
                },
            );
        }
        fx.put(&evidence_path(document), &file.to_text());
    }

    fn fixture() -> Fx {
        let fx = Fx::new();
        fx.put("src/lib.rs", "pub fn run() {}\n");
        fx.put(
            "docs/arch.md",
            &format!("---\nknowledge_sources: src/lib.rs\nstatus: verified\n---\n{NOTE}"),
        );
        fx.service.refresh().unwrap();
        fx
    }

    #[test]
    fn evidence_walks_through_every_state_independently_of_lifecycle() {
        let fx = fixture();
        let state = |fx: &Fx| fx.service.document_evidence("docs/arch.md").unwrap().state;

        assert_eq!(state(&fx), EvidenceState::Unverified, "never acknowledged");
        acknowledge_by_hand(&fx, "docs/arch.md", &["src/lib.rs"]);
        assert_eq!(state(&fx), EvidenceState::Unchanged);

        // Whitespace-only dependency edits are not drift.
        fx.put("src/lib.rs", "pub fn run() {}   \r\n");
        assert_eq!(state(&fx), EvidenceState::Unchanged);

        fx.put("src/lib.rs", "pub fn run() { work(); }\n");
        assert_eq!(state(&fx), EvidenceState::Changed);

        fs::remove_file(fx.repo.join("src/lib.rs")).unwrap();
        assert_eq!(state(&fx), EvidenceState::Missing);

        fx.put("src/lib.rs", "pub fn run() {}\n");
        assert_eq!(state(&fx), EvidenceState::Unchanged);

        // Editing the note makes it unverified, whatever its status says.
        fx.put(
            "docs/arch.md",
            "---\nknowledge_sources: src/lib.rs\nstatus: verified\n---\n# Architecture\nRewritten.\n",
        );
        assert_eq!(state(&fx), EvidenceState::Unverified);
    }

    #[test]
    fn editing_the_declared_dependencies_makes_the_note_unverified() {
        let fx = fixture();
        acknowledge_by_hand(&fx, "docs/arch.md", &["src/lib.rs"]);
        fx.put("src/other.rs", "pub fn other() {}\n");
        fx.put(
            "docs/arch.md",
            &format!("---\nknowledge_sources: src/lib.rs, src/other.rs\n---\n{NOTE}"),
        );
        let evidence = fx.service.document_evidence("docs/arch.md").unwrap();
        assert_eq!(evidence.state, EvidenceState::Unverified);
        assert!(evidence.records.iter().all(|record| record.acknowledged));
    }

    #[test]
    fn a_vanished_or_ambiguous_symbol_is_never_replaced_by_a_lookalike() {
        let fx = Fx::new();
        fx.put("src/lib.rs", "pub fn run() {}\npub fn runner() {}\n");
        fx.put(
            "docs/arch.md",
            &format!("---\nknowledge_sources: src/lib.rs#run\n---\n{NOTE}"),
        );
        fx.service.refresh().unwrap();
        acknowledge_by_hand(&fx, "docs/arch.md", &["src/lib.rs#run"]);
        let state = || fx.service.document_evidence("docs/arch.md").unwrap().state;
        assert_eq!(state(), EvidenceState::Unchanged);

        fx.put("src/lib.rs", "pub fn runner() {}\n");
        assert_eq!(state(), EvidenceState::Missing);
    }

    #[test]
    fn a_moved_document_is_unverified_and_its_old_file_is_listed_as_an_orphan() {
        let fx = fixture();
        acknowledge_by_hand(&fx, "docs/arch.md", &["src/lib.rs"]);
        let body = fs::read_to_string(fx.repo.join("docs/arch.md")).unwrap();
        fs::remove_file(fx.repo.join("docs/arch.md")).unwrap();
        fx.put("docs/design/arch.md", &body);
        fx.service.refresh().unwrap();

        let moved = fx.service.document_evidence("docs/design/arch.md").unwrap();
        assert_eq!(moved.state, EvidenceState::Unverified);
        assert!(!moved.has_file);
        let (documents, findings) = list_evidence_documents(fx.service.root().root());
        assert_eq!(documents, vec!["docs/arch.md".to_owned()]);
        assert!(findings.is_empty());
    }

    #[test]
    fn evidence_survives_a_fresh_checkout_without_the_original_database() {
        let fx = fixture();
        acknowledge_by_hand(&fx, "docs/arch.md", &["src/lib.rs"]);
        // A second service with its own empty cache stands in for a clone.
        let (tmp, peer) = {
            let tmp = tempfile::tempdir().unwrap();
            let root = fx.service.root().clone();
            let peer = RepositoryService::open(
                root,
                &tmp.path().join("index.sqlite"),
                crate::knowledge::repository::ReconcileOptions::from(
                    &crate::config::RepositoryKnowledgeSettings::default(),
                ),
                Default::default(),
            )
            .unwrap();
            (tmp, peer)
        };
        peer.refresh().unwrap();
        assert_eq!(
            peer.document_evidence("docs/arch.md").unwrap().state,
            EvidenceState::Unchanged
        );
        drop(tmp);
    }

    #[test]
    fn indexing_and_fetching_never_acknowledge_anything() {
        let fx = fixture();
        fx.service.refresh().unwrap();
        let _ = fx.service.get(&crate::knowledge::repository::GetRequest {
            path: Some("docs/arch.md".into()),
            max_bytes: 4096,
            ..Default::default()
        });
        assert!(!fx.repo.join(EVIDENCE_DIR).exists());
        assert_eq!(
            fx.service.evidence_state("docs/arch.md"),
            EvidenceState::Unverified
        );
    }

    #[test]
    fn purpose_rows_depend_on_the_file_they_name() {
        let fx = Fx::new();
        fx.put("src/queue.rs", "pub struct Queue;\n");
        fx.put(
            "docs/files.md",
            "| File | Purpose |\n|---|---|\n| `../src/queue.rs` | The queue. |\n",
        );
        fx.service.refresh().unwrap();
        let (_, entry) = fx.service.store().corpus_entry("docs/files.md").unwrap();
        let specs = record_specs(&entry.unwrap());
        let row = specs.iter().find(|s| s.key.starts_with("row:")).unwrap();
        assert_eq!(row.key, "row:src/queue.rs");
        assert_eq!(row.selectors, vec!["src/queue.rs".to_owned()]);
        assert_eq!(row.text, "The queue.");
    }

    #[test]
    fn unknown_documents_are_not_found() {
        let fx = fixture();
        let error = fx.service.document_evidence("docs/nope.md").unwrap_err();
        assert_eq!(error.code(), "source_not_indexed");
    }
}

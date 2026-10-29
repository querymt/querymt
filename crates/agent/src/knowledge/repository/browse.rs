//! Fetching, cataloguing and describing the repository corpus.
//!
//! The read-side companions of [`query`](super::query): `get` returns current
//! section bodies, `list` pages through a catalog, and `stats` describes
//! coverage. Like queries, each operation reconciles the index first and
//! revalidates the source it returns, and resolves everything inside the one
//! checkout the service is bound to: an ID or path can never reach another
//! checkout, and an unknown or foreign ID is simply "not found".
//!
//! # Fetch
//!
//! * A section ID (or `path` plus a `section` heading selector) returns that
//!   section's *body*, paged by bytes with an explicit continuation cursor.
//! * A path or document ID for a document with several sections returns its
//!   section catalog and preamble, never one arbitrary section presented as
//!   the whole document.
//! * A selector whose heading was renamed or removed fails as stale and lists
//!   the sections that exist now; it never returns a different section. A
//!   heading that occurs more than once must be disambiguated.
//!
//! # Pagination
//!
//! Cursors carry the generation they were issued under. If the index moved on,
//! the response says so ([`ListResponse::generation_changed`],
//! [`GetError::GenerationChanged`]) instead of mixing pages of two corpora.

use super::identity::{DocumentId, SectionKey};
use super::query::normalize_relative_path;
use super::reconcile::ReconcileStatus;
use super::service::{RepositoryService, ServiceError};
use super::store::{
    IndexDiagnostic, SCHEMA_VERSION, StoreError, StoredDocument, StoredSection, path_in_scope,
};
use std::collections::BTreeMap;
use std::str::FromStr;
use thiserror::Error;

/// Default and maximum entries per catalog page.
pub const DEFAULT_PAGE: usize = 20;
pub const MAX_PAGE: usize = 50;
/// Bytes of a preamble shown inline in a document catalog.
const PREAMBLE_BYTES: usize = 1024;
const FILE_PREFIX: &str = "file:";

// ─── Fetch ───────────────────────────────────────────────────────────────────

/// What to fetch. Exactly one of `id` or `path` must be set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GetRequest {
    /// A section key, a document ID, or `file:<path>`.
    pub id: Option<String>,
    /// A repository-relative path.
    pub path: Option<String>,
    /// Heading selector (`A > B`, optionally `~N` for the Nth repeat). Only
    /// valid with `path`.
    pub section: Option<String>,
    /// Continuation cursor from a previous response.
    pub cursor: Option<String>,
    /// Largest content slice to return, in bytes.
    pub max_bytes: usize,
}

/// A section named in a catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionRef {
    pub key: String,
    /// Heading breadcrumb, or `None` for a preamble.
    pub selector: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub bytes: usize,
}

impl SectionRef {
    fn of(section: &StoredSection) -> Self {
        Self {
            key: section.key.clone(),
            selector: breadcrumb(section),
            start_line: section.start_line,
            end_line: section.end_line,
            bytes: section.body.len(),
        }
    }
}

fn breadcrumb(section: &StoredSection) -> Option<String> {
    (!section.breadcrumb.is_empty()).then(|| section.breadcrumb.join(" > "))
}

/// A slice of one section's current body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionContent {
    pub generation: u64,
    pub path: String,
    pub document_id: String,
    pub document_title: String,
    pub status: Option<String>,
    pub section: SectionRef,
    /// Byte offset of `content` within the section body.
    pub offset: usize,
    pub content: String,
    /// Length of the whole section body.
    pub total_bytes: usize,
    /// Present when more of the body remains.
    pub next_cursor: Option<String>,
}

/// A document's table of contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentCatalog {
    pub generation: u64,
    pub path: String,
    pub document_id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: Option<String>,
    /// Text before the first heading, bounded.
    pub preamble: Option<String>,
    pub preamble_truncated: bool,
    pub sections: Vec<SectionRef>,
}

/// A path-level record of a selected code file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub generation: u64,
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    Section(SectionContent),
    Catalog(DocumentCatalog),
    File(FileInfo),
}

/// Why a fetch failed. Never carries content from outside the checkout.
#[derive(Debug, Error)]
pub enum GetError {
    #[error("{0}")]
    Invalid(String),
    #[error("`{what}` is not in this checkout's repository knowledge")]
    NotFound { what: String },
    #[error("section `{selector}` no longer exists in `{path}`")]
    StaleSelector {
        path: String,
        selector: String,
        available: Vec<SectionRef>,
    },
    #[error("section `{selector}` matches {} sections in `{path}`", candidates.len())]
    Ambiguous {
        path: String,
        selector: String,
        candidates: Vec<SectionRef>,
    },
    #[error(
        "cursor was issued for generation {cursor} but the index is now at generation \
         {current}; fetch again without a cursor"
    )]
    GenerationChanged { cursor: u64, current: u64 },
    #[error(transparent)]
    Service(#[from] ServiceError),
}

impl GetError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "request_invalid",
            Self::NotFound { .. } => "not_found",
            Self::StaleSelector { .. } => "selector_stale",
            Self::Ambiguous { .. } => "selector_ambiguous",
            Self::GenerationChanged { .. } => "cursor_stale",
            Self::Service(error) => error.code(),
        }
    }
}

impl From<StoreError> for GetError {
    fn from(error: StoreError) -> Self {
        Self::Service(ServiceError::unavailable(error))
    }
}

fn invalid(message: impl Into<String>) -> GetError {
    GetError::Invalid(message.into())
}

/// Parse `"{generation}:{offset}"`.
fn parse_cursor(raw: &str) -> Result<(u64, usize), GetError> {
    raw.split_once(':')
        .and_then(|(generation, offset)| Some((generation.parse().ok()?, offset.parse().ok()?)))
        .ok_or_else(|| invalid("malformed cursor; use the cursor from a previous response"))
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// A section matched by a `path` + `section` selector.
fn select_section<'a>(
    document: &'a StoredDocument,
    selector: &str,
) -> Result<&'a StoredSection, GetError> {
    let selector = selector.trim();
    if let Some(found) = document.sections.iter().find(|s| s.key == selector) {
        return Ok(found);
    }
    let (crumb, occurrence) = match selector.rsplit_once('~') {
        Some((crumb, n)) if n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty() => {
            (crumb.trim(), n.parse::<u32>().ok())
        }
        _ => (selector, None),
    };
    let matches: Vec<&StoredSection> = document
        .sections
        .iter()
        .filter(|s| {
            breadcrumb(s).as_deref() == Some(crumb)
                && occurrence.is_none_or(|occurrence| s.occurrence == occurrence)
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(GetError::StaleSelector {
            path: document.path.clone(),
            selector: selector.to_string(),
            available: document.sections.iter().map(SectionRef::of).collect(),
        }),
        many => Err(GetError::Ambiguous {
            path: document.path.clone(),
            selector: selector.to_string(),
            candidates: many.iter().map(|s| SectionRef::of(s)).collect(),
        }),
    }
}

// ─── Catalog ─────────────────────────────────────────────────────────────────

/// What a catalog entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// An authored document.
    Document,
    /// A path-level record of a selected code file.
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub kind: EntryKind,
    pub path: String,
    /// Document ID; `file:<path>` for code files.
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: Option<String>,
    /// Sections of a document.
    pub sections: usize,
    /// Size of a code file.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListRequest {
    /// Repository-relative directory or file to confine the catalog to.
    pub path: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListResponse {
    pub generation: u64,
    pub status: ReconcileStatus,
    pub entries: Vec<ListEntry>,
    /// Entries in the whole catalog (within the path filter).
    pub total: usize,
    pub offset: usize,
    pub next_cursor: Option<String>,
    /// The cursor belonged to an older generation, so the catalog restarts
    /// from its beginning instead of continuing across two corpora.
    pub generation_changed: bool,
}

// ─── Stats ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct StatsReport {
    pub generation: u64,
    pub status: ReconcileStatus,
    pub index_format: u32,
    /// Every selected source.
    pub sources: usize,
    /// Sources that produced a usable document.
    pub documents: usize,
    pub sections: usize,
    pub code_files: usize,
    /// Selected documents that are unusable (malformed, disabled, duplicate ID).
    pub excluded_sources: usize,
    /// False when selected files were skipped or the manifest is unusable.
    pub coverage_complete: bool,
    /// Diagnostic counts by stable code.
    pub diagnostics: BTreeMap<String, usize>,
    pub coverage: Vec<IndexDiagnostic>,
}

impl RepositoryService {
    /// Fetch current content. See the module documentation.
    pub fn get(&self, request: &GetRequest) -> Result<Fetched, GetError> {
        let cursor = request.cursor.as_deref().map(parse_cursor).transpose()?;
        match (&request.id, &request.path) {
            (Some(_), Some(_)) => return Err(invalid("give either `id` or `path`, not both")),
            (None, None) => return Err(invalid("give an `id` or a `path` to fetch")),
            _ => {}
        }
        if request.section.is_some() && request.id.is_some() {
            return Err(invalid("`section` can only be combined with `path`"));
        }
        if request.max_bytes == 0 {
            return Err(invalid("the byte budget must be positive"));
        }

        // Reconcile first (cheap when nothing changed), as query and list do, so
        // IDs resolve against the current corpus and cursors see its generation.
        self.refresh()?;

        if let Some(id) = &request.id {
            if let Some(path) = id.strip_prefix(FILE_PREFIX) {
                return self.fetch_file(path, cursor);
            }
            if id.contains('#') {
                return self.fetch_section_id(id, cursor, request.max_bytes);
            }
            let document_id = DocumentId::from_str(id)
                .map_err(|_| invalid("unrecognized id; use an id returned by repo_knowledge_*"))?;
            let path = self.path_of_document(document_id.as_str())?;
            return self.fetch_document(&path, None, cursor, request.max_bytes);
        }
        let raw = request.path.as_deref().unwrap_or_default();
        let path = normalize_relative_path(raw)
            .map_err(|reason| invalid(format!("invalid path `{raw}`: {reason}")))?;
        self.fetch_document(&path, request.section.as_deref(), cursor, request.max_bytes)
    }

    fn path_of_document(&self, document_id: &str) -> Result<String, GetError> {
        let (_, found) = self.store().document_by_id(document_id)?;
        found
            .map(|document| document.path)
            .ok_or_else(|| GetError::NotFound {
                what: document_id.to_string(),
            })
    }

    fn fetch_section_id(
        &self,
        id: &str,
        cursor: Option<(u64, usize)>,
        max_bytes: usize,
    ) -> Result<Fetched, GetError> {
        let key = SectionKey::from_str(id).map_err(|_| {
            invalid("unrecognized section id; use an id returned by repo_knowledge_*")
        })?;
        let path = self.path_of_document(key.document_id.as_str())?;
        self.fetch_document(&path, Some(id), cursor, max_bytes)
    }

    fn fetch_file(&self, raw: &str, cursor: Option<(u64, usize)>) -> Result<Fetched, GetError> {
        if cursor.is_some() {
            return Err(invalid("a file record has no continuation"));
        }
        let path = normalize_relative_path(raw)
            .map_err(|reason| invalid(format!("invalid path `{raw}`: {reason}")))?;
        let (generation, source) = self.store().source(&path)?;
        match source {
            Some(source) if source.is_code => Ok(Fetched::File(FileInfo {
                generation,
                path,
                size: source.stamp.size,
            })),
            _ => Err(GetError::NotFound { what: path }),
        }
    }

    /// `selector` is a section key or a heading selector; `None` fetches the
    /// document itself.
    fn fetch_document(
        &self,
        path: &str,
        selector: Option<&str>,
        cursor: Option<(u64, usize)>,
        max_bytes: usize,
    ) -> Result<Fetched, GetError> {
        let current = match self.fresh_document(path) {
            Ok(current) => current,
            Err(ServiceError::NotIndexed { .. }) => {
                // A selected code file is a navigation record, not a document.
                let (generation, source) = self.store().source(path)?;
                return match source {
                    Some(source) if source.is_code && selector.is_none() && cursor.is_none() => {
                        Ok(Fetched::File(FileInfo {
                            generation,
                            path: path.to_string(),
                            size: source.stamp.size,
                        }))
                    }
                    _ => Err(GetError::NotFound {
                        what: path.to_string(),
                    }),
                };
            }
            Err(other) => return Err(other.into()),
        };
        let generation = current.generation;
        let document = current.document;

        if let Some((issued, _)) = cursor {
            if issued != generation {
                return Err(GetError::GenerationChanged {
                    cursor: issued,
                    current: generation,
                });
            }
        }

        let section = match selector {
            Some(selector) => Some(select_section(&document, selector)?),
            None => match document.sections.as_slice() {
                [only] => Some(only),
                _ => None,
            },
        };
        let Some(section) = section else {
            if cursor.is_some() {
                return Err(invalid(
                    "a cursor continues one section; fetch a section id or `section` selector",
                ));
            }
            return Ok(Fetched::Catalog(catalog_of(generation, &document)));
        };

        let offset = cursor.map_or(0, |(_, offset)| offset);
        let body = section.body.as_str();
        if offset > body.len() || !body.is_char_boundary(offset) {
            return Err(invalid("cursor does not point into this section"));
        }
        let mut end = floor_boundary(body, offset.saturating_add(max_bytes));
        if end == offset && offset < body.len() {
            // The budget is smaller than one character: still make progress.
            end = ceil_boundary(body, offset + 1);
        }
        let next_cursor = (end < body.len()).then(|| format!("{generation}:{end}"));
        Ok(Fetched::Section(SectionContent {
            generation,
            path: document.path.clone(),
            document_id: document.id.clone(),
            document_title: document.title.clone(),
            status: document.status.clone(),
            section: SectionRef::of(section),
            offset,
            content: body[offset..end].to_string(),
            total_bytes: body.len(),
            next_cursor,
        }))
    }

    /// A page of the corpus catalog: documents by path, then code files.
    pub fn list(&self, request: &ListRequest) -> Result<ListResponse, GetError> {
        let scope = request
            .path
            .as_deref()
            .map(|raw| {
                normalize_relative_path(raw)
                    .map_err(|reason| invalid(format!("invalid path `{raw}`: {reason}")))
            })
            .transpose()?;
        let cursor = request.cursor.as_deref().map(parse_cursor).transpose()?;
        let limit = request.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);

        let report = self.refresh()?;
        let (generation, mut entries) = self.store().read(|conn| {
            let mut entries = Vec::new();
            let mut statement = conn.prepare(
                "SELECT d.path, d.doc_id, d.title, d.description, d.status, \
                 (SELECT count(*) FROM sections s WHERE s.doc_path = d.path) \
                 FROM documents d ORDER BY d.path",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(ListEntry {
                    kind: EntryKind::Document,
                    path: row.get(0)?,
                    id: row.get(1)?,
                    title: row.get(2)?,
                    description: row.get(3)?,
                    status: row.get(4)?,
                    sections: usize::try_from(row.get::<_, i64>(5)?).unwrap_or_default(),
                    size: None,
                })
            })?;
            for row in rows {
                entries.push(row?);
            }
            let mut statement =
                conn.prepare("SELECT path, size FROM sources WHERE is_code = 1 ORDER BY path")?;
            let rows = statement.query_map([], |row| {
                let path: String = row.get(0)?;
                Ok(ListEntry {
                    kind: EntryKind::File,
                    id: format!("{FILE_PREFIX}{path}"),
                    title: path
                        .rsplit_once('/')
                        .map_or(path.as_str(), |(_, name)| name)
                        .to_string(),
                    path,
                    description: None,
                    status: None,
                    sections: 0,
                    size: Some(u64::try_from(row.get::<_, i64>(1)?).unwrap_or_default()),
                })
            })?;
            for row in rows {
                entries.push(row?);
            }
            Ok(entries)
        })?;
        entries.retain(|entry| path_in_scope(&entry.path, scope.as_deref()));

        let (offset, generation_changed) = match cursor {
            Some((issued, offset)) if issued == generation => (offset, false),
            Some(_) => (0, true),
            None => (0, false),
        };
        let total = entries.len();
        let page: Vec<ListEntry> = entries.into_iter().skip(offset).take(limit).collect();
        let next = offset + page.len();
        Ok(ListResponse {
            generation,
            status: report.status,
            entries: page,
            total,
            offset,
            next_cursor: (next < total).then(|| format!("{generation}:{next}")),
            generation_changed,
        })
    }

    /// Coverage and freshness of the corpus.
    pub fn stats(&self) -> Result<StatsReport, ServiceError> {
        let report = self.refresh()?;
        let state = self.store().state().map_err(ServiceError::unavailable)?;
        let counts = self.store().counts().map_err(ServiceError::unavailable)?;
        let mut diagnostics: BTreeMap<String, usize> = BTreeMap::new();
        let mut all: Vec<IndexDiagnostic> = state.coverage.clone();
        for source in &state.sources {
            all.extend(source.diagnostics.iter().cloned());
        }
        all.sort();
        all.dedup();
        for diagnostic in &all {
            *diagnostics.entry(diagnostic.code.clone()).or_default() += 1;
        }
        let code_files = state.sources.iter().filter(|s| s.is_code).count();
        let excluded_sources = state
            .sources
            .iter()
            .filter(|s| s.is_document && !s.has_document)
            .count();
        Ok(StatsReport {
            generation: state.generation,
            status: report.status,
            index_format: SCHEMA_VERSION,
            sources: state.sources.len(),
            documents: counts.documents,
            sections: counts.sections,
            code_files,
            excluded_sources,
            coverage_complete: state.coverage.is_empty(),
            diagnostics,
            coverage: state.coverage,
        })
    }
}

fn catalog_of(generation: u64, document: &StoredDocument) -> DocumentCatalog {
    let preamble = document
        .sections
        .iter()
        .find(|section| section.breadcrumb.is_empty());
    let (text, truncated) = match preamble {
        Some(section) if !section.body.trim().is_empty() => {
            let body = section.body.trim();
            let end = floor_boundary(body, PREAMBLE_BYTES);
            (Some(body[..end].to_string()), end < body.len())
        }
        _ => (None, false),
    };
    DocumentCatalog {
        generation,
        path: document.path.clone(),
        document_id: document.id.clone(),
        title: document.title.clone(),
        description: document.description.clone(),
        status: document.status.clone(),
        preamble: text,
        preamble_truncated: truncated,
        sections: document.sections.iter().map(SectionRef::of).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepositoryKnowledgeSettings;
    use crate::knowledge::repository::reconcile::ReconcileOptions;
    use crate::knowledge::repository::root::{RootRequest, resolve_root};
    use crate::knowledge::repository::service::RefreshPolicy;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    const MANIFEST: &str = "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n";
    const ARCH: &str = "Intro text before any heading.\n# Architecture\n## Scheduling\nThe scheduler dispatches tasks.\n## Storage\nRows live in SQLite.\n";

    struct Fx {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        service: RepositoryService,
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
            let service = RepositoryService::open(
                root,
                &tmp.path().join("cache/index.sqlite"),
                ReconcileOptions::from(&RepositoryKnowledgeSettings::default()),
                RefreshPolicy {
                    max_attempts: 20,
                    retry_delay: Duration::from_millis(1),
                    busy_timeout: Duration::from_secs(10),
                },
            )
            .unwrap();
            let fx = Self {
                _tmp: tmp,
                repo,
                service,
            };
            fx.put(".qmt/knowledge.toml", MANIFEST);
            fx
        }

        fn put(&self, rel: &str, content: &str) {
            let path = self.repo.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn get(&self, request: GetRequest) -> Result<Fetched, GetError> {
            self.service.get(&GetRequest {
                max_bytes: if request.max_bytes == 0 {
                    4096
                } else {
                    request.max_bytes
                },
                ..request
            })
        }

        fn by_path(&self, path: &str) -> Result<Fetched, GetError> {
            self.get(GetRequest {
                path: Some(path.into()),
                ..Default::default()
            })
        }
    }

    fn section(fetched: Fetched) -> SectionContent {
        match fetched {
            Fetched::Section(section) => section,
            other => panic!("expected a section, got {other:?}"),
        }
    }

    #[test]
    fn a_section_id_returns_the_current_body_not_a_summary() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        let found = fx
            .service
            .query(&crate::knowledge::repository::QueryRequest {
                question: "scheduler dispatches".into(),
                ..Default::default()
            });
        let hit = &found.unwrap().hits[0];
        let body = section(
            fx.get(GetRequest {
                id: Some(hit.id.clone()),
                ..Default::default()
            })
            .unwrap(),
        );
        assert!(body.content.contains("The scheduler dispatches tasks."));
        assert_eq!(body.path, "docs/arch.md");
        assert_eq!(body.next_cursor, None);
        assert_eq!(
            body.section.selector.as_deref(),
            Some("Architecture > Scheduling")
        );

        // The body follows edits, with no watcher.
        fx.put(
            "docs/arch.md",
            &ARCH.replace("dispatches tasks", "dispatches jobs"),
        );
        let fresh = section(
            fx.get(GetRequest {
                id: Some(hit.id.clone()),
                ..Default::default()
            })
            .unwrap(),
        );
        assert!(fresh.content.contains("dispatches jobs"));
    }

    #[test]
    fn a_path_only_request_returns_a_catalog_and_preamble() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        let Fetched::Catalog(catalog) = fx.by_path("docs/arch.md").unwrap() else {
            panic!("a multi-section document must return its catalog");
        };
        assert_eq!(
            catalog.preamble.as_deref(),
            Some("Intro text before any heading.")
        );
        let selectors: Vec<_> = catalog
            .sections
            .iter()
            .map(|s| s.selector.clone())
            .collect();
        assert_eq!(
            selectors,
            vec![
                None,
                Some("Architecture".into()),
                Some("Architecture > Scheduling".into()),
                Some("Architecture > Storage".into()),
            ]
        );
        // Every listed key is fetchable.
        for entry in &catalog.sections {
            section(
                fx.get(GetRequest {
                    id: Some(entry.key.clone()),
                    ..Default::default()
                })
                .unwrap(),
            );
        }
    }

    #[test]
    fn a_single_section_document_is_returned_whole() {
        let fx = Fx::new();
        fx.put("docs/one.md", "# Only\nsingle body\n");
        let whole = section(fx.by_path("docs/one.md").unwrap());
        assert!(whole.content.contains("single body"));
    }

    #[test]
    fn heading_selectors_pick_one_section_and_fail_when_duplicated_or_stale() {
        let fx = Fx::new();
        fx.put(
            "docs/d.md",
            "# Notes\n## Detail\nfirst\n## Detail\nsecond\n## Other\nx\n",
        );
        let pick = |selector: &str| {
            fx.get(GetRequest {
                path: Some("docs/d.md".into()),
                section: Some(selector.into()),
                ..Default::default()
            })
        };
        assert!(
            section(pick("Notes > Other").unwrap())
                .content
                .contains('x')
        );
        assert!(
            section(pick("Notes > Detail~1").unwrap())
                .content
                .contains("second")
        );
        assert!(
            section(pick("Notes > Detail~0").unwrap())
                .content
                .contains("first")
        );

        match pick("Notes > Detail").unwrap_err() {
            GetError::Ambiguous { candidates, .. } => assert_eq!(candidates.len(), 2),
            other => panic!("{other:?}"),
        }
        match pick("Notes > Gone").unwrap_err() {
            GetError::StaleSelector { available, .. } => assert!(!available.is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_renamed_heading_is_stale_and_never_a_different_section() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        let Fetched::Catalog(catalog) = fx.by_path("docs/arch.md").unwrap() else {
            panic!()
        };
        let key = catalog.sections[2].key.clone();
        fx.put(
            "docs/arch.md",
            &ARCH.replace("## Scheduling", "## Dispatch"),
        );
        let error = fx
            .get(GetRequest {
                id: Some(key),
                ..Default::default()
            })
            .unwrap_err();
        match &error {
            GetError::StaleSelector { available, .. } => {
                assert!(
                    available
                        .iter()
                        .any(|s| s.selector.as_deref() == Some("Architecture > Dispatch"))
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(error.code(), "selector_stale");
    }

    #[test]
    fn long_sections_page_with_cursors_that_reassemble_exactly() {
        let fx = Fx::new();
        let text = "héllo wörld ".repeat(200);
        fx.put("docs/long.md", &format!("# Long\n{text}\n"));
        let mut cursor = None;
        let mut assembled = String::new();
        let mut pages = 0;
        loop {
            let page = section(
                fx.get(GetRequest {
                    path: Some("docs/long.md".into()),
                    cursor: cursor.clone(),
                    max_bytes: 300,
                    ..Default::default()
                })
                .unwrap(),
            );
            assert!(page.content.len() <= 300);
            assert_eq!(page.offset, assembled.len());
            assembled.push_str(&page.content);
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => {
                    assert_eq!(page.total_bytes, assembled.len());
                    break;
                }
            }
        }
        assert!(pages > 1);
        assert!(assembled.contains(&text));
    }

    #[test]
    fn a_tiny_budget_still_makes_progress_on_multibyte_text() {
        let fx = Fx::new();
        fx.put("docs/m.md", "# M\néé\n");
        let page = section(
            fx.get(GetRequest {
                path: Some("docs/m.md".into()),
                max_bytes: 1,
                ..Default::default()
            })
            .unwrap(),
        );
        assert!(!page.content.is_empty());
    }

    #[test]
    fn cursors_from_an_older_generation_are_reported_not_followed() {
        let fx = Fx::new();
        fx.put(
            "docs/long.md",
            &format!("# Long\n{}\n", "word ".repeat(200)),
        );
        let first = section(
            fx.get(GetRequest {
                path: Some("docs/long.md".into()),
                max_bytes: 100,
                ..Default::default()
            })
            .unwrap(),
        );
        fx.put("docs/other.md", "# Other\nnew document\n");
        let error = fx
            .get(GetRequest {
                path: Some("docs/long.md".into()),
                cursor: first.next_cursor,
                max_bytes: 100,
                ..Default::default()
            })
            .unwrap_err();
        assert!(
            matches!(error, GetError::GenerationChanged { .. }),
            "{error:?}"
        );
        assert_eq!(error.code(), "cursor_stale");
        for bad in ["nonsense", "1:x", ":3"] {
            let error = fx
                .get(GetRequest {
                    path: Some("docs/long.md".into()),
                    cursor: Some(bad.into()),
                    ..Default::default()
                })
                .unwrap_err();
            assert!(matches!(error, GetError::Invalid(_)), "{bad}");
        }
    }

    #[test]
    fn unsafe_foreign_and_malformed_requests_are_rejected_without_disclosure() {
        let fx = Fx::new();
        fx.put("docs/a.md", "# A\nbody\n");
        fs::write(
            fx.repo.parent().unwrap().join("secret.md"),
            "# Secret\nTOP SECRET\n",
        )
        .unwrap();
        for path in [
            "../secret.md",
            "/etc/passwd",
            "docs/../../secret.md",
            "a\\b",
        ] {
            let error = fx.by_path(path).unwrap_err();
            assert!(matches!(error, GetError::Invalid(_)), "{path}");
            assert!(!error.to_string().contains("TOP SECRET"));
        }
        // Unselected, missing and foreign IDs are all just "not found".
        for id in [
            "path:docs/missing.md",
            "id:somebody-elses-doc",
            "path:../secret.md#Secret",
            "file:docs/a.md",
        ] {
            let error = fx
                .get(GetRequest {
                    id: Some(id.into()),
                    ..Default::default()
                })
                .unwrap_err();
            assert!(
                matches!(error, GetError::NotFound { .. } | GetError::Invalid(_)),
                "{id}: {error:?}"
            );
        }
        assert!(matches!(
            fx.get(GetRequest {
                id: Some("x".into()),
                ..Default::default()
            })
            .unwrap_err(),
            GetError::Invalid(_)
        ));
        assert!(matches!(
            fx.get(GetRequest::default()).unwrap_err(),
            GetError::Invalid(_)
        ));
        let both = GetRequest {
            id: Some("path:docs/a.md".into()),
            path: Some("docs/a.md".into()),
            ..Default::default()
        };
        assert!(matches!(fx.get(both).unwrap_err(), GetError::Invalid(_)));
        let mixed = GetRequest {
            id: Some("path:docs/a.md".into()),
            section: Some("A".into()),
            ..Default::default()
        };
        assert!(matches!(fx.get(mixed).unwrap_err(), GetError::Invalid(_)));
    }

    #[test]
    fn code_files_are_path_only_navigation_records() {
        let fx = Fx::new();
        fx.put("src/lib.rs", "fn main() {}\n");
        for request in [
            GetRequest {
                id: Some("file:src/lib.rs".into()),
                ..Default::default()
            },
            GetRequest {
                path: Some("src/lib.rs".into()),
                ..Default::default()
            },
        ] {
            let Fetched::File(info) = fx.get(request).unwrap() else {
                panic!()
            };
            assert_eq!(info.path, "src/lib.rs");
            assert_eq!(info.size, 13);
        }
    }

    #[test]
    fn explicit_ids_survive_a_move() {
        let fx = Fx::new();
        fx.put("docs/old.md", "---\nid: runtime\n---\n# Runtime\nbody\n");
        let before = section(
            fx.get(GetRequest {
                id: Some("id:runtime".into()),
                ..Default::default()
            })
            .unwrap(),
        );
        assert_eq!(before.path, "docs/old.md");
        fs::rename(fx.repo.join("docs/old.md"), fx.repo.join("docs/new.md")).unwrap();
        let after = section(
            fx.get(GetRequest {
                id: Some("id:runtime".into()),
                ..Default::default()
            })
            .unwrap(),
        );
        assert_eq!(after.path, "docs/new.md");
    }

    #[test]
    fn catalogs_page_deterministically_and_disclose_generation_changes() {
        let fx = Fx::new();
        for n in 0..5 {
            fx.put(
                &format!("docs/d{n}.md"),
                &format!("---\ndescription: doc {n}\n---\n# Doc {n}\nx\n"),
            );
        }
        fx.put("src/a.rs", "");
        fx.put("src/b.rs", "");

        let mut seen = Vec::new();
        let mut cursor = None;
        let mut generation = None;
        loop {
            let page = fx
                .service
                .list(&ListRequest {
                    cursor: cursor.clone(),
                    limit: Some(3),
                    ..Default::default()
                })
                .unwrap();
            assert!(!page.generation_changed);
            assert_eq!(page.total, 7);
            assert_eq!(*generation.get_or_insert(page.generation), page.generation);
            seen.extend(page.entries.iter().map(|e| (e.kind, e.path.clone())));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let paths: Vec<_> = seen.iter().map(|(_, p)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "docs/d0.md",
                "docs/d1.md",
                "docs/d2.md",
                "docs/d3.md",
                "docs/d4.md",
                "src/a.rs",
                "src/b.rs"
            ]
        );
        assert!(seen[..5].iter().all(|(k, _)| *k == EntryKind::Document));
        assert!(seen[5..].iter().all(|(k, _)| *k == EntryKind::File));

        // A cursor from an earlier generation is flagged and restarts.
        let first = fx
            .service
            .list(&ListRequest {
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        fx.put("docs/new.md", "# New\nx\n");
        let resumed = fx
            .service
            .list(&ListRequest {
                cursor: first.next_cursor,
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert!(resumed.generation_changed);
        assert_eq!(resumed.offset, 0);
        assert_eq!(resumed.entries[0].path, "docs/d0.md");
        assert_ne!(resumed.generation, first.generation);
    }

    #[test]
    fn path_only_catalogs_expose_descriptions_and_filter_by_directory() {
        let fx = Fx::new();
        fx.put(
            "docs/a/x.md",
            "---\ntitle: X\ndescription: about x\nstatus: draft\n---\n# X\nb\n",
        );
        fx.put("docs/ab/y.md", "# Y\nb\n");
        fx.put("src/z.rs", "");
        let page = fx
            .service
            .list(&ListRequest {
                path: Some("docs/a".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        let entry = &page.entries[0];
        assert_eq!(entry.path, "docs/a/x.md");
        assert_eq!(entry.description.as_deref(), Some("about x"));
        assert_eq!(entry.status.as_deref(), Some("draft"));
        assert_eq!(entry.id, "path:docs/a/x.md");
        let code = fx
            .service
            .list(&ListRequest {
                path: Some("src".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(code.entries[0].id, "file:src/z.rs");
        assert_eq!(code.entries[0].size, Some(0));
        assert!(matches!(
            fx.service.list(&ListRequest {
                path: Some("../x".into()),
                ..Default::default()
            }),
            Err(GetError::Invalid(_))
        ));
    }

    #[test]
    fn stats_distinguish_indexed_from_excluded_and_incomplete_sources() {
        let fx = Fx::new();
        fx.put("docs/ok.md", "# Ok\nx\n");
        fx.put("docs/bad.md", "---\ntitle: never closed\n# Bad\n");
        fx.put("src/a.rs", "");
        let stats = fx.service.stats().unwrap();
        assert_eq!(stats.documents, 1);
        assert_eq!(stats.code_files, 1);
        assert_eq!(stats.sections, 1);
        assert_eq!(stats.sources, 3);
        assert_eq!(stats.excluded_sources, 1);
        assert!(stats.coverage_complete);
        assert!(!stats.diagnostics.is_empty());
        assert_eq!(stats.index_format, SCHEMA_VERSION);
        assert!(stats.generation > 0);
    }
}

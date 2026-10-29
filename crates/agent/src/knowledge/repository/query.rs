//! Repository retrieval: navigation and documentation lanes.
//!
//! [`RepositoryService::query`] answers a natural-language question from the
//! checkout-local index. It never calls a model, an embedding service or the
//! network, and the question is *text*, never query syntax.
//!
//! # Safe question handling
//!
//! The question is split into alphanumeric words (camelCase identifiers also
//! contribute their parts), lowercased, de-duplicated and capped. Every word is
//! then emitted as a quoted FTS5 string, so operators (`AND`, `NOT`, `NEAR(`,
//! `col:`, `*`, `^`, stray quotes) can only ever be searched for as words. A
//! blank question, or one without a single word, is a clear
//! [`QueryError`], not a storage error.
//!
//! # Lanes
//!
//! * **Navigation** ranks path-level records of selected code files. A
//!   question that names an indexed path exactly outranks everything; otherwise
//!   a file-name match counts more than a directory match. No semantic purpose
//!   is ever invented for a file.
//! * **Documentation** ranks authored sections. Sections of a document the
//!   question names by path come first (in document order); the rest follow in
//!   FTS5 BM25 order over title, heading breadcrumb, tags, description and body.
//! * **All** (default) merges them: exact-path files, exact-path document
//!   sections, then documentation and navigation matches alternating
//!   (documentation first), so a conceptual question is not buried by a file
//!   that merely shares a word.
//!
//! Ordering is fully deterministic: ties break on path, then ordinal. Records
//! are de-duplicated by their retrievable ID.
//!
//! # Freshness
//!
//! Each query first reconciles the index (the metadata fast path means an
//! unchanged corpus reads no content), then revalidates every source it is
//! about to emit, retrying a bounded number of times if one changed in between.

use super::reconcile::ReconcileStatus;
use super::service::{RepositoryService, ServiceError, SourceCheck};
use super::store::{FileRecord, IndexDiagnostic, SearchPlan, SearchSnapshot, SectionMatch};
use std::collections::BTreeSet;
use std::str::FromStr;
use thiserror::Error;

/// Results returned when the caller gives no limit.
pub const DEFAULT_LIMIT: usize = 10;
/// Largest result count a caller can request.
pub const MAX_LIMIT: usize = 50;

const MAX_TOKENS: usize = 32;
const MAX_PATH_CANDIDATES: usize = 32;
const EXCERPT_BYTES: usize = 240;
/// Navigation needs a file-name match (3) or two directory matches (2).
const MIN_NAVIGATION_SCORE: u32 = 2;
const FILENAME_WEIGHT: u32 = 3;
const DIRECTORY_WEIGHT: u32 = 1;

/// Function words that would match nearly every section. Dropped from the
/// search unless that would leave nothing to search for.
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "not", "of", "to", "in", "on", "for", "is", "are", "how",
    "what", "where", "does", "do", "with", "from", "this", "that", "it", "be", "by", "as", "at",
];

/// Which records a query searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lane {
    /// Path-level records of selected code files.
    Navigation,
    /// Authored document sections.
    Documentation,
    /// Both, merged under one limit.
    #[default]
    All,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Navigation => "navigation",
            Self::Documentation => "documentation",
            Self::All => "all",
        }
    }
}

impl FromStr for Lane {
    type Err = QueryError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "navigation" => Ok(Self::Navigation),
            "documentation" => Ok(Self::Documentation),
            "all" => Ok(Self::All),
            other => Err(QueryError::InvalidLane(other.to_string())),
        }
    }
}

/// A retrieval request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryRequest {
    pub question: String,
    pub lane: Lane,
    /// Repository-relative file or directory the results are confined to.
    pub path: Option<String>,
    /// Maximum results; defaults to [`DEFAULT_LIMIT`], capped at [`MAX_LIMIT`].
    pub limit: Option<usize>,
}

/// What kind of record a hit is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    /// A section of an authored document.
    Section,
    /// A path-level record of a selected code file.
    File,
}

/// Where a hit's content comes from. Kept visibly distinct in results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Written by people in a Markdown document.
    Authored,
    /// Derived mechanically from the repository's source tree.
    SourceDerived,
}

/// One retrieved record with its citation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryHit {
    /// Retrievable ID: a section key, or `file:<path>`.
    pub id: String,
    pub kind: HitKind,
    pub provenance: Provenance,
    /// Repository-relative source path.
    pub path: String,
    /// 1-based inclusive lines in the original file (sections only).
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    /// Heading breadcrumb (`A > B`); `None` for a preamble or a file.
    pub selector: Option<String>,
    pub title: String,
    /// Bounded excerpt of the record's own content.
    pub excerpt: String,
    /// The document's raw lifecycle label, if any.
    pub status: Option<String>,
    /// The question named this record's file or document path exactly.
    pub exact: bool,
}

/// The outcome of a query.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResponse {
    /// Index generation every hit was read from.
    pub generation: u64,
    pub status: ReconcileStatus,
    pub lane: Lane,
    pub hits: Vec<QueryHit>,
    /// Distinct matches found before the limit (a lower bound when
    /// [`truncated`](Self::truncated) is set by a saturated search).
    pub total_matches: usize,
    /// More matches exist than are returned.
    pub truncated: bool,
    /// Why coverage of the selected corpus is incomplete, if it is.
    pub coverage: Vec<IndexDiagnostic>,
}

/// Why a query could not be answered.
#[derive(Debug, Error)]
pub enum QueryError {
    #[error("the question is empty; describe what you want to find")]
    EmptyQuestion,
    #[error("the question has no searchable words; use letters or digits")]
    NoSearchableText,
    #[error("invalid path filter `{path}`: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("unknown lane `{0}`; use navigation, documentation or all")]
    InvalidLane(String),
    #[error(transparent)]
    Service(#[from] ServiceError),
}

impl QueryError {
    /// Stable machine-readable diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyQuestion => "question_empty",
            Self::NoSearchableText => "question_not_searchable",
            Self::InvalidPath { .. } => "path_invalid",
            Self::InvalidLane(_) => "lane_invalid",
            Self::Service(error) => error.code(),
        }
    }
}

// ─── Text handling ───────────────────────────────────────────────────────────

/// Normalize a caller-supplied repository-relative path. Rejects anything that
/// could leave the checkout or is not a plain relative path.
pub fn normalize_relative_path(raw: &str) -> Result<String, &'static str> {
    let mut path = raw.trim();
    if path.is_empty() {
        return Err("path is empty");
    }
    if path.contains('\0') || path.contains('\\') {
        return Err("path contains a NUL or backslash");
    }
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    let path = path.trim_end_matches('/');
    if path.starts_with('/') {
        return Err("path must be relative to the repository root");
    }
    let mut chars = path.chars();
    if let (Some(first), Some(':')) = (chars.next(), chars.next()) {
        if first.is_ascii_alphabetic() {
            return Err("path must be relative to the repository root");
        }
    }
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" => return Err("path has an empty segment"),
            ".." => return Err("path must not leave the repository"),
            "." => {}
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return Err("path is empty");
    }
    Ok(segments.join("/"))
}

/// Split `word` at camelCase boundaries (`HTTPServer` -> `HTTP`, `Server`).
fn split_camel(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    for (index, &c) in chars.iter().enumerate() {
        if index > 0 && c.is_uppercase() {
            let previous = chars[index - 1];
            let next_lower = chars.get(index + 1).is_some_and(|n| n.is_lowercase());
            let boundary = previous.is_lowercase() || (previous.is_uppercase() && next_lower);
            if boundary && !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Searchable words of a question. `None` when the question has none.
fn question_tokens(question: &str) -> Option<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut all = Vec::new();
    for word in question
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let mut parts = vec![word.to_lowercase()];
        let camel = split_camel(word);
        if camel.len() > 1 {
            parts.extend(camel.iter().map(|part| part.to_lowercase()));
        }
        for part in parts {
            if part.chars().count() >= 2 && seen.insert(part.clone()) {
                all.push(part);
            }
        }
    }
    if all.is_empty() {
        return None;
    }
    let meaningful: Vec<String> = all
        .iter()
        .filter(|token| !STOPWORDS.contains(&token.as_str()))
        .cloned()
        .collect();
    let mut tokens = if meaningful.is_empty() {
        all
    } else {
        meaningful
    };
    tokens.truncate(MAX_TOKENS);
    Some(tokens)
}

/// An FTS5 expression of quoted words only. Tokens are alphanumeric, so the
/// quoting cannot be escaped from; doubling quotes is belt and braces.
fn fts_expression(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Repository-relative paths the question names verbatim.
fn path_candidates(question: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for raw in question.split_whitespace() {
        let trimmed = raw
            .trim_matches(|c| {
                matches!(
                    c,
                    '"' | '\''
                        | '`'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '<'
                        | '>'
                        | ','
                        | ';'
                        | ':'
                        | '!'
                        | '?'
                )
            })
            .trim_end_matches('.');
        // `path/to/file.rs:42` cites a line; the path is what is named.
        let candidate = match trimmed.rsplit_once(':') {
            Some((head, tail))
                if !head.is_empty()
                    && !tail.is_empty()
                    && tail.bytes().all(|b| b.is_ascii_digit()) =>
            {
                head
            }
            _ => trimmed,
        };
        if let Ok(path) = normalize_relative_path(candidate) {
            if seen.insert(path.clone()) {
                out.push(path);
                if out.len() == MAX_PATH_CANDIDATES {
                    break;
                }
            }
        }
    }
    out
}

/// Alphanumeric lowercase words of a path component.
fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// How well a file path matches the question words. A file-name word counts
/// more than a directory word. No fuzzy matching.
fn navigation_score(path: &str, tokens: &[String]) -> u32 {
    let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let name_words = words(stem);
    let directory_words = words(directory);
    tokens
        .iter()
        .map(|token| {
            if name_words.contains(token) {
                FILENAME_WEIGHT
            } else if directory_words.contains(token) {
                DIRECTORY_WEIGHT
            } else {
                0
            }
        })
        .sum()
}

/// A bounded, whitespace-collapsed excerpt cut on a character boundary.
fn excerpt(body: &str) -> String {
    let mut collapsed = String::new();
    for word in body.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(word);
        if collapsed.len() > EXCERPT_BYTES {
            break;
        }
    }
    if collapsed.len() <= EXCERPT_BYTES {
        return collapsed;
    }
    let mut end = EXCERPT_BYTES;
    while !collapsed.is_char_boundary(end) {
        end -= 1;
    }
    collapsed.truncate(end);
    collapsed.push('…');
    collapsed
}

// ─── Assembly ────────────────────────────────────────────────────────────────

/// A validated request, ready to run against a snapshot.
struct Prepared {
    lane: Lane,
    limit: usize,
    tokens: Vec<String>,
    candidates: Vec<String>,
    plan: SearchPlan,
}

impl Prepared {
    fn new(request: &QueryRequest) -> Result<Self, QueryError> {
        if request.question.trim().is_empty() {
            return Err(QueryError::EmptyQuestion);
        }
        let scope =
            match &request.path {
                None => None,
                Some(raw) => Some(normalize_relative_path(raw).map_err(|reason| {
                    QueryError::InvalidPath {
                        path: raw.clone(),
                        reason,
                    }
                })?),
            };
        let tokens = question_tokens(&request.question).ok_or(QueryError::NoSearchableText)?;
        let limit = request.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let candidates = path_candidates(&request.question);
        let docs = request.lane != Lane::Navigation;
        let plan = SearchPlan {
            fts_expression: docs.then(|| fts_expression(&tokens)),
            scope: scope.clone(),
            exact_paths: if docs { candidates.clone() } else { Vec::new() },
            // Headroom so de-duplication cannot starve the limit.
            fts_limit: (limit * 4).clamp(20, 200),
            include_files: request.lane != Lane::Documentation,
        };
        Ok(Self {
            lane: request.lane,
            limit,
            tokens,
            candidates,
            plan,
        })
    }

    fn file_hit(file: &FileRecord, exact: bool) -> QueryHit {
        let title = file
            .path
            .rsplit_once('/')
            .map_or(file.path.as_str(), |(_, name)| name)
            .to_string();
        QueryHit {
            id: format!("file:{}", file.path),
            kind: HitKind::File,
            provenance: Provenance::SourceDerived,
            path: file.path.clone(),
            start_line: None,
            end_line: None,
            selector: None,
            title,
            excerpt: format!("source file, {} bytes", file.size),
            status: None,
            exact,
        }
    }

    fn section_hit(found: &SectionMatch) -> QueryHit {
        let section = &found.section;
        let selector = (!section.breadcrumb.is_empty()).then(|| section.breadcrumb.join(" > "));
        QueryHit {
            id: section.key.clone(),
            kind: HitKind::Section,
            provenance: Provenance::Authored,
            path: found.doc_path.clone(),
            start_line: Some(section.start_line),
            end_line: Some(section.end_line),
            selector,
            title: section
                .title
                .clone()
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| found.doc_title.clone()),
            excerpt: excerpt(&section.body),
            status: found.doc_status.clone(),
            exact: found.exact,
        }
    }

    fn assemble(
        &self,
        snapshot: &SearchSnapshot,
        status: ReconcileStatus,
        coverage: &[IndexDiagnostic],
    ) -> QueryResponse {
        let mut hits: Vec<QueryHit> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut push = |hit: QueryHit| {
            if seen.insert(hit.id.clone()) {
                hits.push(hit);
            }
        };

        // Navigation candidates.
        let mut exact_files: Vec<&FileRecord> = Vec::new();
        let mut navigation: Vec<(u32, &FileRecord)> = Vec::new();
        for candidate in &self.candidates {
            if let Some(file) = snapshot.files.iter().find(|file| &file.path == candidate) {
                exact_files.push(file);
            }
        }
        for file in &snapshot.files {
            if exact_files.iter().any(|exact| exact.path == file.path) {
                continue;
            }
            let score = navigation_score(&file.path, &self.tokens);
            if score >= MIN_NAVIGATION_SCORE {
                navigation.push((score, file));
            }
        }
        navigation.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));

        // Documentation candidates.
        let (exact_sections, ranked): (Vec<&SectionMatch>, Vec<&SectionMatch>) =
            snapshot.sections.iter().partition(|found| found.exact);

        for file in exact_files {
            push(Self::file_hit(file, true));
        }
        for found in exact_sections {
            push(Self::section_hit(found));
        }
        let mut docs = ranked.into_iter();
        let mut files = navigation.into_iter();
        loop {
            let doc = docs.next();
            let file = files.next();
            if doc.is_none() && file.is_none() {
                break;
            }
            if let Some(found) = doc {
                push(Self::section_hit(found));
            }
            if let Some((_, file)) = file {
                push(Self::file_hit(file, false));
            }
        }

        let total_matches = hits.len();
        hits.truncate(self.limit);
        let truncated = total_matches > hits.len() || snapshot.fts_saturated;
        QueryResponse {
            generation: snapshot.generation,
            status,
            lane: self.lane,
            hits,
            total_matches,
            truncated,
            coverage: coverage.to_vec(),
        }
    }
}

impl RepositoryService {
    /// Answer `request` from the current corpus.
    ///
    /// Reconciles first (cheap when nothing changed), reads one consistent
    /// generation, then revalidates every source it is about to emit. If one
    /// changed in between, the query is retried a bounded number of times and
    /// then fails with [`ServiceError::Updating`]; an outdated hit is never
    /// returned.
    pub fn query(&self, request: &QueryRequest) -> Result<QueryResponse, QueryError> {
        let prepared = Prepared::new(request)?;
        let mut unstable = String::new();
        for _ in 0..self.attempts() {
            let report = self.refresh()?;
            let snapshot = self
                .store()
                .search(&prepared.plan)
                .map_err(ServiceError::unavailable)?;
            let response = prepared.assemble(&snapshot, report.status, &report.diagnostics);

            let paths: BTreeSet<&str> = response.hits.iter().map(|hit| hit.path.as_str()).collect();
            let mut current = true;
            for path in paths {
                if !matches!(self.check_source(path)?, SourceCheck::Current) {
                    unstable = path.to_string();
                    current = false;
                    break;
                }
            }
            if current {
                return Ok(response);
            }
        }
        Err(ServiceError::Updating { path: unstable }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepositoryKnowledgeSettings;
    use crate::knowledge::repository::reconcile::ReconcileOptions;
    use crate::knowledge::repository::root::{ResolvedRoot, RootRequest, resolve_root};
    use crate::knowledge::repository::service::RefreshPolicy;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    const MANIFEST: &str = "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n";

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
            let root: ResolvedRoot = resolve_root(&RootRequest {
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

        fn ask(&self, question: &str) -> QueryResponse {
            self.service
                .query(&QueryRequest {
                    question: question.to_string(),
                    ..Default::default()
                })
                .unwrap()
        }

        fn ask_with(&self, request: QueryRequest) -> Result<QueryResponse, QueryError> {
            self.service.query(&request)
        }
    }

    fn ids(response: &QueryResponse) -> Vec<&str> {
        response.hits.iter().map(|hit| hit.id.as_str()).collect()
    }

    const ARCH: &str = "# Architecture\n## Scheduling\nThe scheduler dispatches queued tasks to workers using a priority queue.\n## Storage\nRows live in SQLite.\n";

    #[test]
    fn conceptual_question_returns_authored_sections_without_a_symbol_match() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        let response = fx.ask("how does the scheduler dispatch tasks?");
        let first = &response.hits[0];
        assert_eq!(first.kind, HitKind::Section);
        assert_eq!(first.provenance, Provenance::Authored);
        assert_eq!(first.path, "docs/arch.md");
        assert!(first.selector.as_deref().unwrap().contains("Scheduling"));
        assert!(first.excerpt.contains("priority queue"));
        assert_eq!(first.start_line, Some(2));
        assert!(
            response
                .hits
                .iter()
                .all(|hit| !hit.excerpt.contains("SQLite"))
        );
    }

    #[test]
    fn an_exact_path_outranks_documents_that_merely_mention_it() {
        let fx = Fx::new();
        fx.put("src/session/store.rs", "fn main() {}\n");
        fx.put(
            "docs/notes.md",
            "# Notes\nsrc/session/store.rs is mentioned. src/session/store.rs again; session store session store.\n",
        );
        let response = fx.ask("src/session/store.rs");
        assert_eq!(response.hits[0].id, "file:src/session/store.rs");
        assert_eq!(response.hits[0].provenance, Provenance::SourceDerived);
        assert!(response.hits[0].exact);
        assert!(response.hits.iter().any(|hit| hit.path == "docs/notes.md"));

        // A quoted path with a line citation names the same file.
        let cited = fx.ask("look at `src/session/store.rs:42`, please");
        assert_eq!(cited.hits[0].id, "file:src/session/store.rs");
    }

    #[test]
    fn a_document_named_by_path_returns_its_sections_first() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        fx.put(
            "docs/other.md",
            "# Other\nmentions docs/arch.md architecture scheduling storage\n",
        );
        let response = fx.ask("docs/arch.md");
        let first: Vec<_> = response
            .hits
            .iter()
            .take_while(|hit| hit.exact)
            .map(|hit| hit.path.as_str())
            .collect();
        assert_eq!(first, vec!["docs/arch.md"; 3]);
    }

    #[test]
    fn file_names_outrank_directories_in_navigation() {
        let fx = Fx::new();
        fx.put("src/session/store.rs", "");
        fx.put("src/session/mod.rs", "");
        fx.put("src/tools/session.rs", "");
        let response = fx
            .ask_with(QueryRequest {
                question: "session".into(),
                lane: Lane::Navigation,
                ..Default::default()
            })
            .unwrap();
        // `session` is a file name once and a directory twice.
        assert_eq!(ids(&response), vec!["file:src/tools/session.rs"]);
    }

    #[test]
    fn lanes_return_only_their_own_records() {
        let fx = Fx::new();
        fx.put("src/session/store.rs", "");
        fx.put("docs/session.md", "# Sessions\nsession store notes\n");
        let nav = fx
            .ask_with(QueryRequest {
                question: "session store".into(),
                lane: Lane::Navigation,
                ..Default::default()
            })
            .unwrap();
        assert!(!nav.hits.is_empty() && nav.hits.iter().all(|h| h.kind == HitKind::File));
        let docs = fx
            .ask_with(QueryRequest {
                question: "session store".into(),
                lane: Lane::Documentation,
                ..Default::default()
            })
            .unwrap();
        assert!(!docs.hits.is_empty() && docs.hits.iter().all(|h| h.kind == HitKind::Section));
        let all = fx.ask("session store");
        assert!(all.hits.iter().any(|h| h.kind == HitKind::File));
        assert!(all.hits.iter().any(|h| h.kind == HitKind::Section));
    }

    #[test]
    fn punctuation_and_operator_lookalikes_are_only_text() {
        let fx = Fx::new();
        fx.put("docs/w.md", "# Widgets\nwidget title notes\n");
        for question in [
            "\"unterminated widget",
            "widget AND NOT (bar OR",
            "title:widget*",
            "NEAR(widget notes)",
            "^widget -notes",
            "widget \"\"\" ' ` ; --",
            "col:\"widget\" ) (",
        ] {
            let response = fx.ask(question);
            assert!(
                response.hits.iter().any(|hit| hit.path == "docs/w.md"),
                "{question:?} should find the widget document"
            );
        }
    }

    #[test]
    fn empty_or_unsearchable_questions_get_clear_diagnostics() {
        let fx = Fx::new();
        for blank in ["", "   ", "\n\t"] {
            let error = fx
                .ask_with(QueryRequest {
                    question: blank.into(),
                    ..Default::default()
                })
                .unwrap_err();
            assert!(matches!(error, QueryError::EmptyQuestion));
            assert_eq!(error.code(), "question_empty");
        }
        for symbols in ["\"", "???", "()*^-", "a"] {
            let error = fx
                .ask_with(QueryRequest {
                    question: symbols.into(),
                    ..Default::default()
                })
                .unwrap_err();
            assert!(matches!(error, QueryError::NoSearchableText), "{symbols:?}");
        }
    }

    #[test]
    fn ties_break_on_path_and_repeat_identically() {
        let fx = Fx::new();
        for name in ["c", "a", "b"] {
            fx.put(&format!("docs/{name}.md"), "# Same\nwidget\n");
        }
        let first = fx.ask("widget");
        let paths: Vec<_> = first.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["docs/a.md", "docs/b.md", "docs/c.md"]);
        for _ in 0..3 {
            assert_eq!(fx.ask("widget").hits, first.hits);
        }
    }

    #[test]
    fn limits_default_to_ten_and_cap_at_fifty() {
        let fx = Fx::new();
        for n in 0..60 {
            fx.put(
                &format!("docs/d{n:02}.md"),
                &format!("# Doc {n:02}\nwidget\n"),
            );
        }
        let default = fx.ask("widget");
        assert_eq!(default.hits.len(), DEFAULT_LIMIT);
        assert!(default.truncated);
        // A saturated search reports a lower bound, never an overstated total.
        assert!(default.total_matches > DEFAULT_LIMIT && default.total_matches <= 60);
        let few = fx
            .ask_with(QueryRequest {
                question: "widget".into(),
                limit: Some(3),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(few.hits.len(), 3);
        let huge = fx
            .ask_with(QueryRequest {
                question: "widget".into(),
                limit: Some(10_000),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(huge.hits.len(), MAX_LIMIT);
        assert!(huge.truncated);
        let zero = fx
            .ask_with(QueryRequest {
                question: "widget".into(),
                limit: Some(0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(zero.hits.len(), 1);
    }

    #[test]
    fn path_filters_confine_results_to_a_subtree() {
        let fx = Fx::new();
        fx.put("docs/a/x.md", "# X\nwidget\n");
        fx.put("docs/ab/z.md", "# Z\nwidget\n");
        fx.put("docs/a_b/y.md", "# Y\nwidget\n");
        fx.put("docs/axb/w.md", "# W\nwidget\n");
        let in_dir = |path: &str| -> Vec<String> {
            fx.ask_with(QueryRequest {
                question: "widget".into(),
                path: Some(path.into()),
                ..Default::default()
            })
            .unwrap()
            .hits
            .into_iter()
            .map(|h| h.path)
            .collect()
        };
        assert_eq!(in_dir("docs/a"), vec!["docs/a/x.md"]);
        assert_eq!(in_dir("./docs/a/"), vec!["docs/a/x.md"]);
        assert_eq!(in_dir("docs/a/x.md"), vec!["docs/a/x.md"]);
        // `_` is a literal, not a wildcard.
        assert_eq!(in_dir("docs/a_b"), vec!["docs/a_b/y.md"]);
    }

    #[test]
    fn unsafe_path_filters_are_rejected() {
        let fx = Fx::new();
        for path in [
            "../etc", "/abs", "a//b", "a/../b", "C:/x", "a\\b", " ", "a\0b",
        ] {
            let error = fx
                .ask_with(QueryRequest {
                    question: "widget".into(),
                    path: Some(path.into()),
                    ..Default::default()
                })
                .unwrap_err();
            assert!(matches!(error, QueryError::InvalidPath { .. }), "{path:?}");
            assert_eq!(error.code(), "path_invalid");
        }
    }

    #[test]
    fn hits_are_unique_by_id() {
        let fx = Fx::new();
        fx.put("docs/arch.md", ARCH);
        let response = fx.ask("docs/arch.md architecture scheduling storage scheduler");
        let mut seen = BTreeSet::new();
        assert!(response.hits.iter().all(|hit| seen.insert(hit.id.clone())));
        assert_eq!(response.hits.len(), 3);
    }

    #[test]
    fn answers_reflect_edits_without_a_watcher() {
        let fx = Fx::new();
        fx.put("docs/a.md", "# A\nalpha\n");
        assert_eq!(fx.ask("alpha").hits.len(), 1);
        fx.put("docs/a.md", "# A\nbeta now with more text\n");
        assert!(fx.ask("alpha").hits.is_empty());
        assert_eq!(fx.ask("beta").hits.len(), 1);
        fs::remove_file(fx.repo.join("docs/a.md")).unwrap();
        assert!(fx.ask("beta").hits.is_empty());
    }

    #[test]
    fn a_missing_manifest_answers_with_status_not_a_crawl() {
        let fx = Fx::new();
        fx.put("docs/a.md", "# A\nalpha\n");
        fs::remove_file(fx.repo.join(".qmt/knowledge.toml")).unwrap();
        let response = fx.ask("alpha");
        assert!(response.hits.is_empty());
        assert_eq!(response.status, ReconcileStatus::NotConfigured);
    }

    #[test]
    fn lifecycle_labels_are_reported_not_filtered() {
        let fx = Fx::new();
        fx.put(
            "docs/old.md",
            "---\nstatus: deprecated\n---\n# Old\nwidget\n",
        );
        let response = fx.ask("widget");
        assert_eq!(response.hits[0].status.as_deref(), Some("deprecated"));
    }

    #[test]
    fn excerpts_are_bounded_and_utf8_safe() {
        let long = "é".repeat(400);
        let out = excerpt(&long);
        assert!(out.ends_with('…'));
        assert!(out.len() <= EXCERPT_BYTES + '…'.len_utf8());
        assert_eq!(excerpt("  a \n b\t c "), "a b c");
    }

    #[test]
    fn tokenizing_splits_identifiers_and_drops_function_words() {
        assert_eq!(
            question_tokens("How does SessionStore::compact work").unwrap(),
            vec!["sessionstore", "session", "store", "compact", "work"]
        );
        assert_eq!(
            question_tokens("HTTPServer").unwrap(),
            vec!["httpserver", "http", "server"]
        );
        // All function words: still searchable rather than empty.
        assert_eq!(
            question_tokens("how to be").unwrap(),
            vec!["how", "to", "be"]
        );
        assert!(question_tokens("a ? !").is_none());
    }
}

//! Document adapters: plain Markdown and workspace dotagents memories.
//!
//! Both adapters reuse the existing dotagents frontmatter conventions
//! (scalars, quoted values, comma-separated lists and JSON-array lists; see
//! [`crate::dotagents::parse_frontmatter_markdown`]) instead of introducing a
//! second parser.
//!
//! * **Plain Markdown** needs no frontmatter. When present, the recognized
//!   optional keys are `id`, `title`, `description`, `tags`, `status`,
//!   `knowledge_sources` and `knowledge_paths`. Every other key is retained in
//!   [`ParsedDocument::extensions`] and otherwise ignored (so, for example,
//!   `enabled: false` has no effect on an ordinary document).
//! * **Workspace memories** (`.agents/memories/<name>.md`) additionally keep
//!   their legacy semantics: parsing and validation are delegated to
//!   [`crate::dotagents::parse_memory_document`], so `enabled: false` excludes
//!   the memory, a memory with neither body nor `content` is invalid, and
//!   `content` metadata stands in for an empty body — exactly as the protocol
//!   loader treats them. The repository reads these files directly and never
//!   touches the protocol import tables.
//!
//! A document that cannot be used is excluded with a source-specific
//! [`DocumentDiagnostic`]; siblings are unaffected. Diagnostics carry only the
//! repository-relative path.
//!
//! This module parses text. It performs no IO, resolves no identity (that is
//! the job of the identity pass) and verifies no `knowledge_sources` or
//! `knowledge_paths` selector; it only records them.

use crate::dotagents::{
    DotagentsLayer, DotagentsSource, field_list, field_string, parse_frontmatter_markdown,
    parse_memory_document,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Frontmatter keys the repository projection understands, in canonical form.
/// Matching is case-insensitive, like the dotagents parsers.
pub const RECOGNIZED_KEYS: &[&str] = &[
    "id",
    "title",
    "description",
    "tags",
    "status",
    "knowledge_sources",
    "knowledge_paths",
];

/// Which adapter handled a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocumentKind {
    /// Ordinary Markdown, frontmatter optional.
    Markdown,
    /// A workspace dotagents memory with legacy enabled/body semantics.
    WorkspaceMemory,
}

/// Choose the adapter for a root-relative, forward-slash path.
///
/// Only flat files directly under `.agents/memories/` are workspace memories,
/// matching the protocol loader's layout (`memories/` holds flat files).
pub fn classify(relative_path: &str) -> DocumentKind {
    workspace_memory_stem(relative_path)
        .map(|_| DocumentKind::WorkspaceMemory)
        .unwrap_or(DocumentKind::Markdown)
}

fn workspace_memory_stem(relative_path: &str) -> Option<&str> {
    let name = relative_path.strip_prefix(".agents/memories/")?;
    if name.contains('/') {
        return None;
    }
    name.strip_suffix(".md").filter(|stem| !stem.is_empty())
}

/// Optional lifecycle label. `status` is an open vocabulary; only `draft` and
/// `deprecated` (case-insensitive) affect delivery eligibility, and an absent or
/// any other label means usable. Lifecycle never alters evidence state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Usable,
    Draft,
    Deprecated,
}

/// Severity of a [`DocumentDiagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    /// Expected exclusion, e.g. a memory that is deliberately disabled.
    Info,
    /// The document is usable but part of its metadata was ignored.
    Warning,
    /// The document is excluded.
    Error,
}

/// A source-specific diagnostic. Never contains absolute paths.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DocumentDiagnostic {
    /// Repository-relative source path.
    pub path: String,
    pub severity: DiagnosticSeverity,
    /// Stable machine-readable code.
    pub code: &'static str,
    pub message: String,
}

impl DocumentDiagnostic {
    fn new(
        path: &str,
        severity: DiagnosticSeverity,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.to_string(),
            severity,
            code,
            message: message.into(),
        }
    }
}

/// Diagnostic codes emitted by the document adapters.
pub mod codes {
    pub const NOT_UTF8: &str = "document_not_utf8";
    pub const FRONTMATTER_MALFORMED: &str = "frontmatter_malformed";
    pub const FIELD_INVALID: &str = "frontmatter_field_invalid";
    pub const FIELD_IGNORED: &str = "frontmatter_field_ignored";
    pub const MEMORY_INVALID: &str = "memory_invalid";
    pub const MEMORY_DISABLED: &str = "memory_disabled";
}

/// A successfully parsed document, before identity and section normalization.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDocument {
    /// Repository-relative path (forward slashes).
    pub relative_path: String,
    pub kind: DocumentKind,
    /// The `id` frontmatter value, if one was written. Path-derived identity is
    /// assigned later, never here.
    pub explicit_id: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    /// Raw lifecycle label.
    pub status: Option<String>,
    /// Declared evidence dependencies (`path` or `path#qualified::symbol`),
    /// recorded verbatim and not yet validated.
    pub knowledge_sources: Vec<String>,
    /// Declared applicability globs, recorded verbatim and not yet validated.
    pub knowledge_paths: Vec<String>,
    /// Frontmatter keys this projection does not interpret, with their original
    /// spelling and parsed values.
    pub extensions: BTreeMap<String, Value>,
    pub had_frontmatter: bool,
    /// Markdown body with the frontmatter fence removed.
    pub body: String,
    /// 1-based line of `body`'s first line within the original file. When the
    /// body is a legacy memory's `content` metadata ([`body_from_metadata`])
    /// there is no body line, and this is 1.
    ///
    /// [`body_from_metadata`]: Self::body_from_metadata
    pub body_start_line: usize,
    /// The body came from a legacy memory's `content` metadata because its
    /// Markdown body was empty.
    pub body_from_metadata: bool,
    /// Number of lines in the original file (a final unterminated line counts).
    pub line_count: usize,
    /// Non-fatal diagnostics (ignored metadata).
    pub diagnostics: Vec<DocumentDiagnostic>,
}

impl Lifecycle {
    /// The lifecycle an author's `status` label means.
    pub fn from_status(status: Option<&str>) -> Self {
        match status.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("draft") => Self::Draft,
            Some("deprecated") => Self::Deprecated,
            _ => Self::Usable,
        }
    }

    /// Whether a document in this lifecycle may become an automatic card.
    /// Draft and deprecated documents stay explicitly retrievable but are
    /// never delivered unasked.
    pub fn allows_automatic_delivery(self) -> bool {
        matches!(self, Self::Usable)
    }
}

impl ParsedDocument {
    pub fn lifecycle(&self) -> Lifecycle {
        Lifecycle::from_status(self.status.as_deref())
    }
}

/// Outcome of parsing one source file.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseOutcome {
    Document(Box<ParsedDocument>),
    /// The document is excluded; see the diagnostic for why.
    Excluded(DocumentDiagnostic),
}

/// Parse one selected file. `relative_path` must already be root-relative with
/// forward slashes.
pub fn parse_document(relative_path: &str, bytes: &[u8]) -> ParseOutcome {
    let kind = classify(relative_path);

    let Ok(text) = std::str::from_utf8(bytes) else {
        return ParseOutcome::Excluded(DocumentDiagnostic::new(
            relative_path,
            DiagnosticSeverity::Error,
            codes::NOT_UTF8,
            "document is not valid UTF-8",
        ));
    };

    let parsed = match parse_frontmatter_markdown(text) {
        Ok(parsed) => parsed,
        Err(error) => {
            return ParseOutcome::Excluded(DocumentDiagnostic::new(
                relative_path,
                DiagnosticSeverity::Error,
                codes::FRONTMATTER_MALFORMED,
                error.message,
            ));
        }
    };

    // Legacy memories keep their protocol semantics by delegating to the
    // protocol parser rather than reimplementing it.
    let mut body = parsed.body.clone();
    let mut body_from_metadata = false;
    if let Some(stem) = workspace_memory_stem(relative_path) {
        let source = DotagentsSource::entry(
            DotagentsLayer::Workspace,
            PathBuf::from(relative_path),
            stem.to_string(),
        );
        match parse_memory_document(text, stem, source) {
            Err(error) => {
                return ParseOutcome::Excluded(DocumentDiagnostic::new(
                    relative_path,
                    DiagnosticSeverity::Error,
                    codes::MEMORY_INVALID,
                    error.message,
                ));
            }
            Ok(memory) if !memory.enabled => {
                return ParseOutcome::Excluded(DocumentDiagnostic::new(
                    relative_path,
                    DiagnosticSeverity::Info,
                    codes::MEMORY_DISABLED,
                    "memory is disabled (`enabled: false`)",
                ));
            }
            Ok(memory) => {
                if memory.body.trim().is_empty()
                    && let Some(content) = memory.content.as_deref()
                    && !content.trim().is_empty()
                {
                    body = content.trim().to_string();
                    body_from_metadata = true;
                }
            }
        }
    }

    let (recognized, extensions) = partition(&parsed.fields);
    let mut diagnostics: Vec<DocumentDiagnostic> = Vec::new();

    // `id` decides identity, so an unusable value excludes the document rather
    // than silently falling back to a path-derived identity.
    if matches!(
        recognized.get("id"),
        Some(Value::Array(_) | Value::Object(_))
    ) {
        return ParseOutcome::Excluded(DocumentDiagnostic::new(
            relative_path,
            DiagnosticSeverity::Error,
            codes::FIELD_INVALID,
            "frontmatter `id` must be a single scalar value",
        ));
    }

    let explicit_id = field_string(&recognized, "id");
    let title = scalar(&recognized, "title", relative_path, &mut diagnostics);
    let description = scalar(&recognized, "description", relative_path, &mut diagnostics);
    let status = scalar(&recognized, "status", relative_path, &mut diagnostics);
    let tags = list(&recognized, "tags", relative_path, &mut diagnostics);
    let knowledge_sources = list(
        &recognized,
        "knowledge_sources",
        relative_path,
        &mut diagnostics,
    );
    let knowledge_paths = list(
        &recognized,
        "knowledge_paths",
        relative_path,
        &mut diagnostics,
    );

    let body_start_line = if body_from_metadata {
        1
    } else {
        body_start_line(text, &body)
    };

    diagnostics.sort();
    ParseOutcome::Document(Box::new(ParsedDocument {
        relative_path: relative_path.to_string(),
        kind,
        explicit_id,
        title,
        description,
        tags,
        status,
        knowledge_sources,
        knowledge_paths,
        extensions,
        had_frontmatter: parsed.had_frontmatter,
        body,
        body_start_line,
        body_from_metadata,
        line_count: text.split_inclusive('\n').count(),
        diagnostics,
    }))
}

/// Result of parsing a set of selected files.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedBatch {
    /// Usable documents sorted by relative path.
    pub documents: Vec<ParsedDocument>,
    /// Every exclusion and ignored-metadata diagnostic, sorted.
    pub diagnostics: Vec<DocumentDiagnostic>,
}

/// Parse many files independently. A bad document is excluded with a
/// diagnostic and never affects its siblings; the result does not depend on
/// input order.
pub fn parse_documents<'a>(inputs: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> ParsedBatch {
    let mut batch = ParsedBatch::default();
    for (path, bytes) in inputs {
        match parse_document(path, bytes) {
            ParseOutcome::Document(document) => {
                batch
                    .diagnostics
                    .extend(document.diagnostics.iter().cloned());
                batch.documents.push(*document);
            }
            ParseOutcome::Excluded(diagnostic) => batch.diagnostics.push(diagnostic),
        }
    }
    batch
        .documents
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    batch.diagnostics.sort();
    batch.diagnostics.dedup();
    batch
}

/// Split frontmatter into recognized keys (re-keyed to the canonical spelling,
/// first match wins in sorted key order) and retained extensions.
fn partition(
    fields: &BTreeMap<String, Value>,
) -> (BTreeMap<String, Value>, BTreeMap<String, Value>) {
    let mut recognized: BTreeMap<String, Value> = BTreeMap::new();
    let mut extensions: BTreeMap<String, Value> = BTreeMap::new();
    for (key, value) in fields {
        match RECOGNIZED_KEYS
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(key))
        {
            Some(canonical) => {
                recognized
                    .entry((*canonical).to_string())
                    .or_insert_with(|| value.clone());
            }
            None => {
                extensions.insert(key.clone(), value.clone());
            }
        }
    }
    (recognized, extensions)
}

fn scalar(
    recognized: &BTreeMap<String, Value>,
    key: &str,
    path: &str,
    diagnostics: &mut Vec<DocumentDiagnostic>,
) -> Option<String> {
    if matches!(
        recognized.get(key),
        Some(Value::Array(_) | Value::Object(_))
    ) {
        diagnostics.push(ignored(path, key, "a single scalar value"));
        return None;
    }
    field_string(recognized, key)
}

fn list(
    recognized: &BTreeMap<String, Value>,
    key: &str,
    path: &str,
    diagnostics: &mut Vec<DocumentDiagnostic>,
) -> Vec<String> {
    if matches!(
        recognized.get(key),
        Some(Value::Number(_) | Value::Bool(_) | Value::Object(_))
    ) {
        diagnostics.push(ignored(path, key, "a list or comma-separated string"));
        return Vec::new();
    }
    let mut seen: Vec<String> = Vec::new();
    for item in field_list(recognized, key).unwrap_or_default() {
        let item = item.trim();
        if !item.is_empty() && !seen.iter().any(|existing| existing == item) {
            seen.push(item.to_string());
        }
    }
    seen
}

fn ignored(path: &str, key: &str, expected: &str) -> DocumentDiagnostic {
    DocumentDiagnostic::new(
        path,
        DiagnosticSeverity::Warning,
        codes::FIELD_IGNORED,
        format!("frontmatter `{key}` was ignored; expected {expected}"),
    )
}

/// 1-based line of the body's first line in the original text. The frontmatter
/// parser returns the body as a suffix of the input, so the lines before it are
/// the lines consumed by the BOM, fences and frontmatter.
fn body_start_line(text: &str, body: &str) -> usize {
    if !text.ends_with(body) {
        return 1;
    }
    let prefix = &text.as_bytes()[..text.len() - body.len()];
    1 + prefix.iter().filter(|byte| **byte == b'\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(path: &str, text: &str) -> ParsedDocument {
        match parse_document(path, text.as_bytes()) {
            ParseOutcome::Document(document) => *document,
            ParseOutcome::Excluded(d) => panic!("expected a document, got {d:?}"),
        }
    }

    fn excluded(path: &str, text: &str) -> DocumentDiagnostic {
        match parse_document(path, text.as_bytes()) {
            ParseOutcome::Excluded(d) => d,
            ParseOutcome::Document(d) => panic!("expected exclusion, got {d:?}"),
        }
    }

    /// The first line of `body` really is at `body_start_line` in `text`.
    fn assert_coordinates(text: &str, document: &ParsedDocument) {
        let first = document.body.lines().next().unwrap_or_default();
        let at = text
            .lines()
            .nth(document.body_start_line - 1)
            .unwrap_or_default();
        assert_eq!(at.trim_end_matches('\r'), first.trim_end_matches('\r'));
    }

    #[test]
    fn plain_markdown_without_frontmatter_needs_nothing() {
        let text = "# Runtime\n\nHow the runtime works.\n";
        let d = doc("docs/runtime.md", text);
        assert_eq!(d.kind, DocumentKind::Markdown);
        assert!(!d.had_frontmatter);
        assert_eq!(d.body, text);
        assert_eq!(d.body_start_line, 1);
        assert!(d.explicit_id.is_none() && d.title.is_none() && d.status.is_none());
        assert!(d.tags.is_empty() && d.extensions.is_empty() && d.diagnostics.is_empty());
        assert_eq!(d.lifecycle(), Lifecycle::Usable);
    }

    #[test]
    fn empty_document_is_valid_markdown() {
        let d = doc("docs/empty.md", "");
        assert_eq!(d.body, "");
        let d = doc("docs/meta-only.md", "---\ntitle: Only metadata\n---\n");
        assert_eq!(d.title.as_deref(), Some("Only metadata"));
        assert_eq!(d.body, "");
    }

    #[test]
    fn bom_is_ignored_and_line_numbers_stay_correct() {
        let text = "\u{feff}# Title\nbody\n";
        let d = doc("docs/a.md", text);
        assert_eq!(d.body, "# Title\nbody\n");
        assert_eq!(d.body_start_line, 1);

        let text = "\u{feff}---\ntitle: T\n---\n# H\n";
        let d = doc("docs/b.md", text);
        assert_eq!(d.title.as_deref(), Some("T"));
        assert_eq!(d.body_start_line, 4);
        assert_coordinates(text, &d);
    }

    #[test]
    fn all_recognized_keys_in_every_supported_list_form() {
        let text = "---\n\
            id: runtime-overview\n\
            title: \"Runtime: overview\"\n\
            description: How, and why, the runtime works\n\
            status: Draft\n\
            tags: [runtime, \"session\"]\n\
            knowledge_sources: crates/agent/src/lib.rs, crates/agent/src/api/agent.rs#AgentBuilder::build\n\
            knowledge_paths:\n  - crates/agent/src/**\n  - docs/runtime.md\n\
            ---\n\
            # Runtime\n";
        let d = doc("docs/runtime.md", text);
        assert_eq!(d.explicit_id.as_deref(), Some("runtime-overview"));
        assert_eq!(d.title.as_deref(), Some("Runtime: overview"));
        // Commas inside a scalar description are preserved.
        assert_eq!(
            d.description.as_deref(),
            Some("How, and why, the runtime works")
        );
        assert_eq!(d.status.as_deref(), Some("Draft"));
        assert_eq!(d.lifecycle(), Lifecycle::Draft);
        assert_eq!(d.tags, vec!["runtime", "session"]);
        assert_eq!(
            d.knowledge_sources,
            vec![
                "crates/agent/src/lib.rs",
                "crates/agent/src/api/agent.rs#AgentBuilder::build"
            ]
        );
        assert_eq!(
            d.knowledge_paths,
            vec!["crates/agent/src/**", "docs/runtime.md"]
        );
        assert!(d.extensions.is_empty());
        assert!(d.diagnostics.is_empty());
        assert_eq!(d.body, "# Runtime\n");
        assert_coordinates(text, &d);
    }

    #[test]
    fn json_array_and_csv_tags_agree() {
        let a = doc("a.md", "---\ntags: [\"x\", \"y\"]\n---\nb\n");
        let b = doc("b.md", "---\ntags: x, y\n---\nb\n");
        let c = doc("c.md", "---\ntags: [x, y]\n---\nb\n");
        assert_eq!(a.tags, b.tags);
        assert_eq!(a.tags, c.tags);
    }

    #[test]
    fn lists_are_trimmed_deduplicated_and_ordered() {
        let d = doc(
            "a.md",
            "---\nknowledge_sources: [\" a.rs \", \"b.rs\", \"a.rs\", \"\"]\n---\nx\n",
        );
        assert_eq!(d.knowledge_sources, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn keys_are_case_insensitive_and_canonicalized() {
        let d = doc(
            "a.md",
            "---\nTitle: Hello\nKNOWLEDGE_PATHS: src/**\n---\nx\n",
        );
        assert_eq!(d.title.as_deref(), Some("Hello"));
        assert_eq!(d.knowledge_paths, vec!["src/**"]);
        assert!(d.extensions.is_empty());
    }

    #[test]
    fn unknown_keys_are_retained_and_ignored() {
        let text = "---\n\
            title: Known\n\
            owner: platform-team\n\
            reviewed: 2026-01-01\n\
            priority: 3\n\
            enabled: false\n\
            ---\n\
            body\n";
        let d = doc("docs/a.md", text);
        assert_eq!(d.title.as_deref(), Some("Known"));
        assert_eq!(d.extensions.len(), 4);
        assert_eq!(d.extensions["owner"], Value::String("platform-team".into()));
        assert_eq!(d.extensions["priority"], Value::from(3));
        // `enabled` is a legacy-memory key; on ordinary Markdown it is inert.
        assert_eq!(d.extensions["enabled"], Value::Bool(false));
        assert_eq!(d.kind, DocumentKind::Markdown);
        assert_eq!(d.body, "body\n");
    }

    #[test]
    fn body_coordinates_with_crlf_and_blank_separator() {
        let text = "---\r\ntitle: T\r\n---\r\n\r\n# Heading\r\ntext\r\n";
        let d = doc("a.md", text);
        assert_eq!(d.title.as_deref(), Some("T"));
        assert_coordinates(text, &d);
        assert_eq!(d.body_start_line, 5);

        let text = "---\ntitle: T\n---\n\n\n# Heading\n";
        let d = doc("a.md", text);
        assert_coordinates(text, &d);
    }

    #[test]
    fn malformed_frontmatter_excludes_only_that_document() {
        for (name, text) in [
            ("unterminated", "---\ntitle: Oops\n# never closed\n"),
            ("no-colon", "---\nthis is not a mapping\n---\nbody\n"),
            ("stray-item", "---\n- orphan\n---\nbody\n"),
        ] {
            let path = format!("docs/{name}.md");
            let d = excluded(&path, text);
            assert_eq!(d.code, codes::FRONTMATTER_MALFORMED, "{name}");
            assert_eq!(d.severity, DiagnosticSeverity::Error);
            assert_eq!(d.path, path);
        }
    }

    #[test]
    fn batch_isolates_failures_and_is_order_independent() {
        let good_plain = "# Plain\n".as_bytes();
        let good_unknown = "---\nowner: me\n---\nx\n".as_bytes();
        let legacy = "---\ntitle: Mem\n---\nremember\n".as_bytes();
        let broken = "---\ntitle: broken\n".as_bytes();
        let bad_utf8: &[u8] = &[0xff, 0xfe, 0x00];

        let forward = vec![
            ("docs/plain.md", good_plain),
            ("docs/unknown.md", good_unknown),
            (".agents/memories/mem.md", legacy),
            ("docs/broken.md", broken),
            ("docs/binary.md", bad_utf8),
        ];
        let mut reversed = forward.clone();
        reversed.reverse();

        let a = parse_documents(forward);
        let b = parse_documents(reversed);
        assert_eq!(a, b);

        let paths: Vec<_> = a
            .documents
            .iter()
            .map(|d| d.relative_path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec![
                ".agents/memories/mem.md",
                "docs/plain.md",
                "docs/unknown.md"
            ]
        );
        let kinds: Vec<_> = a.documents.iter().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DocumentKind::WorkspaceMemory,
                DocumentKind::Markdown,
                DocumentKind::Markdown
            ]
        );
        let codes: Vec<_> = a
            .diagnostics
            .iter()
            .map(|d| (d.path.as_str(), d.code))
            .collect();
        assert_eq!(
            codes,
            vec![
                ("docs/binary.md", codes::NOT_UTF8),
                ("docs/broken.md", codes::FRONTMATTER_MALFORMED)
            ]
        );
    }

    #[test]
    fn invalid_id_excludes_but_other_bad_fields_only_warn() {
        let d = excluded("a.md", "---\nid: [a, b]\n---\nbody\n");
        assert_eq!(d.code, codes::FIELD_INVALID);

        let d = doc(
            "a.md",
            "---\ntitle: [x, y]\ndescription: ok\ntags: 5\nstatus: [a]\n---\nbody\n",
        );
        assert!(d.title.is_none() && d.status.is_none());
        assert_eq!(d.description.as_deref(), Some("ok"));
        assert!(d.tags.is_empty());
        assert_eq!(d.diagnostics.len(), 3);
        assert!(
            d.diagnostics.iter().all(
                |x| x.code == codes::FIELD_IGNORED && x.severity == DiagnosticSeverity::Warning
            )
        );
        // The document is still usable.
        assert_eq!(d.body, "body\n");
    }

    #[test]
    fn empty_and_null_values_are_simply_absent() {
        let d = doc(
            "a.md",
            "---\nid:\ntitle: \"\"\ntags: []\nstatus: null\n---\nx\n",
        );
        assert!(d.explicit_id.is_none() && d.title.is_none() && d.status.is_none());
        assert!(d.tags.is_empty());
        assert!(d.diagnostics.is_empty());
    }

    #[test]
    fn numeric_ids_are_kept_as_text() {
        let d = doc("a.md", "---\nid: 42\n---\nx\n");
        assert_eq!(d.explicit_id.as_deref(), Some("42"));
    }

    #[test]
    fn lifecycle_is_open_vocabulary_and_case_insensitive() {
        for (status, expected) in [
            ("draft", Lifecycle::Draft),
            ("DRAFT", Lifecycle::Draft),
            ("Deprecated", Lifecycle::Deprecated),
            ("stable", Lifecycle::Usable),
            ("in-review", Lifecycle::Usable),
        ] {
            let d = doc("a.md", &format!("---\nstatus: {status}\n---\nx\n"));
            assert_eq!(d.lifecycle(), expected, "{status}");
            // The raw label is retained.
            assert_eq!(d.status.as_deref(), Some(status));
        }
        assert_eq!(doc("a.md", "x").lifecycle(), Lifecycle::Usable);
    }

    #[test]
    fn invalid_utf8_is_excluded_with_a_relative_diagnostic() {
        let d = match parse_document("docs/bin.md", &[0x80, 0x81]) {
            ParseOutcome::Excluded(d) => d,
            other => panic!("{other:?}"),
        };
        assert_eq!(d.code, codes::NOT_UTF8);
        assert_eq!(d.path, "docs/bin.md");
    }

    // ── workspace memories ───────────────────────────────────────────────

    #[test]
    fn only_flat_agents_memories_files_use_the_legacy_adapter() {
        assert_eq!(
            classify(".agents/memories/runtime.md"),
            DocumentKind::WorkspaceMemory
        );
        for markdown in [
            ".agents/memories/nested/runtime.md",
            ".agents/memories/.md",
            ".agents/memories/notes.txt",
            ".agents/memory/runtime.md",
            "agents/memories/runtime.md",
            "docs/.agents/memories/runtime.md",
            "docs/runtime.md",
        ] {
            assert_eq!(classify(markdown), DocumentKind::Markdown, "{markdown}");
        }
    }

    #[test]
    fn legacy_memory_fields_match_the_protocol_parser() {
        let text = "---\n\
            id: arch-notes\n\
            title: Architecture notes\n\
            tags: runtime, session\n\
            importance: high\n\
            ---\n\
            The runtime owns sessions.\n";
        let d = doc(".agents/memories/arch.md", text);
        assert_eq!(d.kind, DocumentKind::WorkspaceMemory);

        let source = DotagentsSource::entry(
            DotagentsLayer::Workspace,
            PathBuf::from(".agents/memories/arch.md"),
            "arch".to_string(),
        );
        let legacy = parse_memory_document(text, "arch", source).unwrap();
        assert_eq!(d.explicit_id.as_deref(), Some(legacy.id.as_str()));
        assert_eq!(d.title, legacy.title);
        assert_eq!(d.tags, legacy.tags);
        assert_eq!(d.body, legacy.body);
        assert_eq!(d.extensions["importance"], Value::String("high".into()));
        assert_coordinates(text, &d);
    }

    #[test]
    fn legacy_memory_without_id_has_no_explicit_id() {
        // The legacy loader falls back to the file stem; the repository must
        // not treat that stem as an explicit identity that survives moves.
        let d = doc(".agents/memories/runtime.md", "---\ntitle: T\n---\nbody\n");
        assert!(d.explicit_id.is_none());
        let d = doc(".agents/memories/runtime.md", "no frontmatter at all\n");
        assert!(d.explicit_id.is_none());
        assert_eq!(d.body, "no frontmatter at all\n");
    }

    #[test]
    fn legacy_enabled_false_excludes_but_only_for_memories() {
        let text = "---\nenabled: false\ntitle: Off\n---\nbody\n";
        let d = excluded(".agents/memories/off.md", text);
        assert_eq!(d.code, codes::MEMORY_DISABLED);
        assert_eq!(d.severity, DiagnosticSeverity::Info);
        assert_eq!(d.path, ".agents/memories/off.md");

        // Legacy accepts `no` as false too.
        let d = excluded(".agents/memories/off.md", "---\nenabled: no\n---\nbody\n");
        assert_eq!(d.code, codes::MEMORY_DISABLED);

        // The same text elsewhere is ordinary Markdown and stays indexed.
        let d = doc("docs/off.md", text);
        assert_eq!(d.kind, DocumentKind::Markdown);
        assert_eq!(d.title.as_deref(), Some("Off"));
    }

    #[test]
    fn legacy_empty_memory_is_invalid_but_empty_markdown_is_not() {
        let d = excluded(".agents/memories/blank.md", "---\ntitle: Blank\n---\n  \n");
        assert_eq!(d.code, codes::MEMORY_INVALID);
        assert_eq!(d.severity, DiagnosticSeverity::Error);
        assert!(
            doc("docs/blank.md", "---\ntitle: Blank\n---\n  \n")
                .body
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn legacy_content_metadata_stands_in_for_an_empty_body() {
        let d = doc(
            ".agents/memories/inline.md",
            "---\ncontent: Remember the cache\n---\n",
        );
        assert!(d.body_from_metadata);
        assert_eq!(d.body, "Remember the cache");
        assert_eq!(d.body_start_line, 1);

        // A real body wins over `content`.
        let d = doc(
            ".agents/memories/both.md",
            "---\ncontent: ignored\n---\nreal body\n",
        );
        assert!(!d.body_from_metadata);
        assert_eq!(d.body, "real body\n");
    }

    #[test]
    fn legacy_memories_accept_the_optional_repository_keys() {
        let d = doc(
            ".agents/memories/m.md",
            "---\ndescription: D\nstatus: deprecated\nknowledge_sources: a.rs\nknowledge_paths: src/**\n---\nbody\n",
        );
        assert_eq!(d.description.as_deref(), Some("D"));
        assert_eq!(d.lifecycle(), Lifecycle::Deprecated);
        assert_eq!(d.knowledge_sources, vec!["a.rs"]);
        assert_eq!(d.knowledge_paths, vec!["src/**"]);
    }

    #[test]
    fn legacy_malformed_frontmatter_is_isolated() {
        let d = excluded(".agents/memories/bad.md", "---\ntitle: never closed\n");
        assert_eq!(d.code, codes::FRONTMATTER_MALFORMED);
    }

    #[test]
    fn diagnostics_never_contain_absolute_paths() {
        let text = "---\nid: [a]\n---\nbody\n";
        let d = excluded("docs/a.md", text);
        assert!(!d.message.contains('/') || !d.message.starts_with('/'));
        let rendered = format!("{d:?}");
        assert!(!rendered.contains("/Users") && !rendered.contains("/var/"));
    }
}

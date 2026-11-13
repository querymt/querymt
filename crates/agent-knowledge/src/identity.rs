//! Document identity, sections, and original source coordinates.
//!
//! This pass turns parsed documents into addressable records:
//!
//! * **Document identity.** An explicit frontmatter `id` survives moves and is
//!   rendered `id:<id>`; otherwise identity derives from the repository-relative
//!   path and is rendered `path:<relative-path>`. The prefixes keep the two
//!   namespaces disjoint, so a path can never collide with an explicit ID.
//!   Explicit IDs are compared ASCII-case-insensitively; two documents that
//!   claim the same ID are *both* excluded with diagnostics, because picking a
//!   winner would make retrieval depend on discovery order.
//! * **Sections.** A document is split at ATX headings (`#` to `######`) into
//!   non-overlapping sections: an optional preamble before the first heading,
//!   then one section per heading running to the next heading of any level.
//!   Hierarchy is carried by the heading *breadcrumb*, not by nesting, so each
//!   line belongs to exactly one section. Headings inside fenced code blocks
//!   are ignored. Setext (underlined) headings are not recognized; such text
//!   stays in the enclosing section.
//! * **Stable section keys.** A [`SectionKey`] is `document ID + heading
//!   breadcrumb + occurrence`, where occurrence disambiguates repeated
//!   breadcrumbs within one document. Renaming or reordering duplicate headings
//!   can invalidate a key; callers must report that instead of returning a
//!   different section.
//! * **Original coordinates.** Every section carries 1-based, inclusive line
//!   numbers in the original file (frontmatter included), so citations point at
//!   the file a reader opens rather than at a derived body.
//!
//! Nothing here performs IO, and nothing machine-local (absolute paths, cache
//! keys, timestamps) enters an ID or key.

use super::document::{
    DiagnosticSeverity, DocumentDiagnostic, DocumentKind, ParsedBatch, ParsedDocument,
};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

const ID_PREFIX: &str = "id:";
const PATH_PREFIX: &str = "path:";
const MAX_EXPLICIT_ID_BYTES: usize = 128;
/// ATX headings go up to level six.
const MAX_HEADING_LEVEL: usize = 6;

/// Diagnostic codes emitted by the identity pass.
pub mod codes {
    pub const ID_INVALID: &str = "document_id_invalid";
    pub const ID_DUPLICATE: &str = "document_id_duplicate";
}

// ─── Document identity ───────────────────────────────────────────────────────

/// A portable document identity: `id:<explicit-id>` or `path:<relative-path>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentId(String);

/// How a [`DocumentId`] was derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdOrigin {
    /// From the document's frontmatter `id`; survives moves.
    Explicit,
    /// From the repository-relative path; changes when the file moves.
    PathDerived,
}

impl DocumentId {
    /// Path-derived identity for a repository-relative, forward-slash path.
    pub fn from_path(relative_path: &str) -> Self {
        Self(format!("{PATH_PREFIX}{relative_path}"))
    }

    /// Explicit identity from an already validated, normalized ID.
    fn explicit(normalized: &str) -> Self {
        Self(format!("{ID_PREFIX}{normalized}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The normalized explicit ID without its `id:` prefix, if this identity is
    /// explicit.
    pub fn explicit_value(&self) -> Option<&str> {
        self.0.strip_prefix(ID_PREFIX)
    }

    pub fn origin(&self) -> IdOrigin {
        if self.0.starts_with(ID_PREFIX) {
            IdOrigin::Explicit
        } else {
            IdOrigin::PathDerived
        }
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for DocumentId {
    type Err = KeyParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if let Some(rest) = input.strip_prefix(ID_PREFIX) {
            return match normalize_explicit_id(rest) {
                Ok(normalized) if normalized == rest => Ok(Self::explicit(rest)),
                _ => Err(KeyParseError("malformed explicit document ID")),
            };
        }
        match input.strip_prefix(PATH_PREFIX) {
            Some(path) if !path.is_empty() => Ok(Self::from_path(path)),
            _ => Err(KeyParseError(
                "document ID must start with `id:` or `path:`",
            )),
        }
    }
}

/// Validate and normalize an explicit ID: trimmed, 1–128 bytes, no whitespace,
/// control characters or `#`, ASCII-lowercased.
pub(super) fn normalize_explicit_id(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("`id` is empty");
    }
    if trimmed.len() > MAX_EXPLICIT_ID_BYTES {
        return Err("`id` is longer than 128 bytes");
    }
    if trimmed
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '#')
    {
        return Err("`id` must not contain whitespace, control characters or `#`");
    }
    Ok(trimmed.to_ascii_lowercase())
}

// ─── Section keys ────────────────────────────────────────────────────────────

/// Why a key or ID string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyParseError(&'static str);

impl fmt::Display for KeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for KeyParseError {}

/// Stable section identity: document ID, heading breadcrumb, occurrence.
///
/// Rendered as `<document-id>#<crumb>[><crumb>...][~<occurrence>]`, where
/// occurrence is omitted when zero and an empty fragment names the preamble.
/// Crumbs percent-encode `%`, `#`, `>`, `~` and control characters; an empty
/// heading title is written `%20`. Parsing accepts only the canonical rendering.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SectionKey {
    pub document_id: DocumentId,
    /// Titles from the outermost enclosing heading to this section's own.
    /// Empty for the preamble.
    pub breadcrumb: Vec<String>,
    /// 0-based index among sections of this document with the same breadcrumb.
    pub occurrence: u32,
}

impl fmt::Display for SectionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#", self.document_id)?;
        for (index, title) in self.breadcrumb.iter().enumerate() {
            if index > 0 {
                f.write_str(">")?;
            }
            f.write_str(&encode_segment(title))?;
        }
        if self.occurrence > 0 {
            write!(f, "~{}", self.occurrence)?;
        }
        Ok(())
    }
}

impl FromStr for SectionKey {
    type Err = KeyParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        // Encoded fragments never contain `#`, so the last one is the separator
        // even when a path-derived document ID contains `#`.
        let (document, fragment) = input
            .rsplit_once('#')
            .ok_or(KeyParseError("section key has no `#` fragment"))?;
        let document_id: DocumentId = document.parse()?;

        let (crumbs, occurrence) = match fragment.rsplit_once('~') {
            Some((crumbs, occurrence)) => (
                crumbs,
                occurrence
                    .parse::<u32>()
                    .map_err(|_| KeyParseError("section occurrence is not a number"))?,
            ),
            None => (fragment, 0),
        };
        let breadcrumb = if crumbs.is_empty() && occurrence == 0 {
            Vec::new()
        } else {
            crumbs
                .split('>')
                .map(decode_segment)
                .collect::<Result<Vec<_>, _>>()?
        };

        let key = Self {
            document_id,
            breadcrumb,
            occurrence,
        };
        if key.to_string() != input {
            return Err(KeyParseError("section key is not in canonical form"));
        }
        Ok(key)
    }
}

fn encode_segment(title: &str) -> String {
    if title.is_empty() {
        return "%20".to_string();
    }
    let mut out = String::with_capacity(title.len());
    for c in title.chars() {
        if matches!(c, '%' | '#' | '>' | '~') || c.is_control() {
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn decode_segment(segment: &str) -> Result<String, KeyParseError> {
    if segment == "%20" {
        return Ok(String::new());
    }
    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = segment
                .get(index + 1..index + 3)
                .ok_or(KeyParseError("truncated percent escape"))?;
            let byte =
                u8::from_str_radix(hex, 16).map_err(|_| KeyParseError("invalid percent escape"))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| KeyParseError("section key is not valid UTF-8"))
}

// ─── Sections ────────────────────────────────────────────────────────────────

/// One non-overlapping section of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// 0-based position within the document.
    pub ordinal: usize,
    pub key: SectionKey,
    /// Heading level `1..=6`; `0` for the preamble (and for a body that came
    /// from legacy `content` metadata).
    pub level: u8,
    /// Heading text with inline whitespace collapsed; `None` for the preamble.
    pub title: Option<String>,
    /// First line in the original file (the heading line, or the first
    /// non-blank body line for a preamble). 1-based.
    pub start_line: usize,
    /// Last non-blank line in the original file. 1-based, inclusive.
    pub end_line: usize,
    /// Section text after the heading line (the whole section for a preamble),
    /// exactly as written, ending at `end_line`.
    pub body: String,
    /// The text came from a legacy memory's `content` metadata, so the range
    /// covers the whole file rather than a body.
    pub from_metadata: bool,
}

/// A section found while scanning, before keys are assigned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawSection {
    level: u8,
    title: Option<String>,
    breadcrumb: Vec<String>,
    start_line: usize,
    end_line: usize,
    body: String,
}

/// Split Markdown `body` into sections. `first_line` is the 1-based line
/// number of `body`'s first line in the original file.
fn split_sections(body: &str, first_line: usize) -> Vec<RawSection> {
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let mut sections: Vec<RawSection> = Vec::new();
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    // (start index, level, title, breadcrumb) of the section being built.
    let mut current: Option<(usize, u8, Option<String>, Vec<String>)> =
        (!lines.is_empty()).then(|| (0, 0, None, Vec::new()));

    let mut finish = |current: Option<(usize, u8, Option<String>, Vec<String>)>, end: usize| {
        let Some((start, level, title, breadcrumb)) = current else {
            return;
        };
        let Some(last) = (start..end).rev().find(|&i| !lines[i].trim().is_empty()) else {
            return; // a blank-only preamble is not a section
        };
        // A preamble begins at its first non-blank line; a heading section's
        // body is the lines after the heading.
        let first = if level == 0 {
            first_nonblank(&lines, start)
        } else {
            start
        };
        let body_from = if level == 0 { first } else { start + 1 };
        sections.push(RawSection {
            level,
            title,
            breadcrumb,
            start_line: first_line + first,
            end_line: first_line + last,
            body: lines[body_from.min(last + 1)..=last].concat(),
        });
    };

    for (index, line) in lines.iter().enumerate() {
        if let Some((fence_char, fence_len)) = fence {
            if is_closing_fence(line, fence_char, fence_len) {
                fence = None;
            }
            continue;
        }
        if let Some(opening) = opening_fence(line) {
            fence = Some(opening);
            continue;
        }
        let Some((level, title)) = parse_atx_heading(line) else {
            continue;
        };

        finish(current.take(), index);
        while stack.last().is_some_and(|(open, _)| *open >= level) {
            stack.pop();
        }
        stack.push((level, title.clone()));
        let breadcrumb = stack.iter().map(|(_, title)| title.clone()).collect();
        current = Some((index, level, Some(title), breadcrumb));
    }
    finish(current.take(), lines.len());
    sections
}

fn first_nonblank(lines: &[&str], from: usize) -> usize {
    (from..lines.len())
        .find(|&i| !lines[i].trim().is_empty())
        .unwrap_or(from)
}

/// Strip up to three leading spaces (more makes an indented code block).
fn strip_indent(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches(' ');
    (line.len() - trimmed.len() <= 3).then_some(trimmed)
}

fn parse_atx_heading(line: &str) -> Option<(u8, String)> {
    let text = strip_indent(line)?.trim_end_matches(['\n', '\r']);
    let hashes = text.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > MAX_HEADING_LEVEL {
        return None;
    }
    let rest = &text[hashes..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let mut content = rest.trim();
    // Optional closing sequence: trailing `#`s preceded by whitespace (or the
    // whole content being `#`s).
    if content.ends_with('#') {
        let without = content.trim_end_matches('#');
        if without.is_empty() {
            content = "";
        } else if without.ends_with([' ', '\t']) {
            content = without.trim_end();
        }
    }
    let title = content.split_whitespace().collect::<Vec<_>>().join(" ");
    Some((u8::try_from(hashes).ok()?, title))
}

pub(super) fn opening_fence(line: &str) -> Option<(char, usize)> {
    let text = strip_indent(line)?.trim_end_matches(['\n', '\r']);
    let fence_char = text.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = text.chars().take_while(|c| *c == fence_char).count();
    if run < 3 {
        return None;
    }
    // A backtick fence's info string may not contain backticks.
    let info = &text[run..];
    if fence_char == '`' && info.contains('`') {
        return None;
    }
    Some((fence_char, run))
}

pub(super) fn is_closing_fence(line: &str, fence_char: char, fence_len: usize) -> bool {
    let Some(text) = strip_indent(line) else {
        return false;
    };
    let text = text.trim_end_matches(['\n', '\r']);
    let run = text.chars().take_while(|c| *c == fence_char).count();
    run >= fence_len && text[run..].trim().is_empty()
}

// ─── Normalized documents ────────────────────────────────────────────────────

/// A parsed document with identity, title and sections.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedDocument {
    pub id: DocumentId,
    pub parsed: ParsedDocument,
    /// Frontmatter `title`, else the first heading, else the file stem.
    /// Verbatim source text; never generated.
    pub title: String,
    /// Sections in document order; every line belongs to at most one.
    pub sections: Vec<Section>,
}

impl NormalizedDocument {
    pub fn relative_path(&self) -> &str {
        &self.parsed.relative_path
    }

    pub fn section(&self, key: &SectionKey) -> Option<&Section> {
        self.sections.iter().find(|section| &section.key == key)
    }
}

/// Result of normalizing a parsed batch.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NormalizedBatch {
    /// Usable documents sorted by relative path; IDs are unique.
    pub documents: Vec<NormalizedDocument>,
    /// Parser diagnostics plus identity diagnostics, sorted.
    pub diagnostics: Vec<DocumentDiagnostic>,
}

/// Assign identities and sections. Documents with an invalid or duplicated
/// explicit ID are excluded with diagnostics; every other document is
/// unaffected, and the outcome does not depend on input order.
pub fn normalize_documents(batch: ParsedBatch) -> NormalizedBatch {
    let ParsedBatch {
        documents,
        mut diagnostics,
    } = batch;

    let mut claimed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut resolved: Vec<(DocumentId, ParsedDocument)> = Vec::new();

    for document in documents {
        let id = match document.explicit_id.as_deref() {
            None => DocumentId::from_path(&document.relative_path),
            Some(raw) => match normalize_explicit_id(raw) {
                Ok(normalized) => {
                    claimed
                        .entry(normalized.clone())
                        .or_default()
                        .push(document.relative_path.clone());
                    DocumentId::explicit(&normalized)
                }
                Err(reason) => {
                    diagnostics.push(DocumentDiagnostic {
                        path: document.relative_path.clone(),
                        severity: DiagnosticSeverity::Error,
                        code: codes::ID_INVALID,
                        message: format!("document excluded: {reason}"),
                    });
                    continue;
                }
            },
        };
        resolved.push((id, document));
    }

    let mut conflicting: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, mut paths) in claimed {
        if paths.len() > 1 {
            paths.sort();
            conflicting.insert(id, paths);
        }
    }

    let mut normalized: Vec<NormalizedDocument> = Vec::new();
    for (id, document) in resolved {
        if let Some(paths) = id
            .as_str()
            .strip_prefix(ID_PREFIX)
            .and_then(|explicit| conflicting.get(explicit))
        {
            diagnostics.push(duplicate_diagnostic(
                &document.relative_path,
                id.explicit_value().unwrap_or_default(),
                paths,
            ));
            continue;
        }
        normalized.push(build_document(id, document));
    }

    normalized.sort_by(|a, b| a.parsed.relative_path.cmp(&b.parsed.relative_path));
    diagnostics.sort();
    diagnostics.dedup();
    NormalizedBatch {
        documents: normalized,
        diagnostics,
    }
}

pub(super) fn duplicate_diagnostic(
    path: &str,
    explicit_id: &str,
    claimants: &[String],
) -> DocumentDiagnostic {
    let others: Vec<&str> = claimants
        .iter()
        .map(String::as_str)
        .filter(|other| *other != path)
        .collect();
    DocumentDiagnostic {
        path: path.to_string(),
        severity: DiagnosticSeverity::Error,
        code: codes::ID_DUPLICATE,
        message: format!(
            "document excluded: explicit id `{explicit_id}` is also claimed by {}",
            others.join(", ")
        ),
    }
}

fn build_document(id: DocumentId, parsed: ParsedDocument) -> NormalizedDocument {
    let sections = if parsed.body_from_metadata {
        // Legacy `content` metadata has no body lines, so cite the whole file.
        vec![Section {
            ordinal: 0,
            key: SectionKey {
                document_id: id.clone(),
                breadcrumb: Vec::new(),
                occurrence: 0,
            },
            level: 0,
            title: None,
            start_line: 1,
            end_line: parsed.line_count.max(1),
            body: parsed.body.clone(),
            from_metadata: true,
        }]
    } else {
        let mut seen: BTreeMap<Vec<String>, u32> = BTreeMap::new();
        split_sections(&parsed.body, parsed.body_start_line)
            .into_iter()
            .enumerate()
            .map(|(ordinal, raw)| {
                let counter = seen.entry(raw.breadcrumb.clone()).or_insert(0);
                let occurrence = *counter;
                *counter += 1;
                Section {
                    ordinal,
                    key: SectionKey {
                        document_id: id.clone(),
                        breadcrumb: raw.breadcrumb,
                        occurrence,
                    },
                    level: raw.level,
                    title: raw.title,
                    start_line: raw.start_line,
                    end_line: raw.end_line,
                    body: raw.body,
                    from_metadata: false,
                }
            })
            .collect()
    };

    let title = parsed
        .title
        .clone()
        .or_else(|| {
            sections
                .iter()
                .find_map(|s| s.title.clone().filter(|t| !t.is_empty()))
        })
        .unwrap_or_else(|| file_stem(&parsed.relative_path));

    NormalizedDocument {
        id,
        parsed,
        title,
        sections,
    }
}

fn file_stem(relative_path: &str) -> String {
    let name = relative_path.rsplit('/').next().unwrap_or(relative_path);
    name.strip_suffix(".md").unwrap_or(name).to_string()
}

/// Whether the adapter that parsed `document` was the legacy-memory adapter.
pub fn is_workspace_memory(document: &NormalizedDocument) -> bool {
    document.parsed.kind == DocumentKind::WorkspaceMemory
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::document::parse_documents;

    fn normalize(inputs: &[(&str, &str)]) -> NormalizedBatch {
        normalize_documents(parse_documents(
            inputs.iter().map(|(path, text)| (*path, text.as_bytes())),
        ))
    }

    fn one(path: &str, text: &str) -> NormalizedDocument {
        let mut batch = normalize(&[(path, text)]);
        assert!(batch.diagnostics.is_empty(), "{:?}", batch.diagnostics);
        batch.documents.remove(0)
    }

    fn titles(document: &NormalizedDocument) -> Vec<Option<&str>> {
        document
            .sections
            .iter()
            .map(|s| s.title.as_deref())
            .collect()
    }

    /// The section's recorded range really holds its heading/first line in the
    /// original file, and its body is exactly the following lines.
    fn assert_coordinates(text: &str, document: &NormalizedDocument) {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        for section in &document.sections {
            let range = &lines[section.start_line - 1..section.end_line];
            if section.level > 0 {
                assert!(range[0].trim_start().starts_with('#'), "{section:?}");
                assert_eq!(range[1..].concat(), section.body, "{section:?}");
            } else {
                assert_eq!(range.concat(), section.body, "{section:?}");
            }
            assert!(!range.last().unwrap().trim().is_empty());
        }
        for pair in document.sections.windows(2) {
            assert!(pair[0].end_line < pair[1].start_line, "sections overlap");
        }
    }

    // ── section splitting ────────────────────────────────────────────────

    #[test]
    fn splits_into_non_overlapping_sections_with_breadcrumbs() {
        let text = "Intro line\n\n# Runtime\nrt body\n\n## Sessions\nsess body\n\n### Resume\nresume body\n\n## Tools\ntools body\n\n# Appendix\nappx\n";
        let d = one("docs/a.md", text);
        assert_eq!(
            titles(&d),
            vec![
                None,
                Some("Runtime"),
                Some("Sessions"),
                Some("Resume"),
                Some("Tools"),
                Some("Appendix")
            ]
        );
        let crumbs: Vec<Vec<&str>> = d
            .sections
            .iter()
            .map(|s| s.key.breadcrumb.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            crumbs,
            vec![
                vec![],
                vec!["Runtime"],
                vec!["Runtime", "Sessions"],
                vec!["Runtime", "Sessions", "Resume"],
                vec!["Runtime", "Tools"],
                vec!["Appendix"],
            ]
        );
        assert_eq!(
            d.sections.iter().map(|s| s.level).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 2, 1]
        );
        assert_eq!(d.sections[1].body, "rt body\n");
        assert_eq!(d.sections[0].body, "Intro line\n");
        assert_eq!(
            d.sections.iter().map(|s| s.ordinal).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5]
        );
        assert_coordinates(text, &d);
    }

    #[test]
    fn coordinates_are_in_the_original_file_including_frontmatter() {
        let text = "---\ntitle: T\ntags: a\n---\n\n# One\nfirst\n\n## Two\nsecond\n";
        let d = one("docs/a.md", text);
        assert_eq!(d.sections[0].start_line, 6);
        assert_eq!(d.sections[0].end_line, 7);
        assert_eq!(d.sections[1].start_line, 9);
        assert_eq!(d.sections[1].end_line, 10);
        assert_coordinates(text, &d);
    }

    #[test]
    fn crlf_and_bom_do_not_shift_coordinates() {
        let text = "\u{feff}---\r\ntitle: T\r\n---\r\n# One\r\nbody\r\n\r\n# Two\r\nmore\r\n";
        let d = one("docs/a.md", text);
        assert_eq!(titles(&d), vec![Some("One"), Some("Two")]);
        assert_eq!((d.sections[0].start_line, d.sections[0].end_line), (4, 5));
        assert_eq!((d.sections[1].start_line, d.sections[1].end_line), (7, 8));
        assert_coordinates(text, &d);
    }

    #[test]
    fn preamble_starts_at_its_first_non_blank_line() {
        let text = "---\ntitle: T\n---\n\n\nlead\nmore lead\n# H\nx\n";
        let d = one("docs/a.md", text);
        assert_eq!(d.sections[0].level, 0);
        assert_eq!((d.sections[0].start_line, d.sections[0].end_line), (6, 7));
        assert_coordinates(text, &d);
    }

    #[test]
    fn documents_without_headings_are_one_preamble_section() {
        let text = "just prose\nand more\n";
        let d = one("docs/a.md", text);
        assert_eq!(d.sections.len(), 1);
        assert_eq!(d.sections[0].key.breadcrumb, Vec::<String>::new());
        assert_eq!(d.sections[0].body, text);
        assert_coordinates(text, &d);
    }

    #[test]
    fn empty_and_blank_documents_have_no_sections() {
        for text in ["", "\n\n  \n", "---\ntitle: Only\n---\n"] {
            assert!(one("docs/a.md", text).sections.is_empty(), "{text:?}");
        }
    }

    #[test]
    fn heading_only_sections_and_unterminated_last_line_are_kept() {
        let text = "# Alone";
        let d = one("docs/a.md", text);
        assert_eq!(d.sections.len(), 1);
        assert_eq!((d.sections[0].start_line, d.sections[0].end_line), (1, 1));
        assert_eq!(d.sections[0].body, "");
        assert_coordinates(text, &d);
    }

    #[test]
    fn trailing_blank_lines_are_not_part_of_a_section() {
        let text = "# A\nbody\n\n\n\n# B\nx\n";
        let d = one("docs/a.md", text);
        assert_eq!(d.sections[0].end_line, 2);
        assert_eq!(d.sections[0].body, "body\n");
        assert_coordinates(text, &d);
    }

    #[test]
    fn headings_inside_fenced_code_are_ignored() {
        let text = "# Real\n```sh\n# comment, not a heading\n## also not\n```\nafter\n~~~\n# tilde\n~~~\n## Next\n";
        let d = one("docs/a.md", text);
        assert_eq!(titles(&d), vec![Some("Real"), Some("Next")]);
        assert_eq!(d.sections[0].end_line, 9);
        assert_coordinates(text, &d);
    }

    #[test]
    fn fence_rules_need_matching_character_and_length() {
        // A shorter or different fence does not close; an unterminated fence
        // swallows the rest of the document.
        let text = "# A\n````\n```\n# inside\n~~~\n````\n# B\n```\n# never closed\n";
        let d = one("docs/a.md", text);
        assert_eq!(titles(&d), vec![Some("A"), Some("B")]);

        // A backtick opening line with a backtick in its info string is not a fence.
        let text = "# A\n``` a`b\n# real heading\n";
        let d = one("docs/a.md", text);
        assert_eq!(titles(&d), vec![Some("A"), Some("real heading")]);
    }

    #[test]
    fn atx_heading_edge_cases() {
        let text = "####### seven\n#nospace\n    # indented code\n   ### ok three spaces ###\n#\t tab sep\n##\n# closing ##   \n# keep # inside\n# trailing#hash\n";
        let d = one("docs/a.md", text);
        assert_eq!(
            titles(&d),
            vec![
                None,
                Some("ok three spaces"),
                Some("tab sep"),
                Some(""),
                Some("closing"),
                Some("keep # inside"),
                Some("trailing#hash"),
            ]
        );
        // The first three lines are plain text in the preamble.
        assert_eq!(d.sections[0].end_line, 3);
    }

    #[test]
    fn heading_titles_collapse_inner_whitespace_but_stay_verbatim() {
        let d = one("docs/a.md", "#   The   `Agent`\tbuilder   \n");
        assert_eq!(d.sections[0].title.as_deref(), Some("The `Agent` builder"));
    }

    #[test]
    fn skipped_levels_still_nest_by_closest_lower_level() {
        let d = one("docs/a.md", "# A\n### C\n## B\n#### D\n");
        let crumbs: Vec<Vec<&str>> = d
            .sections
            .iter()
            .map(|s| s.key.breadcrumb.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(
            crumbs,
            vec![
                vec!["A"],
                vec!["A", "C"],
                vec!["A", "B"],
                vec!["A", "B", "D"]
            ]
        );
    }

    // ── duplicate headings ───────────────────────────────────────────────

    #[test]
    fn duplicate_breadcrumbs_get_increasing_occurrences() {
        let text = "# Notes\na\n# Notes\nb\n## Detail\nc\n# Notes\nd\n";
        let d = one("docs/a.md", text);
        let occ: Vec<u32> = d.sections.iter().map(|s| s.key.occurrence).collect();
        assert_eq!(occ, vec![0, 1, 0, 2]);
        let keys: Vec<String> = d.sections.iter().map(|s| s.key.to_string()).collect();
        assert_eq!(keys[0], "path:docs/a.md#Notes");
        assert_eq!(keys[1], "path:docs/a.md#Notes~1");
        assert_eq!(keys[3], "path:docs/a.md#Notes~2");
        let unique: std::collections::BTreeSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
    }

    #[test]
    fn same_title_under_different_parents_is_distinct_without_occurrence() {
        let d = one("docs/a.md", "# A\n## Setup\nx\n# B\n## Setup\ny\n");
        let keys: Vec<String> = d.sections.iter().map(|s| s.key.to_string()).collect();
        assert!(
            keys.contains(&"path:docs/a.md#A>Setup".to_string()),
            "{keys:?}"
        );
        assert!(
            keys.contains(&"path:docs/a.md#B>Setup".to_string()),
            "{keys:?}"
        );
    }

    #[test]
    fn inserting_an_unrelated_section_keeps_other_keys_stable() {
        let before = one("docs/a.md", "# One\na\n# Two\nb\n");
        let after = one("docs/a.md", "# Zero\nz\n# One\na\n# Two\nb\n");
        let key = |d: &NormalizedDocument, title: &str| {
            d.sections
                .iter()
                .find(|s| s.title.as_deref() == Some(title))
                .unwrap()
                .key
                .clone()
        };
        assert_eq!(key(&before, "Two"), key(&after, "Two"));
        assert_eq!(key(&before, "One"), key(&after, "One"));
    }

    #[test]
    fn renaming_a_heading_invalidates_its_old_key() {
        let before = one("docs/a.md", "# Old name\nbody\n");
        let after = one("docs/a.md", "# New name\nbody\n");
        let old_key = before.sections[0].key.clone();
        // The old key must resolve to nothing, never to the renamed section.
        assert!(after.section(&old_key).is_none());
        assert!(before.section(&old_key).is_some());
    }

    // ── key syntax ───────────────────────────────────────────────────────

    #[test]
    fn keys_round_trip_including_awkward_titles() {
        let d = one(
            "docs/we#ird.md",
            "pre\n# 100% done > almost ~ really #1\n## café ☕ résumé\n##\n# dup\n# dup\n",
        );
        for section in &d.sections {
            let rendered = section.key.to_string();
            let parsed: SectionKey = rendered
                .parse()
                .unwrap_or_else(|e| panic!("{rendered}: {e}"));
            assert_eq!(parsed, section.key, "{rendered}");
        }
        assert!(d.sections.iter().any(|s| {
            s.key
                .to_string()
                .contains("100%25 done %3E almost %7E really %231")
        }));
    }

    #[test]
    fn empty_heading_title_and_preamble_have_distinct_keys() {
        let d = one("docs/a.md", "lead\n#\nbody\n");
        let keys: Vec<String> = d.sections.iter().map(|s| s.key.to_string()).collect();
        assert_eq!(keys, vec!["path:docs/a.md#", "path:docs/a.md#%20"]);
        for key in &keys {
            assert!(key.parse::<SectionKey>().is_ok(), "{key}");
        }
        assert_eq!(
            keys[1].parse::<SectionKey>().unwrap().breadcrumb,
            vec![String::new()]
        );
    }

    #[test]
    fn non_canonical_and_malformed_keys_are_rejected() {
        for bad in [
            "",
            "no-fragment",
            "docs/a.md#Intro",
            "other:docs/a.md#Intro",
            "path:#Intro",
            "id:#Intro",
            "id:Upper#Intro",
            "id:has space#Intro",
            "path:docs/a.md#Intro~0",
            "path:docs/a.md#Intro~x",
            "path:docs/a.md#Intro~01",
            "path:docs/a.md#%zz",
            "path:docs/a.md#%2",
            "path:docs/a.md#%ff",
            "path:docs/a.md#a%2fb",
            "path:docs/a.md#a>>b",
        ] {
            assert!(
                bad.parse::<SectionKey>().is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn explicit_id_keys_do_not_mention_paths() {
        let d = one(
            "docs/runtime.md",
            "---\nid: Runtime-Overview\n---\n# Intro\nx\n",
        );
        assert_eq!(d.id.to_string(), "id:runtime-overview");
        assert_eq!(d.id.origin(), IdOrigin::Explicit);
        assert_eq!(d.sections[0].key.to_string(), "id:runtime-overview#Intro");
        assert!(!d.sections[0].key.to_string().contains("docs/"));
    }

    // ── identities ───────────────────────────────────────────────────────

    #[test]
    fn path_derived_identity_when_no_explicit_id() {
        let d = one("docs/runtime.md", "# Intro\n");
        assert_eq!(d.id.to_string(), "path:docs/runtime.md");
        assert_eq!(d.id.origin(), IdOrigin::PathDerived);
        assert_eq!("path:docs/runtime.md".parse::<DocumentId>().unwrap(), d.id);
    }

    #[test]
    fn explicit_ids_survive_moves_and_path_ids_do_not() {
        let text = "---\nid: runtime\n---\n# Intro\nx\n# Intro\ny\n";
        let before = one("docs/runtime.md", text);
        let after = one("docs/architecture/moved/runtime.md", text);
        assert_eq!(before.id, after.id);
        let keys = |d: &NormalizedDocument| -> Vec<SectionKey> {
            d.sections.iter().map(|s| s.key.clone()).collect()
        };
        assert_eq!(keys(&before), keys(&after));
        assert_ne!(before.relative_path(), after.relative_path());

        let plain = "# Intro\nx\n";
        let before = one("docs/plain.md", plain);
        let after = one("docs/moved/plain.md", plain);
        assert_ne!(before.id, after.id);
    }

    #[test]
    fn explicit_and_path_namespaces_cannot_collide() {
        let batch = normalize(&[
            ("docs/a.md", "---\nid: path:docs/b.md\n---\nx\n"),
            ("docs/b.md", "y\n"),
        ]);
        assert!(batch.diagnostics.is_empty(), "{:?}", batch.diagnostics);
        assert_eq!(batch.documents.len(), 2);
        assert_ne!(batch.documents[0].id, batch.documents[1].id);
    }

    #[test]
    fn invalid_explicit_ids_exclude_only_that_document() {
        let long = format!("---\nid: {}\n---\nx\n", "a".repeat(129));
        for (name, text) in [
            ("hash", "---\nid: has#hash\n---\nx\n".to_string()),
            ("space", "---\nid: \"two words\"\n---\nx\n".to_string()),
            ("long", long),
        ] {
            let path = format!("docs/{name}.md");
            let batch = normalize(&[(path.as_str(), text.as_str()), ("docs/ok.md", "fine\n")]);
            assert_eq!(batch.documents.len(), 1, "{name}");
            assert_eq!(batch.documents[0].relative_path(), "docs/ok.md");
            assert_eq!(batch.diagnostics.len(), 1, "{name}");
            assert_eq!(batch.diagnostics[0].code, codes::ID_INVALID);
            assert_eq!(batch.diagnostics[0].path, path);
        }
    }

    #[test]
    fn duplicate_explicit_ids_disable_every_conflicting_document() {
        let batch = normalize(&[
            ("docs/a.md", "---\nid: shared\n---\nA\n"),
            ("docs/b.md", "---\nid: SHARED\n---\nB\n"),
            ("docs/c.md", "---\nid: shared\n---\nC\n"),
            ("docs/unique.md", "---\nid: unique\n---\nU\n"),
            ("docs/plain.md", "P\n"),
        ]);
        let paths: Vec<_> = batch.documents.iter().map(|d| d.relative_path()).collect();
        assert_eq!(paths, vec!["docs/plain.md", "docs/unique.md"]);
        let dup: Vec<_> = batch
            .diagnostics
            .iter()
            .filter(|d| d.code == codes::ID_DUPLICATE)
            .collect();
        assert_eq!(dup.len(), 3);
        assert_eq!(dup[0].path, "docs/a.md");
        assert!(dup[0].message.contains("docs/b.md") && dup[0].message.contains("docs/c.md"));
        assert!(!dup[0].message.contains("docs/a.md"));
        assert!(dup.iter().all(|d| d.severity == DiagnosticSeverity::Error));
    }

    #[test]
    fn duplicate_resolution_does_not_depend_on_input_order() {
        let inputs = [
            ("docs/a.md", "---\nid: x\n---\nA\n"),
            ("docs/b.md", "---\nid: x\n---\nB\n"),
            ("docs/c.md", "# C\n"),
        ];
        let mut reversed = inputs;
        reversed.reverse();
        assert_eq!(normalize(&inputs), normalize(&reversed));
    }

    #[test]
    fn explicit_id_matching_a_removed_duplicate_becomes_usable_again() {
        let with_both = normalize(&[
            ("docs/a.md", "---\nid: x\n---\nA\n"),
            ("docs/b.md", "---\nid: x\n---\nB\n"),
        ]);
        assert!(with_both.documents.is_empty());
        let after_delete = normalize(&[("docs/a.md", "---\nid: x\n---\nA\n")]);
        assert_eq!(after_delete.documents.len(), 1);
        assert!(after_delete.diagnostics.is_empty());
    }

    // ── mixed inputs ─────────────────────────────────────────────────────

    #[test]
    fn mixed_markdown_unknown_keys_and_legacy_memories_share_one_projection() {
        let batch = normalize(&[
            ("docs/plain.md", "# Plain\nbody\n"),
            (
                "docs/unknown.md",
                "---\nowner: team\ntitle: Unknown keys\n---\n# H\nx\n",
            ),
            (
                ".agents/memories/legacy.md",
                "---\ntitle: Legacy\n---\nremember\n",
            ),
            (
                ".agents/memories/with-id.md",
                "---\nid: Memory-One\n---\n# Sec\nm\n",
            ),
            (
                ".agents/memories/off.md",
                "---\nenabled: false\n---\nhidden\n",
            ),
            ("docs/broken.md", "---\ntitle: nope\n"),
        ]);
        let ids: Vec<String> = batch.documents.iter().map(|d| d.id.to_string()).collect();
        assert_eq!(
            ids,
            vec![
                "path:.agents/memories/legacy.md",
                "id:memory-one",
                "path:docs/plain.md",
                "path:docs/unknown.md",
            ]
        );
        assert!(is_workspace_memory(&batch.documents[0]));
        assert!(!is_workspace_memory(&batch.documents[2]));
        assert_eq!(batch.documents[0].title, "Legacy");
        let codes: Vec<_> = batch
            .diagnostics
            .iter()
            .map(|d| (d.path.as_str(), d.code))
            .collect();
        assert_eq!(
            codes,
            vec![
                (".agents/memories/off.md", "memory_disabled"),
                ("docs/broken.md", "frontmatter_malformed"),
            ]
        );
    }

    #[test]
    fn legacy_content_metadata_is_one_whole_file_section() {
        let text = "---\ncontent: Remember the cache\ntitle: Cache\n---\n";
        let d = one(".agents/memories/inline.md", text);
        assert_eq!(d.sections.len(), 1);
        let section = &d.sections[0];
        assert!(section.from_metadata && section.level == 0);
        assert_eq!(section.body, "Remember the cache");
        assert_eq!((section.start_line, section.end_line), (1, 4));
        assert_eq!(section.key.to_string(), "path:.agents/memories/inline.md#");
    }

    #[test]
    fn titles_fall_back_to_first_heading_then_file_stem() {
        assert_eq!(
            one("docs/a.md", "---\ntitle: Front\n---\n# Head\n").title,
            "Front"
        );
        assert_eq!(
            one("docs/a.md", "lead\n# First\n## Second\n").title,
            "First"
        );
        assert_eq!(one("docs/a.md", "#\n# Real\n").title, "Real");
        assert_eq!(
            one("docs/architecture-notes.md", "no headings\n").title,
            "architecture-notes"
        );
        assert_eq!(one("docs/empty.md", "").title, "empty");
    }

    #[test]
    fn identities_and_keys_are_identical_across_checkouts() {
        // Inputs carry only relative paths; two "clones" must agree exactly.
        let inputs = [
            ("docs/a.md", "---\nid: a\n---\n# One\nx\n# One\ny\n"),
            ("docs/b.md", "# Two\nz\n"),
        ];
        assert_eq!(normalize(&inputs), normalize(&inputs));
        let rendered = format!("{:?}", normalize(&inputs));
        assert!(!rendered.contains("/Users") && !rendered.contains("/var/"));
    }
}

//! Document-relative Markdown reference resolution.
//!
//! References are **metadata about a document**. They never select index
//! inputs and never grant access:
//!
//! * [`extract_links`] only scans text. It performs no IO.
//! * [`resolve_reference`] is purely lexical: it resolves a link target against
//!   the *containing document's* directory, percent-decodes it **before**
//!   normalizing (so `%2e%2e/` is `..`), and rejects anything that would leave
//!   the knowledge root. A rejected reference yields no path at all, so there is
//!   nothing a caller could accidentally open.
//! * External URLs (`http:`, `mailto:`, `file:`, `javascript:`, `//host/...`,
//!   anything with a URI scheme) are inert data. Nothing in this module — or in
//!   indexing and checking — fetches or opens them.
//! * [`verify_local`] is the only function that touches the filesystem. It reads
//!   **metadata only** (never file contents) and reports `EscapesRoot` for any
//!   target whose canonical location, after following symlinks, is outside the
//!   canonical root, without describing what is out there.
//!
//! Markdown links (related material), `knowledge_sources` (evidence
//! dependencies) and `knowledge_paths` (applicability) stay distinct:
//! [`document_references`] reads only the document body and never frontmatter.
//!
//! Recognized syntax: inline links and images (`[t](dest "title")`, `<dest>`
//! with spaces, balanced parentheses), reference definitions (`[label]: dest`,
//! excluding `[^footnote]:`), and autolinks (`<scheme:...>`). Fenced code blocks
//! and inline code spans are skipped. Reference-style *uses* (`[t][label]`)
//! need no handling because their destination is the definition.

use super::document::{DiagnosticSeverity, DocumentDiagnostic};
use super::identity::{NormalizedDocument, is_closing_fence, opening_fence};
use std::path::Path;

/// Diagnostic codes emitted for rejected references.
pub mod codes {
    pub const ESCAPES_ROOT: &str = "reference_escapes_root";
    pub const UNSAFE: &str = "reference_unsafe";
    pub const ABSOLUTE: &str = "reference_unsupported_absolute";
}

/// Syntactic form a link was written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Inline,
    Image,
    Definition,
    Autolink,
}

/// A link destination exactly as written (escapes removed), with its position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLink {
    pub kind: LinkKind,
    pub target: String,
    /// 1-based line in the original file.
    pub line: usize,
}

/// Why a reference was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Resolves above the knowledge root.
    EscapesRoot,
    /// Written as a root-absolute path (`/x`). Not resolved: such a path is
    /// ambiguous between the checkout and the filesystem.
    Absolute,
    /// Contains a NUL, control character or backslash, or a percent escape
    /// that is not valid UTF-8.
    Unsafe,
    /// Empty destination, or one that resolves to the knowledge root itself.
    NotAFile,
}

/// The outcome of resolving one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    /// A URI or network-path reference. Inert: never fetched or opened.
    External {
        scheme: String,
    },
    /// A fragment or query on the containing document itself.
    SameDocument {
        fragment: Option<String>,
    },
    /// A confined, normalized, root-relative path.
    Local {
        /// Forward-slash path relative to the knowledge root; no `.`/`..`.
        path: String,
        fragment: Option<String>,
    },
    Rejected(RejectReason),
}

/// Resolve `target` as written in the document at `document_path`
/// (root-relative, forward slashes). Pure and lexical.
pub fn resolve_reference(document_path: &str, target: &str) -> Reference {
    let target = target.trim();
    if target.is_empty() {
        return Reference::Rejected(RejectReason::NotAFile);
    }
    if let Some(fragment) = target.strip_prefix('#') {
        return Reference::SameDocument {
            fragment: decode_fragment(fragment),
        };
    }
    if target.starts_with("//") {
        return Reference::External {
            scheme: String::new(),
        };
    }
    if let Some(scheme) = scheme_of(target) {
        return Reference::External { scheme };
    }

    let (path_part, fragment) = match target.split_once('#') {
        Some((path, fragment)) => (path, decode_fragment(fragment)),
        None => (target, None),
    };
    let path_part = path_part.split('?').next().unwrap_or_default();
    if path_part.is_empty() {
        return Reference::SameDocument { fragment };
    }

    // Decode first: `%2e%2e` and `%2f` must be seen as `..` and `/` before the
    // confinement decision, never after.
    let Some(decoded) = percent_decode(path_part) else {
        return Reference::Rejected(RejectReason::Unsafe);
    };
    if decoded
        .chars()
        .any(|c| c == '\\' || c == '\0' || c.is_control())
    {
        return Reference::Rejected(RejectReason::Unsafe);
    }
    if decoded.starts_with('/') {
        return Reference::Rejected(RejectReason::Absolute);
    }

    let mut segments: Vec<&str> = document_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or_default()
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Reference::Rejected(RejectReason::EscapesRoot);
                }
            }
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return Reference::Rejected(RejectReason::NotAFile);
    }
    Reference::Local {
        path: segments.join("/"),
        fragment,
    }
}

/// What a confined local target is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetStatus {
    File,
    Directory,
    Missing,
    /// After following symlinks the target is outside the knowledge root.
    EscapesRoot,
    /// Exists but is neither a regular file nor a directory.
    Other,
}

/// Check a [`Reference::Local`] path against the filesystem.
///
/// Metadata only; file contents are never opened. `relative` must come from
/// [`resolve_reference`]. Anything that resolves outside the canonical root
/// (including through a symlink) is `EscapesRoot`, and nothing is revealed about
/// the outside location.
pub fn verify_local(root: &Path, relative: &str) -> TargetStatus {
    let Ok(canonical_root) = root.canonicalize() else {
        return TargetStatus::Missing;
    };
    if relative
        .split('/')
        .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return TargetStatus::EscapesRoot;
    }
    let canonical = match canonical_root.join(relative).canonicalize() {
        Ok(path) => path,
        Err(_) => {
            // A dangling symlink that points outside the root must not be
            // distinguishable from one that points inside it.
            return TargetStatus::Missing;
        }
    };
    if !canonical.starts_with(&canonical_root) {
        return TargetStatus::EscapesRoot;
    }
    match std::fs::metadata(&canonical) {
        Ok(metadata) if metadata.is_file() => TargetStatus::File,
        Ok(metadata) if metadata.is_dir() => TargetStatus::Directory,
        Ok(_) => TargetStatus::Other,
        Err(_) => TargetStatus::Missing,
    }
}

/// One link found in a document, with its resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentReference {
    pub kind: LinkKind,
    /// Destination as written.
    pub target: String,
    /// 1-based line in the original file.
    pub line: usize,
    pub resolution: Reference,
}

/// All references in a document body, in source order. Frontmatter fields such
/// as `knowledge_sources` and `knowledge_paths` are not references.
pub fn document_references(document: &NormalizedDocument) -> Vec<DocumentReference> {
    let parsed = &document.parsed;
    extract_links(&parsed.body, parsed.body_start_line)
        .into_iter()
        .map(|link| DocumentReference {
            resolution: resolve_reference(&parsed.relative_path, &link.target),
            kind: link.kind,
            target: link.target,
            line: link.line,
        })
        .collect()
}

/// Warnings for references that were refused (escapes, unsafe, root-absolute).
/// Missing targets are a separate, filesystem-backed check.
pub fn reference_diagnostics(document: &NormalizedDocument) -> Vec<DocumentDiagnostic> {
    let path = document.relative_path();
    let mut out: Vec<DocumentDiagnostic> = document_references(document)
        .into_iter()
        .filter_map(|reference| {
            let (code, what) = match reference.resolution {
                Reference::Rejected(RejectReason::EscapesRoot) => {
                    (codes::ESCAPES_ROOT, "escapes the knowledge root")
                }
                Reference::Rejected(RejectReason::Unsafe) => {
                    (codes::UNSAFE, "contains an unsafe path")
                }
                Reference::Rejected(RejectReason::Absolute) => {
                    (codes::ABSOLUTE, "is a root-absolute path")
                }
                _ => return None,
            };
            let shown: String = reference.target.chars().take(120).collect();
            Some(DocumentDiagnostic {
                path: path.to_string(),
                severity: DiagnosticSeverity::Warning,
                code,
                message: format!(
                    "line {}: link `{shown}` {what} and was not resolved",
                    reference.line
                ),
            })
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

// ─── Link extraction ─────────────────────────────────────────────────────────

/// Extract link destinations from Markdown `body`. `first_line` is the 1-based
/// line number of `body`'s first line in the original file.
pub fn extract_links(body: &str, first_line: usize) -> Vec<RawLink> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for (index, line) in body.split_inclusive('\n').enumerate() {
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
        scan_line(
            line.trim_end_matches(['\n', '\r']),
            first_line + index,
            &mut out,
        );
    }
    out
}

fn scan_line(line: &str, lineno: usize, out: &mut Vec<RawLink>) {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let lead = chars.iter().take_while(|c| **c == ' ').count();
    let mut i = 0;
    while i < n {
        match chars[i] {
            '\\' => i += 2,
            '`' => i = skip_code_span(&chars, i),
            '[' => {
                if let Some(close) = find_close_bracket(&chars, i) {
                    let image = i > 0 && chars[i - 1] == '!' && !(i > 1 && chars[i - 2] == '\\');
                    if close + 1 < n && chars[close + 1] == '(' {
                        if let Some((target, _)) = parse_destination(&chars, close + 2, true) {
                            out.push(RawLink {
                                kind: if image {
                                    LinkKind::Image
                                } else {
                                    LinkKind::Inline
                                },
                                target,
                                line: lineno,
                            });
                        }
                    } else if i == lead
                        && lead <= 3
                        && chars.get(i + 1) != Some(&'^')
                        && close + 1 < n
                        && chars[close + 1] == ':'
                        && let Some((target, _)) = parse_destination(&chars, close + 2, false)
                    {
                        out.push(RawLink {
                            kind: LinkKind::Definition,
                            target,
                            line: lineno,
                        });
                    }
                }
                // Continue just after the bracket so links nested in link text
                // (`[![alt](img.png)](page.md)`) are found as well.
                i += 1;
            }
            '<' => match chars[i + 1..].iter().position(|c| *c == '>') {
                Some(offset) => {
                    let inner: String = chars[i + 1..i + 1 + offset].iter().collect();
                    if !inner.is_empty()
                        && !inner.chars().any(char::is_whitespace)
                        && scheme_of(&inner).is_some()
                    {
                        out.push(RawLink {
                            kind: LinkKind::Autolink,
                            target: inner,
                            line: lineno,
                        });
                        i += offset + 2;
                    } else {
                        i += 1;
                    }
                }
                None => i += 1,
            },
            _ => i += 1,
        }
    }
}

/// Index just past the code span starting at `start`, or past the backtick run
/// when it is unmatched (then it is literal text).
fn skip_code_span(chars: &[char], start: usize) -> usize {
    let run = chars[start..].iter().take_while(|c| **c == '`').count();
    let mut j = start + run;
    while j < chars.len() {
        if chars[j] == '`' {
            let closing = chars[j..].iter().take_while(|c| **c == '`').count();
            if closing == run {
                return j + closing;
            }
            j += closing;
        } else {
            j += 1;
        }
    }
    start + run
}

fn find_close_bracket(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut j = open + 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '`' => j = skip_code_span(chars, j),
            '[' => {
                depth += 1;
                j += 1;
            }
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
                j += 1;
            }
            _ => j += 1,
        }
    }
    None
}

fn skip_blanks(chars: &[char], mut j: usize) -> usize {
    while j < chars.len() && matches!(chars[j], ' ' | '\t') {
        j += 1;
    }
    j
}

/// Parse a destination starting at `start`. With `in_parens`, parse the rest of
/// an inline link (optional title, then the closing `)`); otherwise parse a
/// reference-definition destination (anything up to whitespace).
fn parse_destination(chars: &[char], start: usize, in_parens: bool) -> Option<(String, usize)> {
    let n = chars.len();
    let mut j = skip_blanks(chars, start);
    let raw: String;

    if j < n && chars[j] == '<' {
        j += 1;
        let begin = j;
        while j < n && chars[j] != '>' && chars[j] != '<' {
            j += if chars[j] == '\\' { 2 } else { 1 };
        }
        if j >= n || chars[j] != '>' {
            return None;
        }
        raw = chars[begin..j].iter().collect();
        j += 1;
    } else {
        let begin = j;
        let mut depth = 0usize;
        while j < n {
            let c = chars[j];
            if c == '\\' {
                j += 2;
                continue;
            }
            if c.is_whitespace() {
                break;
            }
            if in_parens {
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
            }
            j += 1;
        }
        if depth != 0 {
            return None;
        }
        j = j.min(n);
        raw = chars[begin..j].iter().collect();
    }

    let target = unescape(&raw);
    if target.trim().is_empty() {
        return None;
    }
    if !in_parens {
        return Some((target, j));
    }

    j = skip_blanks(chars, j);
    if j < n && matches!(chars[j], '"' | '\'' | '(') {
        let close = if chars[j] == '(' { ')' } else { chars[j] };
        j += 1;
        while j < n && chars[j] != close {
            j += if chars[j] == '\\' { 2 } else { 1 };
        }
        if j >= n {
            return None;
        }
        j = skip_blanks(chars, j + 1);
    }
    (j < n && chars[j] == ')').then_some((target, j + 1))
}

/// Remove backslashes that escape ASCII punctuation.
fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.peek()
            && next.is_ascii_punctuation()
        {
            out.push(*next);
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

/// RFC 3986 scheme (`ALPHA *(ALPHA / DIGIT / "+" / "-" / ".")` then `:`),
/// lowercased. A drive-letter path such as `C:/x` counts as a scheme, which is
/// the safe reading: it is treated as external and never opened.
fn scheme_of(target: &str) -> Option<String> {
    let mut chars = target.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let mut scheme = String::from(first.to_ascii_lowercase());
    for c in chars {
        match c {
            ':' => return Some(scheme),
            c if c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.') => {
                scheme.push(c.to_ascii_lowercase());
            }
            _ => return None,
        }
    }
    None
}

fn decode_fragment(raw: &str) -> Option<String> {
    let raw = raw.split('?').next().unwrap_or_default();
    if raw.is_empty() {
        return None;
    }
    Some(percent_decode(raw).unwrap_or_else(|| raw.to_string()))
}

/// Decode `%XX` escapes. Malformed escapes stay literal; the result must be
/// valid UTF-8 or the whole target is unusable.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = input.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::repository::document::parse_documents;
    use crate::knowledge::repository::identity::normalize_documents;
    use std::fs;
    use std::path::PathBuf;

    fn targets(body: &str) -> Vec<(LinkKind, String, usize)> {
        extract_links(body, 1)
            .into_iter()
            .map(|l| (l.kind, l.target, l.line))
            .collect()
    }

    fn just_targets(body: &str) -> Vec<String> {
        targets(body).into_iter().map(|(_, t, _)| t).collect()
    }

    fn local(path: &str) -> Reference {
        Reference::Local {
            path: path.to_string(),
            fragment: None,
        }
    }

    fn doc(path: &str, text: &str) -> NormalizedDocument {
        let mut batch = normalize_documents(parse_documents([(path, text.as_bytes())]));
        assert!(batch.diagnostics.is_empty(), "{:?}", batch.diagnostics);
        batch.documents.remove(0)
    }

    // ── extraction ───────────────────────────────────────────────────────

    #[test]
    fn extracts_inline_links_images_definitions_and_autolinks() {
        let body = "See [runtime](runtime.md) and ![diagram](img/d.png).\n\
                    Also <https://example.com/x> here.\n\
                    \n\
                    [ref]: ../other/ref.md \"Title\"\n";
        assert_eq!(
            targets(body),
            vec![
                (LinkKind::Inline, "runtime.md".into(), 1),
                (LinkKind::Image, "img/d.png".into(), 1),
                (LinkKind::Autolink, "https://example.com/x".into(), 2),
                (LinkKind::Definition, "../other/ref.md".into(), 4),
            ]
        );
    }

    #[test]
    fn destination_forms_titles_angle_brackets_and_parentheses() {
        let body = "[a](a.md \"t\") [b](b.md 't') [c](c.md (t)) [d](<has space.md>) \
                    [e](f(1).md) [f](g\\).md) [g](  spaced.md  )\n";
        assert_eq!(
            just_targets(body),
            vec![
                "a.md",
                "b.md",
                "c.md",
                "has space.md",
                "f(1).md",
                "g).md",
                "spaced.md"
            ]
        );
    }

    #[test]
    fn invalid_or_empty_destinations_are_not_links() {
        for body in [
            "[a]()\n",
            "[a](   )\n",
            "[a](b c)\n",
            "[a](unclosed\n",
            "[a](<unterminated)\n",
            "[a](b \"unterminated)\n",
            "[a] (spaced.md)\n",
            "plain [brackets] only\n",
        ] {
            assert!(just_targets(body).is_empty(), "{body:?}");
        }
    }

    #[test]
    fn nested_image_in_link_yields_both() {
        // Ordered by opening bracket: the outer link starts first.
        assert_eq!(
            targets("[![logo](logo.png)](home.md)\n"),
            vec![
                (LinkKind::Inline, "home.md".into(), 1),
                (LinkKind::Image, "logo.png".into(), 1),
            ]
        );
    }

    #[test]
    fn code_spans_and_fences_are_skipped() {
        let body = "`[x](in-span.md)` and ``[y](double.md)`` and [ok](real.md)\n\
                    ```md\n[z](fenced.md)\n```\n\
                    ~~~\n[w](tilde.md)\n~~~\n\
                    after [v](after.md)\n";
        assert_eq!(just_targets(body), vec!["real.md", "after.md"]);
        // An unmatched backtick is literal text, not a code span.
        assert_eq!(just_targets("a ` b [k](kept.md)\n"), vec!["kept.md"]);
    }

    #[test]
    fn escapes_footnotes_and_non_leading_definitions_are_not_links() {
        assert!(just_targets("\\[a](b.md)\n").is_empty());
        assert!(just_targets("[^1]: A footnote about something\n").is_empty());
        assert!(just_targets("text [label]: not-a-definition.md\n").is_empty());
        // Up to three leading spaces still make a definition; four do not.
        assert_eq!(just_targets("   [ok]: ok.md\n"), vec!["ok.md"]);
        assert!(just_targets("    [no]: no.md\n").is_empty());
    }

    #[test]
    fn autolinks_need_a_scheme() {
        assert_eq!(
            just_targets("<mailto:a@b.c> <https://x.y> <not a link> <relative.md> <b>text</b>\n"),
            vec!["mailto:a@b.c", "https://x.y"]
        );
    }

    #[test]
    fn line_numbers_are_original_file_lines() {
        let text = "---\ntitle: T\n---\n# H\n\ntext [a](a.md)\n\n[b](b.md)\n";
        let d = doc("docs/x.md", text);
        let lines: Vec<usize> = document_references(&d).iter().map(|r| r.line).collect();
        assert_eq!(lines, vec![6, 8]);
        let original: Vec<&str> = text.lines().collect();
        assert!(original[5].contains("a.md") && original[7].contains("b.md"));
    }

    #[test]
    fn crlf_and_unicode_do_not_break_extraction() {
        assert_eq!(
            just_targets("héllo [ünï](ünï/ça.md) wörld\r\n[b](b.md)\r\n"),
            vec!["ünï/ça.md", "b.md"]
        );
    }

    // ── resolution ───────────────────────────────────────────────────────

    #[test]
    fn resolves_relative_to_the_containing_document() {
        let from = "docs/architecture/runtime.md";
        assert_eq!(
            resolve_reference(from, "sessions.md"),
            local("docs/architecture/sessions.md")
        );
        assert_eq!(
            resolve_reference(from, "./sessions.md"),
            local("docs/architecture/sessions.md")
        );
        assert_eq!(
            resolve_reference(from, "sub/x.md"),
            local("docs/architecture/sub/x.md")
        );
        assert_eq!(
            resolve_reference(from, "../overview.md"),
            local("docs/overview.md")
        );
        assert_eq!(
            resolve_reference(from, "../../README.md"),
            local("README.md")
        );
        assert_eq!(
            resolve_reference(from, "a//b/./c.md"),
            local("docs/architecture/a/b/c.md")
        );
        assert_eq!(
            resolve_reference(from, "../architecture/x.md"),
            local("docs/architecture/x.md")
        );
        // Documents at the root resolve against the root.
        assert_eq!(
            resolve_reference("README.md", "docs/a.md"),
            local("docs/a.md")
        );
        assert_eq!(resolve_reference("README.md", "./a.md"), local("a.md"));
    }

    #[test]
    fn fragments_are_kept_and_queries_dropped() {
        assert_eq!(
            resolve_reference("docs/a.md", "b.md#Some%20Heading"),
            Reference::Local {
                path: "docs/b.md".into(),
                fragment: Some("Some Heading".into())
            }
        );
        assert_eq!(
            resolve_reference("docs/a.md", "b.md?raw=1#frag"),
            Reference::Local {
                path: "docs/b.md".into(),
                fragment: Some("frag".into())
            }
        );
        assert_eq!(
            resolve_reference("docs/a.md", "b.md?raw=1"),
            local("docs/b.md")
        );
        assert_eq!(
            resolve_reference("docs/a.md", "#intro"),
            Reference::SameDocument {
                fragment: Some("intro".into())
            }
        );
        assert_eq!(
            resolve_reference("docs/a.md", "?x=1"),
            Reference::SameDocument { fragment: None }
        );
        assert_eq!(
            resolve_reference("docs/a.md", "#"),
            Reference::SameDocument { fragment: None }
        );
    }

    #[test]
    fn percent_encoded_spaces_and_unicode_decode() {
        assert_eq!(
            resolve_reference("a.md", "my%20notes.md"),
            local("my notes.md")
        );
        assert_eq!(
            resolve_reference("a.md", "caf%C3%A9/r.md"),
            local("café/r.md")
        );
        // A malformed escape stays literal.
        assert_eq!(resolve_reference("a.md", "100%.md"), local("100%.md"));
        assert_eq!(resolve_reference("a.md", "%zz.md"), local("%zz.md"));
    }

    #[test]
    fn traversal_above_the_root_is_rejected_in_every_spelling() {
        for (from, target) in [
            ("README.md", "../x.md"),
            ("docs/a.md", "../../x.md"),
            ("docs/a.md", "../../../etc/passwd"),
            ("docs/a.md", "sub/../../../x.md"),
            ("docs/a.md", "%2e%2e/%2e%2e/x.md"),
            ("docs/a.md", "%2E%2E%2F%2E%2E%2Fx.md"),
            ("docs/a.md", "..%2f..%2fx.md"),
            ("docs/a.md", "./../../x.md"),
        ] {
            assert_eq!(
                resolve_reference(from, target),
                Reference::Rejected(RejectReason::EscapesRoot),
                "{from} -> {target}"
            );
        }
        // Traversal that stays inside is fine.
        assert_eq!(resolve_reference("a/b/c.md", "../../x.md"), local("x.md"));
    }

    #[test]
    fn absolute_unsafe_and_degenerate_targets_are_rejected() {
        for target in ["/etc/passwd", "/", "%2fetc/passwd"] {
            assert_eq!(
                resolve_reference("docs/a.md", target),
                Reference::Rejected(RejectReason::Absolute),
                "{target}"
            );
        }
        for target in [
            "..\\x.md", "a\\b.md", "a%5Cb.md", "a%00.md", "a%0A.md", "%ff.md", "%c3.md",
        ] {
            assert_eq!(
                resolve_reference("docs/a.md", target),
                Reference::Rejected(RejectReason::Unsafe),
                "{target:?}"
            );
        }
        for (from, target) in [
            ("docs/a.md", ""),
            ("docs/a.md", "  "),
            ("docs/a.md", ".."),
            ("README.md", "./"),
            ("README.md", "."),
        ] {
            assert_eq!(
                resolve_reference(from, target),
                Reference::Rejected(RejectReason::NotAFile),
                "{from} -> {target:?}"
            );
        }
        // Pointing at the containing directory is a valid local reference.
        assert_eq!(resolve_reference("docs/a.md", "./"), local("docs"));
        assert_eq!(resolve_reference("docs/a.md", "."), local("docs"));
    }

    #[test]
    fn urls_are_inert_external_references() {
        for (target, scheme) in [
            ("https://example.com/a", "https"),
            ("HTTP://EXAMPLE.COM", "http"),
            ("mailto:me@example.com", "mailto"),
            ("ftp://host/file", "ftp"),
            ("file:///etc/passwd", "file"),
            ("javascript:alert(1)", "javascript"),
            ("data:text/plain;base64,AAAA", "data"),
            ("git+ssh://host/repo.git", "git+ssh"),
            ("C:/Windows/system.ini", "c"),
            ("//cdn.example.com/lib.js", ""),
        ] {
            assert_eq!(
                resolve_reference("docs/a.md", target),
                Reference::External {
                    scheme: scheme.into()
                },
                "{target}"
            );
        }
        // Not a scheme: the `:` is after a path separator or a space.
        assert_eq!(resolve_reference("a.md", "dir/a:b.md"), local("dir/a:b.md"));
        assert_eq!(
            resolve_reference("a.md", "1abc:def.md"),
            local("1abc:def.md")
        );
    }

    // ── filesystem verification ──────────────────────────────────────────

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        (tmp, repo)
    }

    #[test]
    fn verify_reports_file_directory_and_missing() {
        let (_tmp, repo) = repo();
        write(&repo, "docs/a.md", "a");
        assert_eq!(verify_local(&repo, "docs/a.md"), TargetStatus::File);
        assert_eq!(verify_local(&repo, "docs"), TargetStatus::Directory);
        assert_eq!(verify_local(&repo, "docs/nope.md"), TargetStatus::Missing);
        assert_eq!(
            verify_local(&repo, "nope/deeper/x.md"),
            TargetStatus::Missing
        );
        assert_eq!(
            verify_local(&repo.join("absent"), "x.md"),
            TargetStatus::Missing
        );
    }

    #[test]
    fn verify_refuses_unnormalized_input() {
        let (_tmp, repo) = repo();
        for bad in ["../x.md", "docs/../x.md", "./x.md", "a//b", ""] {
            assert_eq!(
                verify_local(&repo, bad),
                TargetStatus::EscapesRoot,
                "{bad:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn verify_never_reads_content() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, repo) = repo();
        write(&repo, "docs/locked.md", "secret");
        fs::set_permissions(
            repo.join("docs/locked.md"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        // Still classified: only metadata is consulted.
        assert_eq!(verify_local(&repo, "docs/locked.md"), TargetStatus::File);
        fs::set_permissions(
            repo.join("docs/locked.md"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_judged_by_where_they_land() {
        use std::os::unix::fs::symlink;
        let (tmp, repo) = repo();
        let outside = tmp.path().join("outside");
        write(&outside, "secret.md", "TOP SECRET");
        write(&repo, "docs/real.md", "real");
        fs::create_dir_all(repo.join("links")).unwrap();
        symlink(outside.join("secret.md"), repo.join("links/leak.md")).unwrap();
        symlink(&outside, repo.join("links/dir")).unwrap();
        symlink(repo.join("docs/real.md"), repo.join("links/alias.md")).unwrap();
        symlink(
            outside.join("absent.md"),
            repo.join("links/dangling-out.md"),
        )
        .unwrap();
        symlink(
            repo.join("docs/absent.md"),
            repo.join("links/dangling-in.md"),
        )
        .unwrap();

        assert_eq!(
            verify_local(&repo, "links/leak.md"),
            TargetStatus::EscapesRoot
        );
        assert_eq!(
            verify_local(&repo, "links/dir/secret.md"),
            TargetStatus::EscapesRoot
        );
        assert_eq!(verify_local(&repo, "links/dir"), TargetStatus::EscapesRoot);
        assert_eq!(verify_local(&repo, "links/alias.md"), TargetStatus::File);
        // Dangling links never reveal where they point.
        assert_eq!(
            verify_local(&repo, "links/dangling-out.md"),
            TargetStatus::Missing
        );
        assert_eq!(
            verify_local(&repo, "links/dangling-in.md"),
            TargetStatus::Missing
        );
    }

    // ── no authorization or selection effect ─────────────────────────────

    #[test]
    fn frontmatter_dependencies_and_associations_are_not_references() {
        let d = doc(
            "docs/a.md",
            "---\nknowledge_sources: src/lib.rs\nknowledge_paths: src/**\n---\nSee [b](b.md).\n",
        );
        let refs = document_references(&d);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, "b.md");
        // The declared fields remain separate, untouched metadata.
        assert_eq!(d.parsed.knowledge_sources, vec!["src/lib.rs"]);
        assert_eq!(d.parsed.knowledge_paths, vec!["src/**"]);
    }

    #[test]
    fn rejected_references_expose_no_path_to_open() {
        let d = doc(
            "docs/a.md",
            "[a](../../etc/passwd) [b](/etc/passwd) [c](file:///etc/passwd) [d](https://x.y/z) [e](ok.md)\n",
        );
        let refs = document_references(&d);
        let openable: Vec<&str> = refs
            .iter()
            .filter_map(|r| match &r.resolution {
                Reference::Local { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(openable, vec!["docs/ok.md"]);
        assert!(matches!(refs[2].resolution, Reference::External { .. }));
        assert!(matches!(refs[3].resolution, Reference::External { .. }));
    }

    #[test]
    fn links_never_select_additional_index_inputs() {
        use crate::knowledge::repository::discovery::{DiscoveryOptions, discover};
        use crate::knowledge::repository::manifest::KnowledgeManifest;
        use crate::knowledge::repository::root::{RootRequest, resolve_root};

        let (tmp, repo) = repo();
        fs::create_dir_all(repo.join(".git")).unwrap();
        write(&outside_of(&tmp), "secret.md", "TOP SECRET");
        write(
            &repo,
            "docs/a.md",
            "[unselected](../private/notes.md) [out](../../outside/secret.md) [web](https://example.com)\n",
        );
        write(&repo, "private/notes.md", "private notes");

        let manifest =
            KnowledgeManifest::parse("version = 1\n[documents]\ninclude = [\"docs/*.md\"]\n")
                .unwrap();
        let root = resolve_root(&RootRequest {
            cwd: Some(&repo),
            ..Default::default()
        })
        .unwrap();
        let found = discover(
            &root,
            &manifest,
            &DiscoveryOptions {
                max_files: 100,
                max_file_bytes: 1 << 20,
                max_total_bytes: 1 << 24,
                allow_ignored: false,
            },
        );
        let paths: Vec<&str> = found
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        // Only the manifest selects inputs; the links in a.md added nothing.
        assert_eq!(paths, vec!["docs/a.md"]);
        assert!(found.is_complete());

        // Resolution confirms the unselected file is referenced yet not granted.
        let text = fs::read_to_string(repo.join("docs/a.md")).unwrap();
        let d = doc("docs/a.md", &text);
        let refs = document_references(&d);
        assert_eq!(refs[0].resolution, local("private/notes.md"));
        assert_eq!(verify_local(&repo, "private/notes.md"), TargetStatus::File);
        assert!(!manifest.documents.is_selected("private/notes.md"));
        assert_eq!(
            refs[1].resolution,
            Reference::Rejected(RejectReason::EscapesRoot)
        );
    }

    fn outside_of(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("outside")
    }

    // ── diagnostics ──────────────────────────────────────────────────────

    #[test]
    fn diagnostics_flag_only_refused_references_with_relative_paths() {
        let d = doc(
            "docs/a.md",
            "ok [a](b.md) up [c](../../x.md) abs [d](/etc/passwd) web [e](https://x.y) bad [f](a%5Cb.md)\nmissing [g](nope.md)\n",
        );
        let diags = reference_diagnostics(&d);
        let codes: Vec<&str> = diags.iter().map(|d| d.code).collect();
        assert_eq!(codes.len(), 3, "{diags:?}");
        assert!(codes.contains(&super::codes::ESCAPES_ROOT));
        assert!(codes.contains(&super::codes::ABSOLUTE));
        assert!(codes.contains(&super::codes::UNSAFE));
        for diagnostic in &diags {
            assert_eq!(diagnostic.path, "docs/a.md");
            assert_eq!(diagnostic.severity, DiagnosticSeverity::Warning);
            assert!(
                diagnostic.message.starts_with("line 1:"),
                "{}",
                diagnostic.message
            );
        }
        // Valid siblings stay usable: diagnostics are warnings, not exclusions.
        assert_eq!(d.relative_path(), "docs/a.md");
    }

    #[test]
    fn long_targets_are_truncated_in_diagnostics() {
        let long = "../".repeat(200);
        let d = doc("a.md", &format!("[x]({long}y.md)\n"));
        let diags = reference_diagnostics(&d);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.len() < 260, "{}", diags[0].message.len());
    }
}

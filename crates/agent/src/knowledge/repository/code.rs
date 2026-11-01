//! Source-derived code records.
//!
//! For every selected code file the index keeps a *path-level* record (see
//! [`SourceRecord`](super::store::SourceRecord)) and, when a symbol extractor
//! exists for its language, a [`CodeRecord`]: one [`SymbolRecord`] per
//! declaration (kind, qualified name, signature, parent, original line range,
//! digest) plus verbatim source documentation.
//!
//! The records are produced by the same tree-sitter extractors the `index`
//! tool uses ([`SymbolIndex`]); nothing here is a second parser and nothing
//! changes the `index` tool's output.
//!
//! # What is *not* recorded
//!
//! Records are syntax, not semantics. Imports are kept as the declaration text
//! that was written (`use a::b;`), never resolved to a target. No purpose,
//! invariant, call edge or owner is inferred from a name or a path, and no
//! model is consulted.
//!
//! # Partial extraction is explicit
//!
//! A file whose language has no extractor, that is not UTF-8, or that fails to
//! parse keeps its path-level record and gets a [`CodeStatus`] saying why plus a
//! diagnostic; it is never silently dropped and never described as indexed.
//!
//! # Source documentation (Rust)
//!
//! `//!` / `/*! */` module documentation and `///` / `/** */` item
//! documentation are copied *verbatim* (comment markers and one following space
//! removed, nothing else) with their original line coordinates. They are
//! labelled source documentation, never presented as a derived purpose.
//!
//! # Invalidation
//!
//! [`CODE_EXTRACTOR_VERSION`] is part of the code fingerprint reconciliation
//! stores next to the corpus. Bump it whenever extraction output could change;
//! the next refresh re-extracts every code file (and only code files).

use super::document::DiagnosticSeverity;
use super::store::IndexDiagnostic;
use crate::index::outline_index::common::get_language_for_extension;
use crate::index::symbol_index::{SymbolEntry, SymbolIndex, SymbolKind};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

/// Version of the extraction format. Bumping it re-extracts all code files.
pub const CODE_EXTRACTOR_VERSION: u32 = 1;

/// Prefix of a symbol's retrievable ID.
pub const SYMBOL_PREFIX: &str = "symbol:";

/// Stable diagnostic codes for partial extraction.
pub mod codes {
    pub const UNSUPPORTED_LANGUAGE: &str = "code_unsupported_language";
    pub const NOT_UTF8: &str = "code_not_utf8";
    pub const PARSE_FAILED: &str = "code_parse_failed";
    pub const EXTRACTOR_PANIC: &str = "code_extractor_panic";
}

/// How far extraction got for one code file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeStatus {
    /// Symbols were extracted (possibly none, for an empty file).
    Extracted,
    /// No extractor exists for the file's language; path-level only.
    Unsupported,
    /// An extractor exists but could not process the file; path-level only.
    Failed,
}

impl CodeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Extracted => "extracted",
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
        }
    }

    pub(super) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "extracted" => Some(Self::Extracted),
            "unsupported" => Some(Self::Unsupported),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One declaration found in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRecord {
    /// Position in source order (parents precede their children).
    pub ordinal: usize,
    /// Portable retrievable ID, stable across checkout paths.
    pub key: String,
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: String,
    /// Declaration text as the extractor reports it. For imports this is the
    /// declaration as written, not a resolved dependency.
    pub signature: String,
    /// Qualified name of the enclosing declaration.
    pub parent: Option<String>,
    /// Nesting depth; 0 for top-level declarations.
    pub depth: u32,
    /// Zero-based repeat index among records sharing `(kind, qualified_name)`.
    pub occurrence: u32,
    /// 1-based inclusive original line range.
    pub start_line: usize,
    pub end_line: usize,
    /// Hex digest of the declaration's bytes (change detection).
    pub digest: String,
    pub byte_len: usize,
    pub line_count: usize,
}

/// Documentation copied verbatim from the source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDoc {
    pub ordinal: usize,
    /// The documented symbol, or `None` for module documentation.
    pub symbol_key: Option<String>,
    /// 1-based inclusive original line range of the comment lines.
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

/// Everything extracted from one code file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRecord {
    /// Language name when the extension is a known one.
    pub language: Option<String>,
    pub status: CodeStatus,
    pub symbols: Vec<SymbolRecord>,
    pub docs: Vec<SourceDoc>,
}

/// A [`CodeRecord`] and the diagnostic describing partial extraction, if any.
#[derive(Debug, Clone)]
pub struct CodeExtraction {
    pub record: CodeRecord,
    pub diagnostic: Option<IndexDiagnostic>,
}

/// The portable ID of a symbol.
pub fn symbol_key(path: &str, kind: &str, qualified_name: &str, occurrence: u32) -> String {
    let mut key = format!("{SYMBOL_PREFIX}{path}#{kind}:{qualified_name}");
    if occurrence > 0 {
        key.push_str(&format!("~{occurrence}"));
    }
    key
}

/// The repository-relative path a symbol ID names, if it is shaped like one.
pub fn symbol_key_path(key: &str) -> Option<&str> {
    let rest = key.strip_prefix(SYMBOL_PREFIX)?;
    rest.split_once('#').map(|(path, _)| path)
}

/// Extract the code record of one file. Pure: no I/O, no clock, no machine-local
/// data, so equal bytes and path give equal records on any checkout.
pub fn extract_code(path: &str, bytes: &[u8]) -> CodeExtraction {
    let extension = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    let Some(language) = get_language_for_extension(extension) else {
        let message = if extension.is_empty() {
            "no symbol extractor for files without an extension; path-level record only".to_string()
        } else {
            format!("no symbol extractor for `.{extension}` files; path-level record only")
        };
        return partial(
            path,
            None,
            CodeStatus::Unsupported,
            DiagnosticSeverity::Info,
            codes::UNSUPPORTED_LANGUAGE,
            message,
        );
    };
    let Ok(source) = std::str::from_utf8(bytes) else {
        return partial(
            path,
            Some(language),
            CodeStatus::Failed,
            DiagnosticSeverity::Warning,
            codes::NOT_UTF8,
            "source is not valid UTF-8; path-level record only".to_string(),
        );
    };

    // A panicking extractor must cost one file its symbols, not every future
    // retrieval its index.
    let index = match catch_unwind(AssertUnwindSafe(|| {
        SymbolIndex::from_source(source, language)
    })) {
        Ok(Ok(index)) => index,
        Ok(Err(error)) => {
            return partial(
                path,
                Some(language),
                CodeStatus::Failed,
                DiagnosticSeverity::Warning,
                codes::PARSE_FAILED,
                format!("symbols could not be extracted ({error}); path-level record only"),
            );
        }
        Err(_) => {
            return partial(
                path,
                Some(language),
                CodeStatus::Failed,
                DiagnosticSeverity::Warning,
                codes::EXTRACTOR_PANIC,
                "the symbol extractor failed on this file; path-level record only".to_string(),
            );
        }
    };

    let mut symbols = Vec::new();
    flatten(path, &index.symbols, 0, &mut BTreeMap::new(), &mut symbols);
    let docs = if language == "rust" {
        rust_docs(source, &symbols)
    } else {
        Vec::new()
    };
    CodeExtraction {
        record: CodeRecord {
            language: Some(language.to_string()),
            status: CodeStatus::Extracted,
            symbols,
            docs,
        },
        diagnostic: None,
    }
}

fn partial(
    path: &str,
    language: Option<&str>,
    status: CodeStatus,
    severity: DiagnosticSeverity,
    code: &str,
    message: String,
) -> CodeExtraction {
    CodeExtraction {
        record: CodeRecord {
            language: language.map(str::to_string),
            status,
            symbols: Vec::new(),
            docs: Vec::new(),
        },
        diagnostic: Some(IndexDiagnostic::new(Some(path), severity, code, message)),
    }
}

/// Pre-order flattening: source order, parents before children.
fn flatten(
    path: &str,
    entries: &[SymbolEntry],
    depth: u32,
    seen: &mut BTreeMap<(&'static str, String), u32>,
    out: &mut Vec<SymbolRecord>,
) {
    for entry in entries {
        let kind = entry.kind.as_str();
        let counter = seen
            .entry((kind, entry.qualified_name.clone()))
            .or_default();
        let occurrence = *counter;
        *counter += 1;
        out.push(SymbolRecord {
            ordinal: out.len(),
            key: symbol_key(path, kind, &entry.qualified_name, occurrence),
            kind: entry.kind,
            name: entry.name.clone(),
            qualified_name: entry.qualified_name.clone(),
            signature: entry.signature.clone(),
            parent: entry.parent.clone(),
            depth,
            occurrence,
            start_line: entry.start_line,
            end_line: entry.end_line,
            digest: entry.digest.hash.to_hex(),
            byte_len: entry.digest.byte_len,
            line_count: entry.digest.line_count,
        });
        flatten(path, &entry.children, depth + 1, seen, out);
    }
}

// ─── Rust documentation ──────────────────────────────────────────────────────

/// How far above a declaration a multi-line attribute is searched for.
const MAX_ATTRIBUTE_LINES: usize = 32;
/// How far above a declaration a block comment's opener is searched for.
const MAX_BLOCK_LINES: usize = 400;

/// A run of comment lines: 0-based inclusive line indexes and the text.
struct Chunk {
    first: usize,
    last: usize,
    text: String,
}

fn rust_docs(source: &str, symbols: &[SymbolRecord]) -> Vec<SourceDoc> {
    let lines: Vec<&str> = source.lines().collect();
    let mut docs = Vec::new();
    if let Some(chunk) = module_doc(&lines) {
        docs.push(chunk);
    }
    for symbol in symbols {
        if symbol.kind == SymbolKind::Import {
            continue;
        }
        if let Some(chunk) = item_doc(&lines, symbol.start_line) {
            docs.push(SourceDoc {
                ordinal: 0,
                symbol_key: Some(symbol.key.clone()),
                start_line: chunk.first + 1,
                end_line: chunk.last + 1,
                text: chunk.text,
            });
        }
    }
    for (ordinal, doc) in docs.iter_mut().enumerate() {
        doc.ordinal = ordinal;
    }
    docs
}

fn module_doc(lines: &[&str]) -> Option<SourceDoc> {
    let first = lines.iter().position(|line| !line.trim().is_empty())?;
    let opener = lines[first].trim_start();
    let chunk = if opener.starts_with("//!") {
        let last = (first..lines.len())
            .take_while(|&i| lines[i].trim_start().starts_with("//!"))
            .last()?;
        Chunk {
            first,
            last,
            text: line_doc_text(&lines[first..=last], "//!"),
        }
    } else if opener.starts_with("/*!") {
        let last = (first..lines.len()).find(|&i| lines[i].contains("*/"))?;
        Chunk {
            first,
            last,
            text: block_doc_text(&lines[first..=last], "/*!"),
        }
    } else {
        return None;
    };
    (!chunk.text.trim().is_empty()).then_some(SourceDoc {
        ordinal: 0,
        symbol_key: None,
        start_line: chunk.first + 1,
        end_line: chunk.last + 1,
        text: chunk.text,
    })
}

/// Outer documentation attached to the declaration starting at 1-based
/// `start_line`: the `///` / `/** */` comments directly above it, looking
/// through attributes.
fn item_doc(lines: &[&str], start_line: usize) -> Option<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut index = start_line.checked_sub(1)?.min(lines.len());
    while index > 0 {
        let above = index - 1;
        let line = lines[above].trim();
        if is_line_doc(line) {
            let mut first = above;
            while first > 0 && is_line_doc(lines[first - 1].trim()) {
                first -= 1;
            }
            chunks.push(Chunk {
                first,
                last: above,
                text: line_doc_text(&lines[first..=above], "///"),
            });
            index = first;
        } else if line.starts_with("#[") {
            index = above;
        } else if line.ends_with("*/") {
            let Some(first) = block_opener(lines, above) else {
                break;
            };
            chunks.push(Chunk {
                first,
                last: above,
                text: block_doc_text(&lines[first..=above], "/**"),
            });
            index = first;
        } else if line.ends_with(']') {
            // The tail of a multi-line attribute: skip to its `#[` line.
            let Some(start) = attribute_start(lines, above) else {
                break;
            };
            index = start;
        } else {
            break;
        }
    }
    let first = chunks.last()?.first;
    let last = chunks.first()?.last;
    let text = chunks
        .iter()
        .rev()
        .map(|chunk| chunk.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(Chunk { first, last, text })
}

fn is_line_doc(line: &str) -> bool {
    line.starts_with("///") && !line.starts_with("////")
}

/// `///` or `//!` lines with the marker and one following space removed.
fn line_doc_text(lines: &[&str], marker: &str) -> String {
    lines
        .iter()
        .map(|line| {
            let rest = line.trim_start().strip_prefix(marker).unwrap_or(line);
            rest.strip_prefix(' ').unwrap_or(rest)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 0-based index of the `/**` line that opens the block ending at `end`.
fn block_opener(lines: &[&str], end: usize) -> Option<usize> {
    let lowest = end.saturating_sub(MAX_BLOCK_LINES);
    (lowest..=end).rev().find(|&i| {
        let line = lines[i].trim_start();
        line.starts_with("/**") && !line.starts_with("/***") && !line.starts_with("/**/")
    })
}

/// Block-comment text: opener and closer removed, a leading `*` and one space
/// removed from continuation lines, blank edge lines trimmed.
fn block_doc_text(lines: &[&str], opener: &str) -> String {
    let mut text: Vec<&str> = Vec::with_capacity(lines.len());
    for (position, line) in lines.iter().enumerate() {
        let mut line = line.trim_start();
        if position == 0 {
            line = line.strip_prefix(opener).unwrap_or(line);
        } else {
            line = line.strip_prefix('*').unwrap_or(line);
        }
        let trimmed_end = line.trim_end();
        if position + 1 == lines.len() {
            line = trimmed_end.strip_suffix("*/").unwrap_or(trimmed_end);
            line = line.trim_end_matches([' ', '\t']);
        }
        text.push(line.strip_prefix(' ').unwrap_or(line));
    }
    while text.first().is_some_and(|line| line.trim().is_empty()) {
        text.remove(0);
    }
    while text.last().is_some_and(|line| line.trim().is_empty()) {
        text.pop();
    }
    text.join("\n")
}

/// 0-based index of the `#[` line that starts the attribute ending at `end`.
fn attribute_start(lines: &[&str], end: usize) -> Option<usize> {
    let lowest = end.saturating_sub(MAX_ATTRIBUTE_LINES);
    for i in (lowest..=end).rev() {
        let line = lines[i].trim();
        if line.starts_with("#[") {
            return Some(i);
        }
        if i != end && (line.ends_with(';') || line.ends_with('{') || line.ends_with('}')) {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &str = "\
//! Scheduler module.
//!
//! Dispatches tasks.

use std::collections::BTreeMap;

/// A queue of tasks.
///
/// Ordered by insertion.
#[derive(Debug)]
pub struct Queue {
    items: Vec<u32>,
}

impl Queue {
    /// Push one task.
    pub fn push(&mut self, item: u32) {
        self.items.push(item);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/** Block documented. */
pub fn helper() {}

#[derive(
    Debug,
    Clone,
)]
/// After a multi-line attribute.
pub enum Mode {
    A,
}

//// not a doc comment
pub fn plain() {}
";

    fn extract(path: &str, source: &str) -> CodeRecord {
        extract_code(path, source.as_bytes()).record
    }

    fn find<'a>(record: &'a CodeRecord, qualified: &str) -> &'a SymbolRecord {
        record
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == qualified)
            .unwrap_or_else(|| panic!("no symbol {qualified}"))
    }

    #[test]
    fn rust_declarations_carry_kind_signature_parent_and_ranges() {
        let record = extract("src/queue.rs", RUST);
        assert_eq!(record.status, CodeStatus::Extracted);
        assert_eq!(record.language.as_deref(), Some("rust"));

        let queue = find(&record, "Queue");
        assert_eq!(queue.kind, SymbolKind::Struct);
        assert_eq!(queue.signature, "pub struct Queue {");
        assert_eq!((queue.start_line, queue.end_line), (11, 13));
        assert_eq!(queue.parent, None);
        assert_eq!(queue.depth, 0);
        assert_eq!(queue.key, "symbol:src/queue.rs#struct:Queue");

        let push = find(&record, "Queue::push");
        assert!(matches!(
            push.kind,
            SymbolKind::Method | SymbolKind::Function
        ));
        assert_eq!(push.parent.as_deref(), Some("Queue"));
        assert_eq!(push.depth, 1);
        assert!(push.signature.contains("fn push"));
        assert_eq!(push.digest.len(), 16);
        assert!(push.start_line < push.end_line);

        // The impl block and the field are declarations of their own.
        assert!(
            record
                .symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Impl)
        );
        assert!(
            record
                .symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Field && symbol.name == "items")
        );
    }

    #[test]
    fn imports_are_syntax_not_resolved_dependencies() {
        let record = extract("src/queue.rs", RUST);
        let import = record
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Import)
            .unwrap();
        assert_eq!(import.signature, "use std::collections::BTreeMap");
        assert_eq!(import.start_line, 5);
    }

    #[test]
    fn symbols_are_in_source_order_with_parents_first() {
        let record = extract("src/queue.rs", RUST);
        let ordinals: Vec<usize> = record.symbols.iter().map(|s| s.ordinal).collect();
        assert_eq!(ordinals, (0..record.symbols.len()).collect::<Vec<_>>());
        let queue = find(&record, "Queue").ordinal;
        let field = find(&record, "Queue::items").ordinal;
        assert!(queue < field);
    }

    #[test]
    fn repeated_declarations_get_distinct_stable_keys() {
        let source = "impl A {}\nimpl A {}\nfn f() {}\nfn f() {}\n";
        let record = extract("a.rs", source);
        let keys: Vec<&str> = record.symbols.iter().map(|s| s.key.as_str()).collect();
        assert!(keys.contains(&"symbol:a.rs#function:f"));
        assert!(keys.contains(&"symbol:a.rs#function:f~1"));
        let unique: std::collections::BTreeSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
    }

    #[test]
    fn rust_documentation_is_verbatim_with_coordinates() {
        let record = extract("src/queue.rs", RUST);

        let module = record.docs.iter().find(|d| d.symbol_key.is_none()).unwrap();
        assert_eq!(module.text, "Scheduler module.\n\nDispatches tasks.");
        assert_eq!((module.start_line, module.end_line), (1, 3));

        let queue = find(&record, "Queue");
        let doc = record
            .docs
            .iter()
            .find(|d| d.symbol_key.as_deref() == Some(queue.key.as_str()))
            .unwrap();
        assert_eq!(doc.text, "A queue of tasks.\n\nOrdered by insertion.");
        assert_eq!((doc.start_line, doc.end_line), (7, 9));

        let push = find(&record, "Queue::push");
        let doc = record
            .docs
            .iter()
            .find(|d| d.symbol_key.as_deref() == Some(push.key.as_str()))
            .unwrap();
        assert_eq!(doc.text, "Push one task.");
    }

    #[test]
    fn block_documentation_and_multi_line_attributes_are_handled() {
        let record = extract("src/queue.rs", RUST);
        let doc_of = |name: &str| {
            let key = &find(&record, name).key;
            record
                .docs
                .iter()
                .find(|d| d.symbol_key.as_deref() == Some(key.as_str()))
        };
        assert_eq!(doc_of("helper").unwrap().text, "Block documented.");
        assert_eq!(
            doc_of("Mode").unwrap().text,
            "After a multi-line attribute."
        );
    }

    #[test]
    fn undocumented_and_four_slash_items_have_no_documentation() {
        let record = extract("src/queue.rs", RUST);
        for name in ["Queue::len", "plain"] {
            let key = &find(&record, name).key;
            assert!(
                record
                    .docs
                    .iter()
                    .all(|d| d.symbol_key.as_deref() != Some(key.as_str())),
                "{name} must have no documentation"
            );
        }
    }

    #[test]
    fn documentation_is_never_invented_for_other_languages() {
        let record = extract(
            "tool.py",
            "# comment\ndef run():\n    '''doc'''\n    pass\n",
        );
        assert_eq!(record.status, CodeStatus::Extracted);
        assert!(record.docs.is_empty());
        assert!(record.symbols.iter().any(|s| s.name == "run"));
    }

    #[test]
    fn unsupported_languages_keep_a_path_level_record_with_a_diagnostic() {
        let extraction = extract_code("data/blob.xyz", b"whatever");
        assert_eq!(extraction.record.status, CodeStatus::Unsupported);
        assert_eq!(extraction.record.language, None);
        assert!(extraction.record.symbols.is_empty());
        assert!(extraction.record.docs.is_empty());
        let diagnostic = extraction.diagnostic.unwrap();
        assert_eq!(diagnostic.code, codes::UNSUPPORTED_LANGUAGE);
        assert_eq!(diagnostic.path.as_deref(), Some("data/blob.xyz"));
        assert_eq!(diagnostic.severity, DiagnosticSeverity::Info);

        let bare = extract_code("Makefile", b"all:\n");
        assert_eq!(bare.record.status, CodeStatus::Unsupported);
    }

    #[test]
    fn non_utf8_source_fails_visibly_without_symbols() {
        let extraction = extract_code("src/bad.rs", &[0xff, 0xfe, b'f', b'n']);
        assert_eq!(extraction.record.status, CodeStatus::Failed);
        assert_eq!(extraction.record.language.as_deref(), Some("rust"));
        assert_eq!(extraction.diagnostic.unwrap().code, codes::NOT_UTF8);
    }

    #[test]
    fn extraction_is_pure_and_checkout_independent() {
        let a = extract_code("src/queue.rs", RUST.as_bytes());
        let b = extract_code("src/queue.rs", RUST.as_bytes());
        assert_eq!(a.record, b.record);
        for symbol in &a.record.symbols {
            assert!(!symbol.key.contains("/Users/") && !symbol.key.starts_with("symbol:/"));
        }
    }

    #[test]
    fn symbol_keys_round_trip_to_their_path() {
        let key = symbol_key("src/a.rs", "function", "A::f", 2);
        assert_eq!(key, "symbol:src/a.rs#function:A::f~2");
        assert_eq!(symbol_key_path(&key), Some("src/a.rs"));
        assert_eq!(symbol_key_path("file:src/a.rs"), None);
    }
}

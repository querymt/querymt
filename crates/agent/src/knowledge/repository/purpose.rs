//! File/Purpose rows: authored navigation records.
//!
//! A document may contain a two-column Markdown table whose first column names
//! a repository path and whose second says what it is for:
//!
//! ```markdown
//! | File      | Purpose                         |
//! |-----------|---------------------------------|
//! | `mod.rs`  | Wires the scheduler together.   |
//! | `queue.rs`|                                 |
//! ```
//!
//! Each row becomes a [`PurposeRow`]: a *citable* record (document path and
//! line) that says what the **author** wrote. The text is never generated, and
//! an empty purpose stays empty.
//!
//! * Tables are optional. Nothing requires one per directory, one per file, or
//!   an exhaustive listing.
//! * A path is resolved relative to the document that contains it, with the
//!   same confinement rules as Markdown links; URLs, fragments and paths that
//!   leave the checkout produce no row.
//! * Rows inside a managed inventory block
//!   (`<!-- qmt:inventory:begin -->` … `<!-- qmt:inventory:end -->`) are flagged
//!   [`PurposeRow::managed`]; rows elsewhere are ordinary. Both are parsed the
//!   same way.
//! * Tables inside fenced code blocks are examples, not data.

use super::identity::{is_closing_fence, opening_fence};
use super::references::{Reference, resolve_reference};
use std::collections::BTreeMap;

/// Opens a managed inventory block.
pub const INVENTORY_BEGIN: &str = "<!-- qmt:inventory:begin -->";
/// Closes a managed inventory block.
pub const INVENTORY_END: &str = "<!-- qmt:inventory:end -->";
/// Prefix of a purpose row's retrievable ID.
pub const PURPOSE_PREFIX: &str = "purpose:";

/// One row of a File/Purpose table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurposeRow {
    /// Document containing the row.
    pub doc_path: String,
    /// Position among the document's rows.
    pub ordinal: usize,
    /// Repository-relative path the row describes (no trailing slash).
    pub target_path: String,
    /// The author's text, whitespace-collapsed. May be empty.
    pub purpose: String,
    /// 1-based line of the row in the document.
    pub line: usize,
    /// Zero-based repeat index among rows of this document naming the same path.
    pub occurrence: u32,
    /// The row sits inside a managed inventory block.
    pub managed: bool,
}

impl PurposeRow {
    /// Portable retrievable ID: `purpose:<document>#<target>[~n]`.
    pub fn key(&self) -> String {
        purpose_key(&self.doc_path, &self.target_path, self.occurrence)
    }
}

/// The portable ID of a purpose row.
pub fn purpose_key(doc_path: &str, target_path: &str, occurrence: u32) -> String {
    let mut key = format!("{PURPOSE_PREFIX}{doc_path}#{target_path}");
    if occurrence > 0 {
        key.push_str(&format!("~{occurrence}"));
    }
    key
}

/// The document path a purpose ID names, if it is shaped like one.
pub fn purpose_key_path(key: &str) -> Option<&str> {
    key.strip_prefix(PURPOSE_PREFIX)?
        .split_once('#')
        .map(|(path, _)| path)
}

/// Lines of `text` that are not inside a fenced code block, as
/// `(zero-based index, line without its terminator)`.
pub(super) fn unfenced_lines(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for (index, raw) in text.split_inclusive('\n').enumerate() {
        match fence {
            Some((ch, len)) => {
                if is_closing_fence(raw, ch, len) {
                    fence = None;
                }
            }
            None => {
                if let Some(opened) = opening_fence(raw) {
                    fence = Some(opened);
                } else {
                    out.push((index, raw.trim_end_matches(['\n', '\r'])));
                }
            }
        }
    }
    out
}

/// Parse every File/Purpose row of a document body. `first_line` is the
/// 1-based line of `body`'s first line in the original file.
pub fn parse_purpose_rows(doc_path: &str, body: &str, first_line: usize) -> Vec<PurposeRow> {
    let lines = unfenced_lines(body);
    let mut rows: Vec<PurposeRow> = Vec::new();
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    let mut managed = false;
    let mut i = 0;
    while i < lines.len() {
        let (index, line) = lines[i];
        let trimmed = line.trim();
        if trimmed == INVENTORY_BEGIN {
            managed = true;
        } else if trimmed == INVENTORY_END {
            managed = false;
        } else if is_header(trimmed)
            && lines
                .get(i + 1)
                .is_some_and(|&(next, text)| next == index + 1 && is_separator(text.trim()))
        {
            i += 2;
            // Rows continue while consecutive lines are table rows.
            let mut expected = index + 2;
            while let Some(&(row_index, text)) = lines.get(i) {
                let text = text.trim();
                if row_index != expected || !text.starts_with('|') {
                    break;
                }
                if let Some((target, purpose)) = parse_row(doc_path, text) {
                    let occurrence = seen.entry(target.clone()).or_default();
                    rows.push(PurposeRow {
                        doc_path: doc_path.to_string(),
                        ordinal: rows.len(),
                        target_path: target,
                        purpose,
                        line: first_line + row_index,
                        occurrence: *occurrence,
                        managed,
                    });
                    *occurrence += 1;
                }
                expected += 1;
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    rows
}

/// Split a table line into trimmed cells. `\|` is a literal pipe.
pub(super) fn cells(line: &str) -> Vec<String> {
    let inner = line.trim();
    let inner = inner.strip_prefix('|').unwrap_or(inner);
    let inner = match inner.strip_suffix('|') {
        Some(rest) if !rest.ends_with('\\') => rest,
        _ => inner,
    };
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                current.push('|');
                chars.next();
            }
            '|' => out.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    out.push(current);
    out.into_iter()
        .map(|cell| cell.trim().to_string())
        .collect()
}

pub(super) fn is_header(line: &str) -> bool {
    if !line.starts_with('|') {
        return false;
    }
    let cells = cells(line);
    cells.len() == 2
        && matches!(cells[0].to_lowercase().as_str(), "file" | "files" | "path")
        && matches!(cells[1].to_lowercase().as_str(), "purpose" | "description")
}

pub(super) fn is_separator(line: &str) -> bool {
    if !line.starts_with('|') {
        return false;
    }
    let cells = cells(line);
    cells.len() == 2
        && cells.iter().all(|cell| {
            let core = cell.trim_matches(':');
            !core.is_empty() && core.chars().all(|c| c == '-')
        })
}

fn parse_row(doc_path: &str, line: &str) -> Option<(String, String)> {
    let cells = cells(line);
    let target = target_text(cells.first()?)?;
    let target_path = match resolve_reference(doc_path, &target) {
        Reference::Local { path, .. } => path.trim_end_matches('/').to_string(),
        _ => return None,
    };
    if target_path.is_empty() {
        return None;
    }
    let purpose = cells
        .get(1)
        .map(|cell| cell.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    Some((target_path, purpose))
}

/// The path written in a first-column cell: `` `a/b.rs` ``, `[text](a/b.rs)` or
/// bare text.
fn target_text(cell: &str) -> Option<String> {
    let cell = cell.trim();
    let text = if let Some(rest) = cell.strip_prefix('[')
        && let Some((_, dest)) = rest.split_once("](")
        && let Some(dest) = dest.strip_suffix(')')
    {
        dest.trim().to_string()
    } else {
        cell.trim_matches('`').trim().to_string()
    };
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(body: &str) -> Vec<PurposeRow> {
        parse_purpose_rows("crates/agent/AGENTS.md", body, 10)
    }

    #[test]
    fn rows_resolve_relative_to_their_document_and_keep_the_authors_text() {
        let found = rows(
            "Intro\n\n| File | Purpose |\n|------|---------|\n| `src/lib.rs` | Crate root. |\n| [util.rs](src/util.rs) | Helpers   for   all. |\n",
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].target_path, "crates/agent/src/lib.rs");
        assert_eq!(found[0].purpose, "Crate root.");
        assert_eq!(found[0].line, 10 + 4);
        assert_eq!(found[1].target_path, "crates/agent/src/util.rs");
        assert_eq!(found[1].purpose, "Helpers for all.");
        assert!(found.iter().all(|row| !row.managed));
    }

    #[test]
    fn empty_purposes_stay_empty_and_directories_lose_their_slash() {
        let found = rows("| Path | Description |\n|:--|--:|\n| `src/` | |\n| `a.rs` |\n");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].target_path, "crates/agent/src");
        assert_eq!(found[0].purpose, "");
        assert_eq!(found[1].target_path, "crates/agent/a.rs");
        assert_eq!(found[1].purpose, "");
    }

    #[test]
    fn rows_inside_a_managed_block_are_flagged_and_ordinary_rows_are_kept() {
        let body = "| File | Purpose |\n|--|--|\n| `a.rs` | Authored. |\n\n<!-- qmt:inventory:begin -->\n| File | Purpose |\n|--|--|\n| `b.rs` | |\n<!-- qmt:inventory:end -->\n\n| File | Purpose |\n|--|--|\n| `c.rs` | After. |\n";
        let found = rows(body);
        let flags: Vec<(&str, bool)> = found
            .iter()
            .map(|row| (row.target_path.rsplit('/').next().unwrap(), row.managed))
            .collect();
        assert_eq!(flags, [("a.rs", false), ("b.rs", true), ("c.rs", false)]);
        assert_eq!(
            found.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
            [0, 1, 2]
        );
    }

    #[test]
    fn urls_fragments_escapes_and_fenced_tables_produce_no_rows() {
        let body = "| File | Purpose |\n|--|--|\n| https://example.com/x | url |\n| #anchor | frag |\n| ../../../../outside.rs | escape |\n| /abs.rs | abs |\n| `ok.rs` | fine |\n\n```\n| File | Purpose |\n|--|--|\n| `fenced.rs` | example |\n```\n";
        let found = rows(body);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].target_path, "crates/agent/ok.rs");
    }

    #[test]
    fn other_tables_and_broken_tables_are_ignored() {
        assert!(rows("| Name | Value |\n|--|--|\n| a.rs | x |\n").is_empty());
        assert!(rows("| File | Purpose |\n| a.rs | no separator |\n").is_empty());
        assert!(rows("| File | Purpose | Extra |\n|--|--|--|\n| a.rs | x | y |\n").is_empty());
    }

    #[test]
    fn escaped_pipes_and_duplicate_targets_are_handled() {
        let found = rows("| File | Purpose |\n|--|--|\n| a.rs | one \\| two |\n| a.rs | again |\n");
        assert_eq!(found[0].purpose, "one | two");
        assert_eq!((found[0].occurrence, found[1].occurrence), (0, 1));
        assert_eq!(
            found[0].key(),
            "purpose:crates/agent/AGENTS.md#crates/agent/a.rs"
        );
        assert_eq!(
            found[1].key(),
            "purpose:crates/agent/AGENTS.md#crates/agent/a.rs~1"
        );
        assert_eq!(
            purpose_key_path(&found[1].key()),
            Some("crates/agent/AGENTS.md")
        );
    }

    #[test]
    fn a_table_ends_at_the_first_non_row_line() {
        let found =
            rows("| File | Purpose |\n|--|--|\n| a.rs | x |\nprose\n| b.rs | not a row |\n");
        assert_eq!(found.len(), 1);
    }
}

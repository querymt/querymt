//! Outline index: structural skeleton extraction for source files.
//!
//! Produces a compact, human/LLM-readable outline of a source file using
//! tree-sitter, exposing exact line ranges per item. Designed to complement
//! `read_tool` by enabling targeted reads to relevant sections.

pub mod common;
mod extractors;

#[cfg(test)]
mod tests;

pub use common::{IndexOptions, OutlineError, Section, SkeletonEntry};

use common::get_language_for_extension;
use extractors::extract_outline;
use std::path::Path;

/// Maximum file size (bytes) that the outline index will attempt to parse.
const DEFAULT_MAX_FILE_BYTES: usize = 1024 * 1024; // 1 MB

/// Produce a structural skeleton for a single source file.
///
/// Returns a list of [`Section`]s (imports, types, functions, tests, etc.)
/// each containing [`SkeletonEntry`] items with exact line ranges.
pub fn index_file(path: &Path, options: &IndexOptions) -> Result<Vec<Section>, OutlineError> {
    let source = std::fs::read_to_string(path).map_err(|e| OutlineError::Io(e.to_string()))?;

    let max_bytes = options.max_file_bytes.unwrap_or(DEFAULT_MAX_FILE_BYTES);
    if source.len() > max_bytes {
        return Err(OutlineError::FileTooLarge {
            size: source.len(),
            limit: max_bytes,
        });
    }

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    let language = get_language_for_extension(ext)
        .ok_or_else(|| OutlineError::UnsupportedLanguage(ext.to_string()))?;

    index_source(&source, language, options)
}

/// Produce a structural skeleton from source text and a known language name.
///
/// This is the core entry point used by both `index_file` (file-based) and
/// tests (string-based).
pub fn index_source(
    source: &str,
    language: &str,
    options: &IndexOptions,
) -> Result<Vec<Section>, OutlineError> {
    extract_outline(source, language, options)
}

/// Format an outline as compact plain text suitable for LLM consumption.
pub fn format_outline(path: &str, language: &str, sections: &[Section]) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(&format!("path: {}\n", path));
    out.push_str(&format!("language: {}\n", language));

    for section in sections {
        if section.entries.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&format!("{}:\n", section.name));
        for entry in &section.entries {
            format_entry(&mut out, entry, 1);
        }
    }

    out
}

fn format_entry(out: &mut String, entry: &SkeletonEntry, depth: usize) {
    let indent = "  ".repeat(depth);
    let range = if entry.start_line == entry.end_line {
        format!("[{}]", entry.start_line)
    } else {
        format!("[{}-{}]", entry.start_line, entry.end_line)
    };
    out.push_str(&format!("{}- {} {}\n", indent, entry.label, range));

    for child in &entry.children {
        format_entry(out, child, depth + 1);
    }
}

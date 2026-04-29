//! Language-specific outline extractors.
//!
//! Each sub-module implements extraction for a single language (or family).
//! The public entry point is [`extract_outline`], which dispatches to the
//! correct extractor based on the language name.

pub(crate) mod helpers;

use super::common::{IndexOptions, OutlineError, Section};
use crate::index::symbol_index::{
    SymbolError, SymbolIndex, outline_projection::symbols_to_sections,
};

/// Extract an outline from source text for the given language.
pub fn extract_outline(
    source: &str,
    language: &str,
    options: &IndexOptions,
) -> Result<Vec<Section>, OutlineError> {
    match language {
        "rust" => SymbolIndex::from_source(source, "rust")
            .map(|index| symbols_to_sections(&index.symbols, options))
            .map_err(symbol_error_to_outline),
        "python" => SymbolIndex::from_source(source, "python")
            .map(|index| symbols_to_sections(&index.symbols, options))
            .map_err(symbol_error_to_outline),
        "typescript" | "javascript" => SymbolIndex::from_source(source, language)
            .map(|index| symbols_to_sections(&index.symbols, options))
            .map_err(symbol_error_to_outline),
        "go" => SymbolIndex::from_source(source, "go")
            .map(|index| symbols_to_sections(&index.symbols, options))
            .map_err(symbol_error_to_outline),
        "java" => SymbolIndex::from_source(source, "java")
            .map(|index| symbols_to_sections(&index.symbols, options))
            .map_err(symbol_error_to_outline),
        "c" => SymbolIndex::from_source(source, "c")
            .map(|index| symbols_to_sections(&index.symbols, options))
            .map_err(symbol_error_to_outline),
        "cpp" => SymbolIndex::from_source(source, "cpp")
            .map(|index| symbols_to_sections(&index.symbols, options))
            .map_err(symbol_error_to_outline),
        "csharp" => SymbolIndex::from_source(source, "csharp")
            .map(|index| symbols_to_sections(&index.symbols, options))
            .map_err(symbol_error_to_outline),
        "ruby" => SymbolIndex::from_source(source, "ruby")
            .map(|index| symbols_to_sections(&index.symbols, options))
            .map_err(symbol_error_to_outline),
        other => Err(OutlineError::UnsupportedLanguage(other.to_string())),
    }
}

fn symbol_error_to_outline(error: SymbolError) -> OutlineError {
    match error {
        SymbolError::Io(msg) => OutlineError::Io(msg),
        SymbolError::UnsupportedExtension(ext) | SymbolError::UnsupportedLanguage(ext) => {
            OutlineError::UnsupportedLanguage(ext)
        }
        SymbolError::ParseError(msg) => OutlineError::ParseError(msg),
    }
}

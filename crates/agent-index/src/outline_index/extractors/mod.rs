//! Language-specific outline extractors.
//!
//! Delegates to the shared [`SymbolIndex`] infrastructure and projects
//! symbols into outline sections. The public entry point is [`extract_outline`].

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
    SymbolIndex::from_source(source, language)
        .map(|index| symbols_to_sections(&index.symbols, options))
        .map_err(symbol_error_to_outline)
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

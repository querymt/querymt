pub mod c_family;
pub mod csharp;
pub mod elixir;
pub mod go;
pub mod java;
pub mod python;
pub mod ruby;
pub mod rust;
pub mod typescript;

use super::{SymbolEntry, SymbolError};

/// Safely slice `source` between byte offsets, snapping both boundaries to
/// valid UTF-8 char boundaries. Shared by all language extractors.
pub(crate) fn safe_slice(source: &str, from: usize, to: usize) -> &str {
    let from = source.floor_char_boundary(from.min(source.len()));
    let to = source.ceil_char_boundary(to.min(source.len()));
    &source[from..to]
}

pub fn extract_symbols(source: &str, language: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    match language {
        "c" => c_family::extract_c(source),
        "cpp" => c_family::extract_cpp(source),
        "csharp" => csharp::extract(source),
        "elixir" => elixir::extract(source),
        "go" => go::extract(source),
        "java" => java::extract(source),
        "python" => python::extract(source),
        "ruby" => ruby::extract(source),
        "rust" => rust::extract(source),
        "typescript" | "javascript" => typescript::extract(source, language),
        other => Err(SymbolError::UnsupportedLanguage(other.to_string())),
    }
}

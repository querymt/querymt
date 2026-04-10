//! Language-specific outline extractors.
//!
//! Each sub-module implements extraction for a single language (or family).
//! The public entry point is [`extract_outline`], which dispatches to the
//! correct extractor based on the language name.

mod c_family;
mod csharp;
mod go;
pub(crate) mod helpers;
mod java;
mod python;
mod ruby;
mod rust;
mod typescript;

use super::common::{IndexOptions, OutlineError, Section};

/// Extract an outline from source text for the given language.
pub fn extract_outline(
    source: &str,
    language: &str,
    options: &IndexOptions,
) -> Result<Vec<Section>, OutlineError> {
    match language {
        "rust" => rust::extract(source, options),
        "python" => python::extract(source, options),
        "typescript" | "javascript" => typescript::extract(source, language, options),
        "go" => go::extract(source, options),
        "java" => java::extract(source, options),
        "c" => c_family::extract_c(source, options),
        "cpp" => c_family::extract_cpp(source, options),
        "csharp" => csharp::extract(source, options),
        "ruby" => ruby::extract(source, options),
        other => Err(OutlineError::UnsupportedLanguage(other.to_string())),
    }
}

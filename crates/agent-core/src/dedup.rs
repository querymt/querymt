//! Duplicate-code warning types carried by tool-result events.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use typeshare::typeshare;

/// Location of a function in source code
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionLocation {
    #[typeshare(serialized_as = "string")]
    pub file_path: PathBuf,
    pub function_name: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// A warning about duplicate code
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateWarning {
    /// The newly written function that is similar to existing code
    pub new_function: FunctionLocation,
    /// Matching functions from the codebase, ordered by similarity (highest first)
    pub matches: Vec<SimilarMatch>,
}

/// A match found in the existing codebase
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarMatch {
    #[typeshare(serialized_as = "string")]
    pub file_path: PathBuf,
    pub function_name: String,
    pub start_line: u32,
    pub end_line: u32,
    pub similarity: f64,
    pub body_text: String,
}

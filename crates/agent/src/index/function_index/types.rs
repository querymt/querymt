//! Types for the function index

use similarity_core::AstFingerprint;
use std::path::PathBuf;

/// Maximum file size (in bytes) that the function index will attempt to parse.
///
/// Files larger than this are skipped during indexing to avoid stack overflows
/// in tree-sitter's recursive AST traversal. Large files (vendored single-header
/// libraries, generated code, etc.) are unlikely to contain functions worth
/// deduplicating and can produce ASTs deep enough to exhaust the thread stack.
const DEFAULT_MAX_FILE_BYTES: usize = 512 * 1024; // 512 KB

/// Configuration for the function index
#[derive(Debug, Clone)]
pub struct FunctionIndexConfig {
    /// Similarity threshold for considering functions as duplicates (0.0 - 1.0)
    pub similarity_threshold: f64,
    /// Minimum number of lines for a function to be indexed
    pub min_function_lines: u32,
    /// Optional path for caching the index to disk
    pub cache_path: Option<PathBuf>,
    /// Maximum source file size (in bytes) to index. Files larger than this are
    /// skipped to prevent stack overflows from deeply nested ASTs.
    pub max_file_bytes: usize,
}

impl Default for FunctionIndexConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.8,
            min_function_lines: 5,
            cache_path: None,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

impl FunctionIndexConfig {
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.similarity_threshold = threshold;
        self
    }

    pub fn with_min_lines(mut self, min_lines: u32) -> Self {
        self.min_function_lines = min_lines;
        self
    }

    pub fn with_cache_path(mut self, path: PathBuf) -> Self {
        self.cache_path = Some(path);
        self
    }
}

/// An indexed function with its fingerprint and metadata
#[derive(Debug, Clone)]
pub struct IndexedFunctionEntry {
    /// Function name
    pub name: String,
    /// File path where the function is defined
    pub file_path: PathBuf,
    /// Start line number (1-indexed)
    pub start_line: u32,
    /// End line number (1-indexed)
    pub end_line: u32,
    /// AST fingerprint for fast pre-filtering (TypeScript/JavaScript only)
    pub fingerprint: AstFingerprint,
    /// Language-agnostic structural fingerprint (SimHash on AST node-kind 3-grams for
    /// tree-sitter languages; syn-derived feature hash for Rust).
    /// Zero means no fingerprint was computed.
    pub structural_fingerprint: u64,
    /// The parsed function body for detailed comparison
    pub body_text: String,
    /// Language category for this function
    pub language: String,
}

/// A match found when searching for similar functions
#[derive(Debug, Clone)]
pub struct SimilarFunctionMatch {
    /// The matching function from the index
    pub function: IndexedFunctionEntry,
    /// Similarity score (0.0 - 1.0)
    pub similarity: f64,
}

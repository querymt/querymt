//! Core data types and helpers for outline extraction.

/// A named section in the outline (e.g. "imports", "types", "functions").
#[derive(Debug, Clone)]
pub struct Section {
    /// Section name (e.g. "imports", "types", "functions", "tests").
    pub name: String,
    /// Entries within this section.
    pub entries: Vec<SkeletonEntry>,
}

impl Section {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entries: Vec::new(),
        }
    }

    pub fn with_entries(name: impl Into<String>, entries: Vec<SkeletonEntry>) -> Self {
        Self {
            name: name.into(),
            entries,
        }
    }
}

/// A single item in the outline skeleton.
#[derive(Debug, Clone)]
pub struct SkeletonEntry {
    /// Display label (e.g. `pub fn run(args: Args) -> Result<()>`).
    pub label: String,
    /// 1-based start line in the source file.
    pub start_line: usize,
    /// 1-based end line in the source file.
    pub end_line: usize,
    /// Child entries (e.g. struct fields, impl methods).
    pub children: Vec<SkeletonEntry>,
}

impl SkeletonEntry {
    pub fn new(label: impl Into<String>, start_line: usize, end_line: usize) -> Self {
        Self {
            label: label.into(),
            start_line,
            end_line,
            children: Vec::new(),
        }
    }

    pub fn with_children(
        label: impl Into<String>,
        start_line: usize,
        end_line: usize,
        children: Vec<SkeletonEntry>,
    ) -> Self {
        Self {
            label: label.into(),
            start_line,
            end_line,
            children,
        }
    }
}

/// Options controlling outline extraction.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// Maximum file size in bytes to parse. `None` uses the module default.
    pub max_file_bytes: Option<usize>,
    /// Maximum number of child entries per container (struct/class/impl).
    /// `None` means unlimited.
    pub max_children_per_item: Option<usize>,
    /// Whether to include test-like items in the output.
    pub include_tests: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: None,
            max_children_per_item: None,
            include_tests: true,
        }
    }
}

/// Errors from outline extraction.
#[derive(Debug, thiserror::Error)]
pub enum OutlineError {
    #[error("I/O error: {0}")]
    Io(String),

    #[error("Unsupported file extension: .{0}")]
    UnsupportedLanguage(String),

    #[error("File too large: {size} bytes exceeds limit of {limit} bytes")]
    FileTooLarge { size: usize, limit: usize },

    #[error("Parse error: {0}")]
    ParseError(String),
}

// ---------------------------------------------------------------------------
// Language / extension mapping
// ---------------------------------------------------------------------------

/// Map a file extension to a language name understood by the extractors.
pub fn get_language_for_extension(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "py" | "pyi" => Some("python"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "c++" => Some("cpp"),
        "cs" => Some("csharp"),
        "rb" => Some("ruby"),
        "ex" | "exs" => Some("elixir"),
        _ => None,
    }
}

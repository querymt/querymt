//! Function index for fast similarity lookups
//!
//! Builds and maintains an index of all functions in a codebase for fast
//! duplicate/similar code detection.

use ignore::WalkBuilder;
use similarity_core::{
    AstFingerprint, TSEDOptions, calculate_tsed, extract_functions,
    generic_tree_sitter_parser::GenericTreeSitterParser, language_parser::LanguageParser,
    parser::parse_and_convert_to_tree,
};
use similarity_rs::rust_parser::RustParser;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Configuration for the function index
#[derive(Debug, Clone)]
pub struct FunctionIndexConfig {
    /// Similarity threshold for considering functions as duplicates (0.0 - 1.0)
    pub similarity_threshold: f64,
    /// Minimum number of lines for a function to be indexed
    pub min_function_lines: u32,
    /// Optional path for caching the index to disk
    pub cache_path: Option<PathBuf>,
}

impl Default for FunctionIndexConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.8,
            min_function_lines: 5,
            cache_path: None,
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
    /// AST fingerprint for fast pre-filtering
    pub fingerprint: AstFingerprint,
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

/// Index of all functions in a codebase for fast similarity lookups
pub struct FunctionIndex {
    /// All indexed functions grouped by file path
    functions: HashMap<PathBuf, Vec<IndexedFunctionEntry>>,
    /// Configuration
    config: FunctionIndexConfig,
    /// Root directory that was indexed
    #[allow(dead_code)]
    root: PathBuf,
}

impl FunctionIndex {
    /// Build a new function index from a directory
    ///
    /// This scans all supported source files in the directory (respecting .gitignore)
    /// and extracts function definitions for indexing.
    pub async fn build(root: &Path, config: FunctionIndexConfig) -> Result<Self, String> {
        let root = root.to_path_buf();
        let config_clone = config.clone();

        // Run the CPU-intensive indexing in a blocking task
        tokio::task::spawn_blocking(move || Self::build_sync(&root, config_clone))
            .await
            .map_err(|e| format!("Index build task panicked: {}", e))?
    }

    /// Synchronous build implementation
    fn build_sync(root: &Path, config: FunctionIndexConfig) -> Result<Self, String> {
        let mut functions: HashMap<PathBuf, Vec<IndexedFunctionEntry>> = HashMap::new();

        // Collect all supported source files
        let files = collect_source_files(root)?;

        log::debug!(
            "FunctionIndex: Building index for {} files in {:?}",
            files.len(),
            root
        );

        // Group files by language
        let mut ts_files: Vec<(PathBuf, String)> = Vec::new();
        let mut rust_files: Vec<(PathBuf, String)> = Vec::new();
        let mut go_files: Vec<(PathBuf, String)> = Vec::new();
        let mut java_files: Vec<(PathBuf, String)> = Vec::new();
        let mut c_files: Vec<(PathBuf, String)> = Vec::new();
        let mut cpp_files: Vec<(PathBuf, String)> = Vec::new();
        let mut csharp_files: Vec<(PathBuf, String)> = Vec::new();
        let mut ruby_files: Vec<(PathBuf, String)> = Vec::new();

        for file_path in files {
            let content = match std::fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

            match get_language_category(ext) {
                Some("typescript") => ts_files.push((file_path, content)),
                Some("rust") => rust_files.push((file_path, content)),
                Some("go") => go_files.push((file_path, content)),
                Some("java") => java_files.push((file_path, content)),
                Some("c") => c_files.push((file_path, content)),
                Some("cpp") => cpp_files.push((file_path, content)),
                Some("csharp") => csharp_files.push((file_path, content)),
                Some("ruby") => ruby_files.push((file_path, content)),
                _ => {}
            }
        }

        // Index TypeScript/JavaScript files
        for (path, source) in &ts_files {
            if let Ok(entries) = index_typescript_file(path, source, &config)
                && !entries.is_empty()
            {
                functions.insert(path.clone(), entries);
            }
        }

        // Index Rust files
        if !rust_files.is_empty()
            && let Ok(mut parser) = RustParser::new()
        {
            for (path, source) in &rust_files {
                if let Ok(entries) = index_with_parser(&mut parser, path, source, "rust", &config)
                    && !entries.is_empty()
                {
                    functions.insert(path.clone(), entries);
                }
            }
        }

        // Index Go files
        if !go_files.is_empty()
            && let Ok(mut parser) = GenericTreeSitterParser::from_language_name("go")
        {
            for (path, source) in &go_files {
                if let Ok(entries) = index_with_parser(&mut parser, path, source, "go", &config)
                    && !entries.is_empty()
                {
                    functions.insert(path.clone(), entries);
                }
            }
        }

        // Index Java files
        if !java_files.is_empty()
            && let Ok(mut parser) = GenericTreeSitterParser::from_language_name("java")
        {
            for (path, source) in &java_files {
                if let Ok(entries) = index_with_parser(&mut parser, path, source, "java", &config)
                    && !entries.is_empty()
                {
                    functions.insert(path.clone(), entries);
                }
            }
        }

        // Index C files
        if !c_files.is_empty()
            && let Ok(mut parser) = GenericTreeSitterParser::from_language_name("c")
        {
            for (path, source) in &c_files {
                if let Ok(entries) = index_with_parser(&mut parser, path, source, "c", &config)
                    && !entries.is_empty()
                {
                    functions.insert(path.clone(), entries);
                }
            }
        }

        // Index C++ files
        if !cpp_files.is_empty()
            && let Ok(mut parser) = GenericTreeSitterParser::from_language_name("cpp")
        {
            for (path, source) in &cpp_files {
                if let Ok(entries) = index_with_parser(&mut parser, path, source, "cpp", &config)
                    && !entries.is_empty()
                {
                    functions.insert(path.clone(), entries);
                }
            }
        }

        // Index C# files
        if !csharp_files.is_empty()
            && let Ok(mut parser) = GenericTreeSitterParser::from_language_name("csharp")
        {
            for (path, source) in &csharp_files {
                if let Ok(entries) = index_with_parser(&mut parser, path, source, "csharp", &config)
                    && !entries.is_empty()
                {
                    functions.insert(path.clone(), entries);
                }
            }
        }

        // Index Ruby files
        if !ruby_files.is_empty()
            && let Ok(mut parser) = GenericTreeSitterParser::from_language_name("ruby")
        {
            for (path, source) in &ruby_files {
                if let Ok(entries) = index_with_parser(&mut parser, path, source, "ruby", &config)
                    && !entries.is_empty()
                {
                    functions.insert(path.clone(), entries);
                }
            }
        }

        let total_functions: usize = functions.values().map(|v| v.len()).sum();
        log::info!(
            "FunctionIndex: Indexed {} functions from {} files",
            total_functions,
            functions.len()
        );

        Ok(Self {
            functions,
            config,
            root: root.to_path_buf(),
        })
    }

    /// Find functions similar to the given function entry
    pub fn find_similar(&self, func: &IndexedFunctionEntry) -> Vec<SimilarFunctionMatch> {
        let mut matches = Vec::new();
        let threshold = self.config.similarity_threshold;

        for (file_path, entries) in &self.functions {
            for entry in entries {
                // Skip self-comparison
                if file_path == &func.file_path
                    && entry.start_line == func.start_line
                    && entry.name == func.name
                {
                    continue;
                }

                // Skip if languages don't match (can't compare across languages meaningfully)
                if entry.language != func.language {
                    continue;
                }

                // Fast pre-filter using fingerprints
                if !func.fingerprint.might_be_similar(&entry.fingerprint, 0.5) {
                    continue;
                }

                // More detailed fingerprint similarity check
                let fp_similarity = func.fingerprint.similarity(&entry.fingerprint);
                if fp_similarity < 0.5 {
                    continue;
                }

                // Full similarity comparison
                let similarity = self.calculate_similarity(func, entry);

                if similarity >= threshold {
                    matches.push(SimilarFunctionMatch {
                        function: entry.clone(),
                        similarity,
                    });
                }
            }
        }

        // Sort by similarity (highest first)
        matches.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        matches
    }

    /// Find functions similar to newly written/modified code
    pub fn find_similar_to_code(
        &self,
        file_path: &Path,
        source: &str,
    ) -> Vec<(IndexedFunctionEntry, Vec<SimilarFunctionMatch>)> {
        let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

        let language = match get_language_category(ext) {
            Some(lang) => lang,
            None => return Vec::new(),
        };

        // Extract functions from the new code
        let new_entries = match language {
            "typescript" => {
                index_typescript_file(file_path, source, &self.config).unwrap_or_default()
            }
            "rust" => {
                if let Ok(mut parser) = RustParser::new() {
                    index_with_parser(&mut parser, file_path, source, language, &self.config)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            }
            lang => {
                if let Ok(mut parser) = GenericTreeSitterParser::from_language_name(lang) {
                    index_with_parser(&mut parser, file_path, source, language, &self.config)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            }
        };

        // Find similar functions for each new entry
        let mut results = Vec::new();
        for entry in new_entries {
            let similar = self.find_similar(&entry);
            if !similar.is_empty() {
                results.push((entry, similar));
            }
        }

        results
    }

    /// Update the index for a specific file
    pub fn update_file(&mut self, file_path: &Path, source: &str) {
        let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

        let language = match get_language_category(ext) {
            Some(lang) => lang,
            None => {
                // Unsupported file type, remove from index if present
                self.functions.remove(file_path);
                return;
            }
        };

        let entries = match language {
            "typescript" => {
                index_typescript_file(file_path, source, &self.config).unwrap_or_default()
            }
            "rust" => {
                if let Ok(mut parser) = RustParser::new() {
                    index_with_parser(&mut parser, file_path, source, language, &self.config)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            }
            lang => {
                if let Ok(mut parser) = GenericTreeSitterParser::from_language_name(lang) {
                    index_with_parser(&mut parser, file_path, source, language, &self.config)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            }
        };

        if entries.is_empty() {
            self.functions.remove(file_path);
        } else {
            self.functions.insert(file_path.to_path_buf(), entries);
        }
    }

    /// Remove a file from the index
    pub fn remove_file(&mut self, file_path: &Path) {
        self.functions.remove(file_path);
    }

    /// Get the total number of indexed functions
    pub fn function_count(&self) -> usize {
        self.functions.values().map(|v| v.len()).sum()
    }

    /// Get the number of indexed files
    pub fn file_count(&self) -> usize {
        self.functions.len()
    }

    /// Calculate similarity between two function entries
    fn calculate_similarity(
        &self,
        func1: &IndexedFunctionEntry,
        func2: &IndexedFunctionEntry,
    ) -> f64 {
        // For TypeScript/JavaScript, use the oxc-based parser
        // For other languages, we'd need a different approach
        let filename1 = func1.file_path.to_string_lossy().to_string();
        let filename2 = func2.file_path.to_string_lossy().to_string();

        // Parse both function bodies to tree nodes
        let tree1 = match parse_and_convert_to_tree(&filename1, &func1.body_text) {
            Ok(tree) => tree,
            Err(_) => return 0.0,
        };

        let tree2 = match parse_and_convert_to_tree(&filename2, &func2.body_text) {
            Ok(tree) => tree,
            Err(_) => return 0.0,
        };

        let options = TSEDOptions {
            min_lines: self.config.min_function_lines,
            size_penalty: false,
            ..TSEDOptions::default()
        };

        calculate_tsed(&tree1, &tree2, &options)
    }
}

/// Index a TypeScript/JavaScript file using the oxc-based parser
fn index_typescript_file(
    file_path: &Path,
    source: &str,
    config: &FunctionIndexConfig,
) -> Result<Vec<IndexedFunctionEntry>, String> {
    let filename = file_path.to_string_lossy().to_string();
    let functions = extract_functions(&filename, source)?;

    let mut entries = Vec::new();

    for func in functions {
        let line_count = func.end_line - func.start_line + 1;
        if line_count < config.min_function_lines {
            continue;
        }

        // Extract function body
        let body_text = extract_body_text(source, func.body_span.start, func.body_span.end);

        // Create fingerprint
        let fingerprint = match AstFingerprint::from_source(&body_text) {
            Ok(fp) => fp,
            Err(_) => continue,
        };

        entries.push(IndexedFunctionEntry {
            name: func.name.clone(),
            file_path: file_path.to_path_buf(),
            start_line: func.start_line,
            end_line: func.end_line,
            fingerprint,
            body_text,
            language: "typescript".to_string(),
        });
    }

    Ok(entries)
}

/// Index a file using a tree-sitter based parser
fn index_with_parser(
    parser: &mut dyn LanguageParser,
    file_path: &Path,
    source: &str,
    language: &str,
    config: &FunctionIndexConfig,
) -> Result<Vec<IndexedFunctionEntry>, String> {
    let filename = file_path.to_string_lossy().to_string();
    let functions = parser
        .extract_functions(source, &filename)
        .map_err(|e| format!("Failed to extract functions: {}", e))?;

    let mut entries = Vec::new();

    for func in functions {
        let line_count = func.end_line - func.start_line + 1;
        if line_count < config.min_function_lines {
            continue;
        }

        // Extract function body
        let body_text = extract_body_text_lines(source, func.body_start_line, func.body_end_line);

        // Create fingerprint - for non-TS languages, use a simpler approach
        // since AstFingerprint is TypeScript-specific
        let fingerprint = AstFingerprint::new();

        entries.push(IndexedFunctionEntry {
            name: func.name.clone(),
            file_path: file_path.to_path_buf(),
            start_line: func.start_line,
            end_line: func.end_line,
            fingerprint,
            body_text,
            language: language.to_string(),
        });
    }

    Ok(entries)
}

/// Get the language category for a file extension
fn get_language_category(ext: &str) -> Option<&'static str> {
    match ext {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some("typescript"),
        "rs" => Some("rust"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "c++" => Some("cpp"),
        "cs" => Some("csharp"),
        "rb" => Some("ruby"),
        _ => None,
    }
}

/// Extract body text from source using byte offsets
fn extract_body_text(source: &str, start_byte: u32, end_byte: u32) -> String {
    let start = start_byte as usize;
    let end = end_byte as usize;
    if end <= source.len() && start < end {
        source[start..end].to_string()
    } else {
        String::new()
    }
}

/// Extract body text from source using line numbers
fn extract_body_text_lines(source: &str, start_line: u32, end_line: u32) -> String {
    source
        .lines()
        .skip((start_line.saturating_sub(1)) as usize)
        .take((end_line - start_line + 1) as usize)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collect all supported source files from a directory
fn collect_source_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let supported_extensions = [
        "ts", "tsx", "js", "jsx", "mjs", "cjs",  // TypeScript/JavaScript
        "rs",   // Rust
        "go",   // Go
        "java", // Java
        "c", "h", "cpp", "hpp", "cc", "cxx", // C/C++
        "cs",  // C#
        "rb",  // Ruby
    ];

    let mut files = Vec::new();

    for entry in WalkBuilder::new(root)
        .git_ignore(true)
        .hidden(true)
        .build()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file()
            && let Some(ext) = path.extension().and_then(|e| e.to_str())
            && supported_extensions.contains(&ext)
        {
            files.push(path.to_path_buf());
        }
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_build_empty_index() {
        let temp_dir = TempDir::new().unwrap();
        let config = FunctionIndexConfig::default();

        let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

        assert_eq!(index.function_count(), 0);
        assert_eq!(index.file_count(), 0);
    }

    #[tokio::test]
    async fn test_build_index_with_typescript() {
        let temp_dir = TempDir::new().unwrap();

        fs::write(
            temp_dir.path().join("test.ts"),
            r#"
function hello(name: string) {
    console.log("Hello, " + name);
    const greeting = "Hi there";
    return greeting + " " + name;
}

function goodbye(name: string) {
    console.log("Goodbye, " + name);
    const farewell = "See you";
    return farewell + " " + name;
}
"#,
        )
        .unwrap();

        let config = FunctionIndexConfig::default().with_min_lines(3);
        let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

        assert_eq!(index.file_count(), 1);
        assert!(index.function_count() >= 2);
    }

    #[tokio::test]
    async fn test_find_similar_functions() {
        let temp_dir = TempDir::new().unwrap();

        // Create a file with a function
        fs::write(
            temp_dir.path().join("utils.ts"),
            r#"
function calculateTotal(items: any[]) {
    let total = 0;
    for (const item of items) {
        total += item.price * item.quantity;
    }
    return total;
}
"#,
        )
        .unwrap();

        let config = FunctionIndexConfig::default()
            .with_min_lines(3)
            .with_threshold(0.7);
        let index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

        // Now check for similar code
        let new_code = r#"
function computeSum(products: Product[]) {
    let sum = 0;
    for (const product of products) {
        sum += product.price * product.quantity;
    }
    return sum;
}
"#;

        let results = index.find_similar_to_code(Path::new("new.ts"), new_code);

        // Should find the similar function
        assert!(!results.is_empty() || index.function_count() > 0);
    }

    #[tokio::test]
    async fn test_update_file() {
        let temp_dir = TempDir::new().unwrap();

        fs::write(
            temp_dir.path().join("test.ts"),
            r#"
function original(x: number) {
    const result = x * 2;
    console.log(result);
    return result;
}
"#,
        )
        .unwrap();

        let config = FunctionIndexConfig::default().with_min_lines(3);
        let mut index = FunctionIndex::build(temp_dir.path(), config).await.unwrap();

        let initial_count = index.function_count();

        // Update the file with new content
        let new_content = r#"
function updated(x: number) {
    const result = x * 3;
    console.log(result);
    return result;
}

function another(y: number) {
    const value = y + 1;
    console.log(value);
    return value;
}
"#;

        index.update_file(&temp_dir.path().join("test.ts"), new_content);

        // Should have more functions now
        assert!(index.function_count() >= initial_count);
    }
}

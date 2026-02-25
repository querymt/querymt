//! FunctionIndex struct and all impl methods

use super::indexing::{
    collect_source_files, get_language_category, index_file_with_language, index_typescript_file,
    index_with_parser,
};
use super::types::{FunctionIndexConfig, IndexedFunctionEntry, SimilarFunctionMatch};
use rayon::prelude::*;
use similarity_core::{
    TSEDOptions, calculate_tsed, generic_tree_sitter_parser::GenericTreeSitterParser,
    language_parser::LanguageParser, parser::parse_and_convert_to_tree,
};
use similarity_py::python_parser::PythonParser;
use similarity_rs::rust_parser::RustParser;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Index of all functions in a codebase for fast similarity lookups
pub struct FunctionIndex {
    /// All indexed functions grouped by file path
    pub(super) functions: HashMap<PathBuf, Vec<IndexedFunctionEntry>>,
    /// Configuration
    pub(super) config: FunctionIndexConfig,
    /// Root directory that was indexed
    #[allow(dead_code)]
    pub(super) root: PathBuf,
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
    ///
    /// # Parallelism and Thread Safety
    ///
    /// This method uses rayon's `par_iter()` for fast initial indexing of large codebases.
    /// Each thread creates its own parser instance within the closure scope, which is safe because:
    /// - Each parser is created fresh for each file and lives only within the closure
    /// - Parsers don't escape their thread or outlive the parsing operation
    /// - Tree-sitter `Node` objects are consumed and converted to owned data before collection
    /// - No parser state is shared between threads
    ///
    /// # IMPORTANT: Incremental Updates Must Be Sequential
    ///
    /// While parallel processing is safe for the initial build, **incremental updates via
    /// `update_file()` MUST use sequential processing**. This is because:
    /// - Tree-sitter parsers contain internal C state with raw pointers
    /// - The `Node` type holds references to tree data that are not thread-safe
    /// - Concurrent parser usage can cause segfaults in `Node::kind()` and similar methods
    ///
    /// See: https://github.com/tree-sitter/tree-sitter/issues/1369
    fn build_sync(root: &Path, config: FunctionIndexConfig) -> Result<Self, String> {
        // Collect all supported source files
        let files = collect_source_files(root)?;

        log::debug!(
            "FunctionIndex: Building index for {} files in {:?}",
            files.len(),
            root
        );

        // Build a dedicated thread pool with a larger stack (64 MB per thread) so
        // that tree-sitter's recursive AST traversal doesn't overflow on deeply
        // nested source files (the default 8 MB is not enough for some C/C++ headers).
        let pool = rayon::ThreadPoolBuilder::new()
            .stack_size(64 * 1024 * 1024)
            .build()
            .map_err(|e| format!("Failed to build rayon thread pool: {}", e))?;

        let functions = pool.install(|| -> Result<HashMap<PathBuf, Vec<IndexedFunctionEntry>>, String> {
            let mut functions: HashMap<PathBuf, Vec<IndexedFunctionEntry>> = HashMap::new();

            // Read files in parallel and group by language.
            // Files exceeding `max_file_bytes` are skipped to avoid stack overflows
            // in tree-sitter's recursive AST traversal on very large/deeply-nested files.
            let max_bytes = config.max_file_bytes;
            let file_contents: Vec<(PathBuf, String, &'static str)> = files
                .par_iter()
                .filter_map(|file_path| {
                    let content = std::fs::read_to_string(file_path).ok()?;
                    if content.len() > max_bytes {
                        log::warn!(
                            "FunctionIndex: skipping {:?} ({} bytes > {} max) to avoid stack overflow",
                            file_path,
                            content.len(),
                            max_bytes,
                        );
                        return None;
                    }
                    let ext = file_path.extension()?.to_str()?;
                    let lang = get_language_category(ext)?;
                    Some((file_path.clone(), content, lang))
                })
                .collect();

            // Group by language
            let mut ts_files: Vec<(PathBuf, String)> = Vec::new();
            let mut rust_files: Vec<(PathBuf, String)> = Vec::new();
            let mut go_files: Vec<(PathBuf, String)> = Vec::new();
            let mut java_files: Vec<(PathBuf, String)> = Vec::new();
            let mut c_files: Vec<(PathBuf, String)> = Vec::new();
            let mut cpp_files: Vec<(PathBuf, String)> = Vec::new();
            let mut csharp_files: Vec<(PathBuf, String)> = Vec::new();
            let mut ruby_files: Vec<(PathBuf, String)> = Vec::new();
            let mut python_files: Vec<(PathBuf, String)> = Vec::new();

            for (path, content, lang) in file_contents {
                match lang {
                    "typescript" => ts_files.push((path, content)),
                    "rust" => rust_files.push((path, content)),
                    "go" => go_files.push((path, content)),
                    "java" => java_files.push((path, content)),
                    "c" => c_files.push((path, content)),
                    "cpp" => cpp_files.push((path, content)),
                    "csharp" => csharp_files.push((path, content)),
                    "ruby" => ruby_files.push((path, content)),
                    "python" => python_files.push((path, content)),
                    _ => {}
                }
            }

            // Index TypeScript/JavaScript files in parallel
            let ts_results: Vec<_> = ts_files
                .par_iter()
                .filter_map(|(path, source)| {
                    let entries = index_typescript_file(path, source, &config).ok()?;
                    if entries.is_empty() {
                        None
                    } else {
                        Some((path.clone(), entries))
                    }
                })
                .collect();

            for (path, entries) in ts_results {
                functions.insert(path, entries);
            }

            // Index Rust files in parallel
            if !rust_files.is_empty() {
                let rust_results: Vec<_> = rust_files
                    .par_iter()
                    .filter_map(|(path, source)| {
                        let mut parser = RustParser::new().ok()?;
                        let entries =
                            index_with_parser(&mut parser, path, source, "rust", &config).ok()?;
                        if entries.is_empty() {
                            None
                        } else {
                            Some((path.clone(), entries))
                        }
                    })
                    .collect();

                for (path, entries) in rust_results {
                    functions.insert(path, entries);
                }
            }

            // Index Go files in parallel
            if !go_files.is_empty() {
                let go_results: Vec<_> = go_files
                    .par_iter()
                    .filter_map(|(path, source)| {
                        let mut parser = GenericTreeSitterParser::from_language_name("go").ok()?;
                        let entries =
                            index_with_parser(&mut parser, path, source, "go", &config).ok()?;
                        if entries.is_empty() {
                            None
                        } else {
                            Some((path.clone(), entries))
                        }
                    })
                    .collect();

                for (path, entries) in go_results {
                    functions.insert(path, entries);
                }
            }

            // Index Java files in parallel
            if !java_files.is_empty() {
                let java_results: Vec<_> = java_files
                    .par_iter()
                    .filter_map(|(path, source)| {
                        let mut parser = GenericTreeSitterParser::from_language_name("java").ok()?;
                        let entries =
                            index_with_parser(&mut parser, path, source, "java", &config).ok()?;
                        if entries.is_empty() {
                            None
                        } else {
                            Some((path.clone(), entries))
                        }
                    })
                    .collect();

                for (path, entries) in java_results {
                    functions.insert(path, entries);
                }
            }

            // Index C files in parallel
            if !c_files.is_empty() {
                let c_results: Vec<_> = c_files
                    .par_iter()
                    .filter_map(|(path, source)| {
                        let mut parser = GenericTreeSitterParser::from_language_name("c").ok()?;
                        let entries =
                            index_with_parser(&mut parser, path, source, "c", &config).ok()?;
                        if entries.is_empty() {
                            None
                        } else {
                            Some((path.clone(), entries))
                        }
                    })
                    .collect();

                for (path, entries) in c_results {
                    functions.insert(path, entries);
                }
            }

            // Index C++ files in parallel
            if !cpp_files.is_empty() {
                let cpp_results: Vec<_> = cpp_files
                    .par_iter()
                    .filter_map(|(path, source)| {
                        let mut parser = GenericTreeSitterParser::from_language_name("cpp").ok()?;
                        let entries =
                            index_with_parser(&mut parser, path, source, "cpp", &config).ok()?;
                        if entries.is_empty() {
                            None
                        } else {
                            Some((path.clone(), entries))
                        }
                    })
                    .collect();

                for (path, entries) in cpp_results {
                    functions.insert(path, entries);
                }
            }

            // Index C# files in parallel
            if !csharp_files.is_empty() {
                let csharp_results: Vec<_> = csharp_files
                    .par_iter()
                    .filter_map(|(path, source)| {
                        let mut parser = GenericTreeSitterParser::from_language_name("csharp").ok()?;
                        let entries =
                            index_with_parser(&mut parser, path, source, "csharp", &config).ok()?;
                        if entries.is_empty() {
                            None
                        } else {
                            Some((path.clone(), entries))
                        }
                    })
                    .collect();

                for (path, entries) in csharp_results {
                    functions.insert(path, entries);
                }
            }

            // Index Ruby files in parallel
            if !ruby_files.is_empty() {
                let ruby_results: Vec<_> = ruby_files
                    .par_iter()
                    .filter_map(|(path, source)| {
                        let mut parser = GenericTreeSitterParser::from_language_name("ruby").ok()?;
                        let entries =
                            index_with_parser(&mut parser, path, source, "ruby", &config).ok()?;
                        if entries.is_empty() {
                            None
                        } else {
                            Some((path.clone(), entries))
                        }
                    })
                    .collect();

                for (path, entries) in ruby_results {
                    functions.insert(path, entries);
                }
            }

            // Index Python files in parallel
            if !python_files.is_empty() {
                let python_results: Vec<_> = python_files
                    .par_iter()
                    .filter_map(|(path, source)| {
                        let mut parser = PythonParser::new().ok()?;
                        let entries =
                            index_with_parser(&mut parser, path, source, "python", &config).ok()?;
                        if entries.is_empty() {
                            None
                        } else {
                            Some((path.clone(), entries))
                        }
                    })
                    .collect();

                for (path, entries) in python_results {
                    functions.insert(path, entries);
                }
            }

            Ok(functions)
        })?;

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

        let total_functions: usize = self.functions.values().map(|v| v.len()).sum();
        log::debug!(
            "FunctionIndex::find_similar: '{}' ({:?}) — searching {} candidate(s) across {} file(s)",
            func.name,
            func.file_path,
            total_functions,
            self.functions.len()
        );

        let mut fingerprint_rejected = 0usize;
        let mut language_rejected = 0usize;
        let mut below_threshold = 0usize;

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
                    language_rejected += 1;
                    continue;
                }

                // ── Cheap pre-filters (no parsing required) ──────────────────

                // Size-ratio filter: reject pairs whose line counts differ by
                // more than 3.3x.  A 10-line function is extremely unlikely to
                // be a duplicate of a 200-line function.
                let probe_lines = (func.end_line.saturating_sub(func.start_line) + 1) as f64;
                let cand_lines = (entry.end_line.saturating_sub(entry.start_line) + 1) as f64;
                let size_ratio = probe_lines.min(cand_lines) / probe_lines.max(cand_lines).max(1.0);
                if size_ratio < 0.3 {
                    fingerprint_rejected += 1;
                    continue;
                }

                // Structural SimHash filter (for non-TypeScript languages).
                // Both fingerprints must be non-zero (i.e. were actually computed).
                // Reject if Hamming distance > 25 out of 64 bits.
                if func.structural_fingerprint != 0 && entry.structural_fingerprint != 0 {
                    let hamming =
                        (func.structural_fingerprint ^ entry.structural_fingerprint).count_ones();
                    if hamming > 25 {
                        fingerprint_rejected += 1;
                        continue;
                    }
                }

                // OXC AstFingerprint filter (TypeScript / JavaScript only).
                // For non-TS languages both fingerprints are empty and always pass.
                if !func.fingerprint.might_be_similar(&entry.fingerprint, 0.5) {
                    fingerprint_rejected += 1;
                    continue;
                }

                // More detailed fingerprint similarity check
                let fp_similarity = func.fingerprint.similarity(&entry.fingerprint);
                if fp_similarity < 0.5 {
                    fingerprint_rejected += 1;
                    continue;
                }

                // Full similarity comparison
                let similarity = self.calculate_similarity(func, entry);

                if similarity >= threshold {
                    matches.push(SimilarFunctionMatch {
                        function: entry.clone(),
                        similarity,
                    });
                } else {
                    below_threshold += 1;
                }
            }
        }

        log::debug!(
            "FunctionIndex::find_similar: '{}' done — {} match(es), {} fingerprint-rejected, \
            {} language-rejected, {} below-threshold",
            func.name,
            matches.len(),
            fingerprint_rejected,
            language_rejected,
            below_threshold
        );

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
        let new_entries = index_file_with_language(file_path, source, language, &self.config);

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
    ///
    /// Sequential by construction when called through the actor mailbox.
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

        let entries = index_file_with_language(file_path, source, language, &self.config);

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

    /// Calculate similarity between two function entries.
    ///
    /// Dispatches to the appropriate parser based on the language stored in each
    /// `IndexedFunctionEntry`.  `find_similar` already guarantees that both entries
    /// share the same language, so we only need to check one of them.
    fn calculate_similarity(
        &self,
        func1: &IndexedFunctionEntry,
        func2: &IndexedFunctionEntry,
    ) -> f64 {
        let filename1 = func1.file_path.to_string_lossy().to_string();
        let filename2 = func2.file_path.to_string_lossy().to_string();

        let (tree1, tree2) = match func1.language.as_str() {
            "typescript" => {
                // OXC-based parser for TypeScript / JavaScript
                let t1 = match parse_and_convert_to_tree(&filename1, &func1.body_text) {
                    Ok(t) => t,
                    Err(e) => {
                        log::debug!(
                            "calculate_similarity: parse failed (typescript) for '{}': {}",
                            func1.name,
                            e
                        );
                        return 0.0;
                    }
                };
                let t2 = match parse_and_convert_to_tree(&filename2, &func2.body_text) {
                    Ok(t) => t,
                    Err(e) => {
                        log::debug!(
                            "calculate_similarity: parse failed (typescript) for '{}': {}",
                            func2.name,
                            e
                        );
                        return 0.0;
                    }
                };
                (t1, t2)
            }
            "rust" => {
                let mut parser = match RustParser::new() {
                    Ok(p) => p,
                    Err(e) => {
                        log::debug!("calculate_similarity: RustParser::new failed: {}", e);
                        return 0.0;
                    }
                };
                let t1 = match parser.parse(&func1.body_text, &filename1) {
                    Ok(t) => t,
                    Err(e) => {
                        log::debug!(
                            "calculate_similarity: parse failed (rust) for '{}': {}",
                            func1.name,
                            e
                        );
                        return 0.0;
                    }
                };
                let t2 = match parser.parse(&func2.body_text, &filename2) {
                    Ok(t) => t,
                    Err(e) => {
                        log::debug!(
                            "calculate_similarity: parse failed (rust) for '{}': {}",
                            func2.name,
                            e
                        );
                        return 0.0;
                    }
                };
                (t1, t2)
            }
            "python" => {
                let mut parser = match PythonParser::new() {
                    Ok(p) => p,
                    Err(e) => {
                        log::debug!("calculate_similarity: PythonParser::new failed: {}", e);
                        return 0.0;
                    }
                };
                let t1 = match parser.parse(&func1.body_text, &filename1) {
                    Ok(t) => t,
                    Err(e) => {
                        log::debug!(
                            "calculate_similarity: parse failed (python) for '{}': {}",
                            func1.name,
                            e
                        );
                        return 0.0;
                    }
                };
                let t2 = match parser.parse(&func2.body_text, &filename2) {
                    Ok(t) => t,
                    Err(e) => {
                        log::debug!(
                            "calculate_similarity: parse failed (python) for '{}': {}",
                            func2.name,
                            e
                        );
                        return 0.0;
                    }
                };
                (t1, t2)
            }
            lang @ ("go" | "java" | "c" | "cpp" | "csharp" | "ruby") => {
                let mut parser = match GenericTreeSitterParser::from_language_name(lang) {
                    Ok(p) => p,
                    Err(e) => {
                        log::debug!(
                            "calculate_similarity: GenericTreeSitterParser::from_language_name('{}') failed: {}",
                            lang,
                            e
                        );
                        return 0.0;
                    }
                };
                let t1 = match parser.parse(&func1.body_text, &filename1) {
                    Ok(t) => t,
                    Err(e) => {
                        log::debug!(
                            "calculate_similarity: parse failed ({}) for '{}': {}",
                            lang,
                            func1.name,
                            e
                        );
                        return 0.0;
                    }
                };
                let t2 = match parser.parse(&func2.body_text, &filename2) {
                    Ok(t) => t,
                    Err(e) => {
                        log::debug!(
                            "calculate_similarity: parse failed ({}) for '{}': {}",
                            lang,
                            func2.name,
                            e
                        );
                        return 0.0;
                    }
                };
                (t1, t2)
            }
            other => {
                log::debug!(
                    "calculate_similarity: unsupported language '{}' for '{}' — skipping",
                    other,
                    func1.name
                );
                return 0.0;
            }
        };

        let options = TSEDOptions {
            min_lines: self.config.min_function_lines,
            size_penalty: false,
            ..TSEDOptions::default()
        };

        calculate_tsed(&tree1, &tree2, &options)
    }
}

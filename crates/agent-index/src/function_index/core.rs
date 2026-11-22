//! FunctionIndex struct and all impl methods

use super::super::error::IndexError;
use super::indexing::{
    collect_source_files, get_language_category, index_file_entries, index_file_with_language,
    parse_pool,
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
use tracing::instrument;

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
    pub async fn build(root: &Path, config: FunctionIndexConfig) -> Result<Self, IndexError> {
        let root = root.to_path_buf();
        let config_clone = config.clone();

        // Run the CPU-intensive indexing in a blocking task
        tokio::task::spawn_blocking(move || Self::build_sync(&root, config_clone)).await?
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
    #[instrument(
        name = "function_index.build",
        skip(root, config),
        fields(
            root = %root.display(),
            files_found = tracing::field::Empty,
            functions_indexed = tracing::field::Empty,
            files_indexed = tracing::field::Empty,
        )
    )]
    fn build_sync(root: &Path, config: FunctionIndexConfig) -> Result<Self, IndexError> {
        // Collect all supported source files
        let files = collect_source_files(root);

        tracing::Span::current().record("files_found", files.len());

        log::debug!(
            "FunctionIndex: Building index for {} files in {:?}",
            files.len(),
            root
        );

        // Every file is read and indexed on the shared parse pool (64 MB stacks), so
        // tree-sitter's recursive AST traversal does not overflow on deeply nested sources.
        // Unreadable files and files without functions are not recorded.
        let functions: HashMap<PathBuf, Vec<IndexedFunctionEntry>> = parse_pool()?.install(|| {
            files
                .par_iter()
                .filter_map(|file_path| {
                    let content = std::fs::read_to_string(file_path).ok()?;
                    let entries = index_file_entries(file_path, &content, &config);
                    (!entries.is_empty()).then(|| (file_path.clone(), entries))
                })
                .collect()
        });

        let total_functions: usize = functions.values().map(|v| v.len()).sum();
        let total_files = functions.len();
        tracing::Span::current().record("functions_indexed", total_functions);
        tracing::Span::current().record("files_indexed", total_files);

        log::info!(
            "FunctionIndex: Indexed {} functions from {} files",
            total_functions,
            total_files
        );

        Ok(Self {
            functions,
            config,
            root: root.to_path_buf(),
        })
    }

    /// Find functions similar to the given function entry
    #[instrument(
        level = "debug",
        name = "function_index.find_similar",
        skip(self, func),
        fields(
            function_name = %func.name,
            language = %func.language,
            candidates_total = tracing::field::Empty,
            fingerprint_rejected = tracing::field::Empty,
            language_rejected = tracing::field::Empty,
            below_threshold = tracing::field::Empty,
            matches_found = tracing::field::Empty,
        )
    )]
    pub fn find_similar(&self, func: &IndexedFunctionEntry) -> Vec<SimilarFunctionMatch> {
        let mut matches = Vec::new();
        let threshold = self.config.similarity_threshold;

        let total_functions: usize = self.functions.values().map(|v| v.len()).sum();
        tracing::Span::current().record("candidates_total", total_functions);

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

        let span = tracing::Span::current();
        span.record("fingerprint_rejected", fingerprint_rejected);
        span.record("language_rejected", language_rejected);
        span.record("below_threshold", below_threshold);
        span.record("matches_found", matches.len());

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
    #[instrument(
        level = "debug",
        name = "function_index.find_similar_to_code",
        skip(self, source),
        fields(
            file = %file_path.display(),
            functions_extracted = tracing::field::Empty,
            functions_with_matches = tracing::field::Empty,
        )
    )]
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
        tracing::Span::current().record("functions_extracted", new_entries.len());

        // Find similar functions for each new entry
        let mut results = Vec::new();
        for entry in new_entries {
            let similar = self.find_similar(&entry);
            if !similar.is_empty() {
                results.push((entry, similar));
            }
        }

        tracing::Span::current().record("functions_with_matches", results.len());

        results
    }

    /// Update the index for a specific file
    ///
    /// Sequential by construction when called through the actor mailbox.
    #[instrument(
        name = "function_index.update_file",
        skip(self, source),
        fields(file = %file_path.display(), functions_indexed = tracing::field::Empty)
    )]
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
        tracing::Span::current().record("functions_indexed", entries.len());

        if entries.is_empty() {
            self.functions.remove(file_path);
        } else {
            self.functions.insert(file_path.to_path_buf(), entries);
        }
    }

    /// Assemble an index from per-file entries that were extracted elsewhere
    /// (for example loaded from the persistent code cache).
    ///
    /// Entries must already carry their TypeScript/JavaScript [`AstFingerprint`]; the code
    /// cache restores it when converting persisted values back into entries.
    /// Files with no entries are not recorded.
    pub fn from_entries(
        root: &Path,
        config: FunctionIndexConfig,
        entries: HashMap<PathBuf, Vec<IndexedFunctionEntry>>,
    ) -> Self {
        let mut index = Self {
            functions: HashMap::with_capacity(entries.len()),
            config,
            root: root.to_path_buf(),
        };
        for (path, file_entries) in entries {
            index.set_file_entries(&path, file_entries);
        }
        index
    }

    /// Replace the entries of one file, or drop the file when `entries` is empty.
    pub fn set_file_entries(&mut self, file_path: &Path, entries: Vec<IndexedFunctionEntry>) {
        if entries.is_empty() {
            self.functions.remove(file_path);
        } else {
            self.functions.insert(file_path.to_path_buf(), entries);
        }
    }

    /// Remove a file from the index
    #[instrument(
        name = "function_index.remove_file",
        level = "debug",
        skip(self),
        fields(file = %file_path.display())
    )]
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
    #[instrument(
        level = "trace",
        name = "function_index.calculate_similarity",
        skip(self, func1, func2),
        fields(
            func1 = %func1.name,
            func2 = %func2.name,
            language = %func1.language,
            similarity = tracing::field::Empty,
        )
    )]
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

        let similarity = calculate_tsed(&tree1, &tree2, &options);
        tracing::Span::current().record("similarity", similarity);
        similarity
    }
}

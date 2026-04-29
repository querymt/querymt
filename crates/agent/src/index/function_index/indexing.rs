//! File indexing helpers: parsing + source file collection

use super::fingerprint::{rust_structural_fingerprint, structural_simhash_from_tree};
use super::types::{FunctionIndexConfig, IndexedFunctionEntry};
use ignore::WalkBuilder;
use similarity_core::{
    AstFingerprint, extract_functions, generic_tree_sitter_parser::GenericTreeSitterParser,
    language_parser::LanguageParser,
};
use similarity_py::python_parser::PythonParser;
use similarity_rs::rust_parser::RustParser;
use std::path::{Path, PathBuf};
use tracing::instrument;

/// Index a TypeScript/JavaScript file using the oxc-based parser
#[instrument(
    level = "trace",
    name = "function_index.index_typescript_file",
    skip(source, config),
    fields(
        file = %file_path.display(),
        functions_found = tracing::field::Empty,
        functions_indexed = tracing::field::Empty,
    )
)]
pub(super) fn index_typescript_file(
    file_path: &Path,
    source: &str,
    config: &FunctionIndexConfig,
) -> Result<Vec<IndexedFunctionEntry>, String> {
    let filename = file_path.to_string_lossy().to_string();
    let functions = extract_functions(&filename, source)?;

    tracing::Span::current().record("functions_found", functions.len());

    let mut entries = Vec::new();
    let mut fingerprint_failures = 0usize;

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
            Err(_) => {
                fingerprint_failures += 1;
                continue;
            }
        };

        entries.push(IndexedFunctionEntry {
            name: func.name.clone(),
            file_path: file_path.to_path_buf(),
            start_line: func.start_line,
            end_line: func.end_line,
            fingerprint,
            // TypeScript already has the OXC AstFingerprint; set structural to 0.
            structural_fingerprint: 0,
            body_text,
            language: "typescript".to_string(),
        });
    }

    if fingerprint_failures > 0 {
        log::debug!(
            "index_typescript_file: {} fingerprint failure(s) in {:?}",
            fingerprint_failures,
            file_path
        );
    }

    tracing::Span::current().record("functions_indexed", entries.len());

    Ok(entries)
}

/// Index a file using a tree-sitter based parser
#[instrument(
    level = "trace",
    name = "function_index.index_with_parser",
    skip(parser, source, config),
    fields(
        file = %file_path.display(),
        language = %language,
        functions_found = tracing::field::Empty,
        functions_indexed = tracing::field::Empty,
    )
)]
pub(super) fn index_with_parser(
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

    tracing::Span::current().record("functions_found", functions.len());

    let mut entries = Vec::new();

    for func in functions {
        let line_count = func.end_line - func.start_line + 1;
        if line_count < config.min_function_lines {
            continue;
        }

        // Extract function body
        let body_text = extract_body_text_lines(source, func.body_start_line, func.body_end_line);

        // Compute the structural fingerprint.
        //
        // For Rust: use syn to derive a type-aware feature hash (param types,
        // return type, control-flow counts, callee SimHash).
        //
        // For other tree-sitter languages: parse the body text with the shared
        // `parser.parse()` to obtain a `TreeNode`, then SimHash the node-kind
        // 3-grams.  This is rename-invariant and language-agnostic.
        let structural_fingerprint: u64 = if language == "rust" {
            // Full function text (signature + body) for syn
            let fn_text = extract_body_text_lines(source, func.start_line, func.end_line);
            rust_structural_fingerprint(&fn_text)
        } else {
            // Tree-sitter parse of the body for SimHash
            match parser.parse(&body_text, &filename) {
                Ok(tree) => structural_simhash_from_tree(&tree),
                Err(e) => {
                    log::debug!(
                        "index_with_parser: structural fingerprint parse failed for '{}' in {:?}: {}",
                        func.name,
                        file_path,
                        e
                    );
                    0
                }
            }
        };

        entries.push(IndexedFunctionEntry {
            name: func.name.clone(),
            file_path: file_path.to_path_buf(),
            start_line: func.start_line,
            end_line: func.end_line,
            fingerprint: AstFingerprint::new(),
            structural_fingerprint,
            body_text,
            language: language.to_string(),
        });
    }

    tracing::Span::current().record("functions_indexed", entries.len());

    Ok(entries)
}

/// Get the language category for a file extension
pub fn get_language_category(ext: &str) -> Option<&'static str> {
    match ext {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some("typescript"),
        "rs" => Some("rust"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "c++" => Some("cpp"),
        "cs" => Some("csharp"),
        "rb" => Some("ruby"),
        "py" => Some("python"),
        _ => None,
    }
}

/// Extract body text from source using byte offsets
pub(super) fn extract_body_text(source: &str, start_byte: u32, end_byte: u32) -> String {
    let start = start_byte as usize;
    let end = end_byte as usize;
    if end <= source.len() && start < end {
        source[start..end].to_string()
    } else {
        String::new()
    }
}

/// Extract body text from source using line numbers
pub(super) fn extract_body_text_lines(source: &str, start_line: u32, end_line: u32) -> String {
    source
        .lines()
        .skip((start_line.saturating_sub(1)) as usize)
        .take((end_line - start_line + 1) as usize)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collect all supported source files from a directory
#[instrument(
    name = "function_index.collect_source_files",
    skip(root),
    fields(
        root = %root.display(),
        files_found = tracing::field::Empty,
    )
)]
pub(super) fn collect_source_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let supported_extensions = [
        "ts", "tsx", "js", "jsx", "mjs", "cjs",  // TypeScript/JavaScript
        "rs",   // Rust
        "go",   // Go
        "java", // Java
        "c", "h", "cpp", "hpp", "cc", "cxx", // C/C++
        "cs",  // C#
        "rb",  // Ruby
        "py",  // Python
    ];

    let mut files = Vec::new();

    // TODO: Consider consolidating with file_index.rs's Override pattern for consistency
    // Currently using .standard_filters() which respects .gitignore and common ignore patterns
    for entry in WalkBuilder::new(root)
        .git_ignore(true)
        .hidden(true)
        .standard_filters(true)
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

    tracing::Span::current().record("files_found", files.len());

    Ok(files)
}

/// Create a parser for the given language name, index the given source, and return entries.
/// Used for incremental updates (`update_file`) and on-the-fly queries (`find_similar_to_code`).
#[instrument(
    level = "trace",
    name = "function_index.index_file_with_language",
    skip(source, config),
    fields(
        file = %file_path.display(),
        language = %language,
        functions_indexed = tracing::field::Empty,
    )
)]
pub fn index_file_with_language(
    file_path: &Path,
    source: &str,
    language: &str,
    config: &FunctionIndexConfig,
) -> Vec<IndexedFunctionEntry> {
    let entries = match language {
        "typescript" => index_typescript_file(file_path, source, config).unwrap_or_default(),
        "rust" => match RustParser::new() {
            Ok(mut parser) => index_with_parser(&mut parser, file_path, source, language, config)
                .unwrap_or_default(),
            Err(e) => {
                log::warn!(
                    "index_file_with_language: failed to create RustParser for {:?}: {}",
                    file_path,
                    e
                );
                Vec::new()
            }
        },
        "python" => match PythonParser::new() {
            Ok(mut parser) => index_with_parser(&mut parser, file_path, source, language, config)
                .unwrap_or_default(),
            Err(e) => {
                log::warn!(
                    "index_file_with_language: failed to create PythonParser for {:?}: {}",
                    file_path,
                    e
                );
                Vec::new()
            }
        },
        lang => match GenericTreeSitterParser::from_language_name(lang) {
            Ok(mut parser) => index_with_parser(&mut parser, file_path, source, language, config)
                .unwrap_or_default(),
            Err(e) => {
                log::warn!(
                    "index_file_with_language: failed to create parser for language '{}' on {:?}: {}",
                    lang,
                    file_path,
                    e
                );
                Vec::new()
            }
        },
    };

    tracing::Span::current().record("functions_indexed", entries.len());

    entries
}

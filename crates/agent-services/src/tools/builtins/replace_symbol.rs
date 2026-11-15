//! AST-backed whole-symbol replacement tool.
//!
//! Resolves symbols by byte ranges from `SymbolIndex`, applies replacements
//! bottom-up within each file, and returns compact metadata. Supports optional
//! hash-based stale-write protection.

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool as ChatTool, ToolResultPart};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::index::symbol_index::{SymbolDigest, SymbolIndex, SymbolKind, parse_kind_filter};
use crate::tools::{
    AliasScope, ArgAlias, CapabilityRequirement, FileAccess, Tool, ToolContext, ToolError,
    WrapRule, effective_root, resolve_arg_path,
};

use super::helpers::{resolve_root, resolve_target};

/// Compatibility spellings accepted inside `replacements` items (never advertised).
const REPLACE_SYMBOL_ARG_ALIASES: &[ArgAlias] = &[
    ArgAlias {
        scope: AliasScope::ArrayItems("replacements"),
        from: "newText",
        to: "new_text",
    },
    ArgAlias {
        scope: AliasScope::ArrayItems("replacements"),
        from: "expectedHash",
        to: "expected_hash",
    },
];

/// A single replacement may be sent flat, without the `replacements` array.
const REPLACE_SYMBOL_WRAP: WrapRule = WrapRule {
    array: "replacements",
    fields: &[
        "path",
        "symbol",
        "kind",
        "occurrence",
        "expected_hash",
        "new_text",
    ],
    required: &["path", "symbol", "new_text"],
};

pub struct ReplaceSymbolTool;

impl ReplaceSymbolTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ReplaceSymbolTool {
    fn default() -> Self {
        Self::new()
    }
}

/// A single replacement request.
#[derive(Debug)]
struct ReplacementRequest {
    path: String,
    symbol: String,
    kind: Option<SymbolKind>,
    occurrence: usize,
    expected_hash: Option<String>,
    new_text: String,
}

/// A resolved replacement ready for application.
#[derive(Debug)]
struct ResolvedReplacement {
    path: PathBuf,
    kind_str: String,
    qualified_name: String,
    old_hash: String,
    old_start_line: usize,
    old_end_line: usize,
    start_byte: usize,
    end_byte: usize,
    new_text: String,
    /// Line range in the *new* content after application.
    new_start_line: usize,
    new_end_line: usize,
    new_hash: String,
}

#[async_trait]
impl Tool for ReplaceSymbolTool {
    fn name(&self) -> &str {
        "replace_symbol"
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Replace entire symbol bodies (functions, structs, classes, etc.) using AST byte ranges. Resolves all replacements before writing anything. Rejects overlapping replacements and stale writes. Returns compact metadata only."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "replacements": {
                            "type": "array",
                            "description": "Symbol replacement requests. All are resolved before any file is written.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string", "description": "File path." },
                                    "symbol": { "type": "string", "description": "Symbol name or qualified name to replace." },
                                    "kind": {
                                        "type": "string",
                                        "description": "Symbol kind filter to disambiguate.",
                                        "default": "any"
                                    },
                                    "occurrence": {
                                        "type": "integer",
                                        "description": "0-based occurrence when multiple symbols match.",
                                        "default": 0,
                                        "minimum": 0
                                    },
                                    "expected_hash": {
                                        "type": "string",
                                        "description": "If provided, the replacement is rejected when the current hash does not match."
                                    },
                                    "new_text": { "type": "string", "description": "New symbol body text." }
                                },
                                "required": ["path", "symbol", "new_text"],
                                "additionalProperties": false
                            }
                        },
                        "root": {
                            "type": "string",
                            "description": "Workspace root directory.",
                            "default": "."
                        }
                    },
                    "required": ["replacements"],
                    "additionalProperties": false
                }),
                strict: None,
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    fn arg_aliases(&self) -> &'static [ArgAlias] {
        REPLACE_SYMBOL_ARG_ALIASES
    }

    fn wrap_single_item(&self) -> Option<WrapRule> {
        Some(REPLACE_SYMBOL_WRAP)
    }

    fn file_access(&self, args: &Value, cwd: Option<&Path>) -> FileAccess {
        // Mirror `call`: `resolve_root` must succeed, and every relative
        // target is joined with that root (see `helpers::resolve_target`).
        let Some(root) = effective_root(args, cwd) else {
            return FileAccess::None;
        };
        let Some(entries) = args.get("replacements").and_then(Value::as_array) else {
            return FileAccess::None;
        };

        let mut writes: Vec<PathBuf> = Vec::with_capacity(entries.len());
        for entry in entries {
            let Some(path) = entry
                .get("path")
                .and_then(Value::as_str)
                .and_then(|raw| resolve_arg_path(raw, Some(&root)))
            else {
                // Malformed entry: the call fails validation before writing.
                return FileAccess::None;
            };
            if !writes.contains(&path) {
                writes.push(path);
            }
        }
        if writes.is_empty() {
            FileAccess::None
        } else {
            FileAccess::writes(writes)
        }
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        let root = resolve_root(&args, context)?;
        let requests = parse_replacements(&args)?;

        // Read/replace/write phases run under the checkout mutation gate
        // when the session has one (see `run_under_mutation_gate`).
        crate::tools::run_under_mutation_gate(context, async {
            // Phase 1: Read all files and resolve all symbols.
            let mut file_contents: HashMap<PathBuf, String> = HashMap::new();
            let mut resolved: Vec<ResolvedReplacement> = Vec::new();

            for req in &requests {
                let target = resolve_target(&req.path, &root);

                // Read file if not already loaded
                let content = match file_contents.get(&target) {
                    Some(c) => c.clone(),
                    None => {
                        let c = tokio::fs::read_to_string(&target).await.map_err(|e| {
                            ToolError::ProviderError(format!(
                                "Failed to read {}: {e}",
                                target.display()
                            ))
                        })?;
                        file_contents.insert(target.clone(), c);
                        file_contents.get(&target).unwrap().clone()
                    }
                };

                let symbol_index = SymbolIndex::from_source_for_path(&target, &content)
                    .map_err(|e| ToolError::ProviderError(e.to_string()))?;

                let matches = symbol_index.find_by_name(&req.symbol, req.kind);
                let symbol = matches.get(req.occurrence).copied().ok_or_else(|| {
                    if matches.is_empty() {
                        ToolError::InvalidRequest(format!(
                            "Symbol '{}' not found in {}",
                            req.symbol,
                            target.display()
                        ))
                    } else {
                        ToolError::InvalidRequest(format!(
                            "Occurrence {} out of range for '{}' in {} ({} matches available)",
                            req.occurrence,
                            req.symbol,
                            target.display(),
                            matches.len()
                        ))
                    }
                })?;

                // Hash check
                if let Some(ref expected) = req.expected_hash
                    && symbol.digest.hash.to_string() != *expected
                {
                    return Err(ToolError::ProviderError(format!(
                        "Hash mismatch for '{}' in {}: expected {}, got {}. File may have changed.",
                        req.symbol,
                        target.display(),
                        expected,
                        symbol.digest.hash
                    )));
                }

                resolved.push(ResolvedReplacement {
                    path: target,
                    kind_str: symbol.kind.as_str().to_string(),
                    qualified_name: symbol.qualified_name.clone(),
                    old_hash: symbol.digest.hash.to_string(),
                    old_start_line: symbol.start_line,
                    old_end_line: symbol.end_line,
                    start_byte: symbol.start_byte,
                    end_byte: symbol.end_byte,
                    new_text: req.new_text.clone(),
                    new_start_line: 0,
                    new_end_line: 0,
                    new_hash: String::new(),
                });
            }

            // Phase 2: Validate no overlaps within each file.
            validate_no_overlaps(&resolved)?;

            // Phase 3: Apply replacements per file, bottom-up by byte offset.
            let mut results = Vec::new();
            let mut files_to_write: HashMap<PathBuf, Vec<usize>> = HashMap::new();
            for (i, r) in resolved.iter().enumerate() {
                files_to_write.entry(r.path.clone()).or_default().push(i);
            }

            for (file_path, indices) in &files_to_write {
                let content = file_contents[file_path].clone();
                let (new_content, file_results) = apply_replacements_to_file(
                    file_path,
                    &content,
                    indices.iter().map(|&i| &resolved[i]).collect(),
                )?;

                // Update resolved entries with new line/hash info
                for (i, result_info) in indices.iter().zip(file_results.iter()) {
                    resolved[*i].new_start_line = result_info.new_start_line;
                    resolved[*i].new_end_line = result_info.new_end_line;
                    resolved[*i].new_hash = result_info.new_hash.clone();
                }

                tokio::fs::write(file_path, &new_content)
                    .await
                    .map_err(|e| {
                        ToolError::ProviderError(format!(
                            "Failed to write {}: {e}",
                            file_path.display()
                        ))
                    })?;

                results.extend(file_results);
            }

            // Phase 4: Format compact output
            let output = format_results(&results);
            Ok(vec![ToolResultPart::text(output)])
        })
        .await
    }
}

fn parse_replacements(args: &Value) -> Result<Vec<ReplacementRequest>, ToolError> {
    let arr = args
        .get("replacements")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidRequest("replacements must be an array".to_string()))?;

    if arr.is_empty() {
        return Err(ToolError::InvalidRequest(
            "replacements must include at least one entry".to_string(),
        ));
    }

    arr.iter()
        .map(|r| {
            let path = r.get("path").and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidRequest("each replacement requires a path".to_string())
            })?;
            let symbol = r.get("symbol").and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidRequest("each replacement requires a symbol".to_string())
            })?;
            let kind_str = r.get("kind").and_then(Value::as_str).unwrap_or("any");
            let kind = parse_kind_filter(kind_str).map_err(ToolError::InvalidRequest)?;
            let occurrence = r.get("occurrence").and_then(Value::as_u64).unwrap_or(0) as usize;
            let expected_hash = r
                .get("expected_hash")
                .and_then(Value::as_str)
                .map(str::to_string);
            let new_text = r.get("new_text").and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidRequest("each replacement requires new_text".to_string())
            })?;

            Ok(ReplacementRequest {
                path: path.to_string(),
                symbol: symbol.to_string(),
                kind,
                occurrence,
                expected_hash,
                new_text: new_text.to_string(),
            })
        })
        .collect()
}

struct FileResult {
    kind_str: String,
    qualified_name: String,
    old_hash: String,
    old_start_line: usize,
    old_end_line: usize,
    new_start_line: usize,
    new_end_line: usize,
    new_hash: String,
}

fn validate_no_overlaps(resolved: &[ResolvedReplacement]) -> Result<(), ToolError> {
    let mut by_file: HashMap<&Path, Vec<(usize, usize, &str)>> = HashMap::new();
    for r in resolved {
        by_file
            .entry(&r.path)
            .or_default()
            .push((r.start_byte, r.end_byte, &r.qualified_name));
    }

    for (path, ranges) in &by_file {
        let mut sorted = ranges.clone();
        sorted.sort_by_key(|(start, _, _)| *start);
        for pair in sorted.windows(2) {
            let (_, e1, n1) = pair[0];
            let (s2, _, n2) = pair[1];
            if e1 > s2 {
                return Err(ToolError::InvalidRequest(format!(
                    "Overlapping replacements for '{n1}' and '{n2}' in {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn apply_replacements_to_file(
    _path: &Path,
    content: &str,
    replacements: Vec<&ResolvedReplacement>,
) -> Result<(String, Vec<FileResult>), ToolError> {
    // Sort bottom-up by byte offset
    let mut sorted: Vec<&ResolvedReplacement> = replacements;
    sorted.sort_by_key(|r| std::cmp::Reverse(r.start_byte));

    let mut bytes = content.as_bytes().to_vec();
    let mut results = Vec::new();

    for r in &sorted {
        let before: String = String::from_utf8_lossy(&bytes[..r.start_byte]).to_string();
        let _before_lines = before.lines().count().max(1);
        // Adjust: line numbers are 1-based, so the start line in current bytes is before_lines (if before is non-empty and doesn't end with newline)
        let _old_start = if before.is_empty() {
            1
        } else {
            before.lines().count() + 1
        };
        let new_bytes = r.new_text.as_bytes();
        bytes.splice(r.start_byte..r.end_byte, new_bytes.iter().copied());

        // Validate UTF-8 after splice
        let _ = String::from_utf8(bytes.clone())
            .map_err(|e| ToolError::ProviderError(format!("UTF-8 error after replacement: {e}")))?;
        // Calculate new line range
        let full_before: String = String::from_utf8_lossy(&bytes[..r.start_byte]).to_string();
        let new_start_line = full_before.lines().count() + 1;
        let new_text_lines = r.new_text.lines().count().max(1);
        let new_end_line = new_start_line + new_text_lines - 1;

        // Compute new hash
        let new_digest = SymbolDigest::new(new_bytes, new_text_lines);
        let new_hash = new_digest.hash.to_string();

        results.push(FileResult {
            kind_str: r.kind_str.clone(),
            qualified_name: r.qualified_name.clone(),
            old_hash: r.old_hash.clone(),
            old_start_line: r.old_start_line,
            old_end_line: r.old_end_line,
            new_start_line,
            new_end_line,
            new_hash,
        });
    }

    let new_content = String::from_utf8(bytes)
        .map_err(|e| ToolError::ProviderError(format!("UTF-8 error after replacements: {e}")))?;

    Ok((new_content, results))
}

fn format_results(results: &[FileResult]) -> String {
    let total_symbols = results.len();
    let mut lines = Vec::new();
    lines.push(format!("Updated {total_symbols} symbol(s)."));

    for r in results {
        let old_h = &r.old_hash[..8.min(r.old_hash.len())];
        let new_h = &r.new_hash[..8.min(r.new_hash.len())];
        lines.push(format!(
            "- replaced {} kind={} lines={}-{} old_hash={}.. new_hash={}",
            r.qualified_name, r.kind_str, r.old_start_line, r.old_end_line, old_h, new_h,
        ));
    }

    lines.push(String::new());
    lines.push(
        "Hint: use get_symbol with force=true only if you need to inspect the new bodies."
            .to_string(),
    );

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use tempfile::TempDir;

    fn text_content(contents: Vec<ToolResultPart>) -> String {
        match contents.into_iter().next().unwrap() {
            ToolResultPart::Text { text } => text,
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn replaces_rust_function_body() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(&path, "fn greet() {\n    println!(\"hello\");\n}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = ReplaceSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({
                    "replacements": [{
                        "path": "lib.rs",
                        "symbol": "greet",
                        "new_text": "fn greet() {\n    println!(\"world\");\n}"
                    }],
                    "root": dir.path()
                }),
                &context,
            )
            .await
            .unwrap(),
        );

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("world"));
        assert!(!written.contains("hello"));
        assert!(result.contains("Updated 1 symbol(s)"));
        assert!(result.contains("greet"));
    }

    #[tokio::test]
    async fn replaces_svelte_script_function() {
        // Symbols extracted from embedded `<script>` content must carry
        // whole-file coordinates so replacement hits the right byte range.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("Counter.svelte");
        tokio::fs::write(
            &path,
            "<script lang=\"ts\">\n  let count = 0;\n\n  function increment(): void {\n    count += 1;\n  }\n</script>\n\n<button on:click={increment}>{count}</button>\n",
        )
        .await
        .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = ReplaceSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({
                    "replacements": [{
                        "path": "Counter.svelte",
                        "symbol": "increment",
                        "new_text": "function increment(): void {\n    count += 2;\n}"
                    }],
                    "root": dir.path()
                }),
                &context,
            )
            .await
            .unwrap(),
        );

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("count += 2;"));
        assert!(!written.contains("count += 1;"));
        // The markup after the script must remain untouched.
        assert!(written.contains("<button on:click={increment}>{count}</button>"));
        assert!(result.contains("increment"));
    }

    #[tokio::test]
    async fn replaces_struct() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(&path, "struct Config {\n    name: String,\n}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = ReplaceSymbolTool::new();

        let _ = text_content(
            tool.call(
                json!({
                    "replacements": [{
                        "path": "lib.rs",
                        "symbol": "Config",
                        "new_text": "struct Config {\n    name: String,\n    version: u32,\n}"
                    }],
                    "root": dir.path()
                }),
                &context,
            )
            .await
            .unwrap(),
        );

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("version"));
    }

    #[tokio::test]
    async fn rejects_hash_mismatch() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(&path, "fn foo() {}\n").await.unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = ReplaceSymbolTool::new();

        let err = tool
            .call(
                json!({
                    "replacements": [{
                        "path": "lib.rs",
                        "symbol": "foo",
                        "expected_hash": "bogus_hash",
                        "new_text": "fn foo() { 1 }"
                    }],
                    "root": dir.path()
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(format!("{err}").contains("Hash mismatch"));
        // File should be unchanged
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "fn foo() {}\n"
        );
    }

    #[tokio::test]
    async fn rejects_overlapping_replacements() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(&path, "fn a() {}\nfn b() {}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = ReplaceSymbolTool::new();

        let err = tool
            .call(
                json!({
                    "replacements": [
                        {"path": "lib.rs", "symbol": "a", "new_text": "fn a() { 1 }"},
                        // "b" doesn't overlap with "a" in this case, but let's make it overlap
                        // by requesting the same symbol twice
                        {"path": "lib.rs", "symbol": "a", "occurrence": 0, "new_text": "fn a() { 2 }"}
                    ],
                    "root": dir.path()
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(format!("{err}").contains("Overlapping"));
    }

    #[tokio::test]
    async fn multi_file_replacement() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "fn alpha() { 1 }\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.rs"), "fn beta() { 2 }\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = ReplaceSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({
                    "replacements": [
                        {"path": "a.rs", "symbol": "alpha", "new_text": "fn alpha() { 10 }"},
                        {"path": "b.rs", "symbol": "beta", "new_text": "fn beta() { 20 }"}
                    ],
                    "root": dir.path()
                }),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("Updated 2 symbol(s)"));
        assert!(result.contains("alpha"));
        assert!(result.contains("beta"));
        assert!(
            tokio::fs::read_to_string(dir.path().join("a.rs"))
                .await
                .unwrap()
                .contains("10")
        );
        assert!(
            tokio::fs::read_to_string(dir.path().join("b.rs"))
                .await
                .unwrap()
                .contains("20")
        );
    }

    #[test]
    fn file_access_writes_every_replacement_path_with_custom_root() {
        let dir = TempDir::new().unwrap();
        let cwd = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(cwd.join("sub")).unwrap();
        let tool = ReplaceSymbolTool::new();

        // Same resolution as `call`: relative paths are joined with `root`
        // (itself resolved against cwd); absolute paths are used as written.
        let access = tool.file_access(
            &json!({
                "root": "sub",
                "replacements": [
                    {"path": "a.rs", "symbol": "alpha", "new_text": "x"},
                    {"path": cwd.join("sub/b.rs").to_str().unwrap(), "symbol": "beta", "new_text": "y"},
                    {"path": "./a.rs", "symbol": "alpha2", "new_text": "z"}
                ]
            }),
            Some(&cwd),
        );

        // Duplicate spellings of a.rs collapse into one entry, under `root`.
        assert_eq!(
            access,
            FileAccess::writes([cwd.join("sub/a.rs"), cwd.join("sub/b.rs")])
        );
    }

    #[tokio::test]
    async fn relative_path_is_resolved_against_root_not_cwd() {
        // Mirrors the real failure: session cwd is one repo, `root` another.
        let cwd_dir = TempDir::new().unwrap();
        let root_dir = TempDir::new().unwrap();
        let target = root_dir.path().join("src/lib.rs");
        tokio::fs::create_dir_all(target.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&target, "fn greet() {}\n").await.unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(cwd_dir.path().to_path_buf()));
        let tool = ReplaceSymbolTool::new();
        let args = json!({
            "replacements": [{
                "path": "src/lib.rs",
                "symbol": "greet",
                "new_text": "fn greet() { 1 }"
            }],
            "root": root_dir.path()
        });

        // `file_access` must predict the same file `call` writes.
        let cwd = dunce::canonicalize(cwd_dir.path()).unwrap();
        let canonical_target = dunce::canonicalize(&target).unwrap();
        assert_eq!(
            tool.file_access(&args, Some(&cwd)),
            FileAccess::writes([canonical_target])
        );

        tool.call(args, &context).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            "fn greet() { 1 }\n"
        );
    }

    #[test]
    fn file_access_is_none_for_invalid_args() {
        let tool = ReplaceSymbolTool::new();
        let cwd = std::env::temp_dir();

        assert_eq!(tool.file_access(&json!({}), Some(&cwd)), FileAccess::None);
        assert_eq!(
            tool.file_access(&json!({"replacements": []}), Some(&cwd)),
            FileAccess::None
        );
        assert_eq!(
            tool.file_access(
                &json!({"replacements": [{"symbol": "a", "new_text": "b"}]}),
                Some(&cwd)
            ),
            FileAccess::None
        );
        // Relative paths cannot be resolved without a cwd.
        assert_eq!(
            tool.file_access(
                &json!({"replacements": [{"path": "a.rs", "symbol": "a", "new_text": "b"}]}),
                None
            ),
            FileAccess::None
        );
    }

    #[tokio::test]
    async fn missing_symbol_returns_error() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("lib.rs"), "fn exists() {}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = ReplaceSymbolTool::new();

        let err = tool
            .call(
                json!({
                    "replacements": [{"path": "lib.rs", "symbol": "missing", "new_text": "fn missing() {}"}],
                    "root": dir.path()
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(format!("{err}").contains("not found"));
    }
}

//! Function body reads with session-scoped digest caching.
//!
//! Supports multi-file batched reads via the `paths` array.

use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool as ChatTool};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::anchors::symbol_cache::{check_symbol_cache, record_symbol_read};
use crate::anchors::{reconcile_file, render_anchored_range};
use crate::index::symbol_index::{SymbolEntry, SymbolIndex, SymbolKind, SymbolKindFilter};
use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

use super::helpers::resolve_root;

pub struct GetFunctionTool;

impl GetFunctionTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GetFunctionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GetFunctionTool {
    fn name(&self) -> &str {
        "get_function"
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Read one or more functions from one or more source files by name. Returns anchor-delimited function bodies on first read or when changed; repeated unchanged reads return only digest metadata unless force=true."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Source file paths, relative to the workspace root or absolute."
                        },
                        "names": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Function names to read. Exact names are matched. Looked up in every listed file."
                        },
                        "root": {
                            "type": "string",
                            "description": "Workspace root directory to resolve relative paths against.",
                            "default": "."
                        },
                        "force": {
                            "type": "boolean",
                            "description": "When true, always return anchored function bodies even if unchanged.",
                            "default": false
                        },
                        "context_lines": {
                            "type": "integer",
                            "description": "Number of surrounding lines before and after each function to include in the anchored body.",
                            "default": 0,
                            "minimum": 0
                        }
                    },
                    "required": ["paths", "names"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<Content>, ToolError> {
        let path_strs = parse_paths(&args)?;
        let names = parse_names(&args)?;
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        let context_lines = args
            .get("context_lines")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let root = resolve_root(&args, context)?;

        let mut file_results = Vec::new();
        for path_str in &path_strs {
            let target = resolve_target(path_str, &root, context)?;
            let content = tokio::fs::read_to_string(&target)
                .await
                .map_err(|e| ToolError::ProviderError(format!("Failed to read file: {e}")))?;
            let symbol_index = SymbolIndex::from_source_for_path(&target, &content)
                .map_err(|e| ToolError::ProviderError(e.to_string()))?;
            let state = reconcile_file(context.session_id(), &target, &content)
                .map_err(ToolError::ProviderError)?;

            let mut symbol_results = Vec::new();
            for name in &names {
                symbol_results.push(render_function_result(
                    context.session_id(),
                    &target,
                    &content,
                    &state,
                    &symbol_index,
                    name,
                    force,
                    context_lines,
                ));
            }
            file_results.push(format!(
                "{}\n{}",
                target.display(),
                symbol_results.join("\n\n")
            ));
        }

        Ok(vec![Content::text(file_results.join("\n\n"))])
    }
}

fn resolve_target(
    path_str: &str,
    root: &Path,
    context: &dyn ToolContext,
) -> Result<PathBuf, ToolError> {
    let resolved = context.resolve_path(path_str)?;
    Ok(if resolved.is_absolute() {
        resolved
    } else {
        root.join(resolved)
    })
}

fn parse_paths(args: &Value) -> Result<Vec<String>, ToolError> {
    let paths = args
        .get("paths")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidRequest("paths must be an array".to_string()))?;

    let parsed: Vec<String> = paths
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();

    if parsed.is_empty() {
        return Err(ToolError::InvalidRequest(
            "paths must include at least one file path".to_string(),
        ));
    }

    Ok(parsed)
}

fn parse_names(args: &Value) -> Result<Vec<String>, ToolError> {
    let names = args
        .get("names")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidRequest("names must be an array".to_string()))?;

    let parsed: Vec<String> = names
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();

    if parsed.is_empty() {
        return Err(ToolError::InvalidRequest(
            "names must include at least one function name".to_string(),
        ));
    }

    Ok(parsed)
}

#[allow(clippy::too_many_arguments)]
fn render_function_result(
    session_id: &str,
    path: &Path,
    content: &str,
    state: &crate::anchors::FileAnchorState,
    symbol_index: &SymbolIndex,
    name: &str,
    force: bool,
    context_lines: usize,
) -> String {
    let matches = symbol_index
        .find_by_name(name, SymbolKindFilter::Any)
        .into_iter()
        .filter(|symbol| matches_function_kind(symbol.kind))
        .collect::<Vec<_>>();

    if matches.is_empty() {
        return render_missing_function(path, name, symbol_index);
    }

    let mut rendered = Vec::new();
    if matches.len() > 1 {
        rendered.push(format!(
            "Ambiguous function '{name}' in {}: {} exact matches. Returning all matches.",
            path.display(),
            matches.len()
        ));
    }

    for function in matches {
        rendered.push(render_single_function(
            session_id,
            path,
            content,
            state,
            function,
            force,
            context_lines,
        ));
    }

    rendered.join("\n\n")
}

fn render_single_function(
    session_id: &str,
    path: &Path,
    content: &str,
    state: &crate::anchors::FileAnchorState,
    function: &SymbolEntry,
    force: bool,
    context_lines: usize,
) -> String {
    let start_offset = function.start_line.saturating_sub(1);
    let end_offset_exclusive = function.end_line;
    let digest = &function.digest;
    let header = format!(
        "{} [{}-{}] hash={} bytes={} lines={}",
        function.qualified_name,
        function.start_line,
        function.end_line,
        digest.hash,
        digest.byte_len,
        digest.line_count
    );

    let unchanged = check_symbol_cache(
        session_id,
        path,
        function.kind.as_str(),
        &function.qualified_name,
        digest,
        start_offset,
        end_offset_exclusive,
    );
    record_symbol_read(
        session_id,
        path,
        function.kind.as_str(),
        &function.qualified_name,
        start_offset,
        end_offset_exclusive,
        digest.clone(),
    );

    if unchanged && !force {
        return format!(
            "- {header}\n  No changes since your last read. Use force=true to re-read the body."
        );
    }

    let render_start = start_offset.saturating_sub(context_lines);
    let render_end = (end_offset_exclusive + context_lines).min(state.line_count);
    let body = render_anchored_range(
        content,
        state,
        render_start,
        render_end.saturating_sub(render_start).max(1),
    );

    format!("- {header}\n{body}")
}

fn render_missing_function(_path: &Path, name: &str, symbol_index: &SymbolIndex) -> String {
    let mut candidates = function_symbols(&symbol_index.symbols)
        .into_iter()
        .take(12)
        .map(|function| {
            format!(
                "{} [{}-{}]",
                function.qualified_name, function.start_line, function.end_line
            )
        })
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        return format!("- No functions found while looking for '{name}'.");
    }

    candidates.sort();
    format!(
        "- No function named '{name}' found. Available candidates: {}",
        candidates.join(", ")
    )
}

fn function_symbols(symbols: &[SymbolEntry]) -> Vec<&SymbolEntry> {
    let mut functions = Vec::new();
    for symbol in symbols {
        if matches_function_kind(symbol.kind) {
            functions.push(symbol);
        }
        functions.extend(function_symbols(&symbol.children));
    }
    functions
}

fn matches_function_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::store::clear_anchor_store_for_tests;
    use crate::anchors::symbol_cache::clear_symbol_cache_for_tests;
    use crate::tools::AgentToolContext;
    use tempfile::TempDir;

    fn text_content(contents: Vec<Content>) -> String {
        match contents.into_iter().next().unwrap() {
            Content::Text { text } => text,
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn first_read_returns_anchored_body_then_unchanged_marker() {
        clear_anchor_store_for_tests();
        clear_symbol_cache_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(&path, "fn alpha() {\n    println!(\"a\");\n}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetFunctionTool::new();

        let first = text_content(
            tool.call(
                json!({"paths": ["lib.rs"], "names": ["alpha"], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );
        assert!(first.contains("hash="));
        assert!(first.contains("§fn alpha()"));

        let second = text_content(
            tool.call(
                json!({"paths": ["lib.rs"], "names": ["alpha"], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );
        assert!(second.contains("No changes since your last read"));
        assert!(!second.contains("§fn alpha()"));
    }

    #[tokio::test]
    async fn force_returns_body_after_unchanged_read() {
        clear_anchor_store_for_tests();
        clear_symbol_cache_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(&path, "fn beta() {\n    println!(\"b\");\n}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetFunctionTool::new();

        let _ = tool
            .call(
                json!({"paths": ["lib.rs"], "names": ["beta"], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap();
        let forced = text_content(
            tool.call(
                json!({"paths": ["lib.rs"], "names": ["beta"], "root": dir.path(), "force": true}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(forced.contains("§fn beta()"));
    }

    #[tokio::test]
    async fn modified_function_returns_updated_body() {
        clear_anchor_store_for_tests();
        clear_symbol_cache_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(&path, "fn gamma() {\n    println!(\"old\");\n}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetFunctionTool::new();

        let _ = tool
            .call(
                json!({"paths": ["lib.rs"], "names": ["gamma"], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap();
        tokio::fs::write(&path, "fn gamma() {\n    println!(\"new\");\n}\n")
            .await
            .unwrap();
        let changed = text_content(
            tool.call(
                json!({"paths": ["lib.rs"], "names": ["gamma"], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(changed.contains("§    println!(\"new\");"));
        assert!(!changed.contains("No changes since your last read"));
    }

    #[tokio::test]
    async fn reads_nested_methods_by_qualified_name() {
        clear_anchor_store_for_tests();
        clear_symbol_cache_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(
            &path,
            "struct Config;\n\nimpl Config {\n    fn new() -> Self {\n        Config\n    }\n}\n",
        )
        .await
        .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetFunctionTool::new();

        let result = text_content(
            tool.call(
                json!({"paths": ["lib.rs"], "names": ["Config::new"], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("Config::new"));
        assert!(result.contains("§    fn new() -> Self"));
    }

    #[tokio::test]
    async fn multi_file_reads_functions_from_all_files() {
        clear_anchor_store_for_tests();
        clear_symbol_cache_for_tests();
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "fn alpha() {\n    1\n}\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.rs"), "fn beta() {\n    2\n}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetFunctionTool::new();

        let result = text_content(
            tool.call(
                json!({"paths": ["a.rs", "b.rs"], "names": ["alpha", "beta"], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("§fn alpha()"));
        assert!(result.contains("§fn beta()"));
        assert!(result.contains("alpha"));
        assert!(result.contains("beta"));
    }

    #[tokio::test]
    async fn missing_symbols_reported_compactly() {
        clear_anchor_store_for_tests();
        clear_symbol_cache_for_tests();
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("lib.rs"), "fn exists() {}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetFunctionTool::new();

        let result = text_content(
            tool.call(
                json!({"paths": ["lib.rs"], "names": ["missing"], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("No function named 'missing'"));
        assert!(result.contains("candidates"));
    }
}

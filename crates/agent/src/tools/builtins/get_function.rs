//! Function body reads with line-numbered output.
//!
//! Supports multi-file batched reads via the `paths` array.

use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool as ChatTool};
use serde_json::{Value, json};
use std::path::Path;

use crate::index::symbol_index::{SymbolEntry, SymbolIndex, SymbolKind};
use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

use super::helpers::{parse_paths, resolve_root, resolve_target};

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
                description: "Read one or more functions from one or more source files by name. Returns line-numbered function bodies with digest metadata."
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
                        "context_lines": {
                            "type": "integer",
                            "description": "Number of surrounding lines before and after each function to include.",
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

            let mut symbol_results = Vec::new();
            for name in &names {
                symbol_results.push(render_function_result(
                    &target,
                    &content,
                    &symbol_index,
                    name,
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

fn render_function_result(
    path: &Path,
    content: &str,
    symbol_index: &SymbolIndex,
    name: &str,
    context_lines: usize,
) -> String {
    let matches = symbol_index
        .find_by_name(name, None)
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
        rendered.push(render_single_function(content, function, context_lines));
    }

    rendered.join("\n\n")
}

fn render_single_function(content: &str, function: &SymbolEntry, context_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
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

    let render_start = start_offset.saturating_sub(context_lines);
    let render_end = (end_offset_exclusive + context_lines).min(total_lines);

    let mut body = String::new();
    for (idx, line) in lines.iter().enumerate().take(render_end).skip(render_start) {
        body.push_str(&format!("{:05}| {}\n", idx + 1, line));
    }

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
    use crate::tools::AgentToolContext;
    use tempfile::TempDir;

    fn text_content(contents: Vec<Content>) -> String {
        match contents.into_iter().next().unwrap() {
            Content::Text { text } => text,
            _ => panic!("expected text content"),
        }
    }

    #[tokio::test]
    async fn first_read_returns_line_numbered_body() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(&path, "fn alpha() {\n    println!(\"a\");\n}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetFunctionTool::new();

        let result = text_content(
            tool.call(
                json!({"paths": ["lib.rs"], "names": ["alpha"], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );
        assert!(result.contains("hash="));
        assert!(result.contains("00001| fn alpha()"));
        assert!(result.contains("00002|     println!(\"a\");"));
    }

    #[tokio::test]
    async fn reads_nested_methods_by_qualified_name() {
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
        assert!(result.contains("00004|     fn new() -> Self"));
    }

    #[tokio::test]
    async fn multi_file_reads_functions_from_all_files() {
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

        assert!(result.contains("00001| fn alpha()"));
        assert!(result.contains("00001| fn beta()"));
        assert!(result.contains("alpha"));
        assert!(result.contains("beta"));
    }

    #[tokio::test]
    async fn missing_symbols_reported_compactly() {
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

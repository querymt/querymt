//! Generic symbol reads backed by SymbolIndex.
//!
//! Supports batch reads via explicit `requests` arrays.
//! Supports `context_mode=relevant` to include imports and parent symbol context.

use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool as ChatTool};
use serde_json::{Value, json};
use std::path::Path;

use crate::index::symbol_index::{SymbolEntry, SymbolIndex, SymbolKind, parse_kind_filter};
use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

use super::helpers::{resolve_root, resolve_target};

pub struct GetSymbolTool;

impl GetSymbolTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GetSymbolTool {
    fn default() -> Self {
        Self::new()
    }
}

/// A single symbol read request targeting one file.
struct SymbolRequest {
    path: String,
    symbol: String,
    kind: Option<SymbolKind>,
    occurrence: usize,
}

#[async_trait]
impl Tool for GetSymbolTool {
    fn name(&self) -> &str {
        "get_symbol"
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Read structured AST symbols from source files. Provide explicit 'requests' entries for per-symbol path, kind, and occurrence control. Returns line-numbered symbol bodies with digest metadata. Use context_mode=relevant to include imports and parent context."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "requests": {
                            "type": "array",
                            "description": "Explicit per-symbol requests with optional kind and occurrence.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string", "description": "File path." },
                                    "symbol": { "type": "string", "description": "Symbol name or qualified name." },
                                    "kind": {
                                        "type": "string",
                                        "description": "Symbol kind filter: function, method, class, struct, enum, trait, impl, type, const, module, test, any.",
                                        "default": "any"
                                    },
                                    "occurrence": {
                                        "type": "integer",
                                        "description": "0-based occurrence when multiple symbols match.",
                                        "default": 0,
                                        "minimum": 0
                                    }
                                },
                                "required": ["path", "symbol"]
                            }
                        },
                        "root": { "type": "string", "description": "Workspace root directory to resolve relative paths against.", "default": "." },
                        "context_lines": { "type": "integer", "description": "Number of surrounding lines before and after the symbol to include.", "default": 0, "minimum": 0 },
                        "context_mode": {
                            "type": "string",
                            "description": "Context mode: 'none' (default), 'lines' (use context_lines), or 'relevant' (include imports and parent symbol context).",
                            "default": "none",
                            "enum": ["none", "lines", "relevant"]
                        },
                        "max_relevant_context_lines": { "type": "integer", "description": "Cap on relevant context lines when context_mode=relevant.", "default": 30, "minimum": 1 }
                    },
                    "required": ["requests"]
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
        let context_lines = args
            .get("context_lines")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let context_mode = args
            .get("context_mode")
            .and_then(Value::as_str)
            .unwrap_or("none");
        let max_relevant = args
            .get("max_relevant_context_lines")
            .and_then(Value::as_u64)
            .unwrap_or(30) as usize;
        let root = resolve_root(&args, context)?;

        let requests = parse_requests(&args)?;
        let mut file_results: Vec<(String, Vec<String>)> = Vec::new();

        for req in &requests {
            let target = resolve_target(&req.path, &root, context)?;
            let content = match tokio::fs::read_to_string(&target).await {
                Ok(c) => c,
                Err(e) => {
                    let err = format!("{}\n- Failed to read file: {e}", target.display());
                    append_file_result(&mut file_results, target, err);
                    continue;
                }
            };
            let symbol_index = match SymbolIndex::from_source_for_path(&target, &content) {
                Ok(idx) => idx,
                Err(e) => {
                    let err = format!("{}\n- Failed to index file: {e}", target.display());
                    append_file_result(&mut file_results, target, err);
                    continue;
                }
            };

            let matches = symbol_index.find_by_name(&req.symbol, req.kind);
            let result = render_symbol_result(
                &target,
                &content,
                &symbol_index,
                &matches,
                &req.symbol,
                req.kind,
                req.occurrence,
                context_lines,
                context_mode,
                max_relevant,
            );
            append_file_result(&mut file_results, target, result);
        }

        let output = file_results
            .iter()
            .map(|(file, results)| format!("{}\n{}", file, results.join("\n\n")))
            .collect::<Vec<_>>()
            .join("\n\n");

        Ok(vec![Content::text(output)])
    }
}

/// Parse explicit symbol read requests.
fn parse_requests(args: &Value) -> Result<Vec<SymbolRequest>, ToolError> {
    let requests = args
        .get("requests")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidRequest("requests must be an array".to_string()))?;

    if requests.is_empty() {
        return Err(ToolError::InvalidRequest(
            "requests must include at least one entry".to_string(),
        ));
    }

    requests
        .iter()
        .map(|r| {
            let path = r.get("path").and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidRequest("each request requires a path".to_string())
            })?;
            let symbol = r.get("symbol").and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidRequest("each request requires a symbol".to_string())
            })?;
            let kind = r.get("kind").and_then(Value::as_str).unwrap_or("any");
            let kind = parse_kind_filter(kind).map_err(ToolError::InvalidRequest)?;
            let occurrence = r.get("occurrence").and_then(Value::as_u64).unwrap_or(0) as usize;
            Ok(SymbolRequest {
                path: path.to_string(),
                symbol: symbol.to_string(),
                kind,
                occurrence,
            })
        })
        .collect()
}

fn append_file_result(
    file_results: &mut Vec<(String, Vec<String>)>,
    target: std::path::PathBuf,
    result: String,
) {
    let display = target.display().to_string();
    if let Some(entry) = file_results.iter_mut().find(|(f, _)| *f == display) {
        entry.1.push(result);
    } else {
        file_results.push((display, vec![result]));
    }
}

#[allow(clippy::too_many_arguments)]
fn render_symbol_result(
    path: &Path,
    content: &str,
    symbol_index: &SymbolIndex,
    matches: &[&SymbolEntry],
    symbol_name: &str,
    kind: Option<SymbolKind>,
    occurrence: usize,
    context_lines: usize,
    context_mode: &str,
    max_relevant: usize,
) -> String {
    if matches.is_empty() {
        return render_missing_symbol(path, symbol_name, kind, symbol_index);
    }

    let Some(symbol) = matches.get(occurrence).copied() else {
        return format!(
            "- Occurrence {occurrence} out of range for symbol '{symbol_name}'. {} matches available: {}",
            matches.len(),
            candidates(matches)
        );
    };

    let mut output = Vec::new();
    if matches.len() > 1 {
        output.push(format!(
            "- Ambiguous symbol '{symbol_name}': {} matches. Returning occurrence {occurrence}. Candidates: {}",
            matches.len(),
            candidates(matches)
        ));
    }
    output.push(render_single_symbol(
        content,
        symbol_index,
        symbol,
        context_lines,
        context_mode,
        max_relevant,
    ));
    output.join("\n\n")
}

#[allow(clippy::too_many_arguments)]
fn render_single_symbol(
    content: &str,
    symbol_index: &SymbolIndex,
    symbol: &SymbolEntry,
    context_lines: usize,
    context_mode: &str,
    max_relevant: usize,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    let start_offset = symbol.start_line.saturating_sub(1);
    let end_offset_exclusive = symbol.end_line;
    let digest = &symbol.digest;
    let header = format!(
        "- {} kind={} [{}-{}] hash={} bytes={} lines={}",
        symbol.qualified_name,
        symbol.kind.as_str(),
        symbol.start_line,
        symbol.end_line,
        digest.hash,
        digest.byte_len,
        digest.line_count
    );

    let effective_context = if context_mode == "none" {
        0
    } else {
        context_lines
    };
    let render_start = start_offset.saturating_sub(effective_context);
    let render_end = (end_offset_exclusive + effective_context).min(total_lines);

    let mut body = String::new();
    for (idx, line) in lines.iter().enumerate().take(render_end).skip(render_start) {
        body.push_str(&format!("{:05}| {}\n", idx + 1, line));
    }

    if context_mode == "relevant" {
        let relevant_ctx = extract_relevant_context(content, symbol_index, symbol, max_relevant);
        if relevant_ctx.is_empty() {
            format!("{header}\n{body}")
        } else {
            format!("{header}\nRelevant context:\n{relevant_ctx}\n\nBody:\n{body}")
        }
    } else {
        format!("{header}\n{body}")
    }
}

/// Extract relevant context lines for a symbol: imports mentioning its identifiers,
/// parent struct/class/impl header, and parent fields.
fn extract_relevant_context(
    content: &str,
    symbol_index: &SymbolIndex,
    symbol: &SymbolEntry,
    max_lines: usize,
) -> String {
    let mut context_lines = Vec::new();
    let mut line_count = 0;

    // 1. Collect imports that mention identifiers from the symbol's name
    let identifiers: Vec<&str> = symbol.name.split('_').collect();
    let imports = symbol_index.imports();
    for imp in &imports {
        if line_count >= max_lines {
            break;
        }
        let sig_lower = imp.signature.to_lowercase();
        let relevant = identifiers
            .iter()
            .any(|id| !id.is_empty() && sig_lower.contains(&id.to_lowercase()))
            || symbol.name == imp.name
            || imp.signature.contains(&symbol.name);
        if relevant {
            let line_idx = imp.start_line.saturating_sub(1);
            if let Some(line_text) = content.lines().nth(line_idx) {
                context_lines.push(format!("{:05}| {}", line_idx + 1, line_text));
                line_count += 1;
            }
        }
    }

    // 2. Parent symbol header (struct/class/impl/trait that contains this symbol)
    if let Some(parent) = symbol_index.find_parent_of(symbol)
        && line_count < max_lines
    {
        let parent_start = parent.start_line.saturating_sub(1);
        if let Some(line_text) = content.lines().nth(parent_start) {
            context_lines.push(format!("{:05}| {}", parent_start + 1, line_text));
            line_count += 1;
        }

        // Add parent fields if this is a method
        if matches!(symbol.kind, SymbolKind::Method) {
            for child in &parent.children {
                if line_count >= max_lines {
                    break;
                }
                if child.kind == SymbolKind::Field {
                    let field_idx = child.start_line.saturating_sub(1);
                    if let Some(line_text) = content.lines().nth(field_idx) {
                        context_lines.push(format!("{:05}| {}", field_idx + 1, line_text));
                        line_count += 1;
                    }
                }
            }
        }
    }

    context_lines.join("\n")
}

fn render_missing_symbol(
    _path: &Path,
    symbol_name: &str,
    kind: Option<SymbolKind>,
    symbol_index: &SymbolIndex,
) -> String {
    let mut candidates = all_symbols(&symbol_index.symbols)
        .into_iter()
        .filter(|symbol| symbol.kind.matches_filter(kind))
        .take(12)
        .map(|symbol| {
            format!(
                "{} kind={} [{}-{}]",
                symbol.qualified_name,
                symbol.kind.as_str(),
                symbol.start_line,
                symbol.end_line
            )
        })
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        return format!("- No symbols found while looking for '{symbol_name}'.");
    }

    candidates.sort();
    format!(
        "- No symbol named '{symbol_name}' found. Available candidates: {}",
        candidates.join(", ")
    )
}

fn candidates(matches: &[&SymbolEntry]) -> String {
    matches
        .iter()
        .map(|symbol| {
            format!(
                "{} kind={} [{}-{}]",
                symbol.qualified_name,
                symbol.kind.as_str(),
                symbol.start_line,
                symbol.end_line
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn all_symbols(symbols: &[SymbolEntry]) -> Vec<&SymbolEntry> {
    let mut entries = Vec::new();
    for symbol in symbols {
        entries.push(symbol);
        entries.extend(all_symbols(&symbol.children));
    }
    entries
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

    #[test]
    fn schema_requires_requests_without_top_level_one_of() {
        let parameters = GetSymbolTool::new().definition().function.parameters;

        assert_eq!(parameters.get("type"), Some(&json!("object")));
        assert_eq!(parameters.get("required"), Some(&json!(["requests"])));
        assert!(parameters.get("oneOf").is_none());
        assert!(parameters.get("paths").is_none());
        assert!(parameters.get("symbols").is_none());
        assert!(parameters.get("kind").is_none());
    }

    #[tokio::test]
    async fn first_read_returns_symbol_body() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(&path, "struct Config {\n    name: String,\n}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({"requests": [{"path": "lib.rs", "symbol": "Config", "kind": "struct"}], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );
        assert!(result.contains("kind=struct"));
        assert!(result.contains("00001| struct Config"));
    }

    #[tokio::test]
    async fn reads_nested_method_by_qualified_name() {
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
        let tool = GetSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({"requests": [{"path": "lib.rs", "symbol": "Config::new", "kind": "method"}], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("Config::new kind=method"));
        assert!(result.contains("00004|     fn new() -> Self"));
    }

    #[tokio::test]
    async fn ambiguous_symbols_report_candidates_and_occurrence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        tokio::fs::write(
            &path,
            "struct A;\nimpl A { fn new() -> Self { A } }\nstruct B;\nimpl B { fn new() -> Self { B } }\n",
        )
        .await
        .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({"requests": [{"path": "lib.rs", "symbol": "new", "kind": "method", "occurrence": 1}], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("Ambiguous symbol 'new'"));
        assert!(result.contains("A::new"));
        assert!(result.contains("B::new"));
        assert!(result.contains("B::new kind=method"));
    }

    #[tokio::test]
    async fn multi_file_reads_symbols_from_all_files() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "struct Alpha { x: i32 }\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.rs"), "struct Beta { y: String }\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({"requests": [
                    {"path": "a.rs", "symbol": "Alpha", "kind": "struct"},
                    {"path": "b.rs", "symbol": "Beta", "kind": "struct"}
                ], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("00001| struct Alpha"));
        assert!(result.contains("00001| struct Beta"));
    }

    #[tokio::test]
    async fn explicit_requests_mode_reads_across_files() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join("types.rs"),
            "struct Config { name: String }\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            dir.path().join("lib.rs"),
            "fn parse() -> Config { Config { name: \"\".into() } }\n",
        )
        .await
        .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({"requests": [
                    {"path": "types.rs", "symbol": "Config", "kind": "struct"},
                    {"path": "lib.rs", "symbol": "parse", "kind": "function"}
                ], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("00001| struct Config"));
        assert!(result.contains("00001| fn parse()"));
    }

    #[tokio::test]
    async fn relevant_context_includes_parent_struct_for_method() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join("lib.rs"),
            "struct Service {\n    url: String,\n}\n\nimpl Service {\n    fn connect(&self) {\n        todo!()\n    }\n}\n",
        )
        .await
        .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({
                    "requests": [{"path": "lib.rs", "symbol": "connect", "kind": "method"}],
                    "context_mode": "relevant",
                    "root": dir.path()
                }),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("Relevant context:"));
        assert!(result.contains("struct Service") || result.contains("impl Service"));
    }

    #[tokio::test]
    async fn go_smoke_reads_function_symbol() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.go");
        tokio::fs::write(&path, "package main\n\nfunc Run() {}\n")
            .await
            .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({"requests": [{"path": "main.go", "symbol": "Run", "kind": "function"}], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("Run kind=function"));
        assert!(result.contains("00003| func Run()"));
    }

    #[tokio::test]
    async fn csharp_smoke_reads_class_symbol() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.cs");
        tokio::fs::write(
            &path,
            "namespace MyApp { public class Config { public bool Validate() { return true; } } }\n",
        )
        .await
        .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({"requests": [{"path": "lib.cs", "symbol": "Config", "kind": "class"}], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(
            result.contains("MyApp::Config kind=class") || result.contains("Config kind=class")
        );
        assert!(
            result.contains("00001| namespace MyApp")
                || result.contains("00001| public class Config")
        );
    }

    #[tokio::test]
    async fn ruby_smoke_reads_method_symbol() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rb");
        tokio::fs::write(
            &path,
            "class Config\n  def validate\n    true\n  end\nend\n",
        )
        .await
        .unwrap();
        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = GetSymbolTool::new();

        let result = text_content(
            tool.call(
                json!({"requests": [{"path": "lib.rb", "symbol": "Config::validate", "kind": "method"}], "root": dir.path()}),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("Config::validate kind=method"));
        assert!(result.contains("00002|   def validate"));
    }
}

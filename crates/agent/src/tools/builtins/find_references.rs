//! Symbol reference discovery tool.
//!
//! Uses `SymbolIndex` for definitions and text-based identifier matching
//! for references. Returns compact anchored results grouped by file.

use async_trait::async_trait;
use ignore::{WalkBuilder, types::TypesBuilder};
use querymt::chat::{Content, FunctionTool, Tool as ChatTool};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::anchors::store::reconcile_file;
use crate::index::symbol_index::{SymbolIndex, SymbolKindFilter};
use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

use super::helpers::resolve_root;

pub struct FindSymbolReferencesTool;

impl FindSymbolReferencesTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FindSymbolReferencesTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct RefMatch {
    path: PathBuf,
    line: usize,
    text: String,
    match_type: String, // "definition" or "reference"
    symbol_name: String,
}

#[async_trait]
impl Tool for FindSymbolReferencesTool {
    fn name(&self) -> &str {
        "find_symbol_references"
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Find definitions and references for symbols across files. Uses AST symbol index for definitions and identifier text matching for references. Returns compact anchored results grouped by file."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Directories or files to search."
                        },
                        "symbols": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Symbol names to find."
                        },
                        "find_type": {
                            "type": "string",
                            "description": "What to find: 'definition', 'reference', or 'both'.",
                            "default": "both",
                            "enum": ["definition", "reference", "both"]
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum total results.",
                            "default": 200,
                            "minimum": 1
                        },
                        "root": {
                            "type": "string",
                            "description": "Workspace root directory.",
                            "default": "."
                        }
                    },
                    "required": ["paths", "symbols"]
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
        let root = resolve_root(&args, context)?;
        let search_paths = parse_paths(&args)?;
        let symbols = parse_symbols(&args)?;
        let find_type = args
            .get("find_type")
            .and_then(Value::as_str)
            .unwrap_or("both");
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(200) as usize;

        let mut matches: Vec<RefMatch> = Vec::new();

        for search_path in &search_paths {
            let resolved = resolve_search_target(search_path, &root, context)?;
            if resolved.is_file() {
                self.search_file(
                    &resolved,
                    &symbols,
                    find_type,
                    max_results - matches.len(),
                    &mut matches,
                )
                .await;
            } else if resolved.is_dir() {
                self.search_dir(
                    &resolved,
                    &symbols,
                    find_type,
                    max_results - matches.len(),
                    &mut matches,
                )
                .await;
            }
            if matches.len() >= max_results {
                break;
            }
        }

        let truncated = matches.len() > max_results;
        matches.truncate(max_results);

        let output = format_matches(&matches, &root, truncated);
        Ok(vec![Content::text(output)])
    }
}

impl FindSymbolReferencesTool {
    async fn search_file(
        &self,
        path: &Path,
        symbols: &[String],
        find_type: &str,
        remaining: usize,
        matches: &mut Vec<RefMatch>,
    ) {
        if remaining == 0 {
            return;
        }
        let content = match tokio::fs::read_to_string(path).await {
            Ok(c) => c,
            Err(_) => return,
        };

        let symbol_index = match SymbolIndex::from_source_for_path(path, &content) {
            Ok(idx) => idx,
            Err(_) => return,
        };

        let state = reconcile_file("refs", path, &content).ok();

        for symbol_name in symbols {
            // Definitions via SymbolIndex
            if find_type == "definition" || find_type == "both" {
                let defs = symbol_index.find_by_name(symbol_name, SymbolKindFilter::Any);
                for def in defs {
                    if matches.len() >= remaining {
                        return;
                    }
                    let line_idx = def.start_line.saturating_sub(1);
                    let text = content.lines().nth(line_idx).unwrap_or("").to_string();
                    let anchor = state
                        .as_ref()
                        .and_then(|s| s.lines.get(line_idx))
                        .map(|l| l.anchor.clone());
                    let line_text = match anchor {
                        Some(a) => format!("{}§{}", a, text),
                        None => text,
                    };
                    matches.push(RefMatch {
                        path: path.to_path_buf(),
                        line: def.start_line,
                        text: line_text,
                        match_type: "definition".to_string(),
                        symbol_name: def.qualified_name.clone(),
                    });
                }
            }

            // References via text matching
            if find_type == "reference" || find_type == "both" {
                for (i, line) in content.lines().enumerate() {
                    if matches.len() >= remaining {
                        return;
                    }
                    if line.contains(symbol_name.as_str()) {
                        // Skip if this line is the definition itself
                        let is_def_line = symbol_index
                            .find_by_name(symbol_name, SymbolKindFilter::Any)
                            .iter()
                            .any(|d| d.start_line == i + 1);
                        if is_def_line && find_type == "both" {
                            continue; // Already reported as definition
                        }
                        let anchor = state
                            .as_ref()
                            .and_then(|s| s.lines.get(i))
                            .map(|l| l.anchor.clone());
                        let line_text = match anchor {
                            Some(a) => format!("{}§{}", a, line),
                            None => line.to_string(),
                        };
                        matches.push(RefMatch {
                            path: path.to_path_buf(),
                            line: i + 1,
                            text: line_text,
                            match_type: "reference".to_string(),
                            symbol_name: symbol_name.clone(),
                        });
                    }
                }
            }
        }
    }

    async fn search_dir(
        &self,
        dir: &Path,
        symbols: &[String],
        find_type: &str,
        max_results: usize,
        matches: &mut Vec<RefMatch>,
    ) {
        let types = TypesBuilder::new()
            .add_defaults()
            .select("rust")
            .select("ts")
            .select("js")
            .select("python")
            .select("go")
            .select("java")
            .select("c")
            .select("cpp")
            .select("csharp")
            .select("ruby")
            .build()
            .unwrap_or_else(|_| TypesBuilder::new().build().unwrap());

        let walker = WalkBuilder::new(dir)
            .types(types)
            .hidden(true)
            .git_ignore(true)
            .build();

        for entry in walker.flatten() {
            if matches.len() >= max_results {
                break;
            }
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                self.search_file(
                    entry.path(),
                    symbols,
                    find_type,
                    max_results - matches.len(),
                    matches,
                )
                .await;
            }
        }
    }
}

fn resolve_search_target(
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
    let result: Vec<String> = paths
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if result.is_empty() {
        return Err(ToolError::InvalidRequest(
            "paths must include at least one path".to_string(),
        ));
    }
    Ok(result)
}

fn parse_symbols(args: &Value) -> Result<Vec<String>, ToolError> {
    let symbols = args
        .get("symbols")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidRequest("symbols must be an array".to_string()))?;
    let result: Vec<String> = symbols
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if result.is_empty() {
        return Err(ToolError::InvalidRequest(
            "symbols must include at least one name".to_string(),
        ));
    }
    Ok(result)
}

fn format_matches(matches: &[RefMatch], root: &Path, truncated: bool) -> String {
    use std::collections::HashMap;
    let mut by_file: HashMap<PathBuf, Vec<&RefMatch>> = HashMap::new();
    for m in matches {
        by_file.entry(m.path.clone()).or_default().push(m);
    }

    let mut lines = Vec::new();
    for (path, file_matches) in &by_file {
        let rel = path.strip_prefix(root).unwrap_or(path);
        lines.push(rel.display().to_string());
        for m in file_matches {
            lines.push(format!(
                "  {} {}:{} {}",
                m.match_type, m.symbol_name, m.line, m.text
            ));
        }
        lines.push(String::new());
    }

    if truncated {
        lines.push("Results capped. Narrow paths or reduce symbols to see more.".to_string());
    }

    lines.join("\n")
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
    async fn finds_definitions_and_references_in_file() {
        clear_anchor_store_for_tests();
        clear_symbol_cache_for_tests();
        let dir = TempDir::new().unwrap();
        tokio::fs::write(
            dir.path().join("lib.rs"),
            "fn parse_config() -> Config {\n    Config::default()\n}\n",
        )
        .await
        .unwrap();

        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = FindSymbolReferencesTool::new();

        let result = text_content(
            tool.call(
                json!({
                    "paths": [dir.path().join("lib.rs").display().to_string()],
                    "symbols": ["parse_config"],
                    "root": dir.path()
                }),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("definition"));
        assert!(result.contains("parse_config"));
    }

    #[tokio::test]
    async fn finds_references_across_files_in_directory() {
        clear_anchor_store_for_tests();
        clear_symbol_cache_for_tests();
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "fn helper() -> i32 { 42 }\n")
            .await
            .unwrap();
        tokio::fs::write(
            dir.path().join("b.rs"),
            "fn main() {\n    let x = helper();\n}\n",
        )
        .await
        .unwrap();

        let context =
            AgentToolContext::basic("session".to_string(), Some(dir.path().to_path_buf()));
        let tool = FindSymbolReferencesTool::new();

        let result = text_content(
            tool.call(
                json!({
                    "paths": ["."],
                    "symbols": ["helper"],
                    "root": dir.path()
                }),
                &context,
            )
            .await
            .unwrap(),
        );

        assert!(result.contains("definition"));
        assert!(result.contains("reference"));
    }
}

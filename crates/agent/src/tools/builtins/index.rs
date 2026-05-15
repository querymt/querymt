//! Index tool — structural skeleton extraction for source files.
//!
//! Produces a compact, token-efficient outline of a source file with exact
//! line ranges per item. Intended for reconnaissance before targeted reads
//! with `read_tool`.

use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool};
use serde_json::{Value, json};

use crate::index::outline_index::common::get_language_for_extension;
use crate::index::outline_index::{IndexOptions, OutlineError, format_outline, index_file};
use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

pub struct IndexTool;

impl Default for IndexTool {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for IndexTool {
    fn name(&self) -> &str {
        "index"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Produce a compact structural skeleton of a source file with exact line ranges per item. Use this before read_tool to understand file structure and target reads to relevant sections. Returns imports, types, classes, traits, impls, functions, tests, etc. with [start-end] line ranges. Supports Rust, Python, TypeScript, JavaScript, Go, Java, C, C++, C#, Ruby, and Elixir."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path to index (absolute or relative to workspace root)."
                        },
                        "root": {
                            "type": "string",
                            "description": "Workspace root directory to resolve relative paths against.",
                            "default": "."
                        },
                        "max_file_bytes": {
                            "type": "integer",
                            "description": "Maximum file size in bytes to parse. Defaults to 1MB.",
                            "minimum": 1
                        },
                        "max_children_per_item": {
                            "type": "integer",
                            "description": "Maximum number of child entries (fields, methods) per container. Unlimited by default.",
                            "minimum": 1
                        },
                        "include_tests": {
                            "type": "boolean",
                            "description": "Whether to include test-related items in the output.",
                            "default": true
                        }
                    },
                    "required": ["path"]
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
        let path_str = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("path is required".to_string()))?;

        let root = args
            .get("root")
            .and_then(Value::as_str)
            .map(|s| context.resolve_path(s))
            .transpose()?
            .or_else(|| context.cwd().map(|p| p.to_path_buf()))
            .ok_or_else(|| ToolError::InvalidRequest("No working directory available".into()))?;

        let path = context.resolve_path(path_str)?;
        let target = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };

        if !target.exists() {
            return Err(ToolError::InvalidRequest(format!(
                "File not found: {}",
                target.display()
            )));
        }

        if target.is_dir() {
            return Err(ToolError::InvalidRequest(
                "index only works on files, not directories. Use glob or ls for directories."
                    .to_string(),
            ));
        }

        let max_file_bytes = args
            .get("max_file_bytes")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        let max_children_per_item = args
            .get("max_children_per_item")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        let include_tests = args
            .get("include_tests")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let options = IndexOptions {
            max_file_bytes,
            max_children_per_item,
            include_tests,
        };

        let sections = index_file(&target, &options).map_err(|e| match e {
            OutlineError::UnsupportedLanguage(ext) => ToolError::InvalidRequest(format!(
                "Unsupported file extension '.{}'. Supported: rs, py, ts, tsx, js, jsx, go, java, c, h, cpp, hpp, cc, cs, rb, ex, exs",
                ext
            )),
            OutlineError::FileTooLarge { size, limit } => ToolError::InvalidRequest(format!(
                "File too large: {} bytes (limit: {} bytes). Use max_file_bytes to increase the limit, or use read_tool with offset/limit instead.",
                size, limit
            )),
            OutlineError::ParseError(msg) => ToolError::ProviderError(format!(
                "Failed to parse file: {}",
                msg
            )),
            OutlineError::Io(msg) => ToolError::ProviderError(format!(
                "Failed to read file: {}",
                msg
            )),
        })?;

        // Determine language name for the header
        let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("");
        let language = get_language_for_extension(ext).unwrap_or("unknown");

        let display_path = target.to_string_lossy().to_string();
        let output = format_outline(&display_path, language, &sections);

        Ok(vec![Content::Text { text: output }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use std::fs;
    use tempfile::TempDir;

    fn first_text(blocks: &[Content]) -> &str {
        for b in blocks {
            if let Content::Text { text } = b {
                return text.as_str();
            }
        }
        panic!("no Content::Text block found");
    }

    #[tokio::test]
    async fn test_index_rust_file() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let rust_source = r#"
use std::collections::HashMap;

pub struct Config {
    pub name: String,
    pub retries: usize,
}

pub fn run(args: Vec<String>) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run() {
        assert!(run(vec![]).is_ok());
    }
}
"#;

        let file_path = temp_dir.path().join("lib.rs");
        fs::write(&file_path, rust_source).unwrap();

        let tool = IndexTool::new();
        let args = json!({ "path": file_path.to_str().unwrap() });
        let result = tool.call(args, &context).await.unwrap();
        let text = first_text(&result);

        assert!(text.contains("language: rust"));
        assert!(text.contains("imports:"));
        assert!(text.contains("types:"));
        assert!(text.contains("pub struct Config"));
        assert!(text.contains("functions:"));
        assert!(text.contains("tests:"));
    }

    #[tokio::test]
    async fn test_index_python_file() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let py_source = r#"
import os
from pathlib import Path

class Config:
    def __init__(self, name: str):
        self.name = name

    def validate(self) -> bool:
        return True

def main():
    config = Config("test")
"#;

        let file_path = temp_dir.path().join("app.py");
        fs::write(&file_path, py_source).unwrap();

        let tool = IndexTool::new();
        let args = json!({ "path": file_path.to_str().unwrap() });
        let result = tool.call(args, &context).await.unwrap();
        let text = first_text(&result);

        assert!(text.contains("language: python"));
        assert!(text.contains("imports:"));
        assert!(text.contains("classes:"));
        assert!(text.contains("class Config"));
        assert!(text.contains("functions:"));
        assert!(text.contains("def main"));
    }

    #[tokio::test]
    async fn test_index_unsupported_extension() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let file_path = temp_dir.path().join("data.xyz");
        fs::write(&file_path, "some content").unwrap();

        let tool = IndexTool::new();
        let args = json!({ "path": file_path.to_str().unwrap() });
        let result = tool.call(args, &context).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unsupported"));
        assert!(err.contains("ex"));
    }

    #[tokio::test]
    async fn test_index_nonexistent_file() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let tool = IndexTool::new();
        let args = json!({ "path": "/nonexistent/file.rs" });
        let result = tool.call(args, &context).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn test_index_directory_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let tool = IndexTool::new();
        let args = json!({ "path": temp_dir.path().to_str().unwrap() });
        let result = tool.call(args, &context).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("directories"));
    }

    #[tokio::test]
    async fn test_index_exclude_tests() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let rust_source = r#"
pub fn run() {}

#[cfg(test)]
mod tests {
    #[test]
    fn test_it() {}
}
"#;

        let file_path = temp_dir.path().join("lib.rs");
        fs::write(&file_path, rust_source).unwrap();

        let tool = IndexTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "include_tests": false
        });
        let result = tool.call(args, &context).await.unwrap();
        let text = first_text(&result);

        assert!(!text.contains("tests:"));
        assert!(text.contains("functions:"));
    }
}

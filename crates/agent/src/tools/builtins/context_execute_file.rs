//! Context-safe file processing tool.
//!
//! Reads a file, indexes its contents for retrieval, and returns a bounded
//! preview. Useful for processing large files without flooding model context.

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

/// Default maximum preview lines returned to model context.
const DEFAULT_PREVIEW_LINES: usize = 80;

pub struct ContextExecuteFileTool;

impl Default for ContextExecuteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextExecuteFileTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for ContextExecuteFileTool {
    fn name(&self) -> &str {
        "context_execute_file"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: concat!(
                    "Read a file, index its full contents for later retrieval, and return ",
                    "a bounded preview. Use this for large files (logs, data files, config dumps) ",
                    "where you need to search the content without loading it all into context. ",
                    "The full file is searchable via `context_search`."
                )
                .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to process."
                        },
                        "preview_lines": {
                            "type": "integer",
                            "description": "Maximum lines in the returned preview (default: 80). Full content is always indexed.",
                            "default": 80
                        },
                        "source_label": {
                            "type": "string",
                            "description": "Optional label for the indexed source. Defaults to the file path."
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

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: The full file content is indexed. Use `context_search` \
             to find specific sections without loading the entire file.",
        )
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let path_str = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("path is required".to_string()))?;

        let resolved_path = context.resolve_path(path_str)?;

        let preview_lines = args
            .get("preview_lines")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_PREVIEW_LINES as u64) as usize;

        let source_label = args
            .get("source_label")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| path_str.to_string());

        // Read the file
        let content = tokio::fs::read_to_string(&resolved_path)
            .await
            .map_err(|e| {
                ToolError::ProviderError(format!(
                    "failed to read file '{}': {}",
                    resolved_path.display(),
                    e
                ))
            })?;

        let total_lines = content.lines().count();
        let total_bytes = content.len();

        // Index the full content (best-effort)
        let indexed = match context
            .index_context_content(&source_label, content.clone())
            .await
        {
            Ok(_) => true,
            Err(e) => {
                log::debug!("context_execute_file: failed to index content: {}", e);
                false
            }
        };

        // Build bounded preview
        let preview = build_preview(&content, preview_lines);

        let result = json!({
            "path": path_str,
            "preview": preview,
            "total_lines": total_lines,
            "total_bytes": total_bytes,
            "indexed": indexed,
            "source_label": source_label,
            "hint": if total_lines > preview_lines {
                "File was large. Use `context_search` to find specific content."
            } else {
                "Full file content shown."
            }
        });

        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

/// Build a bounded preview: head + tail with a gap indicator.
fn build_preview(content: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= max_lines {
        return content.to_string();
    }

    let head_count = max_lines * 2 / 3;
    let tail_count = max_lines - head_count;
    let omitted = lines.len() - head_count - tail_count;

    let mut preview = lines[..head_count].join("\n");
    preview.push_str(&format!("\n\n... ({} lines omitted) ...\n\n", omitted));
    preview.push_str(&lines[lines.len() - tail_count..].join("\n"));
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_context_execute_file_small() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "line 1\nline 2\nline 3").unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = ContextExecuteFileTool::new();

        let args = json!({
            "path": file_path.to_str().unwrap()
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["total_lines"], 3);
        assert!(parsed["preview"].as_str().unwrap().contains("line 1"));
        assert_eq!(parsed["hint"], "Full file content shown.");
    }

    #[tokio::test]
    async fn test_context_execute_file_large() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("large.txt");
        let content = (0..200)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file_path, &content).unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = ContextExecuteFileTool::new();

        let args = json!({
            "path": file_path.to_str().unwrap(),
            "preview_lines": 20
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["total_lines"], 200);
        let preview = parsed["preview"].as_str().unwrap();
        assert!(preview.contains("line 0"));
        assert!(preview.contains("omitted"));
        assert!(preview.contains("line 199"));
        assert_eq!(
            parsed["hint"],
            "File was large. Use `context_search` to find specific content."
        );
    }

    #[tokio::test]
    async fn test_context_execute_file_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = ContextExecuteFileTool::new();

        let args = json!({
            "path": "/nonexistent/file.txt"
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_context_execute_file_custom_label() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("data.csv");
        std::fs::write(&file_path, "col1,col2\na,b").unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = ContextExecuteFileTool::new();

        let args = json!({
            "path": file_path.to_str().unwrap(),
            "source_label": "dataset"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["source_label"], "dataset");
    }
}

//! Read file tool implementation using ToolContext

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

pub struct ReadFileTool;

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Read contents of a file under the workspace. Supports reading the full file or a specific line range."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read, relative to the workspace root or absolute."
                        },
                        "root": {
                            "type": "string",
                            "description": "Workspace root directory to resolve relative paths against.",
                            "default": "."
                        },
                        "start_line": {
                            "type": "integer",
                            "description": "Line number to start reading from (1-indexed, inclusive). If omitted, reads from beginning.",
                            "minimum": 1
                        },
                        "line_count": {
                            "type": "integer",
                            "description": "Number of lines to read from start_line. If omitted, reads to end of file.",
                            "minimum": 1
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

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let path = args
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

        let path = context.resolve_path(path)?;
        let target = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };

        let content = tokio::fs::read_to_string(&target)
            .await
            .map_err(|e| ToolError::ProviderError(format!("read failed: {}", e)))?;

        // Parse optional line range parameters
        let start_line_arg = args
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let line_count_arg = args
            .get("line_count")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        // Split content into lines
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Determine range and validate
        let (start_idx, end_idx, actual_start, actual_end) = match (start_line_arg, line_count_arg)
        {
            (None, None) => {
                // Full file read
                (0, total_lines, None, None)
            }
            (Some(start), None) => {
                // Read from start_line to EOF
                if start < 1 {
                    return Err(ToolError::InvalidRequest(
                        "start_line must be >= 1".to_string(),
                    ));
                }
                if total_lines > 0 && start > total_lines {
                    return Err(ToolError::InvalidRequest(format!(
                        "start_line {} exceeds file length {}",
                        start, total_lines
                    )));
                }
                let start_idx = if total_lines == 0 { 0 } else { start - 1 };
                let end_idx = total_lines;
                (start_idx, end_idx, Some(start), Some(end_idx))
            }
            (Some(start), Some(count)) => {
                // Read specific range
                if start < 1 {
                    return Err(ToolError::InvalidRequest(
                        "start_line must be >= 1".to_string(),
                    ));
                }
                if count < 1 {
                    return Err(ToolError::InvalidRequest(
                        "line_count must be >= 1".to_string(),
                    ));
                }
                if total_lines > 0 && start > total_lines {
                    return Err(ToolError::InvalidRequest(format!(
                        "start_line {} exceeds file length {}",
                        start, total_lines
                    )));
                }
                let start_idx = if total_lines == 0 { 0 } else { start - 1 };
                let end_idx = (start_idx + count).min(total_lines);
                let actual_end = if total_lines == 0 { 0 } else { end_idx };
                (start_idx, end_idx, Some(start), Some(actual_end))
            }
            (None, Some(_)) => {
                return Err(ToolError::InvalidRequest(
                    "line_count requires start_line to be specified".to_string(),
                ));
            }
        };

        // Extract selected lines
        let selected_content = if total_lines == 0 {
            String::new()
        } else {
            lines[start_idx..end_idx].join("\n")
        };

        let result = json!({
            "path": target.display().to_string(),
            "content": selected_content,
            "start_line": actual_start,
            "end_line": actual_end,
            "total_lines": total_lines
        });

        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use serde_json::json;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    async fn create_test_file(dir: &TempDir, name: &str, content: &str) -> PathBuf {
        let file_path = dir.path().join(name);
        let mut file = fs::File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    #[tokio::test]
    async fn test_read_full_file() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path = create_test_file(
            &temp_dir,
            "test.txt",
            "line 1\nline 2\nline 3\nline 4\nline 5",
        )
        .await;

        let tool = ReadFileTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap()
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(
            parsed["content"].as_str().unwrap(),
            "line 1\nline 2\nline 3\nline 4\nline 5"
        );
        assert_eq!(parsed["start_line"], Value::Null);
        assert_eq!(parsed["end_line"], Value::Null);
        assert_eq!(parsed["total_lines"], 5);
    }

    #[tokio::test]
    async fn test_read_with_start_line_only() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path = create_test_file(
            &temp_dir,
            "test.txt",
            "line 1\nline 2\nline 3\nline 4\nline 5",
        )
        .await;

        let tool = ReadFileTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "start_line": 3
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(
            parsed["content"].as_str().unwrap(),
            "line 3\nline 4\nline 5"
        );
        assert_eq!(parsed["start_line"], 3);
        assert_eq!(parsed["end_line"], 5);
        assert_eq!(parsed["total_lines"], 5);
    }

    #[tokio::test]
    async fn test_read_with_start_line_and_count() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path = create_test_file(
            &temp_dir,
            "test.txt",
            "line 1\nline 2\nline 3\nline 4\nline 5",
        )
        .await;

        let tool = ReadFileTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "start_line": 2,
            "line_count": 2
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["content"].as_str().unwrap(), "line 2\nline 3");
        assert_eq!(parsed["start_line"], 2);
        assert_eq!(parsed["end_line"], 3);
        assert_eq!(parsed["total_lines"], 5);
    }

    #[tokio::test]
    async fn test_read_line_count_exceeds_file_length() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path = create_test_file(&temp_dir, "test.txt", "line 1\nline 2\nline 3").await;

        let tool = ReadFileTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "start_line": 2,
            "line_count": 10
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        // Should read from line 2 to EOF (lines 2 and 3)
        assert_eq!(parsed["content"].as_str().unwrap(), "line 2\nline 3");
        assert_eq!(parsed["start_line"], 2);
        assert_eq!(parsed["end_line"], 3);
        assert_eq!(parsed["total_lines"], 3);
    }

    #[tokio::test]
    async fn test_read_empty_file() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path = create_test_file(&temp_dir, "empty.txt", "").await;

        let tool = ReadFileTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap()
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["content"].as_str().unwrap(), "");
        assert_eq!(parsed["start_line"], Value::Null);
        assert_eq!(parsed["end_line"], Value::Null);
        assert_eq!(parsed["total_lines"], 0);
    }

    #[tokio::test]
    async fn test_start_line_zero_error() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path = create_test_file(&temp_dir, "test.txt", "line 1\nline 2").await;

        let tool = ReadFileTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "start_line": 0
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("start_line must be >= 1")
        );
    }

    #[tokio::test]
    async fn test_read_with_relative_path() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        create_test_file(&temp_dir, "test.txt", "content line 1\ncontent line 2").await;

        let tool = ReadFileTool::new();
        let args = json!({
            "path": "test.txt",
            "start_line": 1,
            "line_count": 1
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["content"].as_str().unwrap(), "content line 1");
        assert_eq!(parsed["start_line"], 1);
        assert_eq!(parsed["end_line"], 1);
        assert_eq!(parsed["total_lines"], 2);
    }
}

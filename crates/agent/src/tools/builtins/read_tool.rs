//! Read tool implementation using ToolContext

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

use super::read_shared::{DEFAULT_READ_LIMIT, render_read_output};

pub struct ReadTool;

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for ReadTool {
    fn name(&self) -> &str {
        "read_tool"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Read a file or directory under the workspace. Returns XML-like output with <path>, <type>, and <content> or <entries>. Supports non-recursive pagination via offset/limit for both files and directories."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the target to read, relative to the workspace root or absolute."
                        },
                        "root": {
                            "type": "string",
                            "description": "Workspace root directory to resolve relative paths against.",
                            "default": "."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "0-based pagination offset. For files, this is a line offset. For directories, this is an entry offset.",
                            "default": 0,
                            "minimum": 0
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines (files) or entries (directories) to return. Defaults to 2000.",
                            "default": 2000,
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

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: Use offset/limit to page through large files or directories, and use search_text when you only need specific content.",
        )
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

        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_READ_LIMIT as u64) as usize;

        if limit == 0 {
            return Err(ToolError::InvalidRequest("limit must be >= 1".to_string()));
        }

        let path = context.resolve_path(path)?;
        let target = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };

        render_read_output(&target, offset, limit)
            .await
            .map_err(ToolError::ProviderError)
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
    async fn test_read_file_full() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path = create_test_file(&temp_dir, "test.txt", "line 1\nline 2\nline 3").await;

        let tool = ReadTool::new();
        let args = json!({ "path": file_path.to_str().unwrap() });

        let result = tool.call(args, &context).await.unwrap();

        assert!(result.contains("<type>file</type>"));
        assert!(result.contains("<content>"));
        assert!(result.contains("00001| line 1"));
        assert!(result.contains("00003| line 3"));
        assert!(result.contains("(End of file - total 3 lines)"));
    }

    #[tokio::test]
    async fn test_read_file_with_offset_limit() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path =
            create_test_file(&temp_dir, "test.txt", "line 1\nline 2\nline 3\nline 4").await;

        let tool = ReadTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "offset": 1,
            "limit": 2
        });

        let result = tool.call(args, &context).await.unwrap();

        assert!(result.contains("00002| line 2"));
        assert!(result.contains("00003| line 3"));
        assert!(!result.contains("00001| line 1"));
        assert!(result.contains("Use 'offset' parameter to read beyond line 3"));
    }

    #[tokio::test]
    async fn test_read_directory_non_recursive_with_pagination() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        fs::write(temp_dir.path().join("a.txt"), "a").unwrap();
        fs::write(temp_dir.path().join("b.txt"), "b").unwrap();
        fs::create_dir(temp_dir.path().join("src")).unwrap();
        fs::write(temp_dir.path().join("src").join("nested.txt"), "nested").unwrap();

        let tool = ReadTool::new();
        let args = json!({
            "path": temp_dir.path().to_str().unwrap(),
            "offset": 1,
            "limit": 2
        });

        let result = tool.call(args, &context).await.unwrap();

        assert!(result.contains("<type>directory</type>"));
        assert!(result.contains("<entries>"));
        assert!(!result.contains("nested.txt"));
        assert!(result.contains("(2 entries)"));
    }

    #[tokio::test]
    async fn test_read_directory_truncation_hint() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        fs::write(temp_dir.path().join("a.txt"), "a").unwrap();
        fs::write(temp_dir.path().join("b.txt"), "b").unwrap();
        fs::write(temp_dir.path().join("c.txt"), "c").unwrap();

        let tool = ReadTool::new();
        let args = json!({
            "path": temp_dir.path().to_str().unwrap(),
            "offset": 0,
            "limit": 2
        });

        let result = tool.call(args, &context).await.unwrap();

        assert!(result.contains("(2 entries)"));
        assert!(result.contains("(More entries available. Use a higher offset.)"));
    }
}

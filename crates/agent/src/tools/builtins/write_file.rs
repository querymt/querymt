//! Write file tool implementation using ToolContext

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

pub struct WriteFileTool;

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteFileTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Write content to a file, creating parent directories if needed."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path to write."
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write."
                        },
                        "create_dirs": {
                            "type": "boolean",
                            "description": "Create parent directories if missing.",
                            "default": true
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        if context.is_read_only() {
            return Err(ToolError::PermissionDenied(
                "Session is in read-only mode — file writes are not allowed".to_string(),
            ));
        }

        let path_arg = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("path is required".to_string()))?;

        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("content is required".to_string()))?;

        let create_dirs = args
            .get("create_dirs")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let path = context.resolve_path(path_arg)?;

        if create_dirs && let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::ProviderError(format!("mkdir failed: {}", e)))?;
        }

        tokio::fs::write(&path, content)
            .await
            .map_err(|e| ToolError::ProviderError(format!("write failed: {}", e)))?;

        let result = json!({
            "path": path.display().to_string(),
            "bytes": content.len()
        });

        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_write_file() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = WriteFileTool::new();

        let path = "test.txt";
        let content = "Hello, world!";
        let args = json!({
            "path": path,
            "content": content
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["bytes"], content.len());

        let written_content = fs::read_to_string(temp_dir.path().join(path)).unwrap();
        assert_eq!(written_content, content);
    }

    #[tokio::test]
    async fn test_write_file_create_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = WriteFileTool::new();

        let path = "subdir/test.txt";
        let content = "Hello in subdir!";
        let args = json!({
            "path": path,
            "content": content
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["bytes"], content.len());

        let written_content = fs::read_to_string(temp_dir.path().join(path)).unwrap();
        assert_eq!(written_content, content);
    }
}

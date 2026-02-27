//! Delete file tool implementation using ToolContext

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

pub struct DeleteFileTool;

impl Default for DeleteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DeleteFileTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for DeleteFileTool {
    fn name(&self) -> &str {
        "delete_file"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Delete a file or directory.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to delete."
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
        if context.is_read_only() {
            return Err(ToolError::PermissionDenied(
                "Session is in read-only mode — file deletions are not allowed".to_string(),
            ));
        }

        let path_arg = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("path is required".to_string()))?;

        let path = context.resolve_path(path_arg)?;

        if !path.exists() {
            return Err(ToolError::InvalidRequest(format!(
                "Path '{}' does not exist",
                path.display()
            )));
        }

        let result = if path.is_file() {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| ToolError::ProviderError(format!("delete failed: {}", e)))?;

            json!({
                "success": true,
                "type": "file",
                "path": path.display().to_string()
            })
        } else if path.is_dir() {
            tokio::fs::remove_dir_all(&path)
                .await
                .map_err(|e| ToolError::ProviderError(format!("delete failed: {}", e)))?;

            json!({
                "success": true,
                "type": "directory",
                "path": path.display().to_string()
            })
        } else {
            return Err(ToolError::InvalidRequest(format!(
                "Path '{}' is neither a file nor directory",
                path.display()
            )));
        };

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
    async fn test_delete_file() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = DeleteFileTool::new();

        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "test").unwrap();

        let args = json!({
            "path": "test.txt"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["type"], "file");
        assert!(!file_path.exists());
    }

    #[tokio::test]
    async fn test_delete_dir() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = DeleteFileTool::new();

        let dir_path = temp_dir.path().join("subdir");
        fs::create_dir(&dir_path).unwrap();
        fs::write(dir_path.join("test.txt"), "test").unwrap();

        let args = json!({
            "path": "subdir"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["type"], "directory");
        assert!(!dir_path.exists());
    }
}

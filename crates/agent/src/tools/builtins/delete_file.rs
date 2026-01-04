use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use querymt::error::LLMError;
use serde_json::{Value, json};
use std::path::PathBuf;

use crate::tools::registry::BuiltInTool;

pub struct DeleteFileTool;

impl DeleteFileTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait(?Send)]
impl BuiltInTool for DeleteFileTool {
    fn name(&self) -> &str {
        "delete_file"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Delete a file at the given path.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path to delete."
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, LLMError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| LLMError::InvalidRequest("path is required".to_string()))?;
        let path = PathBuf::from(path);

        tokio::fs::remove_file(&path)
            .await
            .map_err(|e| LLMError::ProviderError(format!("delete failed: {}", e)))?;

        let result = json!({
            "path": path.display().to_string(),
            "deleted": true
        });
        serde_json::to_string(&result)
            .map_err(|e| LLMError::ProviderError(format!("serialize failed: {}", e)))
    }
}

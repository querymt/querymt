use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use querymt::error::LLMError;
use serde_json::{Value, json};
use std::path::PathBuf;

use crate::tools::registry::BuiltInTool;

pub struct WriteFileTool;

impl WriteFileTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait(?Send)]
impl BuiltInTool for WriteFileTool {
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

    async fn call(&self, args: Value) -> Result<String, LLMError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| LLMError::InvalidRequest("path is required".to_string()))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| LLMError::InvalidRequest("content is required".to_string()))?;
        let create_dirs = args
            .get("create_dirs")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let path = PathBuf::from(path);
        if create_dirs {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| LLMError::ProviderError(format!("mkdir failed: {}", e)))?;
            }
        }

        tokio::fs::write(&path, content)
            .await
            .map_err(|e| LLMError::ProviderError(format!("write failed: {}", e)))?;

        let result = json!({
            "path": path.display().to_string(),
            "bytes": content.len()
        });
        serde_json::to_string(&result)
            .map_err(|e| LLMError::ProviderError(format!("serialize failed: {}", e)))
    }
}

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use querymt::error::LLMError;
use serde_json::{Value, json};
use std::path::PathBuf;

use crate::index::search::CodeSearch;
use crate::tools::registry::BuiltInTool;

pub struct SearchTextTool;

impl SearchTextTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait(?Send)]
impl BuiltInTool for SearchTextTool {
    fn name(&self) -> &str {
        "search_text"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Search text files under a root directory using a regex pattern."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for."
                        },
                        "root": {
                            "type": "string",
                            "description": "Root directory to search. Defaults to current directory."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of matches to return.",
                            "default": 50
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, LLMError> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| LLMError::InvalidRequest("pattern is required".to_string()))?;
        let root = args
            .get("root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(50) as usize;

        let mut matches = CodeSearch::search(&root, pattern)
            .map_err(|e| LLMError::ProviderError(format!("search failed: {}", e)))?;
        if matches.len() > max_results {
            matches.truncate(max_results);
        }

        serde_json::to_string(&matches)
            .map_err(|e| LLMError::ProviderError(format!("serialize failed: {}", e)))
    }
}

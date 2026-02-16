//! Web fetch tool implementation using ToolContext

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};
use std::time::Duration;

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("qmt-agent-web-fetch/0.1")
            .build()
            .unwrap();
        Self { client }
    }
}

#[async_trait]
impl ToolTrait for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Fetch a URL and return the response body as text (UTF-8 lossy)."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The URL to fetch."
                        },
                        "max_bytes": {
                            "type": "integer",
                            "description": "Maximum response bytes to return.",
                            "default": 65536
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "Request timeout in milliseconds.",
                            "default": 10000
                        }
                    },
                    "required": ["url"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[]
    }

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: The response was truncated. If overflow storage is enabled, \
             use search_text or read_tool on the saved overflow file to find specific content.",
        )
    }

    async fn call(&self, args: Value, _context: &dyn ToolContext) -> Result<String, ToolError> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("url is required".to_string()))?;
        let max_bytes = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(65_536) as usize;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(10_000);

        let response = self
            .client
            .get(url)
            .timeout(Duration::from_millis(timeout_ms))
            .send()
            .await
            .map_err(|e| ToolError::ProviderError(format!("request failed: {}", e)))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ToolError::ProviderError(format!("read failed: {}", e)))?;

        let mut body = String::from_utf8_lossy(&bytes).to_string();
        let mut truncated = false;
        if body.len() > max_bytes {
            body.truncate(max_bytes);
            truncated = true;
        }

        let result = json!({
            "status": status.as_u16(),
            "truncated": truncated,
            "body": body
        });
        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

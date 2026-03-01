//! Context-safe web fetch tool.
//!
//! Like `web_fetch`, but indexes the full response for retrieval and returns
//! only a bounded preview. Useful for fetching large web pages, API responses,
//! or documentation without flooding model context.

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};
use std::time::Duration;

use crate::tools::{Tool as ToolTrait, ToolContext, ToolError};

/// Default maximum preview bytes returned to model context.
const DEFAULT_PREVIEW_BYTES: usize = 4096;

/// Default request timeout in milliseconds.
const DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// Maximum response body bytes to read from the network.
const MAX_FETCH_BYTES: usize = 5 * 1024 * 1024; // 5 MB

pub struct ContextFetchTool {
    client: reqwest::Client,
}

impl Default for ContextFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("querymt-agent-context-fetch/0.1")
            .build()
            .unwrap();
        Self { client }
    }
}

#[async_trait]
impl ToolTrait for ContextFetchTool {
    fn name(&self) -> &str {
        "context_fetch"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: concat!(
                    "Fetch a URL, index the full response for later retrieval, ",
                    "and return a bounded preview. Use this instead of `web_fetch` ",
                    "when you expect large responses (documentation pages, API dumps, logs). ",
                    "The full response is searchable via `context_search`."
                )
                .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The URL to fetch."
                        },
                        "preview_bytes": {
                            "type": "integer",
                            "description": "Maximum bytes in the returned preview (default: 4096). Full response is always indexed.",
                            "default": 4096
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "Request timeout in milliseconds (default: 15000).",
                            "default": 15000
                        },
                        "source_label": {
                            "type": "string",
                            "description": "Optional label for the indexed source. Defaults to the URL."
                        }
                    },
                    "required": ["url"]
                }),
            },
        }
    }

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: The full response is indexed. Use `context_search` \
             to find specific content without loading the entire response.",
        )
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("url is required".to_string()))?;

        let preview_bytes = args
            .get("preview_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_PREVIEW_BYTES as u64) as usize;

        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);

        let source_label = args
            .get("source_label")
            .and_then(Value::as_str)
            .unwrap_or(url);

        // Fetch the URL
        let response = self
            .client
            .get(url)
            .timeout(Duration::from_millis(timeout_ms))
            .send()
            .await
            .map_err(|e| ToolError::ProviderError(format!("request failed: {}", e)))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        let bytes = response
            .bytes()
            .await
            .map_err(|e| ToolError::ProviderError(format!("read failed: {}", e)))?;

        // Limit to MAX_FETCH_BYTES
        let body_bytes = if bytes.len() > MAX_FETCH_BYTES {
            &bytes[..MAX_FETCH_BYTES]
        } else {
            &bytes[..]
        };
        let body = String::from_utf8_lossy(body_bytes).to_string();

        let total_bytes = body.len();
        let total_lines = body.lines().count();

        // Index the full response (best-effort)
        let indexed = match context
            .index_context_content(source_label, body.clone())
            .await
        {
            Ok(_) => true,
            Err(e) => {
                log::debug!("context_fetch: failed to index response: {}", e);
                false
            }
        };

        // Build bounded preview
        let preview = if body.len() <= preview_bytes {
            body.clone()
        } else {
            let mut preview = body.chars().take(preview_bytes).collect::<String>();
            preview.push_str(&format!(
                "\n\n... ({} bytes omitted) ...",
                total_bytes - preview_bytes
            ));
            preview
        };

        let result = json!({
            "status": status,
            "content_type": content_type,
            "preview": preview,
            "total_bytes": total_bytes,
            "total_lines": total_lines,
            "indexed": indexed,
            "source_label": source_label,
            "hint": if total_bytes > preview_bytes {
                "Response was large. Use `context_search` to find specific content."
            } else {
                "Full response shown."
            }
        });

        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_preview_truncation() {
        let long_body = "a".repeat(10000);
        let preview_bytes = 100;
        let preview = if long_body.len() <= preview_bytes {
            long_body.clone()
        } else {
            let mut preview = long_body.chars().take(preview_bytes).collect::<String>();
            preview.push_str(&format!(
                "\n\n... ({} bytes omitted) ...",
                long_body.len() - preview_bytes
            ));
            preview
        };

        assert!(preview.len() < long_body.len());
        assert!(preview.contains("omitted"));
    }
}

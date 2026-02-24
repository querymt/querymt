//! Browse tool implementation that fetches web content.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use querymt::chat::{FunctionTool, Tool};
use regex::Regex;
use reqwest::Url;
use serde_json::{Value, json};

use crate::tools::{Tool as ToolTrait, ToolContext, ToolError};

const MAX_REDIRECTS: usize = 5;
const MAX_BYTES: usize = 20 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
const SUPPORTED_SCHEMES: [&str; 2] = ["http", "https"];

pub struct BrowseTool {
    client: reqwest::Client,
}

impl Default for BrowseTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowseTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent("querymt-agent-browse/0.1") // NOTE: Proper user agent?
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build browse HTTP client");
        Self { client }
    }
}

fn strip_styles_and_scripts(html: &str) -> String {
    let style_re = Regex::new(r"(?is)<style[^>]*>.*?</style>").expect("valid regex");
    let script_re = Regex::new(r"(?is)<script[^>]*>.*?</script>").expect("valid regex");
    let without_styles = style_re.replace_all(html, "");
    let cleaned = script_re.replace_all(&without_styles, "");
    cleaned.to_string()
}

fn parse_content_type_header(response: &reqwest::Response) -> Result<String, ToolError> {
    let header = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            ToolError::ProviderError("missing or invalid Content-Type header".to_string())
        })?;

    let content_type = header
        .split(';')
        .next()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .ok_or_else(|| {
            ToolError::ProviderError("missing or invalid Content-Type header".to_string())
        })?;

    Ok(content_type)
}

fn is_html_content_type(content_type: &str) -> bool {
    content_type == "text/html" || content_type == "application/xhtml+xml"
}

fn is_allowed_text_content_type(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type == "application/json"
        || content_type.ends_with("+json")
        || content_type == "application/xml"
        || content_type.ends_with("+xml")
        || content_type == "application/javascript"
        || content_type == "application/yaml"
        || content_type == "application/x-yaml"
        || content_type == "application/toml"
        || content_type == "application/x-www-form-urlencoded"
}

fn validate_scheme(url: &Url) -> Result<(), ToolError> {
    if SUPPORTED_SCHEMES.contains(&url.scheme()) {
        Ok(())
    } else {
        Err(ToolError::InvalidRequest(format!(
            "unsupported url scheme: {}",
            url.scheme()
        )))
    }
}

async fn fetch_with_redirects(
    client: &reqwest::Client,
    url: &str,
) -> Result<reqwest::Response, ToolError> {
    let mut current =
        Url::parse(url).map_err(|e| ToolError::InvalidRequest(format!("invalid url: {e}")))?;

    validate_scheme(&current)?;

    for hop in 0..=MAX_REDIRECTS {
        let resp = client.get(current.clone()).send().await.map_err(|e| {
            if e.is_timeout() {
                ToolError::ProviderError("request timed out after 20s".to_string())
            } else {
                ToolError::ProviderError(format!("request failed: {e}"))
            }
        })?;

        if resp.status().is_redirection() {
            let Some(loc) = resp.headers().get(reqwest::header::LOCATION) else {
                return Err(ToolError::ProviderError(
                    "redirect response missing Location header".to_string(),
                ));
            };
            let loc = loc.to_str().map_err(|e| {
                ToolError::ProviderError(format!("invalid redirect Location header: {e}"))
            })?;

            if hop == MAX_REDIRECTS {
                return Err(ToolError::ProviderError(format!(
                    "too many redirects (max {MAX_REDIRECTS})"
                )));
            }

            current = current.join(loc).map_err(|e| {
                ToolError::ProviderError(format!(
                    "failed to resolve redirect location '{loc}': {e}"
                ))
            })?;
            validate_scheme(&current)?;
            continue;
        }

        if !resp.status().is_success() {
            return Err(ToolError::ProviderError(format!(
                "http error {}",
                resp.status().as_u16()
            )));
        }

        return Ok(resp);
    }

    Err(ToolError::ProviderError(
        "unreachable redirect state".to_string(),
    ))
}

fn decode_body(bytes: &[u8]) -> String {
    // TODO: Add proper charset handling if non-UTF-8 responses are needed.
    String::from_utf8_lossy(bytes).into_owned()
}

#[async_trait]
impl ToolTrait for BrowseTool {
    fn name(&self) -> &str {
        "browse"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Fetch a URL. HTML is converted to Markdown; other allowed text content is returned as plain text."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The URL to browse (http/https)."
                        }
                    },
                    "required": ["url"]
                }),
            },
        }
    }

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: The response was truncated. If overflow storage is enabled, use search_text or read_tool on the saved overflow file.",
        )
    }

    async fn call(&self, args: Value, _context: &dyn ToolContext) -> Result<String, ToolError> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("url is required".to_string()))?;

        let response = fetch_with_redirects(&self.client, url).await?;

        let content_type = parse_content_type_header(&response)?;

        if !is_html_content_type(&content_type) && !is_allowed_text_content_type(&content_type) {
            return Err(ToolError::ProviderError(format!(
                "unsupported content-type: {content_type}"
            )));
        }

        let mut buf: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                ToolError::ProviderError(format!("failed to read response body: {e}"))
            })?;
            if buf.len() + chunk.len() > MAX_BYTES {
                return Err(ToolError::ProviderError(format!(
                    "response exceeded max bytes ({MAX_BYTES})"
                )));
            }
            buf.extend_from_slice(&chunk);
        }

        let body = decode_body(&buf);

        if is_html_content_type(&content_type) {
            let cleaned = strip_styles_and_scripts(&body);
            Ok(fast_html2md::parse_html(&cleaned, true))
        } else {
            Ok(body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_style_and_script_tags() {
        let html = "<html><head><style>body{color:red}</style><script>alert(1)</script></head><body><h1>Hi</h1></body></html>";
        let cleaned = strip_styles_and_scripts(html);
        assert!(!cleaned.contains("style"));
        assert!(!cleaned.contains("script"));
        assert!(cleaned.contains("<h1>Hi</h1>"));
    }

    #[test]
    fn recognizes_html_types() {
        assert!(is_html_content_type("text/html"));
        assert!(is_html_content_type("application/xhtml+xml"));
        assert!(!is_html_content_type("text/plain"));
    }

    #[test]
    fn recognizes_allowed_text_types() {
        assert!(is_allowed_text_content_type("text/plain"));
        assert!(is_allowed_text_content_type("application/json"));
        assert!(is_allowed_text_content_type("application/ld+json"));
        assert!(is_allowed_text_content_type("application/rss+xml"));
        assert!(is_allowed_text_content_type("application/toml"));
        assert!(!is_allowed_text_content_type("application/octet-stream"));
    }
}

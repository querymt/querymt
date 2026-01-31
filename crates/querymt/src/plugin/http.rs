use crate::{error::LLMError, HTTPLLMProvider};
use http::{Request, Response};
use serde_json::Value;

pub trait HTTPLLMProviderFactory: Send + Sync {
    fn name(&self) -> &str;

    fn api_key_name(&self) -> Option<String> {
        None
    }

    /// Schema for plugin config
    fn config_schema(&self) -> Value;

    /// Build the HTTP request that lists models.
    fn list_models_request(&self, cfg: &Value) -> Result<Request<Vec<u8>>, LLMError>;

    /// Turn the raw HTTP response into a Vec<String>.
    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError>;

    /// Given a chosen model name, build a sync `HttpLLMProvider`
    // FIXME: refactor to follow rust standards
    #[allow(clippy::wrong_self_convention)]
    fn from_config(&self, cfg: &Value) -> Result<Box<dyn HTTPLLMProvider>, LLMError>;
}

#[allow(improper_ctypes_definitions)]
pub type HTTPFactoryCtor = unsafe extern "C" fn() -> *mut dyn HTTPLLMProviderFactory;

#[macro_export]
macro_rules! handle_http_error {
    ($resp:expr) => {{
        if !$resp.status().is_success() {
            let status = $resp.status();
            let status_code = status.as_u16();

            // Extract retry-after header for rate limit errors
            let retry_after_secs = if status_code == 429 {
                $resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .or_else(|| {
                        // Fallback: try parsing x-ratelimit-reset-requests as duration
                        $resp
                            .headers()
                            .get("x-ratelimit-reset-requests")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| {
                                // Parse formats like "6m0s" or "1s"
                                if s.ends_with('s') {
                                    let num_part = s.trim_end_matches('s');
                                    if let Some(m_pos) = num_part.find('m') {
                                        // Format: "6m0s" -> extract minutes
                                        num_part[..m_pos].parse::<u64>().ok().map(|m| m * 60)
                                    } else {
                                        // Format: "1s"
                                        num_part.parse::<u64>().ok()
                                    }
                                } else {
                                    None
                                }
                            })
                    })
            } else {
                None
            };

            let error_text: String = String::from_utf8($resp.into_body())?;

            // Try to parse JSON and extract error.message for a clean message
            let clean_message =
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&error_text) {
                    json.pointer("/error/message")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("API returned error status: {}", status))
                } else {
                    format!("API returned error status: {}", status)
                };

            // Route to appropriate error variant based on status code
            return Err(match status_code {
                401 | 403 => LLMError::AuthError(clean_message),
                429 => LLMError::RateLimited {
                    message: clean_message,
                    retry_after_secs,
                },
                400 => LLMError::InvalidRequest(clean_message),
                500 | 529 => LLMError::ProviderError(format!("Server error: {}", clean_message)),
                _ => LLMError::ProviderError(clean_message),
            });
        }
    }};
}

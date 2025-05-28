use std::fmt;

/// Error types that can occur when interacting with LLM providers.
#[derive(Debug)]
pub enum LLMError {
    /// HTTP request/response errors
    HttpError(String),
    /// Authentication and authorization errors
    AuthError(String),
    /// Invalid request parameters or format
    InvalidRequest(String),
    /// Errors returned by the LLM provider
    ProviderError(String),
    /// API response parsing or format error
    ResponseFormatError {
        message: String,
        raw_response: String,
    },
    /// JSON serialization/deserialization errors
    JsonError(String),
    /// Tool configuration error
    ToolConfigError(String),
    /// Plugin error
    PluginError(String),
}

impl fmt::Display for LLMError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LLMError::HttpError(e) => write!(f, "HTTP Error: {}", e),
            LLMError::AuthError(e) => write!(f, "Auth Error: {}", e),
            LLMError::InvalidRequest(e) => write!(f, "Invalid Request: {}", e),
            LLMError::ProviderError(e) => write!(f, "Provider Error: {}", e),
            LLMError::ResponseFormatError {
                message,
                raw_response,
            } => {
                write!(
                    f,
                    "Response Format Error: {}. Raw response: {}",
                    message, raw_response
                )
            }
            LLMError::JsonError(e) => write!(f, "JSON Parse Error: {}", e),
            LLMError::ToolConfigError(e) => write!(f, "Tool Configuration Error: {}", e),
            LLMError::PluginError(e) => {
                write!(f, "Plugin Error: {}", e)
            }
        }
    }
}

impl std::error::Error for LLMError {}

/// Converts reqwest HTTP errors into LlmErrors
#[cfg(feature = "reqwest-client")]
impl From<reqwest::Error> for LLMError {
    fn from(err: reqwest::Error) -> Self {
        LLMError::HttpError(err.to_string())
    }
}

impl From<http::Error> for LLMError {
    fn from(err: http::Error) -> Self {
        LLMError::HttpError(err.to_string())
    }
}

impl From<serde_json::Error> for LLMError {
    fn from(err: serde_json::Error) -> Self {
        LLMError::JsonError(format!(
            "{} at line {} column {}",
            err,
            err.line(),
            err.column()
        ))
    }
}

impl From<url::ParseError> for LLMError {
    fn from(err: url::ParseError) -> Self {
        LLMError::InvalidRequest(format!("Error parsing provided url: {}", err))
    }
}

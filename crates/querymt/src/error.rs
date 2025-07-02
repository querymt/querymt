use std::string::FromUtf8Error;

use thiserror::Error;

/// Error types that can occur when interacting with LLM providers.
#[derive(Error, Debug)]
pub enum LLMError {
    /// A wrapper for a generic, user-created error message.
    #[error("Generic Error: {0}")]
    GenericError(String),

    /// A wrapper for provider-specific error messages.
    #[error("LLM Provider Error: {0}")]
    ProviderError(String),

    /// A wrapper for authentication/authorization errors.
    #[error("Auth Error: {0}")]
    AuthError(String),

    /// A wrapper for tool configuration errors.
    #[error("Tool Configuration Error: {0}")]
    ToolConfigError(String),

    /// A wrapper for plugin-related errors.
    #[error("Plugin Error: {0}")]
    PluginError(String),

    /// Errors related to malformed requests.
    #[error("Invalid Request: {0}")]
    InvalidRequest(String),

    /// Errors related to malformed response bodies.
    #[error("Response Format Error: {message}. Raw response: '{raw_response}'")]
    ResponseFormatError {
        message: String,
        raw_response: String,
    },

    #[error("HTTP Error: {0}")]
    HttpError(String),

    /// Handles JSON serialization and deserialization errors.
    #[error("JSON Error")]
    JsonError(#[from] serde_json::Error),

    /// Handles errors from parsing URLs.
    #[error("Invalid URL")]
    InvalidUrl(#[from] url::ParseError),

    /// Handles standard I/O errors.
    #[error("I/O Error")]
    IoError(#[from] std::io::Error),
}

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

impl From<FromUtf8Error> for LLMError {
    fn from(value: FromUtf8Error) -> Self {
        LLMError::GenericError(format!("Error decoding string: {:#}", value))
    }
}

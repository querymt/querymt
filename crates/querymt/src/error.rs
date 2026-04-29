use serde::{Deserialize, Serialize};
use std::string::FromUtf8Error;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportErrorKind {
    ConnectionRefused,
    ConnectionReset,
    Timeout,
    ConnectionClosed,
    Dns,
    Tls,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LLMErrorPayload {
    GenericError {
        message: String,
    },
    ProviderError {
        message: String,
    },
    AuthError {
        message: String,
    },
    ToolConfigError {
        message: String,
    },
    PluginError {
        message: String,
    },
    InvalidRequest {
        message: String,
    },
    ResponseFormatError {
        message: String,
        raw_response: String,
    },
    RateLimited {
        message: String,
        retry_after_secs: Option<u64>,
    },
    HttpStatus {
        status_code: u16,
        message: String,
        retry_after_secs: Option<u64>,
    },
    HttpError {
        message: String,
    },
    Transport {
        kind: TransportErrorKind,
        message: String,
    },
    Cancelled,
    RemoteStreamDisconnected {
        message: String,
    },
    RemoteStreamReconnected {
        message: String,
    },
    NotImplemented {
        message: String,
    },
    JsonError {
        message: String,
    },
    InvalidUrl {
        message: String,
    },
    IoError {
        message: String,
    },
}

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

    /// Rate limit error with optional retry-after information
    #[error("Rate limited: {message}")]
    RateLimited {
        message: String,
        /// Seconds to wait before retrying (from retry-after header)
        retry_after_secs: Option<u64>,
    },

    #[error("HTTP {status_code}: {message}")]
    HttpStatus {
        status_code: u16,
        message: String,
        retry_after_secs: Option<u64>,
    },

    #[error("HTTP Error: {0}")]
    HttpError(String),

    #[error("{message}")]
    Transport {
        kind: TransportErrorKind,
        message: String,
    },

    /// Request was cancelled by the caller (e.g. timeout, user interrupt).
    #[error("Cancelled")]
    Cancelled,

    /// Remote stream transport disconnected but may reconnect.
    #[error("Remote stream disconnected: {message}")]
    RemoteStreamDisconnected { message: String },

    /// Remote stream transport reconnected and delivery resumed.
    #[error("Remote stream reconnected: {message}")]
    RemoteStreamReconnected { message: String },

    /// Feature or functionality not implemented by this provider.
    #[error("Not Implemented: {0}")]
    NotImplemented(String),

    /// Handles JSON serialization and deserialization errors.
    #[error("JSON Error: {0}")]
    JsonError(#[from] serde_json::Error),

    /// Handles errors from parsing URLs.
    #[error("Invalid URL")]
    InvalidUrl(#[from] url::ParseError),

    /// Handles standard I/O errors.
    #[error("I/O Error")]
    IoError(#[from] std::io::Error),
}

impl LLMError {
    pub fn to_payload(&self) -> LLMErrorPayload {
        match self {
            Self::GenericError(message) => LLMErrorPayload::GenericError {
                message: message.clone(),
            },
            Self::ProviderError(message) => LLMErrorPayload::ProviderError {
                message: message.clone(),
            },
            Self::AuthError(message) => LLMErrorPayload::AuthError {
                message: message.clone(),
            },
            Self::ToolConfigError(message) => LLMErrorPayload::ToolConfigError {
                message: message.clone(),
            },
            Self::PluginError(message) => LLMErrorPayload::PluginError {
                message: message.clone(),
            },
            Self::InvalidRequest(message) => LLMErrorPayload::InvalidRequest {
                message: message.clone(),
            },
            Self::ResponseFormatError {
                message,
                raw_response,
            } => LLMErrorPayload::ResponseFormatError {
                message: message.clone(),
                raw_response: raw_response.clone(),
            },
            Self::RateLimited {
                message,
                retry_after_secs,
            } => LLMErrorPayload::RateLimited {
                message: message.clone(),
                retry_after_secs: *retry_after_secs,
            },
            Self::HttpStatus {
                status_code,
                message,
                retry_after_secs,
            } => LLMErrorPayload::HttpStatus {
                status_code: *status_code,
                message: message.clone(),
                retry_after_secs: *retry_after_secs,
            },
            Self::HttpError(message) => LLMErrorPayload::HttpError {
                message: message.clone(),
            },
            Self::Transport { kind, message } => LLMErrorPayload::Transport {
                kind: *kind,
                message: message.clone(),
            },
            Self::Cancelled => LLMErrorPayload::Cancelled,
            Self::RemoteStreamDisconnected { message } => {
                LLMErrorPayload::RemoteStreamDisconnected {
                    message: message.clone(),
                }
            }
            Self::RemoteStreamReconnected { message } => LLMErrorPayload::RemoteStreamReconnected {
                message: message.clone(),
            },
            Self::NotImplemented(message) => LLMErrorPayload::NotImplemented {
                message: message.clone(),
            },
            Self::JsonError(err) => LLMErrorPayload::JsonError {
                message: err.to_string(),
            },
            Self::InvalidUrl(err) => LLMErrorPayload::InvalidUrl {
                message: err.to_string(),
            },
            Self::IoError(err) => LLMErrorPayload::IoError {
                message: err.to_string(),
            },
        }
    }

    pub fn from_payload(payload: LLMErrorPayload) -> Self {
        match payload {
            LLMErrorPayload::GenericError { message } => Self::GenericError(message),
            LLMErrorPayload::ProviderError { message } => Self::ProviderError(message),
            LLMErrorPayload::AuthError { message } => Self::AuthError(message),
            LLMErrorPayload::ToolConfigError { message } => Self::ToolConfigError(message),
            LLMErrorPayload::PluginError { message } => Self::PluginError(message),
            LLMErrorPayload::InvalidRequest { message } => Self::InvalidRequest(message),
            LLMErrorPayload::ResponseFormatError {
                message,
                raw_response,
            } => Self::ResponseFormatError {
                message,
                raw_response,
            },
            LLMErrorPayload::RateLimited {
                message,
                retry_after_secs,
            } => Self::RateLimited {
                message,
                retry_after_secs,
            },
            LLMErrorPayload::HttpStatus {
                status_code,
                message,
                retry_after_secs,
            } => Self::HttpStatus {
                status_code,
                message,
                retry_after_secs,
            },
            LLMErrorPayload::HttpError { message } => Self::HttpError(message),
            LLMErrorPayload::Transport { kind, message } => Self::Transport { kind, message },
            LLMErrorPayload::Cancelled => Self::Cancelled,
            LLMErrorPayload::RemoteStreamDisconnected { message } => {
                Self::RemoteStreamDisconnected { message }
            }
            LLMErrorPayload::RemoteStreamReconnected { message } => {
                Self::RemoteStreamReconnected { message }
            }
            LLMErrorPayload::NotImplemented { message } => Self::NotImplemented(message),
            LLMErrorPayload::JsonError { message } => Self::PluginError(message),
            LLMErrorPayload::InvalidUrl { message } => Self::HttpError(message),
            LLMErrorPayload::IoError { message } => Self::Transport {
                kind: TransportErrorKind::Other,
                message,
            },
        }
    }

    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::RateLimited {
                retry_after_secs, ..
            }
            | Self::HttpStatus {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }

    pub fn is_retryable_setup_failure(&self) -> bool {
        self.is_retryable()
    }

    /// Whether this error is worth retrying (transient infrastructure error).
    ///
    /// Strategy: most transport/infrastructure errors are transient and succeed
    /// on a second attempt. Only semantic/auth/validation errors are not retryable.
    /// The mesh-specific `RemoteStreamDisconnected`/`RemoteStreamReconnected` events
    /// are excluded — they have their own handling in the streaming loop.
    pub fn is_retryable(&self) -> bool {
        match self {
            // Always retry: transient infrastructure
            Self::Transport { .. } => true,
            Self::HttpError(_) => true, // unclassified HTTP transport error — could be transient
            Self::RateLimited { .. } => true,
            Self::HttpStatus { status_code, .. } => {
                matches!(status_code, 429 | 500..=599)
            }
            Self::PluginError(_) => true, // may be a transient WASM/HTTP issue
            Self::IoError { .. } => true,

            // Never retry: semantic errors
            Self::AuthError(_) => false,
            Self::InvalidRequest(_) => false,
            Self::ProviderError(_) => false,
            Self::ToolConfigError(_) => false,
            Self::ResponseFormatError { .. } => false,
            Self::GenericError(_) => false,
            Self::Cancelled => false,
            Self::JsonError { .. } => false,
            Self::InvalidUrl { .. } => false,
            Self::NotImplemented(_) => false,

            // Mesh transport events — handled by the existing continue logic
            Self::RemoteStreamDisconnected { .. } => false,
            Self::RemoteStreamReconnected { .. } => false,
        }
    }
}

pub fn parse_retry_after(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| {
            headers
                .get("x-ratelimit-reset-requests")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| {
                    if s.ends_with('s') {
                        let num_part = s.trim_end_matches('s');
                        if let Some(m_pos) = num_part.find('m') {
                            num_part[..m_pos].parse::<u64>().ok().map(|m| m * 60)
                        } else {
                            num_part.parse::<u64>().ok()
                        }
                    } else {
                        None
                    }
                })
        })
}

pub fn classify_http_status(status_code: u16, headers: &http::HeaderMap, body: &[u8]) -> LLMError {
    if status_code == 499 {
        return LLMError::Cancelled;
    }

    let retry_after_secs = parse_retry_after(headers);
    let clean_message = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|json| {
            json.pointer("/error/message")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            format!("{}", String::from_utf8_lossy(body))
                .trim()
                .to_string()
        })
        .trim()
        .to_string();
    let message = if clean_message.is_empty() {
        format!("HTTP {}", status_code)
    } else {
        clean_message
    };

    match status_code {
        401 | 403 => LLMError::AuthError(message),
        429 => LLMError::RateLimited {
            message,
            retry_after_secs,
        },
        400 => LLMError::InvalidRequest(message),
        500..=599 => LLMError::HttpStatus {
            status_code,
            message,
            retry_after_secs,
        },
        _ => LLMError::ProviderError(message),
    }
}

pub fn transport_error(kind: TransportErrorKind, message: impl Into<String>) -> LLMError {
    LLMError::Transport {
        kind,
        message: message.into(),
    }
}

#[cfg(feature = "http-client")]
impl From<reqwest::Error> for LLMError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            return transport_error(TransportErrorKind::Timeout, err.to_string());
        }
        if err.is_connect() {
            return transport_error(TransportErrorKind::ConnectionRefused, err.to_string());
        }
        if err.is_body() {
            return transport_error(TransportErrorKind::Other, err.to_string());
        }
        if err.is_decode() {
            return transport_error(TransportErrorKind::Other, err.to_string());
        }
        if let Some(status) = err.status() {
            return LLMError::HttpStatus {
                status_code: status.as_u16(),
                message: err.to_string(),
                retry_after_secs: None,
            };
        }
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

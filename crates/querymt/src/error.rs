use serde::{Deserialize, Serialize};
use std::string::FromUtf8Error;
use std::time::{Duration, SystemTime};
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    ServerOverloaded,
    RateLimited,
    QuotaExceeded,
    ContextWindowExceeded,
    Authentication,
    InvalidRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderErrorContext {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ProviderErrorKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
    pub transient: bool,
}

impl ProviderErrorContext {
    pub fn builder(provider: impl Into<String>) -> ProviderErrorContextBuilder {
        ProviderErrorContextBuilder {
            provider: provider.into(),
            kind: None,
            code: None,
            error_type: None,
            request_id: None,
            retry_after_secs: None,
            transient: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderErrorContextBuilder {
    provider: String,
    kind: Option<ProviderErrorKind>,
    code: Option<String>,
    error_type: Option<String>,
    request_id: Option<String>,
    retry_after_secs: Option<u64>,
    transient: bool,
}

impl ProviderErrorContextBuilder {
    pub fn kind(mut self, kind: ProviderErrorKind) -> Self {
        self.kind = Some(kind);
        self
    }

    pub fn code(mut self, code: Option<String>) -> Self {
        self.code = code;
        self
    }

    pub fn error_type(mut self, error_type: Option<String>) -> Self {
        self.error_type = error_type;
        self
    }

    pub fn request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn retry_after_secs(mut self, retry_after_secs: Option<u64>) -> Self {
        self.retry_after_secs = retry_after_secs;
        self
    }

    pub fn transient(mut self, transient: bool) -> Self {
        self.transient = transient;
        self
    }

    pub fn build(self) -> ProviderErrorContext {
        ProviderErrorContext {
            provider: self.provider,
            kind: self.kind,
            code: self.code,
            error_type: self.error_type,
            request_id: self.request_id,
            retry_after_secs: self.retry_after_secs,
            transient: self.transient,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LLMErrorPayload {
    GenericError {
        message: String,
    },
    ProviderError {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<ProviderErrorContext>,
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

impl LLMErrorPayload {
    /// Whether this serialized error represents a transient failure worth retrying.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::ProviderError {
                context: Some(context),
                ..
            } => match context.kind {
                Some(ProviderErrorKind::ServerOverloaded | ProviderErrorKind::RateLimited) => true,
                Some(
                    ProviderErrorKind::QuotaExceeded
                    | ProviderErrorKind::ContextWindowExceeded
                    | ProviderErrorKind::Authentication
                    | ProviderErrorKind::InvalidRequest,
                ) => false,
                None => context.transient,
            },
            Self::RateLimited { .. }
            | Self::HttpError { .. }
            | Self::Transport { .. }
            | Self::PluginError { .. }
            | Self::IoError { .. } => true,
            Self::HttpStatus { status_code, .. } => matches!(status_code, 429 | 500..=599),
            Self::GenericError { .. }
            | Self::ProviderError { context: None, .. }
            | Self::AuthError { .. }
            | Self::ToolConfigError { .. }
            | Self::InvalidRequest { .. }
            | Self::ResponseFormatError { .. }
            | Self::Cancelled
            | Self::RemoteStreamDisconnected { .. }
            | Self::RemoteStreamReconnected { .. }
            | Self::NotImplemented { .. }
            | Self::JsonError { .. }
            | Self::InvalidUrl { .. } => false,
        }
    }
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

    /// A provider response error with structured classification metadata.
    ///
    /// Prefer semantic variants ([`Self::ServerOverloaded`], [`Self::RateLimited`],
    /// etc.) when the provider mapper knows the failure kind. This catch-all keeps
    /// opaque vendor diagnostics (`code` / `request_id`) for unknown failures.
    #[error("LLM Provider Error: {message}")]
    ProviderResponseError {
        message: String,
        context: Box<ProviderErrorContext>,
    },

    /// Provider capacity / overload. Retryable in QueryMT (product choice).
    #[error("Server overloaded: {message}")]
    ServerOverloaded {
        message: String,
        context: Box<ProviderErrorContext>,
    },

    /// Plan/billing quota exhausted — not a short-term rate limit.
    #[error("Quota exceeded: {message}")]
    QuotaExceeded {
        message: String,
        context: Box<ProviderErrorContext>,
    },

    /// Prompt/context exceeds the model window.
    #[error("Context window exceeded: {message}")]
    ContextWindowExceeded {
        message: String,
        context: Box<ProviderErrorContext>,
    },

    /// Provider rate limit with diagnostics retained for ACP/remote consumers.
    #[error("Rate limited: {message}")]
    ProviderRateLimited {
        message: String,
        context: Box<ProviderErrorContext>,
    },

    /// Provider authentication failure with diagnostics retained.
    #[error("Auth Error: {message}")]
    ProviderAuthError {
        message: String,
        context: Box<ProviderErrorContext>,
    },

    /// Provider request rejection with diagnostics retained.
    #[error("Invalid Request: {message}")]
    ProviderInvalidRequest {
        message: String,
        context: Box<ProviderErrorContext>,
    },

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
                context: None,
            },
            Self::ProviderResponseError { message, context } => LLMErrorPayload::ProviderError {
                message: message.clone(),
                context: Some((**context).clone()),
            },
            Self::ServerOverloaded { message, context }
            | Self::ProviderRateLimited { message, context }
            | Self::QuotaExceeded { message, context }
            | Self::ContextWindowExceeded { message, context }
            | Self::ProviderAuthError { message, context }
            | Self::ProviderInvalidRequest { message, context } => LLMErrorPayload::ProviderError {
                message: message.clone(),
                context: Some((**context).clone()),
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
            LLMErrorPayload::ProviderError { message, context } => match context {
                Some(context) => match context.kind {
                    Some(ProviderErrorKind::ServerOverloaded) => Self::ServerOverloaded {
                        message,
                        context: Box::new(context),
                    },
                    Some(ProviderErrorKind::RateLimited) => Self::ProviderRateLimited {
                        message,
                        context: Box::new(context),
                    },
                    Some(ProviderErrorKind::QuotaExceeded) => Self::QuotaExceeded {
                        message,
                        context: Box::new(context),
                    },
                    Some(ProviderErrorKind::ContextWindowExceeded) => Self::ContextWindowExceeded {
                        message,
                        context: Box::new(context),
                    },
                    Some(ProviderErrorKind::Authentication) => Self::ProviderAuthError {
                        message,
                        context: Box::new(context),
                    },
                    Some(ProviderErrorKind::InvalidRequest) => Self::ProviderInvalidRequest {
                        message,
                        context: Box::new(context),
                    },
                    None => Self::ProviderResponseError {
                        message,
                        context: Box::new(context),
                    },
                },
                None => Self::ProviderError(message),
            },
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
            Self::ProviderResponseError { context, .. }
            | Self::ServerOverloaded { context, .. }
            | Self::ProviderRateLimited { context, .. }
            | Self::QuotaExceeded { context, .. }
            | Self::ContextWindowExceeded { context, .. }
            | Self::ProviderAuthError { context, .. }
            | Self::ProviderInvalidRequest { context, .. } => context.retry_after_secs,
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
    ///
    /// Retry policy is keyed off **unified** error kinds. Vendor code tables live
    /// in providers; they map wire errors into these variants before core sees them.
    pub fn is_retryable(&self) -> bool {
        match self {
            // Always retry: transient infrastructure / capacity
            Self::Transport { .. } => true,
            Self::HttpError(_) => true, // unclassified HTTP transport error — could be transient
            Self::RateLimited { .. } | Self::ProviderRateLimited { .. } => true,
            // QueryMT product choice: retry overload (upstream Codex marks this non-retryable).
            Self::ServerOverloaded { .. } => true,
            Self::HttpStatus { status_code, .. } => {
                matches!(status_code, 429 | 500..=599)
            }
            Self::PluginError(_) => true, // may be a transient WASM/HTTP issue
            Self::IoError { .. } => true,

            // Never retry: semantic errors
            Self::AuthError(_) | Self::ProviderAuthError { .. } => false,
            Self::InvalidRequest(_) | Self::ProviderInvalidRequest { .. } => false,
            Self::ProviderError(_) => false,
            // Catch-all: providers set `context.transient` when they intentionally
            // emit an unclassified-but-retryable failure.
            Self::ProviderResponseError { context, .. } => context.transient,
            Self::QuotaExceeded { .. } => false,
            Self::ContextWindowExceeded { .. } => false,
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

/// Convert a [`Duration`] to whole seconds, rounding sub-second values up to 1.
fn duration_to_secs(d: Duration) -> u64 {
    let secs = d.as_secs();
    if secs > 0 {
        secs
    } else if !d.is_zero() {
        1
    } else {
        0
    }
}

/// Parse an arbitrary retry-after value string into whole seconds.
///
/// Tries in order:
/// 1. Plain integer seconds (RFC 7231 delay-seconds)
/// 2. Duration strings via `humantime` (`"30s"`, `"1m30s"`, `"500ms"`, `"1.5s"`)
fn parse_retry_after_value(s: &str) -> Option<u64> {
    let s = s.trim();
    // Fast path: plain integer seconds
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs);
    }
    // Duration strings: "30s", "1m30s", "500ms", "1.5s", etc.
    humantime::parse_duration(s).ok().map(duration_to_secs)
}

/// Parse the standard `Retry-After` header value.
///
/// Per RFC 7231 §7.1.3 the value is either:
/// - An integer delay in seconds, or
/// - An HTTP-date indicating when to retry.
fn parse_retry_after_header(s: &str) -> Option<u64> {
    let s = s.trim();
    // Integer delay-seconds
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs);
    }
    // HTTP-date: compute remaining seconds from now
    httpdate::parse_http_date(s)
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|target| {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .and_then(|now| target.checked_sub(now))
        })
        .map(duration_to_secs)
}

/// Extract `retry_after` from HTTP response headers.
///
/// Checks in order:
/// 1. Standard `Retry-After` (integer or HTTP-date)
/// 2. Anthropic-style `retry-after-ms` (milliseconds as integer)
/// 3. Provider-specific `x-ratelimit-reset-requests` (duration string)
pub fn parse_retry_after(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_retry_after_header)
        .or_else(|| {
            headers
                .get("retry-after-ms")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| {
                    s.parse::<u64>()
                        .ok()
                        .map(|ms| duration_to_secs(Duration::from_millis(ms)))
                })
        })
        .or_else(|| {
            headers
                .get("x-ratelimit-reset-requests")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after_value)
        })
}

/// Extract `retry_after` from a single JSON value (numeric or string).
fn json_retry_after_value(v: &serde_json::Value) -> Option<u64> {
    if let Some(n) = v.as_f64() {
        let secs = n as u64;
        return Some(if secs > 0 {
            secs
        } else if n > 0.0 {
            1
        } else {
            0
        });
    }
    v.as_str().and_then(parse_retry_after_value)
}

/// Extract `retry_after` from a parsed JSON response body.
///
/// Checks common locations where providers embed retry hints:
/// - `error.retry_after` / `error.retry_after_secs`
/// - top-level `retry_after` / `retry_after_secs`
///
/// Vendor-agnostic field lookup only — no error-code classification.
pub fn extract_retry_after_from_json(json: &serde_json::Value) -> Option<u64> {
    [
        json.pointer("/error/retry_after"),
        json.pointer("/error/retry_after_secs"),
        json.get("retry_after"),
        json.get("retry_after_secs"),
    ]
    .into_iter()
    .flatten()
    .find_map(json_retry_after_value)
}

/// Parse `"try again in 11.054s"` style delays from a provider message.
///
/// Used by providers (e.g. OpenAI/Codex rate-limit messages) after they have
/// already decided the failure is a rate limit from structured `code`/`type`.
pub fn parse_retry_after_from_message(message: &str) -> Option<u64> {
    // Mirror openai/codex: `(?i)try again in\s*(\d+(?:\.\d+)?)\s*(s|ms|seconds?)`
    let lower = message.to_ascii_lowercase();
    let marker = "try again in";
    let rest = lower.split_once(marker)?.1.trim_start();
    let mut num = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            chars.next();
        } else {
            break;
        }
    }
    if num.is_empty() {
        return None;
    }
    let value: f64 = num.parse().ok()?;
    // skip whitespace
    while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
        chars.next();
    }
    let unit: String = chars
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();
    if unit == "ms" {
        let millis = value as u64;
        return Some(if millis == 0 && value > 0.0 {
            1
        } else {
            duration_to_secs(Duration::from_millis(millis.max(1)))
        });
    }
    if unit == "s" || unit.starts_with("second") {
        return Some(if value <= 0.0 {
            0
        } else if value < 1.0 {
            1
        } else {
            value.ceil() as u64
        });
    }
    None
}

pub fn classify_http_status(status_code: u16, headers: &http::HeaderMap, body: &[u8]) -> LLMError {
    if status_code == 499 {
        return LLMError::Cancelled;
    }

    // Parse body JSON once; reuse for both retry-after and message extraction.
    let body_json = serde_json::from_slice::<serde_json::Value>(body).ok();

    let retry_after_secs = parse_retry_after(headers)
        .or_else(|| body_json.as_ref().and_then(extract_retry_after_from_json));

    let clean_message = body_json
        .as_ref()
        .and_then(|json| json.pointer("/error/message"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── duration_to_secs ─────────────────────────────────────────────────

    #[test]
    fn duration_to_secs_zero() {
        assert_eq!(duration_to_secs(Duration::ZERO), 0);
    }

    #[test]
    fn duration_to_secs_subsecond_rounds_up() {
        assert_eq!(duration_to_secs(Duration::from_millis(60)), 1);
        assert_eq!(duration_to_secs(Duration::from_millis(500)), 1);
        assert_eq!(duration_to_secs(Duration::from_nanos(1)), 1);
    }

    #[test]
    fn duration_to_secs_whole_seconds_preserved() {
        assert_eq!(duration_to_secs(Duration::from_secs(30)), 30);
        assert_eq!(duration_to_secs(Duration::from_secs(90)), 90);
    }

    // ── parse_retry_after_value ──────────────────────────────────────────

    #[test]
    fn parse_value_plain_integer() {
        assert_eq!(parse_retry_after_value("30"), Some(30));
        assert_eq!(parse_retry_after_value("0"), Some(0));
    }

    #[test]
    fn parse_value_duration_strings() {
        assert_eq!(parse_retry_after_value("30s"), Some(30));
        assert_eq!(parse_retry_after_value("1m"), Some(60));
        assert_eq!(parse_retry_after_value("1m30s"), Some(90));
        assert_eq!(parse_retry_after_value("2h"), Some(7200));
        assert_eq!(parse_retry_after_value("1h 30s"), Some(3630));
    }

    #[test]
    fn parse_value_subsecond_durations_round_up() {
        assert_eq!(parse_retry_after_value("500ms"), Some(1));
        assert_eq!(parse_retry_after_value("60ms"), Some(1));
    }

    #[test]
    fn parse_value_fractional_seconds() {
        assert_eq!(parse_retry_after_value("1.5s"), Some(1)); // as_secs floors
    }

    #[test]
    fn parse_value_garbage_returns_none() {
        assert_eq!(parse_retry_after_value("abc"), None);
        assert_eq!(parse_retry_after_value(""), None);
    }

    // ── parse_retry_after_header ─────────────────────────────────────────

    #[test]
    fn parse_header_integer() {
        assert_eq!(parse_retry_after_header("60"), Some(60));
        assert_eq!(parse_retry_after_header("0"), Some(0));
    }

    #[test]
    fn parse_header_http_date_future() {
        // A date 120 seconds from now should yield approximately 120s.
        let future = SystemTime::now() + Duration::from_secs(120);
        let http_date = httpdate::fmt_http_date(future);
        let secs = parse_retry_after_header(&http_date).expect("should parse HTTP-date");
        assert!((100..=130).contains(&secs), "expected ~120s, got {secs}");
    }

    #[test]
    fn parse_header_http_date_past_returns_zero() {
        let past = SystemTime::UNIX_EPOCH + Duration::from_secs(60);
        let http_date = httpdate::fmt_http_date(past);
        // A date in the past means the retry time has already elapsed.
        // checked_sub returns None, so the whole thing returns None.
        assert_eq!(parse_retry_after_header(&http_date), None);
    }

    #[test]
    fn parse_header_garbage_returns_none() {
        assert_eq!(parse_retry_after_header("not-a-date"), None);
    }

    // ── parse_retry_after (headers) ──────────────────────────────────────

    #[test]
    fn parse_headers_retry_after_integer() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(30));
    }

    #[test]
    fn parse_headers_retry_after_ms() {
        let mut headers = http::HeaderMap::new();
        headers.insert("retry-after-ms", "60000".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(60));
    }

    #[test]
    fn parse_headers_retry_after_ms_subsecond_rounds_up() {
        let mut headers = http::HeaderMap::new();
        headers.insert("retry-after-ms", "60".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(1));
    }

    #[test]
    fn parse_headers_x_ratelimit_reset() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-ratelimit-reset-requests", "1m30s".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(90));
    }

    #[test]
    fn parse_headers_prefers_standard_over_provider() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, "10".parse().unwrap());
        headers.insert("retry-after-ms", "5000".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(10));
    }

    #[test]
    fn parse_headers_nothing_returns_none() {
        let headers = http::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
    }

    // ── json_retry_after_value ───────────────────────────────────────────

    #[test]
    fn json_value_numeric() {
        assert_eq!(json_retry_after_value(&serde_json::json!(30)), Some(30));
        assert_eq!(json_retry_after_value(&serde_json::json!(0)), Some(0));
    }

    #[test]
    fn json_value_fractional_rounds_up() {
        assert_eq!(json_retry_after_value(&serde_json::json!(0.5)), Some(1));
    }

    #[test]
    fn json_value_string_duration() {
        assert_eq!(json_retry_after_value(&serde_json::json!("30s")), Some(30));
        assert_eq!(
            json_retry_after_value(&serde_json::json!("1m30s")),
            Some(90)
        );
        assert_eq!(json_retry_after_value(&serde_json::json!("500ms")), Some(1));
    }

    // ── extract_retry_after_from_json ────────────────────────────────────

    #[test]
    fn extract_from_json_error_nested() {
        let json = serde_json::json!({ "error": { "retry_after": 30, "message": "slow down" } });
        assert_eq!(extract_retry_after_from_json(&json), Some(30));
    }

    #[test]
    fn extract_from_json_top_level() {
        let json = serde_json::json!({ "retry_after_secs": 60 });
        assert_eq!(extract_retry_after_from_json(&json), Some(60));
    }

    #[test]
    fn extract_from_json_string_value() {
        let json = serde_json::json!({ "error": { "retry_after": "1m30s" } });
        assert_eq!(extract_retry_after_from_json(&json), Some(90));
    }

    #[test]
    fn extract_from_json_nothing() {
        let json = serde_json::json!({ "error": { "message": "nope" } });
        assert_eq!(extract_retry_after_from_json(&json), None);
    }

    // ── classify_http_status integration ─────────────────────────────────

    #[test]
    fn classify_429_with_header_and_body() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, "30".parse().unwrap());
        let body = br#"{"error":{"message":"Rate limited"}}"#;
        let err = classify_http_status(429, &headers, body);
        assert_eq!(err.retry_after_secs(), Some(30));
    }

    #[test]
    fn classify_429_with_body_only() {
        let headers = http::HeaderMap::new();
        let body = br#"{"error":{"message":"slow down","retry_after":60}}"#;
        let err = classify_http_status(429, &headers, body);
        assert_eq!(err.retry_after_secs(), Some(60));
    }

    #[test]
    fn classify_429_no_retry_hint() {
        let headers = http::HeaderMap::new();
        let body = br#"{"error":{"message":"usage limit reached"}}"#;
        let err = classify_http_status(429, &headers, body);
        assert_eq!(err.retry_after_secs(), None);
    }

    #[test]
    fn classify_500_with_x_ratelimit_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-ratelimit-reset-requests", "1m0s".parse().unwrap());
        let body = b"Server Error";
        let err = classify_http_status(503, &headers, body);
        assert_eq!(err.retry_after_secs(), Some(60));
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum LegacyPayload {
        ProviderError { message: String },
    }

    #[test]
    fn legacy_provider_payload_deserialization() {
        let payload: LLMErrorPayload = serde_json::from_value(serde_json::json!({
            "type": "provider_error",
            "message": "legacy failure"
        }))
        .unwrap();

        assert_eq!(
            payload,
            LLMErrorPayload::ProviderError {
                message: "legacy failure".to_owned(),
                context: None,
            }
        );
        assert!(matches!(
            LLMError::from_payload(payload),
            LLMError::ProviderError(message) if message == "legacy failure"
        ));
    }

    #[test]
    fn structured_semantic_errors_round_trip_through_legacy_provider_tag() {
        let overloaded = LLMError::ServerOverloaded {
            message: "busy".into(),
            context: Box::new(
                ProviderErrorContext::builder("codex")
                    .kind(ProviderErrorKind::ServerOverloaded)
                    .code(Some("server_is_overloaded".into()))
                    .request_id(Some("req-1".into()))
                    .transient(true)
                    .build(),
            ),
        };
        let payload = overloaded.to_payload();
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["type"], "provider_error");
        assert_eq!(json["context"]["kind"], "server_overloaded");
        assert_eq!(json["context"]["code"], "server_is_overloaded");
        let legacy: LegacyPayload = serde_json::from_value(json.clone()).unwrap();
        assert!(matches!(
            legacy,
            LegacyPayload::ProviderError { message } if message == "busy"
        ));
        let restored =
            LLMError::from_payload(serde_json::from_value::<LLMErrorPayload>(json).unwrap());
        assert!(matches!(
            restored,
            LLMError::ServerOverloaded { message, context }
                if message == "busy"
                    && context.code.as_deref() == Some("server_is_overloaded")
                    && restored.is_retryable()
        ));

        let quota = LLMError::QuotaExceeded {
            message: "no credits".into(),
            context: Box::new(
                ProviderErrorContext::builder("openai")
                    .kind(ProviderErrorKind::QuotaExceeded)
                    .code(Some("insufficient_quota".into()))
                    .build(),
            ),
        };
        assert!(!quota.is_retryable());
        assert!(matches!(
            LLMError::from_payload(quota.to_payload()),
            LLMError::QuotaExceeded { .. }
        ));

        let context = LLMError::ContextWindowExceeded {
            message: "too long".into(),
            context: Box::new(
                ProviderErrorContext::builder("codex")
                    .kind(ProviderErrorKind::ContextWindowExceeded)
                    .code(Some("context_length_exceeded".into()))
                    .build(),
            ),
        };
        assert!(!context.is_retryable());
        assert!(matches!(
            LLMError::from_payload(context.to_payload()),
            LLMError::ContextWindowExceeded { .. }
        ));
    }

    #[test]
    fn payload_retryability_uses_known_kind_instead_of_transient_hint() {
        let overloaded = LLMErrorPayload::ProviderError {
            message: "busy".into(),
            context: Some(
                ProviderErrorContext::builder("test")
                    .kind(ProviderErrorKind::ServerOverloaded)
                    .transient(false)
                    .build(),
            ),
        };
        assert!(overloaded.is_retryable());

        let quota = LLMErrorPayload::ProviderError {
            message: "no credits".into(),
            context: Some(
                ProviderErrorContext::builder("test")
                    .kind(ProviderErrorKind::QuotaExceeded)
                    .transient(true)
                    .build(),
            ),
        };
        assert!(!quota.is_retryable());
    }

    #[test]
    fn provider_error_context_builder_sets_optional_metadata() {
        let context = ProviderErrorContext::builder("codex")
            .kind(ProviderErrorKind::ServerOverloaded)
            .code(Some("server_is_overloaded".into()))
            .error_type(Some("server_error".into()))
            .request_id(Some("req-1".into()))
            .retry_after_secs(Some(2))
            .transient(true)
            .build();

        assert_eq!(context.provider, "codex");
        assert_eq!(context.code.as_deref(), Some("server_is_overloaded"));
        assert_eq!(context.error_type.as_deref(), Some("server_error"));
        assert_eq!(context.request_id.as_deref(), Some("req-1"));
        assert_eq!(context.retry_after_secs, Some(2));
        assert!(context.transient);
    }

    #[test]
    fn server_overloaded_is_retryable_by_kind() {
        let err = LLMError::ServerOverloaded {
            message: "capacity".into(),
            context: Box::new(
                ProviderErrorContext::builder("codex")
                    .code(Some("server_is_overloaded".into()))
                    .build(),
            ),
        };
        // Kind wins even if context.transient were false.
        assert!(err.is_retryable());
    }

    #[test]
    fn provider_response_error_honors_context_transient_flag() {
        let transient = LLMError::ProviderResponseError {
            message: "maybe".into(),
            context: Box::new(
                ProviderErrorContext::builder("x")
                    .code(Some("unknown".into()))
                    .retry_after_secs(Some(5))
                    .transient(true)
                    .build(),
            ),
        };
        assert!(transient.is_retryable());
        assert_eq!(transient.retry_after_secs(), Some(5));

        let permanent = LLMError::ProviderResponseError {
            message: "nope".into(),
            context: Box::new(
                ProviderErrorContext::builder("x")
                    .code(Some("unknown".into()))
                    .build(),
            ),
        };
        assert!(!permanent.is_retryable());
    }

    #[test]
    fn payload_retryability_is_stable_without_reconstruction() {
        assert!(
            !LLMErrorPayload::JsonError {
                message: "bad JSON".into(),
            }
            .is_retryable()
        );
        assert!(
            !LLMErrorPayload::InvalidUrl {
                message: "bad URL".into(),
            }
            .is_retryable()
        );
        assert!(
            LLMErrorPayload::IoError {
                message: "connection reset".into(),
            }
            .is_retryable()
        );
        assert!(
            LLMErrorPayload::ProviderError {
                message: "temporary".into(),
                context: Some(
                    ProviderErrorContext::builder("test")
                        .transient(true)
                        .build(),
                ),
            }
            .is_retryable()
        );
        assert!(
            !LLMErrorPayload::ProviderError {
                message: "permanent".into(),
                context: None,
            }
            .is_retryable()
        );
    }

    #[test]
    fn parse_retry_after_from_message_matches_codex_style() {
        assert_eq!(
            parse_retry_after_from_message(
                "Rate limit reached. Please try again in 11.054s. Visit docs."
            ),
            Some(12) // ceil
        );
        assert_eq!(
            parse_retry_after_from_message("Please try again in 30 seconds."),
            Some(30)
        );
        assert_eq!(
            parse_retry_after_from_message("Please try again in 500ms"),
            Some(1)
        );
        assert_eq!(parse_retry_after_from_message("no delay mentioned"), None);
    }
}

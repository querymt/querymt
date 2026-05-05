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
fn extract_retry_after_from_json(json: &serde_json::Value) -> Option<u64> {
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
}

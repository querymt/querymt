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

impl ProviderErrorKind {
    /// Unified retry policy for provider failures. This is the single source
    /// of truth — [`LLMErrorPayload::is_retryable`] reconstructs then defers here.
    ///
    /// QueryMT product choice: overload is retried (upstream Codex marks it
    /// non-retryable). Quota/context/auth/request failures never are.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::ServerOverloaded | Self::RateLimited)
    }
}

/// Diagnostics for a structured provider failure.
///
/// Used both before attribution (`provider` empty) and on the wire / in
/// [`LLMError::ProviderResponseError`] (adapter stamps `provider` once).
/// There is deliberately no parallel "classified" struct with the same fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderErrorContext {
    /// Registry / factory identity. Empty string until the HTTP adapter stamps it.
    #[serde(default)]
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
    /// Retryability for *unclassified* failures. A known [`Self::kind`] always
    /// wins; this flag is only consulted when `kind` is `None`.
    pub transient: bool,
}

impl ProviderErrorContext {
    /// Empty context for a wire classifier that has not stamped identity yet.
    pub fn unattributed() -> Self {
        Self {
            provider: String::new(),
            kind: None,
            code: None,
            error_type: None,
            request_id: None,
            retry_after_secs: None,
            transient: false,
        }
    }

    /// Unified retry decision: known kinds follow [`ProviderErrorKind`]
    /// policy; unknown kinds fall back to the provider's transient hint.
    pub fn is_retryable(&self) -> bool {
        self.kind
            .map_or(self.transient, ProviderErrorKind::is_retryable)
    }

    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    pub fn set_retry_after_if_missing(&mut self, retry_after_secs: Option<u64>) {
        if self.retry_after_secs.is_none() {
            self.retry_after_secs = retry_after_secs;
        }
    }
}

impl std::fmt::Display for ProviderErrorContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.provider.is_empty() {
            write!(f, "unattributed")?;
        } else {
            write!(f, "{}", self.provider)?;
        }
        if let Some(request_id) = &self.request_id {
            write!(f, ", request_id={request_id}")?;
        }
        Ok(())
    }
}

/// Structured provider failure before (or after) identity is stamped.
///
/// Wire classifiers build this; the HTTP adapter calls [`Self::attribute`]
/// once with the factory name. Same [`ProviderErrorContext`] shape ends up on
/// the wire — no parallel field set to keep in sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub message: String,
    pub context: ProviderErrorContext,
}

impl ProviderFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            context: ProviderErrorContext::unattributed(),
        }
    }

    pub fn kind(mut self, kind: ProviderErrorKind) -> Self {
        self.context.kind = Some(kind);
        // Known kinds own retry policy — keep transient consistent so the
        // dual field cannot disagree with kind.is_retryable().
        self.context.transient = kind.is_retryable();
        self
    }

    pub fn code(mut self, code: Option<String>) -> Self {
        self.context.code = code;
        self
    }

    pub fn error_type(mut self, error_type: Option<String>) -> Self {
        self.context.error_type = error_type;
        self
    }

    pub fn request_id(mut self, request_id: Option<String>) -> Self {
        self.context.request_id = request_id;
        self
    }

    pub fn retry_after_secs(mut self, retry_after_secs: Option<u64>) -> Self {
        self.context.retry_after_secs = retry_after_secs;
        self
    }

    /// Retryability hint for *unclassified* failures (`kind == None`).
    /// Ignored once a known kind is set (see [`Self::kind`]).
    pub fn transient(mut self, transient: bool) -> Self {
        if self.context.kind.is_none() {
            self.context.transient = transient;
        }
        self
    }

    pub fn set_retry_after_if_missing(&mut self, retry_after_secs: Option<u64>) {
        self.context.set_retry_after_if_missing(retry_after_secs);
    }

    pub fn is_retryable(&self) -> bool {
        self.context.is_retryable()
    }

    /// Attach provider identity and produce the structured [`LLMError`].
    ///
    /// Prefer letting the HTTP adapter call this once via
    /// [`ProviderDecodeError::attribute`] rather than stamping at every
    /// classifier call site.
    pub fn attribute(self, provider: impl Into<String>) -> LLMError {
        LLMError::ProviderResponseError {
            message: self.message,
            context: Box::new(self.context.with_provider(provider)),
        }
    }
}

/// Failure from a provider HTTP/SSE decoder before provider identity is attached.
///
/// - [`Self::Classified`]: vendor envelope / status mapped to a unified kind —
///   needs [`Self::attribute`].
/// - [`Self::Terminal`]: already-final error (parse failure, cancel, …).
///   [`Self::attribute`] returns it unchanged (except re-stamping any nested
///   structured provider context).
///
/// No `From<LLMError>`: construct terminal variants with the named helpers so
/// accidental double-attribution is not a silent footgun.
#[derive(Debug)]
pub enum ProviderDecodeError {
    Classified(ProviderFailure),
    Terminal(LLMError),
}

impl ProviderDecodeError {
    pub fn response_format(message: impl Into<String>, raw_response: impl Into<String>) -> Self {
        Self::Terminal(LLMError::ResponseFormatError {
            message: message.into(),
            raw_response: raw_response.into(),
        })
    }

    pub fn terminal(error: LLMError) -> Self {
        Self::Terminal(error)
    }

    /// Stamp provider identity onto classified failures; pass terminal errors through.
    pub fn attribute(self, provider: impl Into<String>) -> LLMError {
        match self {
            Self::Classified(error) => error.attribute(provider),
            Self::Terminal(error) => error.with_provider(provider),
        }
    }
}

impl From<ProviderFailure> for ProviderDecodeError {
    fn from(error: ProviderFailure) -> Self {
        Self::Classified(error)
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
    ///
    /// Single source of truth: reconstruct then ask [`LLMError::is_retryable`].
    /// `from_payload` must stay lossless for retry semantics (see tests).
    pub fn is_retryable(&self) -> bool {
        LLMError::from_payload(self.clone()).is_retryable()
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

    /// A provider failure with structured classification metadata.
    ///
    /// The failure kind lives in `context.kind` — there is deliberately no
    /// per-kind enum variant, so the classification can never disagree with
    /// the context. Unknown failures keep opaque vendor diagnostics
    /// (`code` / `request_id`) with `kind: None`.
    #[error("LLM Provider Error ({context}): {message}")]
    ProviderResponseError {
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
    ///
    /// Stored as a string so wire payloads round-trip without changing retryability.
    #[error("JSON Error: {0}")]
    JsonError(String),

    /// Handles errors from parsing URLs.
    ///
    /// Stored as a string so wire payloads round-trip without changing retryability.
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

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
            Self::JsonError(message) => LLMErrorPayload::JsonError {
                message: message.clone(),
            },
            Self::InvalidUrl(message) => LLMErrorPayload::InvalidUrl {
                message: message.clone(),
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
                Some(context) => Self::ProviderResponseError {
                    message,
                    context: Box::new(context),
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
            // Lossless: keep payload tags as distinct runtime variants so
            // payload.is_retryable() == from_payload(p).is_retryable().
            LLMErrorPayload::JsonError { message } => Self::JsonError(message),
            LLMErrorPayload::InvalidUrl { message } => Self::InvalidUrl(message),
            // Io has no dedicated runtime variant; Transport is the stable
            // retryable home and matches payload IoError retryability.
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
            Self::ProviderResponseError { context, .. } => context.retry_after_secs,
            _ => None,
        }
    }

    /// Whether this error is a rate-limit failure (for UI events / wait messaging).
    ///
    /// Two representations exist on purpose:
    /// - [`Self::RateLimited`] / bare HTTP 429: unattributed generic HTTP path
    /// - [`Self::ProviderResponseError`] with [`ProviderErrorKind::RateLimited`]:
    ///   provider-attributed chat path
    ///
    /// Callers must use this helper instead of matching one form only.
    pub fn is_rate_limited(&self) -> bool {
        match self {
            Self::RateLimited { .. } => true,
            Self::HttpStatus {
                status_code: 429, ..
            } => true,
            Self::ProviderResponseError { context, .. } => {
                context.kind == Some(ProviderErrorKind::RateLimited)
            }
            _ => false,
        }
    }

    /// Stamp provider identity onto structured provider failures.
    ///
    /// Used by [`ProviderDecodeError::attribute`] for terminal variants that
    /// already carry a [`Self::ProviderResponseError`] (e.g. status-only
    /// classification) so the adapter can still attach identity once.
    /// Non-provider variants are returned unchanged.
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        if let Self::ProviderResponseError { context, .. } = &mut self {
            context.provider = provider.into();
        }
        self
    }

    /// Message + retry-after when [`Self::is_rate_limited`] is true.
    pub fn rate_limit_info(&self) -> Option<(String, Option<u64>)> {
        match self {
            Self::RateLimited {
                message,
                retry_after_secs,
            } => Some((message.clone(), *retry_after_secs)),
            Self::HttpStatus {
                status_code: 429,
                message,
                retry_after_secs,
            } => Some((message.clone(), *retry_after_secs)),
            Self::ProviderResponseError { message, context }
                if context.kind == Some(ProviderErrorKind::RateLimited) =>
            {
                Some((message.clone(), context.retry_after_secs))
            }
            _ => None,
        }
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
            // Always retry: transient infrastructure
            Self::Transport { .. } => true,
            Self::HttpError(_) => true, // unclassified HTTP transport error — could be transient
            Self::RateLimited { .. } => true,
            Self::HttpStatus { status_code, .. } => {
                matches!(status_code, 429 | 500..=599)
            }
            Self::PluginError(_) => true, // may be a transient WASM/HTTP issue
            Self::IoError { .. } => true,

            // Structured provider failure: unified kind policy, transient
            // hint for unclassified failures.
            Self::ProviderResponseError { context, .. } => context.is_retryable(),

            // Never retry: semantic errors
            Self::AuthError(_) => false,
            Self::InvalidRequest(_) => false,
            Self::ProviderError(_) => false,
            Self::ToolConfigError(_) => false,
            Self::ResponseFormatError { .. } => false,
            Self::GenericError(_) => false,
            Self::Cancelled => false,
            Self::JsonError(_) => false,
            Self::InvalidUrl(_) => false,
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

/// Extract the human-readable message and retry hint from an error response.
fn status_message_and_retry(
    status_code: u16,
    headers: &http::HeaderMap,
    body: &[u8],
) -> (String, Option<u64>) {
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
    (message, retry_after_secs)
}

/// Classify a non-success HTTP status into a structured provider error when
/// the body carries no recognizable vendor error envelope.
///
/// Unlike [`classify_http_status`], the result is still unattributed:
/// provider classifiers return this so the failure picks up provider identity
/// at [`ProviderDecodeError::attribute`]. Callers must special-case status
/// 499 ([`LLMError::Cancelled`]) themselves.
pub fn classify_status_only(
    status_code: u16,
    headers: &http::HeaderMap,
    body: &[u8],
) -> ProviderFailure {
    let (message, retry_after_secs) = status_message_and_retry(status_code, headers, body);
    let kind = match status_code {
        429 => Some(ProviderErrorKind::RateLimited),
        401 | 403 => Some(ProviderErrorKind::Authentication),
        400 | 422 => Some(ProviderErrorKind::InvalidRequest),
        _ => None,
    };
    let transient = kind.map_or(
        matches!(status_code, 500..=599),
        ProviderErrorKind::is_retryable,
    );
    let mut failure = ProviderFailure::new(message)
        .retry_after_secs(retry_after_secs)
        .transient(transient);
    if let Some(kind) = kind {
        failure = failure.kind(kind);
    }
    failure
}

/// Generic HTTP status classifier for paths that have no provider identity.
///
/// Rate limits stay on the legacy [`LLMError::RateLimited`] variant here so
/// unattributed generic HTTP failures keep the stable wire payload shape.
/// Provider-attributed chat paths must use [`classify_status_only`] +
/// [`ProviderFailure::attribute`] (or a vendor envelope classifier)
/// and produce [`LLMError::ProviderResponseError`] instead — never emit both
/// forms from the same boundary.
pub fn classify_http_status(status_code: u16, headers: &http::HeaderMap, body: &[u8]) -> LLMError {
    if status_code == 499 {
        return LLMError::Cancelled;
    }

    let (message, retry_after_secs) = status_message_and_retry(status_code, headers, body);

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

impl From<serde_json::Error> for LLMError {
    fn from(err: serde_json::Error) -> Self {
        Self::JsonError(err.to_string())
    }
}

impl From<url::ParseError> for LLMError {
    fn from(err: url::ParseError) -> Self {
        Self::InvalidUrl(err.to_string())
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
    fn classified_error_attributes_provider_and_preserves_kind() {
        let error = ProviderFailure::new("busy")
            .kind(ProviderErrorKind::ServerOverloaded)
            .code(Some("server_is_overloaded".into()))
            .error_type(Some("server_error".into()))
            .request_id(Some("req-1".into()))
            .retry_after_secs(Some(2))
            .transient(true)
            .attribute("xai");

        match error {
            LLMError::ProviderResponseError { message, context } => {
                assert_eq!(message, "busy");
                assert_eq!(context.provider, "xai");
                assert_eq!(context.kind, Some(ProviderErrorKind::ServerOverloaded));
                assert_eq!(context.code.as_deref(), Some("server_is_overloaded"));
                assert_eq!(context.request_id.as_deref(), Some("req-1"));
                assert_eq!(context.retry_after_secs, Some(2));
                assert!(context.transient);
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    #[test]
    fn decode_error_terminal_ignores_provider_on_attribute() {
        let error =
            ProviderDecodeError::response_format("invalid chunk", "raw").attribute("openai");

        assert!(matches!(
            error,
            LLMError::ResponseFormatError {
                ref message,
                ref raw_response,
            } if message == "invalid chunk" && raw_response == "raw"
        ));
    }

    #[test]
    fn decode_error_classified_attributes_provider() {
        let error = ProviderDecodeError::from(
            ProviderFailure::new("busy").kind(ProviderErrorKind::RateLimited),
        )
        .attribute("groq");

        match error {
            LLMError::ProviderResponseError { message, context } => {
                assert_eq!(message, "busy");
                assert_eq!(context.provider, "groq");
                assert_eq!(context.kind, Some(ProviderErrorKind::RateLimited));
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    #[test]
    fn structured_semantic_errors_round_trip_through_legacy_provider_tag() {
        let overloaded = ProviderFailure::new("busy")
            .kind(ProviderErrorKind::ServerOverloaded)
            .code(Some("server_is_overloaded".into()))
            .request_id(Some("req-1".into()))
            .transient(true)
            .attribute("codex");
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
            LLMError::ProviderResponseError { message, context }
                if message == "busy"
                    && context.kind == Some(ProviderErrorKind::ServerOverloaded)
                    && context.code.as_deref() == Some("server_is_overloaded")
                    && restored.is_retryable()
        ));

        let quota = ProviderFailure::new("no credits")
            .kind(ProviderErrorKind::QuotaExceeded)
            .code(Some("insufficient_quota".into()))
            .attribute("openai");
        assert!(!quota.is_retryable());
        assert!(matches!(
            LLMError::from_payload(quota.to_payload()),
            LLMError::ProviderResponseError { context, .. }
                if context.kind == Some(ProviderErrorKind::QuotaExceeded)
        ));

        let context = ProviderFailure::new("too long")
            .kind(ProviderErrorKind::ContextWindowExceeded)
            .code(Some("context_length_exceeded".into()))
            .attribute("codex");
        assert!(!context.is_retryable());
        assert!(matches!(
            LLMError::from_payload(context.to_payload()),
            LLMError::ProviderResponseError { context, .. }
                if context.kind == Some(ProviderErrorKind::ContextWindowExceeded)
        ));
    }

    #[test]
    fn payload_retryability_uses_known_kind_instead_of_transient_hint() {
        // Builder: kind() forces transient to match kind policy. Wire payloads
        // can still carry a disagreeing transient flag from older peers — kind
        // still wins via is_retryable().
        let overloaded = LLMErrorPayload::ProviderError {
            message: "busy".into(),
            context: Some(ProviderErrorContext {
                provider: "test".into(),
                kind: Some(ProviderErrorKind::ServerOverloaded),
                code: None,
                error_type: None,
                request_id: None,
                retry_after_secs: None,
                transient: false, // disagreeing hint must lose
            }),
        };
        assert!(overloaded.is_retryable());

        let quota = LLMErrorPayload::ProviderError {
            message: "no credits".into(),
            context: Some(ProviderErrorContext {
                provider: "test".into(),
                kind: Some(ProviderErrorKind::QuotaExceeded),
                code: None,
                error_type: None,
                request_id: None,
                retry_after_secs: None,
                transient: true, // disagreeing hint must lose
            }),
        };
        assert!(!quota.is_retryable());
    }

    #[test]
    fn classified_kind_forces_transient_consistency() {
        let overloaded = ProviderFailure::new("busy")
            .transient(false)
            .kind(ProviderErrorKind::ServerOverloaded);
        assert!(overloaded.is_retryable());
        assert!(overloaded.context.transient);

        let quota = ProviderFailure::new("nope")
            .transient(true)
            .kind(ProviderErrorKind::QuotaExceeded);
        assert!(!quota.is_retryable());
        assert!(!quota.context.transient);

        // After kind is set, transient() is a no-op.
        let locked = ProviderFailure::new("busy")
            .kind(ProviderErrorKind::ServerOverloaded)
            .transient(false);
        assert!(locked.context.transient);
        assert!(locked.is_retryable());
    }

    #[test]
    fn provider_failure_attribute_copies_metadata_into_context() {
        let error = ProviderFailure::new("busy")
            .kind(ProviderErrorKind::ServerOverloaded)
            .code(Some("server_is_overloaded".into()))
            .error_type(Some("server_error".into()))
            .request_id(Some("req-1".into()))
            .retry_after_secs(Some(2))
            .transient(true)
            .attribute("codex");

        match error {
            LLMError::ProviderResponseError { message, context } => {
                assert_eq!(message, "busy");
                assert_eq!(context.provider, "codex");
                assert_eq!(context.kind, Some(ProviderErrorKind::ServerOverloaded));
                assert_eq!(context.code.as_deref(), Some("server_is_overloaded"));
                assert_eq!(context.error_type.as_deref(), Some("server_error"));
                assert_eq!(context.request_id.as_deref(), Some("req-1"));
                assert_eq!(context.retry_after_secs, Some(2));
                assert!(context.transient);
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    #[test]
    fn server_overloaded_is_retryable_by_kind() {
        let err = ProviderFailure::new("capacity")
            .kind(ProviderErrorKind::ServerOverloaded)
            .code(Some("server_is_overloaded".into()))
            .attribute("codex");
        // Kind wins even if context.transient were false.
        assert!(err.is_retryable());
    }

    #[test]
    fn provider_response_error_honors_context_transient_flag() {
        let transient = ProviderFailure::new("maybe")
            .code(Some("unknown".into()))
            .retry_after_secs(Some(5))
            .transient(true)
            .attribute("x");
        assert!(transient.is_retryable());
        assert_eq!(transient.retry_after_secs(), Some(5));

        let permanent = ProviderFailure::new("nope")
            .code(Some("unknown".into()))
            .attribute("x");
        assert!(!permanent.is_retryable());
    }

    #[test]
    fn payload_retryability_matches_reconstructed_error() {
        let cases: Vec<LLMErrorPayload> = vec![
            LLMErrorPayload::JsonError {
                message: "bad JSON".into(),
            },
            LLMErrorPayload::InvalidUrl {
                message: "bad URL".into(),
            },
            LLMErrorPayload::IoError {
                message: "connection reset".into(),
            },
            LLMErrorPayload::ProviderError {
                message: "temporary".into(),
                context: Some(
                    ProviderFailure::new("temporary")
                        .transient(true)
                        .context
                        .with_provider("test"),
                ),
            },
            LLMErrorPayload::ProviderError {
                message: "permanent".into(),
                context: None,
            },
            LLMErrorPayload::RateLimited {
                message: "slow".into(),
                retry_after_secs: Some(1),
            },
            LLMErrorPayload::HttpStatus {
                status_code: 503,
                message: "unavailable".into(),
                retry_after_secs: None,
            },
            LLMErrorPayload::AuthError {
                message: "nope".into(),
            },
        ];
        for payload in cases {
            let reconstructed = LLMError::from_payload(payload.clone());
            assert_eq!(
                payload.is_retryable(),
                reconstructed.is_retryable(),
                "retryability diverged for {payload:?} → {reconstructed:?}"
            );
        }

        // Json/InvalidUrl round-trip keeps the same variant family.
        assert!(matches!(
            LLMError::from_payload(LLMErrorPayload::JsonError {
                message: "x".into()
            }),
            LLMError::JsonError(_)
        ));
        assert!(matches!(
            LLMError::from_payload(LLMErrorPayload::InvalidUrl {
                message: "x".into()
            }),
            LLMError::InvalidUrl(_)
        ));
        assert!(
            !LLMErrorPayload::JsonError {
                message: "x".into()
            }
            .is_retryable()
        );
        assert!(
            !LLMErrorPayload::InvalidUrl {
                message: "x".into()
            }
            .is_retryable()
        );
    }

    #[test]
    fn classify_status_only_maps_status_to_unified_kinds() {
        let headers = http::HeaderMap::new();

        let rate_limited =
            classify_status_only(429, &headers, br#"{"error":{"message":"slow down"}}"#);
        assert_eq!(
            rate_limited.context.kind,
            Some(ProviderErrorKind::RateLimited)
        );
        assert_eq!(rate_limited.message, "slow down");
        assert!(rate_limited.context.transient);

        let auth = classify_status_only(401, &headers, b"bad key");
        assert_eq!(auth.context.kind, Some(ProviderErrorKind::Authentication));
        assert!(!auth.context.transient);

        let invalid = classify_status_only(422, &headers, b"bad field");
        assert_eq!(
            invalid.context.kind,
            Some(ProviderErrorKind::InvalidRequest)
        );
        assert!(!invalid.context.transient);

        // Unknown 5xx: unclassified but transient.
        let overloaded = classify_status_only(503, &headers, b"Server Error");
        assert_eq!(overloaded.context.kind, None);
        assert!(overloaded.context.transient);
        assert!(overloaded.attribute("x").is_retryable());

        // Unknown 4xx: unclassified and permanent.
        let weird = classify_status_only(418, &headers, b"teapot");
        assert_eq!(weird.context.kind, None);
        assert!(!weird.context.transient);
    }

    #[test]
    fn rate_limit_info_covers_legacy_and_structured_forms() {
        let legacy = LLMError::RateLimited {
            message: "slow".into(),
            retry_after_secs: Some(3),
        };
        assert!(legacy.is_rate_limited());
        assert_eq!(legacy.rate_limit_info(), Some(("slow".into(), Some(3))));

        let structured = ProviderFailure::new("slow")
            .kind(ProviderErrorKind::RateLimited)
            .retry_after_secs(Some(4))
            .attribute("openai");
        assert!(structured.is_rate_limited());
        assert_eq!(structured.rate_limit_info(), Some(("slow".into(), Some(4))));

        let http_429 = LLMError::HttpStatus {
            status_code: 429,
            message: "too many".into(),
            retry_after_secs: Some(1),
        };
        assert!(http_429.is_rate_limited());
        assert_eq!(
            http_429.rate_limit_info(),
            Some(("too many".into(), Some(1)))
        );

        let other = LLMError::GenericError("nope".into());
        assert!(!other.is_rate_limited());
        assert!(other.rate_limit_info().is_none());
    }

    #[test]
    fn display_includes_provider_and_request_id() {
        let error = ProviderFailure::new("busy")
            .kind(ProviderErrorKind::ServerOverloaded)
            .request_id(Some("req-9".into()))
            .attribute("openai");
        let rendered = error.to_string();
        assert!(rendered.contains("openai"), "missing provider: {rendered}");
        assert!(
            rendered.contains("request_id=req-9"),
            "missing request id: {rendered}"
        );
        assert!(rendered.contains("busy"), "missing message: {rendered}");
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

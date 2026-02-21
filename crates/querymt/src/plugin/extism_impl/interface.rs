use crate::{
    chat::{ChatMessage, ChatResponse, FinishReason, Tool},
    completion::CompletionRequest,
    error::LLMError,
    stt, tts, ToolCall, Usage,
};
use serde::{Deserialize, Serialize};
use std::fmt;

// ============================================================================
// Structured error transport across the extism WASM boundary
// ============================================================================

/// Error codes for plugin ↔ host communication.
///
/// These are sent as the `WithReturnCode` integer on the plugin side
/// and received via `call_get_error_code` on the host side.
/// The error string is JSON-serialized [`PluginError`].
pub mod error_codes {
    pub const GENERIC: i32 = 0;
    pub const PROVIDER: i32 = 1;
    pub const AUTH: i32 = 2;
    pub const INVALID_REQUEST: i32 = 3;
    pub const RATE_LIMITED: i32 = 4;
    pub const HTTP: i32 = 5;
    pub const NOT_IMPLEMENTED: i32 = 6;
    pub const CANCELLED: i32 = 7;
}

/// Structured error that crosses the WASM boundary as JSON.
///
/// On the plugin side, [`LLMError`] is converted into this type and serialized
/// to JSON as the extism error string, paired with an [`error_codes`] integer
/// via `WithReturnCode`. On the host side, `call_get_error_code` yields
/// `(error_string, code)` — the code selects the variant and the JSON string
/// is deserialized back into this type to reconstruct the original [`LLMError`].
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", content = "data")]
pub enum PluginError {
    #[error("LLM Provider Error: {0}")]
    Provider(String),

    #[error("Auth Error: {0}")]
    Auth(String),

    #[error("Invalid Request: {0}")]
    InvalidRequest(String),

    #[error("Rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after_secs: Option<u64>,
    },

    #[error("HTTP Error: {0}")]
    Http(String),

    #[error("Cancelled")]
    Cancelled,

    #[error("Not Implemented: {0}")]
    NotImplemented(String),

    #[error("{0}")]
    Generic(String),
}

impl PluginError {
    /// Convert an [`LLMError`] into a `PluginError` for WASM transport.
    pub fn from_llm_error(err: &LLMError) -> Self {
        match err {
            LLMError::ProviderError(msg) => Self::Provider(msg.clone()),
            LLMError::AuthError(msg) => Self::Auth(msg.clone()),
            LLMError::InvalidRequest(msg) => Self::InvalidRequest(msg.clone()),
            LLMError::RateLimited {
                message,
                retry_after_secs,
            } => Self::RateLimited {
                message: message.clone(),
                retry_after_secs: *retry_after_secs,
            },
            LLMError::HttpError(msg) => Self::Http(msg.clone()),
            LLMError::Cancelled => Self::Cancelled,
            LLMError::NotImplemented(msg) => Self::NotImplemented(msg.clone()),
            other => Self::Generic(other.to_string()),
        }
    }

    /// Error code for this variant, matching [`error_codes`].
    pub fn code(&self) -> i32 {
        match self {
            Self::Provider(_) => error_codes::PROVIDER,
            Self::Auth(_) => error_codes::AUTH,
            Self::InvalidRequest(_) => error_codes::INVALID_REQUEST,
            Self::RateLimited { .. } => error_codes::RATE_LIMITED,
            Self::Http(_) => error_codes::HTTP,
            Self::Cancelled => error_codes::CANCELLED,
            Self::NotImplemented(_) => error_codes::NOT_IMPLEMENTED,
            Self::Generic(_) => error_codes::GENERIC,
        }
    }

    /// Serialize an [`LLMError`] into a `(json_string, error_code)` pair
    /// suitable for sending across the WASM boundary.
    pub fn encode(err: &LLMError) -> (String, i32) {
        let pe = Self::from_llm_error(err);
        let code = pe.code();
        let json = serde_json::to_string(&pe).unwrap_or_else(|_| err.to_string());
        (json, code)
    }

    /// Reconstruct an [`LLMError`] from an error code and JSON string
    /// received from the WASM plugin.
    ///
    /// The error code determines which [`LLMError`] variant to construct.
    /// The JSON string is deserialized for the structured payload (e.g.
    /// `retry_after_secs` on rate-limit errors). Falls back to using the
    /// raw string as the message if JSON parsing fails.
    pub fn decode(code: i32, json: &str) -> LLMError {
        // Try to deserialize the JSON into PluginError first
        let msg_from_json = || -> String {
            serde_json::from_str::<PluginError>(json)
                .map(|pe| match pe {
                    Self::Provider(m)
                    | Self::Auth(m)
                    | Self::InvalidRequest(m)
                    | Self::Http(m)
                    | Self::NotImplemented(m)
                    | Self::Generic(m) => m,
                    Self::Cancelled => "cancelled".to_string(),
                    Self::RateLimited { message, .. } => message,
                })
                .unwrap_or_else(|_| json.to_string())
        };

        match code {
            error_codes::PROVIDER => LLMError::ProviderError(msg_from_json()),
            error_codes::AUTH => LLMError::AuthError(msg_from_json()),
            error_codes::INVALID_REQUEST => LLMError::InvalidRequest(msg_from_json()),
            error_codes::RATE_LIMITED => {
                if let Ok(PluginError::RateLimited {
                    message,
                    retry_after_secs,
                }) = serde_json::from_str::<PluginError>(json)
                {
                    LLMError::RateLimited {
                        message,
                        retry_after_secs,
                    }
                } else {
                    LLMError::RateLimited {
                        message: msg_from_json(),
                        retry_after_secs: None,
                    }
                }
            }
            error_codes::HTTP => LLMError::HttpError(msg_from_json()),
            error_codes::CANCELLED => LLMError::Cancelled,
            error_codes::NOT_IMPLEMENTED => LLMError::NotImplemented(msg_from_json()),
            _ => LLMError::PluginError(msg_from_json()),
        }
    }
}

// ============================================================================
// HTTP streaming result type
// ============================================================================

/// Result of opening an HTTP stream, returned by qmt_http_stream_open.
///
/// Using a result type avoids WASM traps for recoverable HTTP errors.
/// The host function returns `Ok(())` with this serialized in the output,
/// allowing the guest to handle errors gracefully and propagate them via
/// `WithReturnCode` with proper error codes.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "status")]
pub enum StreamOpenResult {
    /// Stream opened successfully
    #[serde(rename = "ok")]
    Ok { stream_id: i64 },

    /// Stream open was cancelled (e.g., by user or cancellation signal)
    #[serde(rename = "cancelled")]
    Cancelled,

    /// Stream open failed with an error (e.g., HTTP 429, auth error, etc.)
    #[serde(rename = "error")]
    Error {
        /// Serialized PluginError JSON
        plugin_error: String,
        /// Error code from error_codes module
        error_code: i32,
    },
}

/// Log record transported from an Extism WASM plugin to the host.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExtismLogRecord {
    /// Numeric log level (Error=1, Warn=2, Info=3, Debug=4, Trace=5)
    pub level: usize,
    /// Original plugin log target.
    pub target: String,
    /// Formatted log message.
    pub message: String,
}

pub trait BinaryCodec {
    type Bytes: AsRef<[u8]>;
    type Error;

    fn to_bytes(&self) -> Result<Self::Bytes, Self::Error>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Error>
    where
        Self: Sized;
}

#[allow(dead_code)]
pub trait FromBytesOwned: Sized {
    type Error;

    fn from_bytes_owned(bytes: &[u8]) -> Result<Self, Self::Error>;
}

#[derive(Deserialize, Serialize)]
pub struct ExtismChatRequest<C> {
    pub cfg: C,
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<Tool>>,
}

#[derive(Serialize, Deserialize)]
pub struct ExtismEmbedRequest<C> {
    pub cfg: C,
    pub inputs: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct ExtismCompleteRequest<C> {
    pub cfg: C,
    pub req: CompletionRequest,
}

#[derive(Deserialize, Serialize)]
pub struct ExtismSttRequest<C> {
    pub cfg: C,
    pub audio_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl<C> ExtismSttRequest<C> {
    pub fn into_stt_request(self) -> Result<stt::SttRequest, crate::error::LLMError> {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

        let audio = BASE64
            .decode(self.audio_base64)
            .map_err(|e| crate::error::LLMError::InvalidRequest(e.to_string()))?;

        Ok(stt::SttRequest {
            audio,
            filename: self.filename,
            mime_type: self.mime_type,
            model: self.model,
            language: self.language,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExtismSttResponse {
    pub text: String,
}

#[derive(Deserialize, Serialize)]
pub struct ExtismTtsRequest<C> {
    pub cfg: C,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExtismTtsResponse {
    pub audio_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl ExtismTtsResponse {
    pub fn into_tts_response(self) -> Result<tts::TtsResponse, crate::error::LLMError> {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

        let audio = BASE64
            .decode(self.audio_base64)
            .map_err(|e| crate::error::LLMError::InvalidRequest(e.to_string()))?;

        Ok(tts::TtsResponse {
            audio,
            mime_type: self.mime_type,
        })
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ExtismChatResponse {
    pub text: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub thinking: Option<String>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<FinishReason>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExtismChatChunk {
    pub chunk: crate::chat::StreamChunk,
    pub usage: Option<Usage>,
}

impl fmt::Display for ExtismChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // If there’s a top‐level `text`, show that…
        if let Some(ref txt) = self.text {
            write!(f, "{}", txt)
        } else {
            // …otherwise Fall back to Debug or JSON:
            write!(f, "{:?}", self)
        }
    }
}

impl ChatResponse for ExtismChatResponse {
    fn text(&self) -> Option<String> {
        self.text.clone()
    }
    fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        self.tool_calls.clone()
    }
    fn thinking(&self) -> Option<String> {
        self.thinking.clone()
    }
    fn usage(&self) -> Option<Usage> {
        self.usage.clone()
    }
    fn finish_reason(&self) -> Option<FinishReason> {
        self.finish_reason
    }
}

impl From<Box<dyn ChatResponse>> for ExtismChatResponse {
    fn from(r: Box<dyn ChatResponse>) -> Self {
        ExtismChatResponse {
            text: r.text(),
            tool_calls: r.tool_calls(),
            thinking: r.thinking(),
            usage: r.usage(),
            finish_reason: r.finish_reason(),
        }
    }
}

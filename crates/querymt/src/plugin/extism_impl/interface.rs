use crate::{
    ToolCall, Usage,
    chat::{ChatMessage, ChatResponse, FinishReason, Tool},
    completion::CompletionRequest,
    error::{LLMError, LLMErrorPayload},
    stt, tts,
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
    pub const STRUCTURED: i32 = 1;
}

/// Structured error that crosses the WASM boundary as JSON.
///
/// On the plugin side, [`LLMError`] is converted into a serializable payload and
/// sent as JSON paired with a stable code. On the host side, the payload is
/// reconstructed back into the original [`LLMError`] without stringifying it.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{payload:?}")]
pub struct PluginError {
    pub payload: LLMErrorPayload,
}

impl PluginError {
    /// Convert an [`LLMError`] into a `PluginError` for WASM transport.
    pub fn from_llm_error(err: &LLMError) -> Self {
        Self {
            payload: err.to_payload(),
        }
    }

    /// Error code for this variant, matching [`error_codes`].
    pub fn code(&self) -> i32 {
        error_codes::STRUCTURED
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
    pub fn decode(code: i32, json: &str) -> LLMError {
        if code == error_codes::STRUCTURED {
            return serde_json::from_str::<PluginError>(json)
                .map(|pe| LLMError::from_payload(pe.payload))
                .unwrap_or_else(|_| LLMError::PluginError(json.to_string()));
        }
        LLMError::PluginError(json.to_string())
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
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

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

/// Voice configuration for the extism WASM boundary.
///
/// Mirrors [`tts::VoiceConfig`] but encodes audio bytes as base64 strings
/// so the payload stays JSON-friendly across the WASM boundary (same pattern
/// as [`ExtismSttRequest::audio_base64`]).
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtismVoiceConfig {
    Preset {
        name: String,
    },
    Clone {
        /// Base64-encoded reference audio.
        reference_audio_base64: String,
        reference_text: String,
    },
    Design {
        description: String,
    },
}

impl ExtismVoiceConfig {
    /// Convert from the core [`tts::VoiceConfig`] to the extism wire format.
    pub fn from_voice_config(vc: &tts::VoiceConfig) -> Self {
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

        match vc {
            tts::VoiceConfig::Preset { name } => Self::Preset { name: name.clone() },
            tts::VoiceConfig::Clone {
                reference_audio,
                reference_text,
            } => Self::Clone {
                reference_audio_base64: BASE64.encode(reference_audio),
                reference_text: reference_text.clone(),
            },
            tts::VoiceConfig::Design { description } => Self::Design {
                description: description.clone(),
            },
        }
    }

    /// Convert back to the core [`tts::VoiceConfig`], decoding base64 audio.
    pub fn into_voice_config(self) -> Result<tts::VoiceConfig, crate::error::LLMError> {
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

        match self {
            Self::Preset { name } => Ok(tts::VoiceConfig::Preset { name }),
            Self::Clone {
                reference_audio_base64,
                reference_text,
            } => {
                let reference_audio = BASE64
                    .decode(reference_audio_base64)
                    .map_err(|e| crate::error::LLMError::InvalidRequest(e.to_string()))?;
                Ok(tts::VoiceConfig::Clone {
                    reference_audio,
                    reference_text,
                })
            }
            Self::Design { description } => Ok(tts::VoiceConfig::Design { description }),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub struct ExtismTtsRequest<C> {
    pub cfg: C,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_config: Option<ExtismVoiceConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExtismTtsResponse {
    pub audio_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl ExtismTtsResponse {
    pub fn into_tts_response(self) -> Result<tts::TtsResponse, crate::error::LLMError> {
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

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

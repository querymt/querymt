pub use crate::chat::{ITEM_AWARE_CHAT_CONTRACT_VERSION, messages_require_item_aware_contract};
use crate::{
    ToolCall, Usage,
    chat::{ChatMessage, ChatOutput, FinishReason, Tool},
    completion::CompletionRequest,
    error::{LLMError, LLMErrorPayload},
    plugin::extism_impl::SerializableHttpResponse,
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

#[cfg(test)]
mod plugin_error_tests {
    use super::*;
    use crate::error::{ProviderErrorKind, ProviderFailure};

    #[test]
    fn structured_provider_error_round_trips_across_plugin_boundary() {
        let error = LLMError::from(
            ProviderFailure::new(ProviderErrorKind::ServerOverloaded, "plugin busy")
                .with_code(Some("server_is_overloaded".into()))
                .with_request_id(Some("req-plugin".into()))
                .with_retry_after_secs(Some(2)),
        );

        let (json, code) = PluginError::encode(&error);
        let decoded = PluginError::decode(code, &json);

        match decoded {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.kind(), ProviderErrorKind::ServerOverloaded);
                assert_eq!(failure.code(), Some("server_is_overloaded"));
                assert_eq!(failure.request_id(), Some("req-plugin"));
                assert_eq!(failure.retry_after_secs(), Some(2));
            }
            other => panic!("expected ProviderResponseError, got {other}"),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_aware_contract_version: Option<u32>,
}

#[derive(Deserialize, Serialize)]
pub struct ExtismChatParseRequest<C> {
    pub cfg: C,
    pub resp: SerializableHttpResponse,
}

#[derive(Deserialize, Serialize)]
pub struct ExtismChatChunkParseRequest {
    pub parser_id: i64,
    pub chunk: Vec<u8>,
}

#[derive(Deserialize, Serialize)]
pub struct ExtismCompleteParseRequest<C> {
    pub cfg: C,
    pub resp: SerializableHttpResponse,
}

#[derive(Deserialize, Serialize)]
pub struct ExtismEmbedParseRequest<C> {
    pub cfg: C,
    pub resp: SerializableHttpResponse,
}

#[derive(Deserialize, Serialize)]
pub struct ExtismListModelsParseRequest {
    pub resp: SerializableHttpResponse,
}

#[derive(Serialize, Deserialize)]
pub struct ExtismEmbedRequest<C> {
    pub cfg: C,
    pub inputs: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ExtismListModelsRequest {
    pub cfg: serde_json::Value,
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

/// Canonical chat response crossing the Extism boundary.
///
/// The in-memory value has one authority. Serialization retains the shipped
/// flattened projection alongside `output` until the contract version changes;
/// deserialization accepts both old flattened and current canonical responses.
#[derive(Debug)]
pub struct ExtismChatResponse {
    pub output: ChatOutput,
}

#[derive(Deserialize)]
struct ExtismChatResponseDto {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    finish_reason: Option<FinishReason>,
    #[serde(default)]
    output: Option<ChatOutput>,
}

impl Serialize for ExtismChatResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("ExtismChatResponse", 6)?;
        state.serialize_field("text", &self.output.text())?;
        state.serialize_field("tool_calls", &self.output.tool_calls())?;
        state.serialize_field("thinking", &self.output.thinking())?;
        state.serialize_field("usage", &self.output.usage)?;
        state.serialize_field("finish_reason", &self.output.finish_reason)?;
        state.serialize_field("output", &self.output)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ExtismChatResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let dto = ExtismChatResponseDto::deserialize(deserializer)?;
        Ok(Self {
            output: dto.output.unwrap_or_else(|| {
                ChatOutput::from_projections(
                    dto.thinking,
                    dto.text,
                    dto.tool_calls,
                    dto.usage,
                    dto.finish_reason,
                )
            }),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtismChatChunk {
    pub chunk: crate::chat::StreamChunk,
}

impl Serialize for ExtismChatChunk {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let usage = match &self.chunk {
            crate::chat::StreamChunk::Usage(usage) => Some(usage),
            _ => None,
        };
        let mut state = serializer.serialize_struct("ExtismChatChunk", 2)?;
        state.serialize_field("chunk", &self.chunk)?;
        state.serialize_field("usage", &usage)?;
        state.end()
    }
}

impl fmt::Display for ExtismChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.output)
    }
}

impl ExtismChatResponse {
    pub fn into_canonical_output(self) -> ChatOutput {
        self.output
    }

    pub fn output(&self) -> &ChatOutput {
        &self.output
    }
}

impl From<ChatOutput> for ExtismChatResponse {
    fn from(output: ChatOutput) -> Self {
        ExtismChatResponse { output }
    }
}

#[cfg(test)]
mod item_aware_tests {
    use super::*;
    use crate::chat::{ChatOutput, ChatOutputStatus};
    use serde_json::json;

    #[test]
    fn legacy_extism_payloads_default_item_aware_fields() {
        let request: ExtismChatRequest<serde_json::Value> = serde_json::from_value(json!({
            "cfg": {},
            "messages": [],
            "tools": null
        }))
        .expect("legacy request");
        assert_eq!(request.item_aware_contract_version, None);

        let response: ExtismChatResponse = serde_json::from_value(json!({
            "text": "legacy",
            "tool_calls": null,
            "thinking": null,
            "usage": null,
            "finish_reason": "Stop"
        }))
        .expect("legacy response");
        assert_eq!(response.output.text().as_deref(), Some("legacy"));

        let legacy_history: ExtismChatRequest<serde_json::Value> = serde_json::from_value(json!({
            "cfg": {},
            "messages": [{
                "role": "User",
                "content": [{"type": "text", "text": "hello"}]
            }],
            "tools": null
        }))
        .expect("legacy history request");
        let saved = serde_json::to_value(&legacy_history).expect("serialize canonical request");
        let message = &saved["messages"][0];
        assert!(message.get("content").is_none());
        assert_eq!(message["input"][0]["type"], "text");
        assert_eq!(message["input"][0]["text"], "hello");
    }

    #[test]
    fn structured_extism_response_round_trips_losslessly() {
        let output = ChatOutput {
            response_id: Some("response-1".into()),
            items: vec![crate::chat::ChatOutputItem::FunctionCall(
                crate::chat::ChatFunctionCallItem {
                    item_id: Some("item-1".into()),
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    arguments: "{not valid json".into(),
                    status: Some(ChatOutputStatus::Completed),
                    extensions: Default::default(),
                },
            )],
            status: Some(ChatOutputStatus::Completed),
            extensions: [("provider_field".into(), json!({"nested": true}))]
                .into_iter()
                .collect(),
            ..ChatOutput::default()
        };
        let response = ExtismChatResponse::from(output.clone());

        let encoded = serde_json::to_vec(&response).expect("serialize response");
        let encoded_value: serde_json::Value =
            serde_json::from_slice(&encoded).expect("response JSON");
        assert_eq!(encoded_value.as_object().unwrap().len(), 6);
        assert!(encoded_value.get("output").is_some());
        assert_eq!(encoded_value.get("text"), Some(&serde_json::Value::Null));
        assert!(encoded_value["tool_calls"].is_array());

        let decoded: ExtismChatResponse =
            serde_json::from_slice(&encoded).expect("deserialize response");
        assert_eq!(decoded.output(), &output);
        let decoded_call = match &decoded.output().items[0] {
            crate::chat::ChatOutputItem::FunctionCall(call) => call,
            other => panic!("expected function call, got {other:?}"),
        };
        assert_eq!(decoded_call.item_id.as_deref(), Some("item-1"));
        assert_eq!(decoded_call.call_id, "call-1");
        assert_eq!(decoded_call.arguments, "{not valid json");
    }

    /// End-to-end over the Extism transport: a structured assistant response is
    /// carried back as a request history item (response -> reload -> tool result
    /// -> second request) with item/call IDs, raw arguments, and order intact.
    #[test]
    fn structured_extism_history_round_trips_into_second_request() {
        use crate::chat::{
            ChatFunctionCallItem, ChatMessage, ChatMessageItem, ChatMessagePart, ChatOutputItem,
            ChatReasoningItem, ChatReasoningPart,
        };

        let raw_arguments = "{\"query\":\"rust\",\"limit\": 2 }";
        let output = ChatOutput {
            response_id: Some("resp-extism".into()),
            status: Some(ChatOutputStatus::Completed),
            items: vec![
                ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: Some("reasoning-extism".into()),
                    summary: vec![ChatReasoningPart::text("thinking")],
                    content: Vec::new(),
                    encrypted_content: Some("encrypted-continuation".into()),
                    signature: None,
                    status: None,
                    extensions: Default::default(),
                }),
                ChatOutputItem::Message(ChatMessageItem {
                    id: Some("message-extism".into()),
                    role: crate::chat::ChatRole::Assistant,
                    phase: None,
                    status: None,
                    parts: vec![ChatMessagePart::Text {
                        text: "running lookup".into(),
                        annotations: Vec::new(),
                        extensions: Default::default(),
                    }],
                    extensions: Default::default(),
                }),
                ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                    item_id: Some("fc-item-extism".into()),
                    call_id: "call-extism".into(),
                    name: "lookup".into(),
                    arguments: raw_arguments.into(),
                    status: None,
                    extensions: Default::default(),
                }),
            ],
            ..ChatOutput::default()
        };

        let assistant = ChatMessage::from_assistant_output(output);

        // Second request carries the structured turn plus its tool result.
        let mut tool_result = crate::chat::ToolResult::new("call-extism");
        tool_result.name = Some("lookup".into());
        tool_result.parts.push(crate::chat::ToolResultPart::Text {
            text: "result payload".into(),
        });
        let result = ChatMessage::from_user_parts(vec![crate::chat::ChatInputPart::tool_result(
            tool_result,
        )]);
        let request = ExtismChatRequest {
            cfg: json!({}),
            messages: vec![
                ChatMessage::from_user_parts(vec![crate::chat::ChatInputPart::text("look it up")]),
                assistant,
                result,
            ],
            tools: None,
            item_aware_contract_version: None,
        };
        assert!(
            messages_require_item_aware_contract(&request.messages),
            "structured history must advertise the item-aware contract"
        );

        let encoded = serde_json::to_vec(&request).expect("serialize request");
        let decoded: ExtismChatRequest<serde_json::Value> =
            serde_json::from_slice(&encoded).expect("deserialize request");

        let reloaded = &decoded.messages[1];
        let reloaded_output = reloaded.output().expect("structured output survives");
        assert_eq!(
            reloaded_output.items.len(),
            3,
            "no duplicate projected items across the plugin boundary"
        );

        let ChatOutputItem::Reasoning(reasoning) = &reloaded_output.items[0] else {
            panic!("expected reasoning item");
        };
        assert_eq!(
            reasoning.encrypted_content.as_deref(),
            Some("encrypted-continuation"),
            "encrypted continuation survives the plugin boundary"
        );

        let ChatOutputItem::FunctionCall(call) = &reloaded_output.items[2] else {
            panic!("expected function call item");
        };
        assert_eq!(call.call_id, "call-extism");
        assert_eq!(call.item_id.as_deref(), Some("fc-item-extism"));
        assert_eq!(call.arguments, raw_arguments, "raw arguments byte-exact");

        // The tool result references the original call ID.
        let tool_result = decoded.messages[2]
            .input()
            .expect("canonical tool result input")
            .iter()
            .find_map(|part| match part {
                crate::chat::ChatInputPart::ToolResult(result) => Some(result),
                _ => None,
            })
            .expect("tool result present");
        assert_eq!(tool_result.call_id, "call-extism");
    }
}

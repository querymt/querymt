use querymt::ToolCall;
use querymt::Usage;
use querymt::chat::{ChatMessage, ChatOutput, FinishReason, StreamChunk, Tool};
pub use querymt::chat::{ITEM_AWARE_CHAT_CONTRACT_VERSION, messages_require_item_aware_contract};
use querymt::error::LLMErrorPayload;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct GetProviderContractInfo;

/// Wire-protocol generation advertised by mesh provider hosts.
///
/// Bump whenever a remote message, reply, or error type changes shape in a
/// way peers built from older revisions cannot decode. Clients verify this
/// via [`GetProviderContractInfo`] before the first provider call and fail
/// fast with an actionable message on mismatch instead of surfacing opaque
/// MessagePack decoding complaints.
pub const MESH_PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderContractInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_aware_chat_version: Option<u32>,
    /// Wire-protocol generation of the answering peer. Absent on peers built
    /// before protocol versioning; the client treats that as a mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ProviderChatResponse {
    pub output: ChatOutput,
}

#[derive(Deserialize)]
struct ProviderChatResponseDto {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    output: Option<ChatOutput>,
}

impl Serialize for ProviderChatResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("ProviderChatResponse", 6)?;
        state.serialize_field("text", &self.output.text())?;
        state.serialize_field("thinking", &self.output.thinking())?;
        state.serialize_field("tool_calls", &self.output.tool_calls().unwrap_or_default())?;
        state.serialize_field("usage", &self.output.usage)?;
        state.serialize_field(
            "finish_reason",
            &self
                .output
                .finish_reason
                .map(|reason| format!("{reason:?}")),
        )?;
        state.serialize_field("output", &self.output)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ProviderChatResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let dto = ProviderChatResponseDto::deserialize(deserializer)?;
        let output = dto.output.unwrap_or_else(|| {
            let finish_reason = dto.finish_reason.as_deref().map(parse_finish_reason);
            let tool_calls = (!dto.tool_calls.is_empty()).then_some(dto.tool_calls);
            ChatOutput::from_projections(
                dto.thinking,
                dto.text,
                tool_calls,
                dto.usage,
                finish_reason,
            )
        });
        Ok(Self { output })
    }
}

fn parse_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "Stop" => FinishReason::Stop,
        "Length" => FinishReason::Length,
        "ContentFilter" => FinishReason::ContentFilter,
        "ToolCalls" => FinishReason::ToolCalls,
        "Error" => FinishReason::Error,
        "Other" => FinishReason::Other,
        _ => FinishReason::Unknown,
    }
}

impl fmt::Display for ProviderChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.output.text().as_deref().unwrap_or("[no text]"))
    }
}

impl ProviderChatResponse {
    pub fn into_canonical_output(self) -> ChatOutput {
        self.output
    }

    pub fn output(&self) -> &ChatOutput {
        &self.output
    }
}

impl From<ChatOutput> for ProviderChatResponse {
    fn from(output: ChatOutput) -> Self {
        ProviderChatResponse { output }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum StreamRelayMessage {
    Chunk(StreamChunk),
    ChunkBatch(Vec<StreamChunk>),
    Heartbeat {
        phase: ProviderStreamPhase,
        elapsed_ms: u64,
        idle_ms: u64,
        chunk_count: u64,
    },
    ProviderError {
        error: LLMErrorPayload,
    },
    TransportDisconnected {
        reason: String,
    },
    TransportReconnected {
        buffered_chunks: usize,
    },
    TransportFailed {
        error: LLMErrorPayload,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunkRelay {
    pub message: StreamRelayMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderChatRequest {
    pub provider: String,
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_aware_contract_version: Option<u32>,
}

pub type GenericProviderStreamRequest<TRouterRef> = ProviderStreamRequest<TRouterRef>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStreamRequest<TRouterRef> {
    pub provider: String,
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<Tool>>,
    pub session_id: String,
    pub request_id: String,
    pub stream_router_ref: TRouterRef,
    pub reconnect_grace_secs: u64,
    #[serde(default = "default_stream_heartbeat_secs")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_stream_lease_ttl_secs")]
    pub lease_ttl_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_aware_contract_version: Option<u32>,
}

pub fn default_stream_heartbeat_secs() -> u64 {
    10
}

pub fn default_stream_lease_ttl_secs() -> u64 {
    60
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamPhase {
    OpeningUpstream,
    WaitingFirstChunk,
    Streaming,
    ReceiverDisconnected,
    GraceExpired,
    LeaseExpired,
    Cancelling,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStreamStatus {
    pub session_id: String,
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub phase: ProviderStreamPhase,
    pub elapsed_ms: u64,
    pub idle_ms: u64,
    pub chunk_count: u64,
    pub receiver_connected: bool,
    pub lease_expires_in_ms: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelProviderStreamRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewProviderStreamLease {
    pub session_id: String,
    pub request_id: String,
    pub lease_ttl_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetProviderStreamStatus {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

pub fn keep_stream_message_buffered(message: &StreamRelayMessage) -> bool {
    !matches!(
        message,
        StreamRelayMessage::Heartbeat { .. }
            | StreamRelayMessage::TransportDisconnected { .. }
            | StreamRelayMessage::TransportReconnected { .. }
    )
}

pub fn relay_message_is_terminal(message: &StreamRelayMessage) -> bool {
    matches!(
        message,
        StreamRelayMessage::Chunk(chunk) if querymt::chat::chunk_is_terminal(chunk)
    ) || matches!(
        message,
        StreamRelayMessage::ProviderError { .. } | StreamRelayMessage::TransportFailed { .. }
    ) || matches!(
        message,
        StreamRelayMessage::ChunkBatch(chunks)
            if chunks.iter().any(querymt::chat::chunk_is_terminal)
    )
}

pub fn should_ack_relay_message(
    message: &StreamRelayMessage,
    unacked_batches: u32,
    last_ack_at: std::time::Duration,
    ack_window_batches: u32,
    ack_window_interval: std::time::Duration,
) -> bool {
    if relay_message_is_terminal(message) {
        return true;
    }

    match message {
        StreamRelayMessage::Chunk(_) | StreamRelayMessage::ChunkBatch(_) => {
            unacked_batches >= ack_window_batches || last_ack_at >= ack_window_interval
        }
        StreamRelayMessage::Heartbeat { .. }
        | StreamRelayMessage::TransportDisconnected { .. }
        | StreamRelayMessage::TransportReconnected { .. } => false,
        StreamRelayMessage::ProviderError { .. } | StreamRelayMessage::TransportFailed { .. } => {
            true
        }
    }
}

#[cfg(test)]
mod item_aware_tests {
    use super::*;
    use querymt::chat::{
        ChatFunctionCallItem, ChatMessage, ChatOutput, ChatOutputItem, ChatOutputStatus,
        ChatStreamAccumulator, StructuredStreamEvent,
    };
    use serde_json::json;

    fn structured_message() -> ChatMessage {
        let mut message = ChatMessage::assistant().build();
        message
            .replace_output(ChatOutput {
                response_id: Some("response-1".into()),
                status: Some(ChatOutputStatus::Completed),
                // A provider-only identity makes fidelity item-aware.
                items: vec![ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                    item_id: Some("item-1".into()),
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    arguments: "{}".into(),
                    status: None,
                    extensions: Default::default(),
                })],
                extensions: [("opaque".into(), json!({"preserved": true}))]
                    .into_iter()
                    .collect(),
                ..ChatOutput::default()
            })
            .expect("structured assistant message");
        message
    }

    #[test]
    fn legacy_remote_payloads_default_item_aware_fields() {
        let request: ProviderChatRequest = serde_json::from_value(json!({
            "provider": "demo",
            "model": "m1",
            "messages": [],
            "tools": null
        }))
        .expect("legacy request");
        assert_eq!(request.item_aware_contract_version, None);

        let response: ProviderChatResponse = serde_json::from_value(json!({
            "text": "legacy",
            "thinking": null,
            "tool_calls": [],
            "usage": null,
            "finish_reason": "Stop"
        }))
        .expect("legacy response");
        assert_eq!(response.output.text().as_deref(), Some("legacy"));

        let legacy_history: ProviderChatRequest = serde_json::from_value(json!({
            "provider": "demo",
            "model": "m1",
            "messages": [{
                "role": "Assistant",
                "content": [{"type": "text", "text": "legacy answer"}]
            }],
            "tools": null
        }))
        .expect("legacy history request");
        let saved = serde_json::to_value(&legacy_history).expect("serialize canonical request");
        let message = &saved["messages"][0];
        assert!(message.get("content").is_none());
        assert!(message.get("input").is_none());
        assert_eq!(message["output"]["items"][0]["type"], "message");
    }

    #[test]
    fn structured_remote_response_and_stream_event_round_trip() {
        let output = ChatOutput {
            items: vec![ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                item_id: Some("item-1".into()),
                call_id: "call-1".into(),
                name: "lookup".into(),
                arguments: "{not valid json".into(),
                status: Some(ChatOutputStatus::Completed),
                extensions: Default::default(),
            })],
            ..structured_message().output().expect("output").clone()
        };
        let response = ProviderChatResponse::from(output.clone());
        let encoded = serde_json::to_vec(&response).expect("serialize response");
        let encoded_value: serde_json::Value =
            serde_json::from_slice(&encoded).expect("response JSON");
        assert_eq!(encoded_value.as_object().unwrap().len(), 6);
        assert!(encoded_value.get("output").is_some());
        assert!(encoded_value["tool_calls"].is_array());
        assert!(encoded_value["finish_reason"].is_null());

        let decoded: ProviderChatResponse =
            serde_json::from_slice(&encoded).expect("deserialize response");
        assert_eq!(decoded.output(), &output);
        let decoded_call = match &decoded.output().items[0] {
            ChatOutputItem::FunctionCall(call) => call,
            other => panic!("expected function call, got {other:?}"),
        };
        assert_eq!(decoded_call.item_id.as_deref(), Some("item-1"));
        assert_eq!(decoded_call.call_id, "call-1");
        assert_eq!(decoded_call.arguments, "{not valid json");

        let events = [
            StructuredStreamEvent::ResponseMetadata {
                response_id: Some("response-1".into()),
                status: Some(ChatOutputStatus::InProgress),
                usage: None,
                finish_reason: None,
                provenance: None,
            },
            StructuredStreamEvent::ItemCompleted {
                output_index: 0,
                item: output.items[0].clone(),
            },
            StructuredStreamEvent::ResponseTerminal {
                status: ChatOutputStatus::Completed,
                usage: None,
                finish_reason: Some(FinishReason::Stop),
                detail: None,
            },
        ];
        let mut accumulator = ChatStreamAccumulator::new();
        for event in events {
            let relay = StreamRelayMessage::Chunk(StreamChunk::Structured(event));
            let encoded = serde_json::to_vec(&relay).expect("serialize chunk");
            let decoded: StreamRelayMessage =
                serde_json::from_slice(&encoded).expect("deserialize chunk");
            let StreamRelayMessage::Chunk(chunk) = decoded else {
                panic!("expected relayed chunk")
            };
            accumulator.push(&chunk).expect("accumulate chunk");
        }
        let streamed = accumulator.finish_success().expect("complete stream");
        assert_eq!(streamed.items, output.items);
    }

    #[test]
    fn requests_advertise_item_aware_contract_only_when_needed() {
        let config = crate::RemoteProviderClientConfig::new("peer", "demo", "m1");
        let legacy = config.build_chat_request(&[ChatMessage::user().text("hi").build()], None);
        assert_eq!(legacy.item_aware_contract_version, None);

        let structured = config.build_chat_request(&[structured_message()], None);
        assert_eq!(
            structured.item_aware_contract_version,
            Some(ITEM_AWARE_CHAT_CONTRACT_VERSION)
        );
    }

    /// End-to-end over the remote transport: a structured response is carried
    /// back as request history (response -> reload -> tool result -> second
    /// request) with item/call IDs, raw arguments, reasoning continuation, and
    /// order intact, and with no duplicate projected items.
    #[test]
    fn structured_remote_history_round_trips_into_second_request() {
        use querymt::chat::{
            ChatFunctionCallItem, ChatInputPart, ChatMessageItem, ChatMessagePart, ChatOutputItem,
            ChatReasoningItem, ChatReasoningPart, ToolResult, ToolResultPart,
        };

        let config = crate::RemoteProviderClientConfig::new("peer", "demo", "m1");
        let raw_arguments = "{\"query\":\"rust\",\"limit\": 2 }";

        // The remote returns structured output.
        let output = ChatOutput {
            response_id: Some("resp-remote".into()),
            status: Some(ChatOutputStatus::Completed),
            items: vec![
                ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: Some("reasoning-remote".into()),
                    summary: vec![ChatReasoningPart::text("thinking")],
                    content: Vec::new(),
                    encrypted_content: Some("encrypted-continuation".into()),
                    signature: None,
                    status: None,
                    extensions: Default::default(),
                }),
                ChatOutputItem::Message(ChatMessageItem {
                    id: Some("message-remote".into()),
                    role: querymt::chat::ChatRole::Assistant,
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
                    item_id: Some("fc-item-remote".into()),
                    call_id: "call-remote".into(),
                    name: "lookup".into(),
                    arguments: raw_arguments.into(),
                    status: None,
                    extensions: Default::default(),
                }),
            ],
            ..ChatOutput::default()
        };
        let response = ProviderChatResponse::from(output.clone());
        let encoded = serde_json::to_vec(&response).expect("serialize response");
        let decoded: ProviderChatResponse =
            serde_json::from_slice(&encoded).expect("deserialize response");
        let decoded_output = decoded.output();
        assert_eq!(decoded_output.items.len(), 3);

        // Reload the structured turn into request history plus its tool result.
        let assistant = ChatMessage::from_assistant_output(decoded_output.clone());

        let mut tool_result = ToolResult::new("call-remote".to_string());
        tool_result.name = Some("lookup".into());
        tool_result
            .parts
            .push(ToolResultPart::text("result payload"));
        let result = ChatMessage::from_user_parts(vec![ChatInputPart::tool_result(tool_result)]);

        let request = config.build_chat_request(
            &[
                ChatMessage::from_user_parts(vec![ChatInputPart::text("look it up")]),
                assistant,
                result,
            ],
            None,
        );
        assert_eq!(
            request.item_aware_contract_version,
            Some(ITEM_AWARE_CHAT_CONTRACT_VERSION),
            "structured history must advertise the item-aware contract"
        );

        // Round-trip the request across the transport and inspect the reloaded
        // structured turn.
        let encoded = serde_json::to_vec(&request).expect("serialize request");
        let decoded: ProviderChatRequest =
            serde_json::from_slice(&encoded).expect("deserialize request");

        let reloaded = &decoded.messages[1];
        let reloaded_output = reloaded
            .output()
            .expect("structured output survives the transport");
        assert_eq!(
            reloaded_output.items.len(),
            3,
            "no duplicate projected items across the remote boundary"
        );

        let ChatOutputItem::Reasoning(reasoning) = &reloaded_output.items[0] else {
            panic!("expected reasoning item");
        };
        assert_eq!(
            reasoning.encrypted_content.as_deref(),
            Some("encrypted-continuation"),
            "reasoning continuation survives the remote boundary"
        );

        let ChatOutputItem::FunctionCall(call) = &reloaded_output.items[2] else {
            panic!("expected function call item");
        };
        assert_eq!(call.call_id, "call-remote");
        assert_eq!(call.item_id.as_deref(), Some("fc-item-remote"));
        assert_eq!(call.arguments, raw_arguments, "raw arguments byte-exact");

        // The tool result still references the original call ID.
        let tool_result = decoded.messages[2]
            .portable_input_parts()
            .into_iter()
            .find_map(|part| match part {
                querymt::chat::ChatInputPart::ToolResult(result) => Some(result),
                _ => None,
            })
            .expect("tool result present");
        assert_eq!(tool_result.call_id, "call-remote");
    }
}

/// Round trips that use kameo's exact wire codec pair: the client encodes
/// messages with [`rmp_serde::to_vec_named`] and the host decodes them with
/// [`rmp_serde::decode::from_slice`]. MessagePack writes the map header with
/// the *declared* field count before any entry, so a manual `Serialize` impl
/// whose declared length diverges from the fields it actually writes corrupts
/// the stream. serde_json cannot catch that class of bug because JSON ignores
/// the declared length entirely.
#[cfg(test)]
mod rmp_wire_tests {
    use super::*;
    use querymt::chat::{CacheHint, ChatInputPart};

    fn assert_rmp_round_trip_equal<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let encoded = rmp_serde::to_vec_named(value).expect("rmp named encode");
        let decoded: T = rmp_serde::decode::from_slice(&encoded).expect("rmp decode");
        let original = serde_json::to_value(value).expect("original json value");
        let decoded = serde_json::to_value(&decoded).expect("decoded json value");
        assert_eq!(original, decoded);
    }

    fn chat_request(messages: Vec<ChatMessage>) -> ProviderChatRequest {
        ProviderChatRequest {
            provider: "llama_cpp".into(),
            model: "hf:demo/model.gguf".into(),
            messages,
            tools: None,
            params: None,
            item_aware_contract_version: None,
        }
    }

    /// Regression: `ChatMessage::serialize` used to declare three fields while
    /// writing only two whenever no cache hint was set. rmp-serde trusts the
    /// declared map length, so decoding a multi-message request consumed the
    /// second message's map header as a field identifier and failed with
    /// `invalid type: map, expected field identifier` on the remote host.
    #[test]
    fn chat_request_with_two_cache_less_messages_round_trips_over_rmp() {
        let messages = vec![
            ChatMessage::from_user_parts(vec![ChatInputPart::text("preamble")]),
            ChatMessage::from_user_parts(vec![ChatInputPart::text("question")]),
        ];
        assert_rmp_round_trip_equal(&chat_request(messages));
    }

    #[test]
    fn chat_request_with_mixed_cache_hints_round_trips_over_rmp() {
        let messages = vec![
            ChatMessage::user()
                .text("cached prefix")
                .cache(CacheHint::Ephemeral {
                    ttl_seconds: Some(300),
                })
                .build(),
            ChatMessage::from_user_parts(vec![ChatInputPart::text("follow-up")]),
        ];
        assert_rmp_round_trip_equal(&chat_request(messages));
    }

    #[test]
    fn single_chat_messages_round_trip_over_rmp() {
        assert_rmp_round_trip_equal(&ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "solo",
        )]));
        assert_rmp_round_trip_equal(
            &ChatMessage::user()
                .text("cached")
                .cache(CacheHint::Ephemeral { ttl_seconds: None })
                .build(),
        );
    }

    #[test]
    fn provider_chat_response_round_trips_over_rmp() {
        let response = ProviderChatResponse::from(ChatOutput::from_projections(
            None,
            Some("hello".to_string()),
            None,
            None,
            None,
        ));
        let decoded = {
            let encoded = rmp_serde::to_vec_named(&response).expect("rmp named encode");
            let decoded: ProviderChatResponse =
                rmp_serde::decode::from_slice(&encoded).expect("rmp decode");
            decoded
        };
        assert_eq!(decoded.output.text().as_deref(), Some("hello"));
    }

    #[test]
    fn stream_relay_messages_round_trip_over_rmp() {
        let provider_error = || LLMErrorPayload::ProviderError {
            message: "provider exploded".into(),
            kind: None,
            code: None,
            error_type: None,
            request_id: None,
            retry_after_secs: None,
        };
        let messages = vec![
            StreamRelayMessage::Chunk(StreamChunk::Text("delta".into())),
            StreamRelayMessage::ChunkBatch(vec![
                StreamChunk::Text("a".into()),
                StreamChunk::Text("b".into()),
            ]),
            StreamRelayMessage::Heartbeat {
                phase: ProviderStreamPhase::Streaming,
                elapsed_ms: 1,
                idle_ms: 2,
                chunk_count: 3,
            },
            StreamRelayMessage::ProviderError {
                error: provider_error(),
            },
            StreamRelayMessage::TransportFailed {
                error: LLMErrorPayload::Transport {
                    kind: querymt::error::TransportErrorKind::ConnectionClosed,
                    message: "link lost".into(),
                },
            },
        ];
        for message in &messages {
            assert_rmp_round_trip_equal(message);
        }
    }
}

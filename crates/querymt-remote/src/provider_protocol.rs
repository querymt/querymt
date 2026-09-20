use querymt::ToolCall;
use querymt::Usage;
use querymt::chat::{ChatMessage, ChatOutput, FinishReason, StreamChunk, Tool};
use querymt::error::LLMErrorPayload;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const ITEM_AWARE_CHAT_CONTRACT_VERSION: u32 = 1;

/// Whether any message retains item-aware-only fidelity semantics.
///
/// Derived from the concrete retained semantics, not the mere presence of a
/// canonical output value, so portable output can cross a legacy boundary.
pub fn messages_require_item_aware_contract(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|message| {
        message
            .output()
            .is_some_and(|output| output.requires_item_aware_fidelity())
    })
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct GetProviderContractInfo;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderContractInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_aware_chat_version: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderChatResponse {
    pub text: Option<String>,
    pub thinking: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ChatOutput>,
}

impl fmt::Display for ProviderChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.text {
            Some(text) => write!(f, "{}", text),
            None => write!(f, "[no text]"),
        }
    }
}

impl ProviderChatResponse {
    /// Project the canonical output, falling back to the legacy flat fields
    /// when the peer only returned a legacy payload.
    pub fn to_canonical_output(&self) -> ChatOutput {
        if let Some(output) = &self.output {
            return output.clone();
        }

        let finish_reason = self.finish_reason.as_deref().map(|reason| match reason {
            "Stop" => FinishReason::Stop,
            "Length" => FinishReason::Length,
            "ContentFilter" => FinishReason::ContentFilter,
            "ToolCalls" => FinishReason::ToolCalls,
            "Error" => FinishReason::Error,
            "Other" => FinishReason::Other,
            _ => FinishReason::Unknown,
        });
        let tool_calls = if self.tool_calls.is_empty() {
            None
        } else {
            Some(self.tool_calls.clone())
        };

        ChatOutput::from_projections(
            self.thinking.clone(),
            self.text.clone(),
            tool_calls,
            self.usage.clone(),
            finish_reason,
        )
    }

    /// Authoritative ordered output when the peer returned one.
    pub fn output(&self) -> Option<&ChatOutput> {
        self.output.as_ref()
    }
}

impl From<ChatOutput> for ProviderChatResponse {
    fn from(output: ChatOutput) -> Self {
        let text = output.text();
        let thinking = output.thinking();
        let tool_calls = output.tool_calls().unwrap_or_default();
        let usage = output.usage.clone();
        let finish_reason = output.finish_reason.map(|reason| format!("{:?}", reason));

        ProviderChatResponse {
            text,
            thinking,
            tool_calls,
            usage,
            finish_reason,
            output: Some(output),
        }
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
        assert!(response.output.is_none());

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
        let response = ProviderChatResponse {
            text: None,
            thinking: None,
            tool_calls: Vec::new(),
            usage: None,
            finish_reason: Some("Stop".into()),
            output: Some(output.clone()),
        };
        let encoded = serde_json::to_vec(&response).expect("serialize response");
        let decoded: ProviderChatResponse =
            serde_json::from_slice(&encoded).expect("deserialize response");
        assert_eq!(decoded.output(), Some(&output));
        let decoded_call = match &decoded.output().expect("output").items[0] {
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
        let response = ProviderChatResponse {
            text: output.text(),
            thinking: output.thinking(),
            tool_calls: output.tool_calls().unwrap_or_default(),
            usage: None,
            finish_reason: Some("ToolCalls".into()),
            output: Some(output.clone()),
        };
        let encoded = serde_json::to_vec(&response).expect("serialize response");
        let decoded: ProviderChatResponse =
            serde_json::from_slice(&encoded).expect("deserialize response");
        let decoded_output = decoded.output().expect("structured output survives");
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
            .input_parts()
            .into_iter()
            .find_map(|part| match part {
                querymt::chat::ChatInputPart::ToolResult(result) => Some(result),
                _ => None,
            })
            .expect("tool result present");
        assert_eq!(tool_result.call_id, "call-remote");
    }
}

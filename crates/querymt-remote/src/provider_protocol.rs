use querymt::ToolCall;
use querymt::Usage;
use querymt::chat::{ChatMessage, ChatResponse, FinishReason, StreamChunk, Tool};
use querymt::error::LLMErrorPayload;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderChatResponse {
    pub text: Option<String>,
    pub thinking: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

impl fmt::Display for ProviderChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.text {
            Some(text) => write!(f, "{}", text),
            None => write!(f, "[no text]"),
        }
    }
}

impl ChatResponse for ProviderChatResponse {
    fn text(&self) -> Option<String> {
        self.text.clone()
    }

    fn thinking(&self) -> Option<String> {
        self.thinking.clone()
    }

    fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        if self.tool_calls.is_empty() {
            None
        } else {
            Some(self.tool_calls.clone())
        }
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        self.finish_reason.as_deref().map(|reason| match reason {
            "Stop" => FinishReason::Stop,
            "Length" => FinishReason::Length,
            "ContentFilter" => FinishReason::ContentFilter,
            "ToolCalls" => FinishReason::ToolCalls,
            "Error" => FinishReason::Error,
            "Other" => FinishReason::Other,
            _ => FinishReason::Unknown,
        })
    }

    fn usage(&self) -> Option<Usage> {
        self.usage.clone()
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
        StreamRelayMessage::Chunk(StreamChunk::Done { .. })
            | StreamRelayMessage::ProviderError { .. }
            | StreamRelayMessage::TransportFailed { .. }
    ) || matches!(
        message,
        StreamRelayMessage::ChunkBatch(chunks)
            if chunks.iter().any(|chunk| matches!(chunk, StreamChunk::Done { .. }))
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

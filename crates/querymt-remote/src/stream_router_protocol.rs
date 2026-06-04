use crate::StreamRelayMessage;
use querymt::chat::StreamChunk;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutedStreamRelayMessage {
    pub request_id: String,
    pub message: StreamRelayMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRouterStatus {
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutedRequestStatus {
    pub request_id: String,
    pub has_consumer: bool,
    pub buffered_messages: usize,
    pub phase: RequestPhase,
    pub created_at_elapsed_ms: u64,
    pub last_message_at_elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestPhase {
    AwaitingStream,
    Streaming,
    ConsumerDisconnected,
    Completed,
    Failed,
    Cancelled,
}

pub fn terminal_request_phase(message: &StreamRelayMessage) -> Option<RequestPhase> {
    match message {
        StreamRelayMessage::Chunk(StreamChunk::Done { .. }) => Some(RequestPhase::Completed),
        StreamRelayMessage::ChunkBatch(chunks)
            if chunks.iter().any(|c| matches!(c, StreamChunk::Done { .. })) =>
        {
            Some(RequestPhase::Completed)
        }
        StreamRelayMessage::ProviderError { .. } | StreamRelayMessage::TransportFailed { .. } => {
            Some(RequestPhase::Failed)
        }
        _ => None,
    }
}

impl fmt::Display for RequestPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestPhase::AwaitingStream => write!(f, "awaiting_stream"),
            RequestPhase::Streaming => write!(f, "streaming"),
            RequestPhase::ConsumerDisconnected => write!(f, "consumer_disconnected"),
            RequestPhase::Completed => write!(f, "completed"),
            RequestPhase::Failed => write!(f, "failed"),
            RequestPhase::Cancelled => write!(f, "cancelled"),
        }
    }
}

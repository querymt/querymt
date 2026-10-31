use crate::StreamRelayMessage;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutedStreamRelayMessage {
    pub request_id: String,
    pub message: StreamRelayMessage,
    /// Per-request host sequence number for data, terminal and stream-failure
    /// messages. `None` for heartbeats, connection notices and peers without
    /// sequencing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
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
        StreamRelayMessage::Chunk(chunk) if querymt::chat::chunk_is_terminal(chunk) => {
            Some(RequestPhase::Completed)
        }
        StreamRelayMessage::ChunkBatch(chunks)
            if chunks.iter().any(querymt::chat::chunk_is_terminal) =>
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

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::StreamChunk;

    /// The wire shape before `seq` was added.
    #[derive(Debug, Serialize, Deserialize)]
    struct OldRoutedStreamRelayMessage {
        request_id: String,
        message: StreamRelayMessage,
    }

    fn text() -> StreamRelayMessage {
        StreamRelayMessage::Chunk(StreamChunk::Text("hi".to_string()))
    }

    #[test]
    fn old_payload_decodes_with_no_seq() {
        let bytes = rmp_serde::to_vec_named(&OldRoutedStreamRelayMessage {
            request_id: "r".into(),
            message: text(),
        })
        .expect("encode old");
        let decoded: RoutedStreamRelayMessage = rmp_serde::from_slice(&bytes).expect("decode new");
        assert_eq!(decoded.request_id, "r");
        assert_eq!(decoded.seq, None);
    }

    #[test]
    fn new_payload_decodes_into_old_shape() {
        let bytes = rmp_serde::to_vec_named(&RoutedStreamRelayMessage {
            request_id: "r".into(),
            message: text(),
            seq: Some(7),
        })
        .expect("encode new");
        let decoded: OldRoutedStreamRelayMessage =
            rmp_serde::from_slice(&bytes).expect("old decoder ignores seq");
        assert_eq!(decoded.request_id, "r");
    }

    #[test]
    fn new_payload_round_trips_seq() {
        let bytes = rmp_serde::to_vec_named(&RoutedStreamRelayMessage {
            request_id: "r".into(),
            message: text(),
            seq: Some(7),
        })
        .expect("encode");
        let decoded: RoutedStreamRelayMessage = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(decoded.seq, Some(7));
    }
}

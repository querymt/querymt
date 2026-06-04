use querymt::chat::StreamChunk;
use querymt::error::{LLMError, TransportErrorKind};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct RemoteProviderStreamState {
    disconnected_since: Option<Instant>,
    chunk_index: u64,
    pending_chunks: VecDeque<StreamChunk>,
    first_chunk_recorded: bool,
}

impl RemoteProviderStreamState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_pending(&mut self, started_at: Instant) -> Option<(StreamChunk, u64, Option<u64>)> {
        let chunk = self.pending_chunks.pop_front()?;
        self.chunk_index += 1;
        let first_chunk_ms = self.record_first_chunk(started_at);
        Some((chunk, self.chunk_index, first_chunk_ms))
    }

    pub fn push_batch_and_take_first(
        &mut self,
        started_at: Instant,
        chunks: Vec<StreamChunk>,
    ) -> Option<(StreamChunk, u64, Option<u64>, usize)> {
        let batch_len = chunks.len();
        let first_chunk_ms = if batch_len > 0 {
            self.record_first_chunk(started_at)
        } else {
            None
        };
        self.pending_chunks.extend(chunks);
        let (chunk, chunk_index, _) = self.take_pending(started_at)?;
        Some((chunk, chunk_index, first_chunk_ms, batch_len))
    }

    pub fn note_chunk(&mut self, started_at: Instant) -> (u64, Option<u64>) {
        self.chunk_index += 1;
        let first_chunk_ms = self.record_first_chunk(started_at);
        (self.chunk_index, first_chunk_ms)
    }

    pub fn note_disconnect(&mut self) {
        self.disconnected_since.get_or_insert_with(Instant::now);
    }

    pub fn note_reconnect(&mut self) {
        self.disconnected_since = None;
    }

    pub fn reconnect_remaining(&self, reconnect_grace: Duration) -> Option<Duration> {
        let since = self.disconnected_since?;
        Some(reconnect_grace.saturating_sub(since.elapsed()))
    }

    pub fn is_disconnected(&self) -> bool {
        self.disconnected_since.is_some()
    }

    pub fn closed_error(&self, peer_alive: bool) -> Option<LLMError> {
        if self.is_disconnected() || !peer_alive {
            Some(LLMError::Transport {
                kind: TransportErrorKind::ConnectionClosed,
                message: format!("stream receiver closed (peer_alive={})", peer_alive),
            })
        } else {
            None
        }
    }

    fn record_first_chunk(&mut self, started_at: Instant) -> Option<u64> {
        if self.first_chunk_recorded {
            None
        } else {
            self.first_chunk_recorded = true;
            Some(started_at.elapsed().as_millis() as u64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::{FinishReason, StreamChunk};

    #[test]
    fn note_chunk_only_reports_first_chunk_latency_once() {
        let mut state = RemoteProviderStreamState::new();
        let started_at = Instant::now() - Duration::from_millis(25);

        let (first_index, first_latency) = state.note_chunk(started_at);
        let (second_index, second_latency) = state.note_chunk(started_at);

        assert_eq!(first_index, 1);
        assert!(first_latency.is_some());
        assert_eq!(second_index, 2);
        assert_eq!(second_latency, None);
    }

    #[test]
    fn push_batch_returns_first_chunk_and_buffers_remaining_order() {
        let mut state = RemoteProviderStreamState::new();
        let started_at = Instant::now() - Duration::from_millis(10);

        let first = state
            .push_batch_and_take_first(
                started_at,
                vec![
                    StreamChunk::Text("one".to_string()),
                    StreamChunk::Text("two".to_string()),
                    StreamChunk::Done {
                        finish_reason: FinishReason::Stop,
                    },
                ],
            )
            .unwrap();

        assert!(matches!(first.0, StreamChunk::Text(ref text) if text == "one"));
        assert_eq!(first.1, 1);
        assert!(first.2.is_some());
        assert_eq!(first.3, 3);

        let second = state.take_pending(started_at).unwrap();
        assert!(matches!(second.0, StreamChunk::Text(ref text) if text == "two"));
        assert_eq!(second.1, 2);
        assert_eq!(second.2, None);

        let third = state.take_pending(started_at).unwrap();
        assert!(matches!(third.0, StreamChunk::Done { .. }));
        assert_eq!(third.1, 3);
        assert_eq!(third.2, None);

        assert!(state.take_pending(started_at).is_none());
    }

    #[test]
    fn reconnect_tracking_sets_and_clears_disconnect_state() {
        let mut state = RemoteProviderStreamState::new();
        assert!(!state.is_disconnected());

        state.note_disconnect();
        assert!(state.is_disconnected());
        assert!(state.reconnect_remaining(Duration::from_secs(5)).is_some());

        state.note_reconnect();
        assert!(!state.is_disconnected());
        assert_eq!(state.reconnect_remaining(Duration::from_secs(5)), None);
    }

    #[test]
    fn reconnect_remaining_saturates_at_zero() {
        let mut state = RemoteProviderStreamState::new();
        state.note_disconnect();
        std::thread::sleep(Duration::from_millis(10));

        assert_eq!(
            state.reconnect_remaining(Duration::from_millis(1)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn closed_error_depends_on_disconnect_or_peer_liveness() {
        let mut state = RemoteProviderStreamState::new();
        assert!(state.closed_error(true).is_none());

        let err = state
            .closed_error(false)
            .expect("dead peer should produce error");
        assert!(matches!(
            err,
            LLMError::Transport {
                kind: TransportErrorKind::ConnectionClosed,
                ..
            }
        ));

        state.note_disconnect();
        let err = state
            .closed_error(true)
            .expect("disconnected stream should produce error");
        assert!(matches!(
            err,
            LLMError::Transport {
                kind: TransportErrorKind::ConnectionClosed,
                ..
            }
        ));
    }
}

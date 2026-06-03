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
        if batch_len > 0 {
            self.record_first_chunk(started_at);
        }
        self.pending_chunks.extend(chunks);
        let (chunk, chunk_index, first_chunk_ms) = self.take_pending(started_at)?;
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

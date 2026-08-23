use crate::acp::protocol::ContentBlock;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

pub const MAX_STEERING_MESSAGES: usize = 32;
pub const MAX_STEERING_BYTES: usize = 256 * 1024;
pub const MAX_STEERING_INPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDelivery {
    Steer,
    Queue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitInput {
    pub session_id: String,
    #[serde(default)]
    pub client_input_id: Option<String>,
    #[serde(default)]
    pub expected_run_id: Option<String>,
    pub delivery: InputDelivery,
    pub prompt: Vec<ContentBlock>,
}

#[typeshare::typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "data", rename_all = "snake_case")]
pub enum SubmitInputResult {
    Steered {
        run_id: String,
        input_id: String,
        position: u32,
    },
    Queued {
        input_id: String,
        position: u32,
    },
    Started {
        run_id: String,
        input_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Starting,
    Model,
    Tools,
    Waiting,
    Closing,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TurnControlError {
    #[error("there is no active run to steer")]
    NoActiveRun,
    #[error("expected_run_id is required for steering")]
    ExpectedRunIdRequired,
    #[error("active run mismatch: expected {expected}, active {active}")]
    RunMismatch { expected: String, active: String },
    #[error("run {run_id} is closing and no longer accepts steering")]
    RunClosing { run_id: String },
    #[error("steering queue is full")]
    QueueFull,
    #[error("steering input is too large ({bytes} bytes, maximum {max_bytes})")]
    InputTooLarge { bytes: usize, max_bytes: usize },
    #[error("slash commands cannot be delivered as steering; queue the input instead")]
    SlashCommandNotSteerable,
    #[error("submission session mismatch: expected {expected}, received {received}")]
    SessionMismatch { expected: String, received: String },
}

#[derive(Debug, Clone)]
pub struct SteeringInput {
    pub input_id: String,
    pub client_input_id: Option<String>,
    pub blocks: Vec<ContentBlock>,
    pub accepted_at_ms: u64,
}

#[derive(Debug)]
struct SteeringInboxState {
    accepting: bool,
    total_bytes: usize,
    pending: VecDeque<SteeringInput>,
}

#[derive(Debug)]
pub struct SteeringInbox {
    state: Mutex<SteeringInboxState>,
    changed: Notify,
}

impl SteeringInbox {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SteeringInboxState {
                accepting: true,
                total_bytes: 0,
                pending: VecDeque::new(),
            }),
            changed: Notify::new(),
        }
    }

    pub async fn push(
        &self,
        input_id: String,
        client_input_id: Option<String>,
        blocks: Vec<ContentBlock>,
    ) -> Result<usize, TurnControlError> {
        let bytes = serde_json::to_vec(&blocks)
            .map(|value| value.len())
            .unwrap_or(usize::MAX);
        if bytes > MAX_STEERING_INPUT_BYTES {
            return Err(TurnControlError::InputTooLarge {
                bytes,
                max_bytes: MAX_STEERING_INPUT_BYTES,
            });
        }
        if blocks.iter().any(|block| match block {
            ContentBlock::Text(text) => text.text.trim_start().starts_with('/'),
            _ => false,
        }) {
            return Err(TurnControlError::SlashCommandNotSteerable);
        }

        let mut state = self.state.lock().await;
        if !state.accepting {
            return Err(TurnControlError::RunClosing {
                run_id: String::new(),
            });
        }
        if state.pending.len() >= MAX_STEERING_MESSAGES
            || state.total_bytes.saturating_add(bytes) > MAX_STEERING_BYTES
        {
            return Err(TurnControlError::QueueFull);
        }
        state.total_bytes += bytes;
        state.pending.push_back(SteeringInput {
            input_id,
            client_input_id,
            blocks,
            accepted_at_ms: now_ms(),
        });
        let position = state.pending.len();
        drop(state);
        self.changed.notify_waiters();
        Ok(position)
    }

    pub async fn drain(&self) -> Vec<SteeringInput> {
        let mut state = self.state.lock().await;
        state.total_bytes = 0;
        state.pending.drain(..).collect()
    }

    pub async fn pending_count(&self) -> usize {
        self.state.lock().await.pending.len()
    }

    pub async fn begin_closing_and_drain(&self) -> Vec<SteeringInput> {
        let mut state = self.state.lock().await;
        state.accepting = false;
        state.total_bytes = 0;
        state.pending.drain(..).collect()
    }

    pub async fn discard(&self) -> Vec<SteeringInput> {
        self.begin_closing_and_drain().await
    }

    pub async fn notified(&self) {
        self.changed.notified().await;
    }
}

impl Default for SteeringInbox {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct ActiveRun {
    pub run_id: String,
    pub generation: u64,
    pub cancellation: CancellationToken,
    pub steering: Arc<SteeringInbox>,
    pub phase: RunPhase,
    pub started_at_ms: u64,
}

impl ActiveRun {
    pub fn new(run_id: String, generation: u64, cancellation: CancellationToken) -> Self {
        Self {
            run_id,
            generation,
            cancellation,
            steering: Arc::new(SteeringInbox::new()),
            phase: RunPhase::Starting,
            started_at_ms: now_ms(),
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::TextContent;

    fn text(value: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(TextContent::new(value))]
    }

    #[tokio::test]
    async fn inbox_preserves_order_and_closes_atomically() {
        let inbox = SteeringInbox::new();
        assert_eq!(inbox.push("a".into(), None, text("one")).await.unwrap(), 1);
        assert_eq!(inbox.push("b".into(), None, text("two")).await.unwrap(), 2);
        let drained = inbox.begin_closing_and_drain().await;
        assert_eq!(
            drained
                .iter()
                .map(|item| item.input_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(matches!(
            inbox.push("c".into(), None, text("three")).await,
            Err(TurnControlError::RunClosing { .. })
        ));
    }

    #[tokio::test]
    async fn inbox_rejects_slash_commands() {
        let inbox = SteeringInbox::new();
        assert_eq!(
            inbox
                .push("a".into(), None, text(" /compact"))
                .await
                .unwrap_err(),
            TurnControlError::SlashCommandNotSteerable
        );
    }
}

//! Common test driver implementations for middleware testing

use crate::middleware::{ExecutionState, MiddlewareDriver, Result};
use agent_client_protocol::StopReason;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ============================================================================
// StateRecordingDriver - Records the names of states it sees
// ============================================================================

pub struct StateRecordingDriver {
    seen_states: Arc<Mutex<Vec<String>>>,
}

impl StateRecordingDriver {
    pub fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let states = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                seen_states: states.clone(),
            },
            states,
        )
    }
}

#[async_trait]
impl MiddlewareDriver for StateRecordingDriver {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.seen_states
            .lock()
            .unwrap()
            .push(state.name().to_string());
        Ok(state)
    }

    fn reset(&self) {
        self.seen_states.lock().unwrap().clear();
    }

    fn name(&self) -> &'static str {
        "StateRecordingDriver"
    }
}

// ============================================================================
// MessageInjectingDriver - Injects a message into BeforeTurn state
// ============================================================================

pub struct MessageInjectingDriver {
    pub inject_content: String,
}

#[async_trait]
impl MiddlewareDriver for MessageInjectingDriver {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        match state {
            ExecutionState::BeforeTurn { context } => Ok(ExecutionState::BeforeTurn {
                context: Arc::new(context.inject_message(self.inject_content.clone())),
            }),
            other => Ok(other),
        }
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "MessageInjectingDriver"
    }
}

// ============================================================================
// PassThroughDriver - Simply passes state through unchanged
// ============================================================================

pub struct PassThroughDriver;

#[async_trait]
impl MiddlewareDriver for PassThroughDriver {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        Ok(state)
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "PassThrough"
    }
}

// ============================================================================
// StopDriver - Always returns Stopped state
// ============================================================================

pub struct StopDriver {
    pub reason: StopReason,
    pub message: &'static str,
}

impl StopDriver {
    pub fn new(reason: StopReason, message: &'static str) -> Self {
        Self { reason, message }
    }
}

#[async_trait]
impl MiddlewareDriver for StopDriver {
    async fn next_state(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(ExecutionState::Stopped {
            reason: self.reason,
            message: self.message.into(),
        })
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "StopDriver"
    }
}

// ============================================================================
// AlwaysStopDriver - Alias for StopDriver (for backward compatibility)
// ============================================================================

pub struct AlwaysStopDriver {
    pub reason: StopReason,
}

#[async_trait]
impl MiddlewareDriver for AlwaysStopDriver {
    async fn next_state(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(ExecutionState::Stopped {
            reason: self.reason,
            message: "stopped by middleware".into(),
        })
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "AlwaysStopDriver"
    }
}

// ============================================================================
// CompleteDriver - Always returns Complete state
// ============================================================================

pub struct CompleteDriver;

#[async_trait]
impl MiddlewareDriver for CompleteDriver {
    async fn next_state(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(ExecutionState::Complete)
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "CompleteDriver"
    }
}

// ============================================================================
// CancelDriver - Always returns Cancelled state
// ============================================================================

pub struct CancelDriver;

#[async_trait]
impl MiddlewareDriver for CancelDriver {
    async fn next_state(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(ExecutionState::Cancelled)
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "CancelDriver"
    }
}

// ============================================================================
// CountingDriver - Counts how many times next_state is called
// ============================================================================

pub struct CountingDriver {
    pub count: AtomicUsize,
}

impl CountingDriver {
    pub fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
        }
    }

    pub fn with_initial(initial: usize) -> Self {
        Self {
            count: AtomicUsize::new(initial),
        }
    }
}

impl Default for CountingDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MiddlewareDriver for CountingDriver {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(state)
    }

    fn reset(&self) {
        self.count.store(0, Ordering::SeqCst);
    }

    fn name(&self) -> &'static str {
        "CountingDriver"
    }
}

// ============================================================================
// ErrorDriver - Always returns an error
// ============================================================================

pub struct ErrorDriver;

#[async_trait]
impl MiddlewareDriver for ErrorDriver {
    async fn next_state(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Err(crate::middleware::MiddlewareError::ExecutionError(
            "test error".into(),
        ))
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "ErrorDriver"
    }
}

// ============================================================================
// BeforeTurnToCallLlmDriver - Transforms BeforeTurn to CallLlm
// ============================================================================

pub struct BeforeTurnToCallLlmDriver;

#[async_trait]
impl MiddlewareDriver for BeforeTurnToCallLlmDriver {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        match state {
            ExecutionState::BeforeTurn { context } => Ok(ExecutionState::CallLlm {
                context,
                tools: Arc::from([]),
            }),
            other => Ok(other),
        }
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "BeforeTurnToCallLlmDriver"
    }
}

// ============================================================================
// StopOnBeforeTurn - Stops execution when it sees BeforeTurn state
// ============================================================================

pub struct StopOnBeforeTurn;

#[async_trait]
impl MiddlewareDriver for StopOnBeforeTurn {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        match state {
            ExecutionState::BeforeTurn { .. } => Ok(ExecutionState::Stopped {
                reason: StopReason::EndTurn,
                message: "stopped".into(),
            }),
            other => Ok(other),
        }
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "StopOnBeforeTurn"
    }
}

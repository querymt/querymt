//! Common test driver implementations for middleware testing

use crate::events::StopType;
use crate::middleware::{ExecutionState, MiddlewareDriver, Result};
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

    fn record(&self, state: &ExecutionState) {
        self.seen_states
            .lock()
            .unwrap()
            .push(state.name().to_string());
    }
}

#[async_trait]
impl MiddlewareDriver for StateRecordingDriver {
    async fn on_turn_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.record(&state);
        Ok(state)
    }

    async fn on_step_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.record(&state);
        Ok(state)
    }

    async fn on_after_llm(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.record(&state);
        Ok(state)
    }

    async fn on_processing_tool_calls(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.record(&state);
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
// MessageInjectingDriver - Injects a message into BeforeLlmCall state
// ============================================================================

pub struct MessageInjectingDriver {
    pub inject_content: String,
}

#[async_trait]
impl MiddlewareDriver for MessageInjectingDriver {
    async fn on_turn_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.inject(state)
    }

    async fn on_step_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.inject(state)
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "MessageInjectingDriver"
    }
}

impl MessageInjectingDriver {
    fn inject(&self, state: ExecutionState) -> Result<ExecutionState> {
        match state {
            ExecutionState::BeforeLlmCall { context } => Ok(ExecutionState::BeforeLlmCall {
                context: Arc::new(context.inject_message(self.inject_content.clone())),
            }),
            other => Ok(other),
        }
    }
}

// ============================================================================
// PassThroughDriver - Simply passes state through unchanged
// ============================================================================

pub struct PassThroughDriver;

#[async_trait]
impl MiddlewareDriver for PassThroughDriver {
    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "PassThrough"
    }
}

// ============================================================================
// StopDriver - Always returns Stopped state
// ============================================================================

pub struct StopDriver {
    pub stop_type: StopType,
    pub message: &'static str,
}

impl StopDriver {
    pub fn new(stop_type: StopType, message: &'static str) -> Self {
        Self { stop_type, message }
    }

    fn stopped_state(&self) -> ExecutionState {
        ExecutionState::Stopped {
            message: self.message.into(),
            stop_type: self.stop_type,
        }
    }
}

#[async_trait]
impl MiddlewareDriver for StopDriver {
    async fn on_turn_start(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(self.stopped_state())
    }

    async fn on_step_start(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(self.stopped_state())
    }

    async fn on_after_llm(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(self.stopped_state())
    }

    async fn on_processing_tool_calls(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(self.stopped_state())
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
    pub stop_type: StopType,
}

#[async_trait]
impl MiddlewareDriver for AlwaysStopDriver {
    async fn on_turn_start(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(ExecutionState::Stopped {
            message: "stopped by middleware".into(),
            stop_type: self.stop_type,
        })
    }

    async fn on_step_start(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
    }

    async fn on_after_llm(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
    }

    async fn on_processing_tool_calls(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
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
    async fn on_turn_start(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(ExecutionState::Complete)
    }

    async fn on_step_start(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
    }

    async fn on_after_llm(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
    }

    async fn on_processing_tool_calls(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
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
    async fn on_turn_start(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Ok(ExecutionState::Cancelled)
    }

    async fn on_step_start(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
    }

    async fn on_after_llm(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
    }

    async fn on_processing_tool_calls(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "CancelDriver"
    }
}

// ============================================================================
// CountingDriver - Counts how many times hook methods are called
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

    fn bump(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

impl Default for CountingDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MiddlewareDriver for CountingDriver {
    async fn on_turn_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.bump();
        Ok(state)
    }

    async fn on_step_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.bump();
        Ok(state)
    }

    async fn on_after_llm(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.bump();
        Ok(state)
    }

    async fn on_processing_tool_calls(&self, state: ExecutionState) -> Result<ExecutionState> {
        self.bump();
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
    async fn on_turn_start(&self, _state: ExecutionState) -> Result<ExecutionState> {
        Err(crate::middleware::MiddlewareError::ExecutionError(
            "test error".into(),
        ))
    }

    async fn on_step_start(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
    }

    async fn on_after_llm(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
    }

    async fn on_processing_tool_calls(&self, _state: ExecutionState) -> Result<ExecutionState> {
        self.on_turn_start(_state).await
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "ErrorDriver"
    }
}

// ============================================================================
// BeforeLlmCallToCallLlmDriver - Transforms BeforeLlmCall to CallLlm
// ============================================================================

pub struct BeforeLlmCallToCallLlmDriver;

#[async_trait]
impl MiddlewareDriver for BeforeLlmCallToCallLlmDriver {
    async fn on_step_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        match state {
            ExecutionState::BeforeLlmCall { context } => Ok(ExecutionState::CallLlm {
                context,
                tools: Arc::from([]),
            }),
            other => Ok(other),
        }
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "BeforeLlmCallToCallLlmDriver"
    }
}

// ============================================================================
// StopOnBeforeLlmCall - Stops execution when it sees BeforeLlmCall state
// ============================================================================

pub struct StopOnBeforeLlmCall;

#[async_trait]
impl MiddlewareDriver for StopOnBeforeLlmCall {
    async fn on_step_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        match state {
            ExecutionState::BeforeLlmCall { .. } => Ok(ExecutionState::Stopped {
                message: "stopped".into(),
                stop_type: StopType::Other,
            }),
            other => Ok(other),
        }
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "StopOnBeforeLlmCall"
    }
}

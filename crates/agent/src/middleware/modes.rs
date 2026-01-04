use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{ExecutionState, MiddlewareDriver, Result};
use log::trace;

/// Middleware that injects a reminder message when plan mode is enabled
pub struct PlanModeMiddleware {
    enabled: Arc<AtomicBool>,
    reminder: String,
}

impl PlanModeMiddleware {
    pub fn new(enabled: Arc<AtomicBool>, reminder: String) -> Self {
        Self { enabled, reminder }
    }
}

#[async_trait]
impl MiddlewareDriver for PlanModeMiddleware {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        match state {
            ExecutionState::BeforeTurn { ref context } if self.enabled.load(Ordering::Relaxed) => {
                trace!("PlanModeMiddleware: injecting reminder message");
                let new_context = context.inject_message(self.reminder.clone());
                Ok(ExecutionState::BeforeTurn {
                    context: Arc::new(new_context),
                })
            }
            other => Ok(other),
        }
    }

    fn reset(&self) {
        // No state to reset
    }

    fn name(&self) -> &'static str {
        "PlanModeMiddleware"
    }
}

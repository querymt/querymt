use crate::middleware::{ExecutionState, MiddlewareDriver, Result};
use crate::session::store::SessionStore;
use async_trait::async_trait;
use log::{debug, trace};
use parking_lot::Mutex;
use std::sync::Arc;

/// Middleware that auto-completes tasks when:
/// 1. Agent's last turn had no tool calls
/// 2. All delegations for the task are complete or failed (if any exist)
/// 3. Task is still in Active status
///
/// NOTE: Primary task completion is now handled in `transition_after_llm` when
/// `FinishReason::Stop` is received. This middleware serves as a fallback for
/// cases where finish_reason is not available or unknown.
pub struct TaskAutoCompletionMiddleware {
    _store: Arc<dyn SessionStore>,
}

impl TaskAutoCompletionMiddleware {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        debug!("Creating TaskAutoCompletionMiddleware");
        Self { _store: store }
    }
}

#[async_trait]
impl MiddlewareDriver for TaskAutoCompletionMiddleware {
    async fn on_after_llm(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
        trace!(
            "TaskAutoCompletionMiddleware is observational; explicit complete_task controls completion for state {}",
            state.name()
        );
        Ok(state)
    }

    fn reset(&self) {
        trace!("TaskAutoCompletionMiddleware::reset");
    }

    fn name(&self) -> &'static str {
        "TaskAutoCompletionMiddleware"
    }
}

/// Middleware that detects and warns about duplicate/repetitive tool calls
/// Helps prevent agents from calling the same tool repeatedly with identical arguments
pub struct DuplicateToolCallMiddleware {
    store: Arc<dyn SessionStore>,
    last_check: Mutex<std::collections::HashMap<String, usize>>, // session_id -> last_checked_history_len
}

impl DuplicateToolCallMiddleware {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        debug!("Creating DuplicateToolCallMiddleware");
        Self {
            store,
            last_check: Mutex::new(std::collections::HashMap::new()),
        }
    }

    async fn check_for_duplicates(&self, session_id: &str) -> Option<String> {
        use crate::model::MessagePart;

        // Get session history
        let history = match self.store.get_history(session_id).await {
            Ok(h) => h,
            Err(_) => return None,
        };

        // Track last check to avoid re-checking same history
        let mut last_check = self.last_check.lock();
        let last_checked_len = last_check.get(session_id).copied().unwrap_or(0);

        if history.len() <= last_checked_len {
            return None; // Nothing new to check
        }

        last_check.insert(session_id.to_string(), history.len());
        drop(last_check);

        // Look at the last few messages to detect duplicate tool calls
        let recent_tools: Vec<(String, String)> = history
            .iter()
            .rev()
            .take(5)
            .flat_map(|msg| {
                msg.parts.iter().filter_map(|part| {
                    if let MessagePart::ToolUse(tool_call) = part {
                        Some((
                            tool_call.function.name.clone(),
                            tool_call.function.arguments.clone(),
                        ))
                    } else {
                        None
                    }
                })
            })
            .collect();

        // Check if the same tool was called twice in a row with same args
        if recent_tools.len() >= 2 {
            let (tool1, args1) = &recent_tools[0];
            let (tool2, args2) = &recent_tools[1];

            if tool1 == tool2 && args1 == args2 {
                return Some(format!(
                    "⚠️ DUPLICATE TOOL CALL DETECTED: You just called '{}' twice with identical arguments.\n\
                     This often indicates a loop or that you forgot to check the result of your previous action.\n\
                     Please verify the previous tool call succeeded before calling it again, or use a different approach.",
                    tool1
                ));
            }
        }

        None
    }
}

#[async_trait]
impl MiddlewareDriver for DuplicateToolCallMiddleware {
    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
        trace!(
            "DuplicateToolCallMiddleware::on_step_start entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeLlmCall { ref context } => {
                if let Some(warning) = self.check_for_duplicates(&context.session_id).await {
                    debug!("DuplicateToolCallMiddleware: duplicate detected, injecting warning");

                    let new_context = context.inject_message(warning);
                    Ok(ExecutionState::BeforeLlmCall {
                        context: Arc::new(new_context),
                    })
                } else {
                    trace!("DuplicateToolCallMiddleware: no duplicates detected");
                    Ok(state)
                }
            }
            _ => {
                trace!(
                    "DuplicateToolCallMiddleware: pass-through for state {}",
                    state.name()
                );
                Ok(state)
            }
        }
    }

    fn reset(&self) {
        debug!("DuplicateToolCallMiddleware::reset - clearing last_check cache");
        let mut last_check = self.last_check.lock();
        last_check.clear();
    }

    fn name(&self) -> &'static str {
        "DuplicateToolCallMiddleware"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mocks::MockSessionStore;

    // ── TaskAutoCompletionMiddleware ─────────────────────────────────────────

    #[test]
    fn task_auto_completion_name() {
        let mut mock = MockSessionStore::new();
        mock.expect_get_session().never();
        let m = TaskAutoCompletionMiddleware::new(Arc::new(mock));
        assert_eq!(m.name(), "TaskAutoCompletionMiddleware");
    }

    #[test]
    fn task_auto_completion_reset_does_not_panic() {
        let mut mock = MockSessionStore::new();
        mock.expect_get_session().never();
        let m = TaskAutoCompletionMiddleware::new(Arc::new(mock));
        m.reset();
    }

    // ── DuplicateToolCallMiddleware ──────────────────────────────────────────

    #[test]
    fn duplicate_tool_call_name() {
        let mut mock = MockSessionStore::new();
        mock.expect_get_history().never();
        let m = DuplicateToolCallMiddleware::new(Arc::new(mock));
        assert_eq!(m.name(), "DuplicateToolCallMiddleware");
    }

    #[test]
    fn duplicate_tool_call_reset_clears_cache() {
        let mut mock = MockSessionStore::new();
        mock.expect_get_history().never();
        let m = DuplicateToolCallMiddleware::new(Arc::new(mock));
        // Add something to last_check to test clearing
        {
            let mut cache = m.last_check.lock();
            cache.insert("sess-1".to_string(), 5);
        }
        m.reset();
        let cache = m.last_check.lock();
        assert!(cache.is_empty(), "reset() should clear the cache");
    }

    #[tokio::test]
    async fn task_auto_completion_passes_through_non_after_llm_state() {
        let mut mock = MockSessionStore::new();
        mock.expect_get_session().never();
        let m = TaskAutoCompletionMiddleware::new(Arc::new(mock));

        use crate::middleware::{AgentStats, ConversationContext, ExecutionState};
        let ctx = Arc::new(ConversationContext::new(
            "sess-1".into(),
            Arc::from([]),
            Arc::new(AgentStats::default()),
            "mock".into(),
            "mock-model".into(),
        ));
        let state = ExecutionState::BeforeLlmCall { context: ctx };
        let result = m.on_after_llm(state, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn duplicate_tool_call_passes_through_non_before_llm_state() {
        let mut mock = MockSessionStore::new();
        mock.expect_get_history().never();
        let m = DuplicateToolCallMiddleware::new(Arc::new(mock));

        use crate::middleware::{AgentStats, ConversationContext, ExecutionState, LlmResponse};
        let ctx = Arc::new(ConversationContext::new(
            "sess-1".into(),
            Arc::from([]),
            Arc::new(AgentStats::default()),
            "mock".into(),
            "mock-model".into(),
        ));
        let response = Arc::new(LlmResponse::new("hi".to_string(), vec![], None, None));
        let state = ExecutionState::AfterLlm {
            response,
            context: ctx,
        };
        let result = m.on_step_start(state, None).await;
        assert!(result.is_ok());
    }
}

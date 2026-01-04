use crate::middleware::{ExecutionState, MiddlewareDriver, Result};
use crate::session::domain::{DelegationStatus, TaskStatus};
use crate::session::store::SessionStore;
use async_trait::async_trait;
use log::{debug, trace};
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
    store: Arc<dyn SessionStore>,
}

impl TaskAutoCompletionMiddleware {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        debug!("Creating TaskAutoCompletionMiddleware");
        Self { store }
    }
}

#[async_trait]
impl MiddlewareDriver for TaskAutoCompletionMiddleware {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "TaskAutoCompletionMiddleware::next_state entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::AfterLlm {
                ref response,
                ref context,
            } => {
                // First check: did the LLM response have tool calls?
                if response.has_tool_calls() {
                    trace!("TaskAutoCompletionMiddleware: LLM response has tool calls, skipping");
                    return Ok(state);
                }

                // Get session to find active task
                let session = match self.store.get_session(&context.session_id).await {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        debug!(
                            "TaskAutoCompletionMiddleware: session not found: {}",
                            context.session_id
                        );
                        return Ok(state);
                    }
                    Err(e) => {
                        debug!(
                            "TaskAutoCompletionMiddleware: error fetching session: {}",
                            e
                        );
                        return Ok(state);
                    }
                };

                let Some(task_internal_id) = session.active_task_id else {
                    trace!("TaskAutoCompletionMiddleware: no active task, skipping");
                    return Ok(state);
                };

                let tasks = match self.store.list_tasks(&context.session_id).await {
                    Ok(t) => t,
                    Err(e) => {
                        debug!("TaskAutoCompletionMiddleware: error listing tasks: {}", e);
                        return Ok(state);
                    }
                };

                let Some(task) = tasks.into_iter().find(|task| task.id == task_internal_id) else {
                    trace!(
                        "TaskAutoCompletionMiddleware: task not found: {}",
                        task_internal_id
                    );
                    return Ok(state);
                };

                let task_public_id = task.public_id.clone();

                // Only auto-complete Active tasks
                if task.status != TaskStatus::Active {
                    trace!(
                        "TaskAutoCompletionMiddleware: task status is {:?}, not Active",
                        task.status
                    );
                    return Ok(state);
                }

                // Check if all delegations are complete
                let delegations = match self.store.list_delegations(&context.session_id).await {
                    Ok(d) => d,
                    Err(e) => {
                        debug!(
                            "TaskAutoCompletionMiddleware: error fetching delegations: {}",
                            e
                        );
                        return Ok(state);
                    }
                };

                let task_delegations: Vec<_> = delegations
                    .into_iter()
                    .filter(|d| d.task_id == Some(task_internal_id))
                    .collect();

                // Check if all delegations are in terminal state (if any exist)
                // If there are no delegations, we still auto-complete since the model
                // may have determined the work is already done without needing delegations
                let all_delegations_done = task_delegations.is_empty()
                    || task_delegations.iter().all(|d| {
                        matches!(
                            d.status,
                            DelegationStatus::Complete | DelegationStatus::Failed
                        )
                    });

                if all_delegations_done {
                    debug!(
                        "TaskAutoCompletionMiddleware: all delegations complete, auto-completing task: {}",
                        task_public_id
                    );

                    if let Err(e) = self
                        .store
                        .update_task_status(&task_public_id, TaskStatus::Done)
                        .await
                    {
                        debug!(
                            "TaskAutoCompletionMiddleware: failed to update task status: {}",
                            e
                        );
                    } else {
                        debug!(
                            "TaskAutoCompletionMiddleware: task {} marked as Done",
                            task_public_id
                        );
                    }
                }

                Ok(state)
            }
            _ => {
                trace!(
                    "TaskAutoCompletionMiddleware: pass-through for state {}",
                    state.name()
                );
                Ok(state)
            }
        }
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
    last_check: std::sync::Mutex<std::collections::HashMap<String, usize>>, // session_id -> last_checked_history_len
}

impl DuplicateToolCallMiddleware {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        debug!("Creating DuplicateToolCallMiddleware");
        Self {
            store,
            last_check: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        let mut last_check = self.last_check.lock().unwrap();
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
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "DuplicateToolCallMiddleware::next_state entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeTurn { ref context } => {
                if let Some(warning) = self.check_for_duplicates(&context.session_id).await {
                    debug!("DuplicateToolCallMiddleware: duplicate detected, injecting warning");

                    let new_context = context.inject_message(warning);
                    Ok(ExecutionState::BeforeTurn {
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
        let mut last_check = self.last_check.lock().unwrap();
        last_check.clear();
    }

    fn name(&self) -> &'static str {
        "DuplicateToolCallMiddleware"
    }
}

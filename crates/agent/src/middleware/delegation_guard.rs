use crate::events::StopType;
use crate::middleware::{ExecutionState, MiddlewareDriver, Result};
use crate::session::domain::DelegationStatus;
use crate::session::store::SessionStore;
use async_trait::async_trait;
use log::{debug, trace};
use querymt::chat::ChatRole;
use std::sync::Arc;
use time::OffsetDateTime;

/// Prevents duplicate concurrent delegations and enforces retry limits
/// This is an OPTIONAL middleware - add it if you want delegation deduplication
pub struct DelegationGuardMiddleware {
    store: Arc<dyn SessionStore>,
    max_retries: u32,
    duplicate_window_secs: i64,
}

impl DelegationGuardMiddleware {
    /// Create a new delegation guard with default settings
    /// - max_retries: 3 (stop after 3 failed attempts)
    /// - duplicate_window: 5 seconds (prevent retry within 5s of failure)
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        debug!(
            "Creating DelegationGuardMiddleware with max_retries={}, duplicate_window_secs={}",
            3, 5
        );
        Self {
            store,
            max_retries: 3,
            duplicate_window_secs: 5,
        }
    }

    /// Set maximum retry attempts before blocking
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        debug!(
            "DelegationGuardMiddleware: setting max_retries = {}",
            max_retries
        );
        self.max_retries = max_retries;
        self
    }

    /// Set minimum seconds between retry attempts
    pub fn with_duplicate_window_secs(mut self, secs: i64) -> Self {
        debug!(
            "DelegationGuardMiddleware: setting duplicate_window_secs = {}",
            secs
        );
        self.duplicate_window_secs = secs;
        self
    }

    /// Check if a delegation should be blocked based on existing delegations
    async fn check_delegation_allowed(
        &self,
        session_id: &str,
        objective_hash: &crate::hash::RapidHash,
        target_agent_id: &str,
    ) -> Option<String> {
        // Get all delegations for this session
        let delegations = match self.store.list_delegations(session_id).await {
            Ok(d) => d,
            Err(_) => return None,
        };

        // Check for duplicates or retry limit violations
        for del in &delegations {
            if &del.objective_hash == objective_hash && del.target_agent_id == target_agent_id {
                match del.status {
                    DelegationStatus::Running | DelegationStatus::Requested => {
                        return Some(format!(
                            "⚠️ DUPLICATE DELEGATION BLOCKED\n\
                             \n\
                             A delegation with the same objective to '{}' is already in progress.\n\
                             Delegation ID: {}\n\
                             Status: {:?}\n\
                             \n\
                             Please wait for the current delegation to complete before retrying.",
                            target_agent_id, del.public_id, del.status
                        ));
                    }
                    DelegationStatus::Failed => {
                        // Check retry count
                        if del.retry_count >= self.max_retries {
                            return Some(format!(
                                "⚠️ MAX RETRIES EXCEEDED\n\
                                 \n\
                                 This delegation has failed {} times (limit: {}).\n\
                                 Previous delegation ID: {}\n\
                                 \n\
                                 The task appears to be stuck. Please:\n\
                                 1. Review why previous attempts failed (check error messages)\n\
                                 2. Modify your approach or break down the task differently\n\
                                 3. Consider delegating to a different agent\n\
                                 4. Try a different strategy entirely",
                                del.retry_count, self.max_retries, del.public_id
                            ));
                        }

                        // Check if failed recently (within duplicate window)
                        if let Some(completed) = del.completed_at {
                            let now = OffsetDateTime::now_utc();
                            let elapsed = (now - completed).whole_seconds();
                            if elapsed < self.duplicate_window_secs {
                                return Some(format!(
                                    "⚠️ RETRY TOO SOON\n\
                                     \n\
                                     Previous delegation failed {} seconds ago.\n\
                                     Please wait at least {} seconds before retrying.\n\
                                     \n\
                                     Suggestion: Analyze the failure before immediately retrying.",
                                    elapsed, self.duplicate_window_secs
                                ));
                            }
                        }
                    }
                    DelegationStatus::Complete | DelegationStatus::Cancelled => {
                        // Complete or Cancelled is fine, allow new delegation
                    }
                }
            }
        }

        None
    }
}

#[async_trait]
impl MiddlewareDriver for DelegationGuardMiddleware {
    async fn on_after_llm(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "DelegationGuardMiddleware::on_after_llm entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::AfterLlm {
                response: _,
                ref context,
            } => {
                use crate::model::MessagePart;

                // Get the session history
                let history = match self.store.get_history(&context.session_id).await {
                    Ok(h) => h,
                    Err(_) => return Ok(state),
                };

                // Look for delegation tool calls in the last assistant message
                if let Some(last_msg) = history.last() {
                    if last_msg.role != ChatRole::Assistant {
                        return Ok(state);
                    }

                    // Check for delegate tool calls
                    for part in &last_msg.parts {
                        if let MessagePart::ToolUse(tool_call) = part
                            && tool_call.function.name == "delegate"
                        {
                            // Parse the delegation arguments
                            if let Ok(args) = serde_json::from_str::<serde_json::Value>(
                                &tool_call.function.arguments,
                            ) {
                                let target_agent_id = args
                                    .get("target_agent_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let objective =
                                    args.get("objective").and_then(|v| v.as_str()).unwrap_or("");

                                if !target_agent_id.is_empty() && !objective.is_empty() {
                                    // Compute objective hash
                                    let objective_hash =
                                        crate::hash::RapidHash::new(objective.as_bytes());

                                    // Check if this delegation should be blocked
                                    if let Some(warning) = self
                                        .check_delegation_allowed(
                                            &context.session_id,
                                            &objective_hash,
                                            target_agent_id,
                                        )
                                        .await
                                    {
                                        debug!(
                                            "DelegationGuardMiddleware: blocking duplicate delegation"
                                        );

                                        // Inject warning message and stop execution
                                        let _new_context = context.inject_message(warning);
                                        return Ok(ExecutionState::Stopped {
                                            message: "Delegation blocked by guard".into(),
                                            stop_type: StopType::DelegationBlocked,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                trace!("DelegationGuardMiddleware: no delegation blocking needed");
                Ok(state)
            }
            _ => {
                trace!(
                    "DelegationGuardMiddleware: pass-through for state {}",
                    state.name()
                );
                Ok(state)
            }
        }
    }

    fn reset(&self) {
        trace!("DelegationGuardMiddleware::reset");
        // No state to reset - all checks are based on database
    }

    fn name(&self) -> &'static str {
        "DelegationGuardMiddleware"
    }
}

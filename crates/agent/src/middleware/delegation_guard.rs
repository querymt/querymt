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
    async fn on_after_llm(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
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
                                            context: Some(context.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::RapidHash;
    use crate::session::domain::{Delegation, DelegationStatus};
    use crate::test_utils::mocks::MockSessionStore;
    use time::OffsetDateTime;

    fn make_guard(store: Arc<dyn SessionStore>) -> DelegationGuardMiddleware {
        DelegationGuardMiddleware::new(store)
    }

    #[test]
    fn new_sets_default_config() {
        let mut mock = MockSessionStore::new();
        // We need to keep the mock minimal
        mock.expect_list_delegations().returning(|_| Ok(vec![]));
        let guard = make_guard(Arc::new(mock));
        // Verify defaults by calling with_max_retries
        let guard2 = DelegationGuardMiddleware::new({
            let mut m = MockSessionStore::new();
            m.expect_list_delegations().returning(|_| Ok(vec![]));
            Arc::new(m) as Arc<dyn SessionStore>
        });
        assert_eq!(guard2.max_retries, 3);
        assert_eq!(guard2.duplicate_window_secs, 5);
        let _ = guard;
    }

    #[test]
    fn with_max_retries_sets_value() {
        let mut mock = MockSessionStore::new();
        mock.expect_list_delegations().returning(|_| Ok(vec![]));
        let guard = make_guard(Arc::new(mock)).with_max_retries(10);
        assert_eq!(guard.max_retries, 10);
    }

    #[test]
    fn with_duplicate_window_secs_sets_value() {
        let mut mock = MockSessionStore::new();
        mock.expect_list_delegations().returning(|_| Ok(vec![]));
        let guard = make_guard(Arc::new(mock)).with_duplicate_window_secs(30);
        assert_eq!(guard.duplicate_window_secs, 30);
    }

    #[test]
    fn name_returns_string() {
        let mut mock = MockSessionStore::new();
        mock.expect_list_delegations().returning(|_| Ok(vec![]));
        let guard = make_guard(Arc::new(mock));
        assert_eq!(guard.name(), "DelegationGuardMiddleware");
    }

    #[test]
    fn reset_does_not_panic() {
        let mut mock = MockSessionStore::new();
        mock.expect_list_delegations().returning(|_| Ok(vec![]));
        let guard = make_guard(Arc::new(mock));
        guard.reset(); // Should not panic
    }

    #[tokio::test]
    async fn check_delegation_allowed_no_delegations_returns_none() {
        let mut mock = MockSessionStore::new();
        mock.expect_list_delegations().returning(|_| Ok(vec![]));
        let guard = make_guard(Arc::new(mock));
        let hash = RapidHash::new(b"objective");
        let result = guard
            .check_delegation_allowed("sess-1", &hash, "agent-1")
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn check_delegation_allowed_blocks_running_duplicate() {
        let objective = "do the thing";
        let hash = RapidHash::new(objective.as_bytes());
        let hash_clone = hash.clone();

        let delegation = Delegation {
            id: 1,
            public_id: "del-1".to_string(),
            session_id: 1,
            task_id: None,
            target_agent_id: "agent-x".to_string(),
            objective: objective.to_string(),
            objective_hash: hash_clone,
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: None,
            planning_summary: None,
            status: DelegationStatus::Running,
            retry_count: 0,
            created_at: OffsetDateTime::now_utc(),
            completed_at: None,
        };

        let mut mock = MockSessionStore::new();
        mock.expect_list_delegations()
            .returning(move |_| Ok(vec![delegation.clone()]));
        let guard = make_guard(Arc::new(mock));

        let result = guard
            .check_delegation_allowed("sess-1", &hash, "agent-x")
            .await;
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.contains("DUPLICATE DELEGATION BLOCKED") || msg.contains("in progress"));
    }

    #[tokio::test]
    async fn check_delegation_allowed_blocks_when_max_retries_exceeded() {
        let objective = "repeated task";
        let hash = RapidHash::new(objective.as_bytes());
        let hash_clone = hash.clone();

        let delegation = Delegation {
            id: 1,
            public_id: "del-failed".to_string(),
            session_id: 1,
            task_id: None,
            target_agent_id: "agent-y".to_string(),
            objective: objective.to_string(),
            objective_hash: hash_clone,
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: None,
            planning_summary: None,
            status: DelegationStatus::Failed,
            retry_count: 5, // exceeds default max of 3
            created_at: OffsetDateTime::now_utc(),
            completed_at: Some(OffsetDateTime::now_utc() - time::Duration::seconds(60)),
        };

        let mut mock = MockSessionStore::new();
        mock.expect_list_delegations()
            .returning(move |_| Ok(vec![delegation.clone()]));
        let guard = make_guard(Arc::new(mock));

        let result = guard
            .check_delegation_allowed("sess-1", &hash, "agent-y")
            .await;
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.contains("MAX RETRIES EXCEEDED") || msg.contains("retries"));
    }

    #[tokio::test]
    async fn check_delegation_allowed_permits_completed_delegation() {
        let objective = "completed task";
        let hash = RapidHash::new(objective.as_bytes());
        let hash_clone = hash.clone();

        let delegation = Delegation {
            id: 1,
            public_id: "del-done".to_string(),
            session_id: 1,
            task_id: None,
            target_agent_id: "agent-z".to_string(),
            objective: objective.to_string(),
            objective_hash: hash_clone,
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: None,
            planning_summary: None,
            status: DelegationStatus::Complete, // completed is fine
            retry_count: 0,
            created_at: OffsetDateTime::now_utc(),
            completed_at: Some(OffsetDateTime::now_utc()),
        };

        let mut mock = MockSessionStore::new();
        mock.expect_list_delegations()
            .returning(move |_| Ok(vec![delegation.clone()]));
        let guard = make_guard(Arc::new(mock));

        let result = guard
            .check_delegation_allowed("sess-1", &hash, "agent-z")
            .await;
        assert!(
            result.is_none(),
            "Completed delegation should allow new delegation"
        );
    }
}

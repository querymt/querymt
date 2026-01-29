use crate::events::StopType;
use crate::middleware::{ExecutionState, MiddlewareDriver, Result};
use crate::session::domain::DelegationStatus;
use crate::session::store::SessionStore;
use async_trait::async_trait;
use log::{debug, trace};
use querymt::chat::ChatRole;
use std::sync::Arc;
use time::OffsetDateTime;

#[derive(Debug, Clone)]
pub struct DelegationConfig {
    /// When to inject available agents into context
    pub context_timing: crate::agent::DelegationContextTiming,
    /// Whether to prevent duplicate delegations to same agent
    pub prevent_duplicates: bool,
    /// Auto-inject available agents when registry is provided
    pub auto_inject: bool,
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            context_timing: crate::agent::DelegationContextTiming::FirstTurnOnly,
            prevent_duplicates: false,
            auto_inject: true,
        }
    }
}

/// Middleware that handles delegation context injection and duplicate prevention
pub struct DelegationMiddleware {
    config: DelegationConfig,
    agent_registry: Arc<dyn crate::delegation::AgentRegistry>,
    store: Arc<dyn SessionStore>,
    max_retries: u32,
    duplicate_window_secs: i64,
}

impl DelegationMiddleware {
    pub fn new(
        store: Arc<dyn SessionStore>,
        agent_registry: Arc<dyn crate::delegation::AgentRegistry>,
        config: DelegationConfig,
    ) -> Self {
        debug!(
            "Creating DelegationMiddleware with timing: {:?}, prevent_duplicates: {}",
            config.context_timing, config.prevent_duplicates
        );
        Self {
            config,
            agent_registry,
            store,
            max_retries: 3,
            duplicate_window_secs: 5,
        }
    }

    fn format_agent_context(&self) -> String {
        let agents = self.agent_registry.list_agents();
        if agents.is_empty() {
            trace!("DelegationMiddleware: no agents to inject");
            return String::new();
        }

        let agent_count = agents.len();
        let mut output = String::from("## Available Agents for Delegation\n\n");
        output.push_str(
            "You have access to the following specialized agents via the `delegate` tool:\n\n",
        );

        for agent in agents {
            output.push_str(&format!("### {} (`{}`)\n", agent.name, agent.id));
            output.push_str(&format!("**Description:** {}\n", agent.description));
            if !agent.capabilities.is_empty() {
                output.push_str(&format!(
                    "**Capabilities:** {}\n",
                    agent.capabilities.join(", ")
                ));
            }
            output.push('\n');
        }

        output.push_str(
            "Use the `delegate` tool to assign tasks to these agents when their expertise is needed.\n",
        );

        debug!(
            "DelegationMiddleware: formatted context for {} agents",
            agent_count
        );

        output
    }

    fn should_inject(&self, context: &crate::middleware::ConversationContext) -> bool {
        use crate::agent::DelegationContextTiming;

        if !self.config.auto_inject {
            return false;
        }

        let should = match self.config.context_timing {
            DelegationContextTiming::FirstTurnOnly => {
                let user_count = context.stats.turns;
                let result = user_count == 1;
                trace!(
                    "DelegationMiddleware: FirstTurnOnly check - user_count = {}, should_inject = {}",
                    user_count, result
                );
                result
            }
            DelegationContextTiming::EveryTurn => {
                trace!("DelegationMiddleware: EveryTurn - should_inject = true");
                true
            }
            DelegationContextTiming::Disabled => {
                trace!("DelegationMiddleware: Disabled - should_inject = false");
                false
            }
        };

        if should {
            debug!("DelegationMiddleware: will inject agent context");
        }

        should
    }

    async fn check_delegation_allowed(
        &self,
        session_id: &str,
        objective_hash: &crate::hash::RapidHash,
        target_agent_id: &str,
    ) -> Option<String> {
        let delegations = match self.store.list_delegations(session_id).await {
            Ok(d) => d,
            Err(_) => return None,
        };

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
                            target_agent_id, del.id, del.status
                        ));
                    }
                    DelegationStatus::Failed => {
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
                                del.retry_count, self.max_retries, del.id
                            ));
                        }

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
                    DelegationStatus::Complete => {}
                }
            }
        }

        None
    }
}

#[async_trait]
impl MiddlewareDriver for DelegationMiddleware {
    async fn on_turn_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "DelegationMiddleware::on_turn_start entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeLlmCall { ref context } => {
                if self.should_inject(context) {
                    let message = self.format_agent_context();
                    if !message.is_empty() {
                        debug!("DelegationMiddleware: injecting agent context message");
                        let new_context = context.inject_message(message);
                        return Ok(ExecutionState::BeforeLlmCall {
                            context: Arc::new(new_context),
                        });
                    }
                }

                Ok(state)
            }
            _ => Ok(state),
        }
    }

    async fn on_after_llm(&self, state: ExecutionState) -> Result<ExecutionState> {
        match state {
            ExecutionState::AfterLlm { ref context, .. } => {
                if !self.config.prevent_duplicates {
                    return Ok(state);
                }

                let history = match self.store.get_history(&context.session_id).await {
                    Ok(h) => h,
                    Err(_) => return Ok(state),
                };

                if let Some(last_msg) = history.last() {
                    if last_msg.role != ChatRole::Assistant {
                        return Ok(state);
                    }

                    use crate::model::MessagePart;

                    for part in &last_msg.parts {
                        if let MessagePart::ToolUse(tool_call) = part
                            && tool_call.function.name == "delegate"
                            && let Ok(args) = serde_json::from_str::<serde_json::Value>(
                                &tool_call.function.arguments,
                            )
                        {
                            let target_agent_id = args
                                .get("target_agent_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let objective =
                                args.get("objective").and_then(|v| v.as_str()).unwrap_or("");

                            if !target_agent_id.is_empty() && !objective.is_empty() {
                                let objective_hash =
                                    crate::hash::RapidHash::new(objective.as_bytes());

                                if let Some(warning) = self
                                    .check_delegation_allowed(
                                        &context.session_id,
                                        &objective_hash,
                                        target_agent_id,
                                    )
                                    .await
                                {
                                    debug!("DelegationMiddleware: blocking duplicate delegation");

                                    return Ok(ExecutionState::Stopped {
                                        message: warning.into(),
                                        stop_type: StopType::DelegationBlocked,
                                    });
                                }
                            }
                        }
                    }
                }

                Ok(state)
            }
            _ => Ok(state),
        }
    }

    fn reset(&self) {
        trace!("DelegationMiddleware::reset");
    }

    fn name(&self) -> &'static str {
        "DelegationMiddleware"
    }
}

/// Middleware that injects agent registry context into the conversation
pub struct DelegationContextMiddleware {
    agent_registry: Arc<dyn crate::delegation::AgentRegistry>,
    timing: crate::agent::DelegationContextTiming,
}

impl DelegationContextMiddleware {
    pub fn new(
        agent_registry: Arc<dyn crate::delegation::AgentRegistry>,
        timing: crate::agent::DelegationContextTiming,
    ) -> Self {
        debug!(
            "Creating DelegationContextMiddleware with timing: {:?} and {} agents",
            timing,
            agent_registry.list_agents().len()
        );
        Self {
            agent_registry,
            timing,
        }
    }

    fn format_agent_context(&self) -> String {
        let agents = self.agent_registry.list_agents();
        if agents.is_empty() {
            trace!("DelegationContextMiddleware: no agents to inject");
            return String::new();
        }

        let agent_count = agents.len();
        let mut output = String::from("## Available Agents for Delegation\n\n");
        output.push_str(
            "You have access to the following specialized agents via the `delegate` tool:\n\n",
        );

        for agent in agents {
            output.push_str(&format!("### {} (`{}`)\n", agent.name, agent.id));
            output.push_str(&format!("**Description:** {}\n", agent.description));
            if !agent.capabilities.is_empty() {
                output.push_str(&format!(
                    "**Capabilities:** {}\n",
                    agent.capabilities.join(", ")
                ));
            }
            output.push('\n');
        }

        output.push_str("Use the `delegate` tool to assign tasks to these agents when their expertise is needed.\n");

        debug!(
            "DelegationContextMiddleware: formatted context for {} agents",
            agent_count
        );

        output
    }

    fn should_inject(&self, context: &crate::middleware::ConversationContext) -> bool {
        use crate::agent::DelegationContextTiming;

        let should = match self.timing {
            DelegationContextTiming::FirstTurnOnly => {
                // Count user messages, not total history length
                let user_count = context.stats.turns;
                let result = user_count == 1;
                trace!(
                    "DelegationContextMiddleware: FirstTurnOnly check - user_count = {}, should_inject = {}",
                    user_count, result
                );
                result
            }
            DelegationContextTiming::EveryTurn => {
                trace!("DelegationContextMiddleware: EveryTurn - should_inject = true");
                true
            }
            DelegationContextTiming::Disabled => {
                trace!("DelegationContextMiddleware: Disabled - should_inject = false");
                false
            }
        };

        if should {
            debug!("DelegationContextMiddleware: will inject agent context");
        }

        should
    }
}

#[async_trait]
impl MiddlewareDriver for DelegationContextMiddleware {
    async fn on_turn_start(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "DelegationContextMiddleware::on_turn_start entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeLlmCall { ref context } => {
                if self.should_inject(context) {
                    let message = self.format_agent_context();
                    if !message.is_empty() {
                        debug!("DelegationContextMiddleware: injecting agent context message");
                        let new_context = context.inject_message(message);
                        return Ok(ExecutionState::BeforeLlmCall {
                            context: Arc::new(new_context),
                        });
                    }
                }

                trace!("DelegationContextMiddleware: no injection, passing through");
                Ok(state)
            }
            _ => {
                trace!(
                    "DelegationContextMiddleware: pass-through for state {}",
                    state.name()
                );
                Ok(state)
            }
        }
    }

    fn reset(&self) {
        trace!("DelegationContextMiddleware::reset");
    }

    fn name(&self) -> &'static str {
        "DelegationContextMiddleware"
    }
}

#[cfg(test)]
mod tests {
    // Tests removed for now - need full AgentRegistry trait implementation
    // Will be added back once core architecture is complete
}

use agent_client_protocol::StopReason;
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiddlewareAction {
    Continue,
    Stop,
    Compact,
    InjectMessage,
}

#[derive(Debug, Clone)]
pub struct MiddlewareResult {
    pub action: MiddlewareAction,
    pub message: Option<String>,
    pub reason: Option<String>,
    pub stop_reason: Option<StopReason>,
}

impl Default for MiddlewareResult {
    fn default() -> Self {
        Self {
            action: MiddlewareAction::Continue,
            message: None,
            reason: None,
            stop_reason: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentStats {
    pub steps: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub context_tokens: usize,
}

impl Default for AgentStats {
    fn default() -> Self {
        Self {
            steps: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            context_tokens: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConversationContext {
    pub session_id: String,
    pub history_len: usize,
    pub stats: AgentStats,
}

#[async_trait(?Send)]
pub trait ConversationMiddleware: Send + Sync {
    async fn before_turn(&self, _context: &ConversationContext) -> MiddlewareResult {
        MiddlewareResult::default()
    }

    async fn after_turn(&self, _context: &ConversationContext) -> MiddlewareResult {
        MiddlewareResult::default()
    }

    fn reset(&self) {}
}

#[derive(Clone, Default)]
pub struct MiddlewarePipeline {
    middlewares: Vec<Arc<dyn ConversationMiddleware>>,
}

impl MiddlewarePipeline {
    pub fn new() -> Self {
        Self {
            middlewares: Vec::new(),
        }
    }

    pub fn add<M: ConversationMiddleware + 'static>(&mut self, middleware: M) -> &mut Self {
        self.middlewares.push(Arc::new(middleware));
        self
    }

    pub fn extend(&mut self, middlewares: Vec<Arc<dyn ConversationMiddleware>>) -> &mut Self {
        self.middlewares.extend(middlewares);
        self
    }

    pub fn clear(&mut self) {
        self.middlewares.clear();
    }

    pub fn reset(&self) {
        for mw in &self.middlewares {
            mw.reset();
        }
    }

    pub async fn run_before_turn(&self, context: &ConversationContext) -> MiddlewareResult {
        let mut messages_to_inject = Vec::new();

        for mw in &self.middlewares {
            let result = mw.before_turn(context).await;
            match result.action {
                MiddlewareAction::InjectMessage => {
                    if let Some(msg) = result.message {
                        messages_to_inject.push(msg);
                    }
                }
                MiddlewareAction::Stop | MiddlewareAction::Compact => return result,
                MiddlewareAction::Continue => {}
            }
        }

        if messages_to_inject.is_empty() {
            MiddlewareResult::default()
        } else {
            MiddlewareResult {
                action: MiddlewareAction::InjectMessage,
                message: Some(messages_to_inject.join("\n\n")),
                ..MiddlewareResult::default()
            }
        }
    }

    pub async fn run_after_turn(&self, context: &ConversationContext) -> MiddlewareResult {
        for mw in &self.middlewares {
            let result = mw.after_turn(context).await;
            match result.action {
                MiddlewareAction::Stop | MiddlewareAction::Compact => return result,
                MiddlewareAction::Continue => {}
                MiddlewareAction::InjectMessage => {
                    log::warn!("InjectMessage is not supported in after_turn; ignoring middleware");
                }
            }
        }

        MiddlewareResult::default()
    }
}

pub struct MaxStepsMiddleware {
    max_steps: usize,
}

impl MaxStepsMiddleware {
    pub fn new(max_steps: usize) -> Self {
        Self { max_steps }
    }
}

#[async_trait(?Send)]
impl ConversationMiddleware for MaxStepsMiddleware {
    async fn before_turn(&self, context: &ConversationContext) -> MiddlewareResult {
        if context.stats.steps >= self.max_steps {
            return MiddlewareResult {
                action: MiddlewareAction::Stop,
                reason: Some(format!("Max steps ({}) reached", self.max_steps)),
                stop_reason: Some(StopReason::MaxTurnRequests),
                message: None,
            };
        }
        MiddlewareResult::default()
    }
}

pub struct TurnLimitMiddleware {
    max_turns: usize,
}

impl TurnLimitMiddleware {
    pub fn new(max_turns: usize) -> Self {
        Self { max_turns }
    }
}

#[async_trait(?Send)]
impl ConversationMiddleware for TurnLimitMiddleware {
    async fn before_turn(&self, context: &ConversationContext) -> MiddlewareResult {
        if context.stats.steps >= self.max_turns {
            return MiddlewareResult {
                action: MiddlewareAction::Stop,
                reason: Some(format!("Turn limit ({}) reached", self.max_turns)),
                stop_reason: Some(StopReason::MaxTurnRequests),
                message: None,
            };
        }
        MiddlewareResult::default()
    }
}

pub struct PriceLimitMiddleware {
    max_cost: f64,
    input_cost_per_million: f64,
    output_cost_per_million: f64,
}

impl PriceLimitMiddleware {
    pub fn new(max_cost: f64, input_cost_per_million: f64, output_cost_per_million: f64) -> Self {
        Self {
            max_cost,
            input_cost_per_million,
            output_cost_per_million,
        }
    }

    fn total_cost(&self, stats: &AgentStats) -> f64 {
        let input_cost =
            (stats.total_input_tokens as f64 / 1_000_000.0) * self.input_cost_per_million;
        let output_cost =
            (stats.total_output_tokens as f64 / 1_000_000.0) * self.output_cost_per_million;
        input_cost + output_cost
    }
}

#[async_trait(?Send)]
impl ConversationMiddleware for PriceLimitMiddleware {
    async fn before_turn(&self, context: &ConversationContext) -> MiddlewareResult {
        let total_cost = self.total_cost(&context.stats);
        if total_cost > self.max_cost {
            return MiddlewareResult {
                action: MiddlewareAction::Stop,
                reason: Some(format!(
                    "Price limit exceeded: ${:.4} > ${:.2}",
                    total_cost, self.max_cost
                )),
                stop_reason: Some(StopReason::MaxTokens),
                message: None,
            };
        }
        MiddlewareResult::default()
    }
}

pub struct AutoCompactMiddleware {
    threshold_tokens: usize,
}

impl AutoCompactMiddleware {
    pub fn new(threshold_tokens: usize) -> Self {
        Self { threshold_tokens }
    }
}

#[async_trait(?Send)]
impl ConversationMiddleware for AutoCompactMiddleware {
    async fn before_turn(&self, context: &ConversationContext) -> MiddlewareResult {
        if context.stats.context_tokens >= self.threshold_tokens {
            return MiddlewareResult {
                action: MiddlewareAction::Compact,
                message: None,
                reason: Some(format!(
                    "Context token threshold ({}) reached",
                    self.threshold_tokens
                )),
                stop_reason: None,
            };
        }
        MiddlewareResult::default()
    }
}

pub struct ContextWarningMiddleware {
    threshold_percent: f64,
    max_context_tokens: usize,
    warned_sessions: Mutex<HashSet<String>>,
}

impl ContextWarningMiddleware {
    pub fn new(threshold_percent: f64, max_context_tokens: usize) -> Self {
        Self {
            threshold_percent,
            max_context_tokens,
            warned_sessions: Mutex::new(HashSet::new()),
        }
    }
}

#[async_trait(?Send)]
impl ConversationMiddleware for ContextWarningMiddleware {
    async fn before_turn(&self, context: &ConversationContext) -> MiddlewareResult {
        let mut warned = self.warned_sessions.lock().unwrap();
        if warned.contains(&context.session_id) {
            return MiddlewareResult::default();
        }

        let threshold = (self.max_context_tokens as f64 * self.threshold_percent) as usize;
        if context.stats.context_tokens >= threshold {
            warned.insert(context.session_id.clone());
            let percent =
                (context.stats.context_tokens as f64 / self.max_context_tokens as f64) * 100.0;
            return MiddlewareResult {
                action: MiddlewareAction::InjectMessage,
                message: Some(format!(
                    "Warning: context usage is at {:.0}% ({} / {} tokens)",
                    percent, context.stats.context_tokens, self.max_context_tokens
                )),
                reason: None,
                stop_reason: None,
            };
        }
        MiddlewareResult::default()
    }

    fn reset(&self) {
        let mut warned = self.warned_sessions.lock().unwrap();
        warned.clear();
    }
}

pub struct PlanModeMiddleware {
    enabled: Arc<AtomicBool>,
    reminder: String,
}

impl PlanModeMiddleware {
    pub fn new(enabled: Arc<AtomicBool>, reminder: String) -> Self {
        Self { enabled, reminder }
    }
}

#[async_trait(?Send)]
impl ConversationMiddleware for PlanModeMiddleware {
    async fn before_turn(&self, _context: &ConversationContext) -> MiddlewareResult {
        if self.enabled.load(Ordering::Relaxed) {
            return MiddlewareResult {
                action: MiddlewareAction::InjectMessage,
                message: Some(self.reminder.clone()),
                reason: None,
                stop_reason: None,
            };
        }
        MiddlewareResult::default()
    }
}

use crate::middleware::{
    AgentStats, ConversationContext, ExecutionState, MiddlewareDriver, Result,
};
use crate::model_info::{ModelInfoSource, get_model_info};
use agent_client_protocol::StopReason;
use async_trait::async_trait;
use log::{debug, trace};
use querymt::providers::ModelPricing;

#[derive(Debug, Clone)]
pub struct LimitsConfig {
    pub max_steps: Option<usize>,
    pub max_turns: Option<usize>,
    pub max_price_usd: Option<f64>,
    /// Source for model info (pricing, capabilities, etc.) - default is FromSession
    pub model_info_source: ModelInfoSource,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_steps: None,
            max_turns: None,
            max_price_usd: None,
            model_info_source: ModelInfoSource::FromSession,
        }
    }
}

impl LimitsConfig {
    /// Use dynamic model info lookup (default)
    pub fn with_dynamic_model_info() -> Self {
        Self::default()
    }

    /// Use manual pricing (explicit costs)
    pub fn with_manual_pricing(input: f64, output: f64) -> Self {
        Self {
            model_info_source: ModelInfoSource::manual().pricing(input, output),
            ..Default::default()
        }
    }

    pub fn max_steps(mut self, steps: usize) -> Self {
        self.max_steps = Some(steps);
        self
    }

    pub fn max_turns(mut self, turns: usize) -> Self {
        self.max_turns = Some(turns);
        self
    }

    pub fn max_price_usd(mut self, price: f64) -> Self {
        self.max_price_usd = Some(price);
        self
    }

    pub fn model_info_source(mut self, source: ModelInfoSource) -> Self {
        self.model_info_source = source;
        self
    }
}

/// Middleware that enforces step, turn, and price limits
pub struct LimitsMiddleware {
    config: LimitsConfig,
    last_model: std::sync::Mutex<Option<(String, String)>>,
}

impl LimitsMiddleware {
    pub fn new(config: LimitsConfig) -> Self {
        debug!(
            "Creating LimitsMiddleware with max_steps={:?}, max_turns={:?}, max_price_usd={:?}",
            config.max_steps, config.max_turns, config.max_price_usd
        );

        Self {
            config,
            last_model: std::sync::Mutex::new(None),
        }
    }

    /// Calculate total cost for current context
    fn total_cost(&self, stats: &AgentStats, context: &ConversationContext) -> Option<f64> {
        match &self.config.model_info_source {
            ModelInfoSource::FromSession => {
                // Use ModelInfo.calculate_cost() method
                get_model_info(&context.provider, &context.model)?
                    .calculate_cost(stats.total_input_tokens, stats.total_output_tokens)
            }
            ModelInfoSource::Manual {
                input_cost_per_million,
                output_cost_per_million,
                ..
            } => {
                let input = (*input_cost_per_million)?;
                let output = (*output_cost_per_million)?;

                // Use ModelPricing.calculate_cost() method
                let pricing = ModelPricing {
                    input: Some(input),
                    output: Some(output),
                    cache_read: None,
                    cache_write: None,
                };
                pricing.calculate_cost(stats.total_input_tokens, stats.total_output_tokens)
            }
        }
    }

    /// Check if provider changed (for logging/debugging)
    fn check_provider_changed(&self, context: &ConversationContext) {
        let mut last = self.last_model.lock().unwrap();
        let current = (context.provider.to_string(), context.model.to_string());

        if last.as_ref() != Some(&current) {
            *last = Some(current.clone());
            debug!(
                "LimitsMiddleware: Provider changed to {}/{}",
                current.0, current.1
            );
        }
    }
}

#[async_trait]
impl MiddlewareDriver for LimitsMiddleware {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "LimitsMiddleware::next_state entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeTurn { ref context } => {
                self.check_provider_changed(context);

                if let Some(max_steps) = self.config.max_steps
                    && context.stats.steps >= max_steps
                {
                    debug!(
                        "LimitsMiddleware: stopping execution, {} steps >= {}",
                        context.stats.steps, max_steps
                    );
                    return Ok(ExecutionState::Stopped {
                        reason: StopReason::MaxTurnRequests,
                        message: format!("Max steps ({}) reached", max_steps).into(),
                    });
                }

                if let Some(max_turns) = self.config.max_turns
                    && context.stats.steps >= max_turns
                {
                    debug!(
                        "LimitsMiddleware: stopping execution, {} turns >= {}",
                        context.stats.steps, max_turns
                    );
                    return Ok(ExecutionState::Stopped {
                        reason: StopReason::MaxTurnRequests,
                        message: format!("Turn limit ({}) reached", max_turns).into(),
                    });
                }

                if let Some(max_price) = self.config.max_price_usd
                    && let Some(total_cost) = self.total_cost(&context.stats, context)
                    && total_cost > max_price
                {
                    debug!(
                        "LimitsMiddleware: stopping execution, cost ${:.4} > max ${}",
                        total_cost, max_price
                    );
                    return Ok(ExecutionState::Stopped {
                        reason: StopReason::MaxTokens,
                        message: format!(
                            "Price limit exceeded: ${:.4} > ${:.2}",
                            total_cost, max_price
                        )
                        .into(),
                    });
                }

                Ok(state)
            }
            _ => Ok(state),
        }
    }

    fn reset(&self) {
        trace!("LimitsMiddleware::reset");
        let mut last = self.last_model.lock().unwrap();
        *last = None;
    }

    fn name(&self) -> &'static str {
        "LimitsMiddleware"
    }
}

/// Middleware that stops execution after a maximum number of steps
pub struct MaxStepsMiddleware {
    max_steps: usize,
}

impl MaxStepsMiddleware {
    pub fn new(max_steps: usize) -> Self {
        debug!("Creating MaxStepsMiddleware with max_steps = {}", max_steps);
        Self { max_steps }
    }
}

#[async_trait]
impl MiddlewareDriver for MaxStepsMiddleware {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "MaxStepsMiddleware::next_state entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeTurn { ref context } => {
                let current_steps = context.stats.steps;
                trace!(
                    "MaxStepsMiddleware: current steps = {}, max = {}",
                    current_steps, self.max_steps
                );

                if current_steps >= self.max_steps {
                    debug!(
                        "MaxStepsMiddleware: stopping execution, {} steps >= {}",
                        current_steps, self.max_steps
                    );
                    Ok(ExecutionState::Stopped {
                        reason: StopReason::MaxTurnRequests,
                        message: format!("Max steps ({}) reached", self.max_steps).into(),
                    })
                } else {
                    trace!("MaxStepsMiddleware: allowing execution to continue");
                    Ok(state)
                }
            }
            _ => {
                trace!(
                    "MaxStepsMiddleware: pass-through for state {}",
                    state.name()
                );
                Ok(state)
            }
        }
    }

    fn reset(&self) {
        trace!("MaxStepsMiddleware::reset");
    }

    fn name(&self) -> &'static str {
        "MaxStepsMiddleware"
    }
}

/// Middleware that limits the maximum number of conversation turns
pub struct TurnLimitMiddleware {
    max_turns: usize,
}

impl TurnLimitMiddleware {
    pub fn new(max_turns: usize) -> Self {
        debug!(
            "Creating TurnLimitMiddleware with max_turns = {}",
            max_turns
        );
        Self { max_turns }
    }
}

#[async_trait]
impl MiddlewareDriver for TurnLimitMiddleware {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "TurnLimitMiddleware::next_state entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeTurn { ref context } => {
                let current_steps = context.stats.steps;
                trace!(
                    "TurnLimitMiddleware: current steps = {}, max = {}",
                    current_steps, self.max_turns
                );

                if current_steps >= self.max_turns {
                    debug!(
                        "TurnLimitMiddleware: stopping execution, {} steps >= {}",
                        current_steps, self.max_turns
                    );
                    Ok(ExecutionState::Stopped {
                        reason: StopReason::MaxTurnRequests,
                        message: format!("Turn limit ({}) reached", self.max_turns).into(),
                    })
                } else {
                    trace!("TurnLimitMiddleware: allowing execution to continue");
                    Ok(state)
                }
            }
            _ => {
                trace!(
                    "TurnLimitMiddleware: pass-through for state {}",
                    state.name()
                );
                Ok(state)
            }
        }
    }

    fn reset(&self) {
        trace!("TurnLimitMiddleware::reset");
    }

    fn name(&self) -> &'static str {
        "TurnLimitMiddleware"
    }
}

/// Middleware that enforces a price limit based on token usage
pub struct PriceLimitMiddleware {
    max_cost: f64,
    input_cost_per_million: f64,
    output_cost_per_million: f64,
}

impl PriceLimitMiddleware {
    pub fn new(max_cost: f64, input_cost_per_million: f64, output_cost_per_million: f64) -> Self {
        debug!(
            "Creating PriceLimitMiddleware: max_cost = ${}, input_cost = ${}/1M, output_cost = ${}/1M",
            max_cost, input_cost_per_million, output_cost_per_million
        );
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
        let total = input_cost + output_cost;
        trace!("PriceLimitMiddleware: total cost = ${:.4}", total);
        total
    }
}

#[async_trait]
impl MiddlewareDriver for PriceLimitMiddleware {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "PriceLimitMiddleware::next_state entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeTurn { ref context } => {
                let total_cost = self.total_cost(&context.stats);

                if total_cost > self.max_cost {
                    debug!(
                        "PriceLimitMiddleware: stopping execution, cost ${:.4} > max ${}",
                        total_cost, self.max_cost
                    );
                    Ok(ExecutionState::Stopped {
                        reason: StopReason::MaxTokens,
                        message: format!(
                            "Price limit exceeded: ${:.4} > ${:.2}",
                            total_cost, self.max_cost
                        )
                        .into(),
                    })
                } else {
                    trace!(
                        "PriceLimitMiddleware: cost ${:.4} <= max ${}, allowing execution",
                        total_cost, self.max_cost
                    );
                    Ok(state)
                }
            }
            _ => {
                trace!(
                    "PriceLimitMiddleware: pass-through for state {}",
                    state.name()
                );
                Ok(state)
            }
        }
    }

    fn reset(&self) {
        trace!("PriceLimitMiddleware::reset");
    }

    fn name(&self) -> &'static str {
        "PriceLimitMiddleware"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{AgentStats, ConversationContext};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_max_steps_stop() {
        let middleware = MaxStepsMiddleware::new(5);
        let stats = Arc::new(AgentStats {
            steps: 5,
            ..Default::default()
        });
        let context = Arc::new(ConversationContext {
            session_id: "test".into(),
            messages: Arc::from([]),
            stats,
            provider: "mock".into(),
            model: "mock-model".into(),
        });

        let state = ExecutionState::BeforeTurn { context };
        let result = middleware.next_state(state).await.unwrap();

        assert!(matches!(
            result,
            ExecutionState::Stopped {
                reason: StopReason::MaxTurnRequests,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_max_steps_continue() {
        let middleware = MaxStepsMiddleware::new(5);
        let stats = Arc::new(AgentStats {
            steps: 3,
            ..Default::default()
        });
        let context = Arc::new(ConversationContext {
            session_id: "test".into(),
            messages: Arc::from([]),
            stats,
            provider: "mock".into(),
            model: "mock-model".into(),
        });

        let state = ExecutionState::BeforeTurn { context };
        let result = middleware.next_state(state).await.unwrap();

        assert!(matches!(result, ExecutionState::BeforeTurn { .. }));
    }
}

use crate::agent::utils::u32_from_usize;
use crate::events::StopType;
use crate::middleware::factory::MiddlewareFactory;
use crate::middleware::{ConversationContext, ExecutionState, MiddlewareDriver, Result};
use crate::model_info::{InvalidManualPricing, ModelInfoSource};
use async_trait::async_trait;
use log::{debug, trace};
use parking_lot::Mutex;
use querymt::providers::ModelPricing;
use serde::Deserialize;
use std::sync::Arc;

/// Configuration for execution limits
///
/// # Terminology
///
/// - **Steps**: Individual LLM calls (including tool use). Each time the agent calls
///   the LLM API, `steps` is incremented. This includes intermediate calls for tool
///   execution.
///
/// - **Turns**: User/Assistant message pairs. Each user message starts a new turn.
///   A single turn may involve multiple steps if the agent needs to call tools.
///
/// # Example
///
/// ```text
/// User: "What's the weather in SF?"
/// Agent: [calls weather tool] → [gets result] → [responds to user]
///
/// This is: 1 turn, 2+ steps (initial call + tool calls + final response)
/// ```
#[derive(Debug, Clone)]
pub struct LimitsConfig {
    /// Maximum number of LLM calls (including tool use loops)
    pub max_steps: Option<usize>,
    /// Maximum number of user/assistant conversation turns (based on recorded turns)
    pub max_turns: Option<usize>,
    /// Maximum total cost in USD
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
///
/// - **Steps**: Counts every LLM API call (via `context.stats.steps`)
/// - **Turns**: Counts user/assistant message pairs (via `context.stats.turns`)
/// - **Price**: Calculates cumulative cost based on token usage
pub struct LimitsMiddleware {
    config: LimitsConfig,
    last_model: Mutex<Option<(String, String)>>,
}

impl LimitsMiddleware {
    /// Build the middleware, rejecting invalid manual pricing (a negative or
    /// non-finite rate). Configuration entry points use this so a bad rate
    /// fails validation instead of silently disabling budget valuation.
    pub fn try_new(config: LimitsConfig) -> std::result::Result<Self, InvalidManualPricing> {
        config.model_info_source.budget_pricing()?;
        Ok(Self::new(config))
    }

    pub fn new(config: LimitsConfig) -> Self {
        if let Err(error) = config.model_info_source.budget_pricing() {
            log::error!("LimitsMiddleware: {error}; manual budget valuation is disabled");
        }
        debug!(
            "Creating LimitsMiddleware with max_steps={:?}, max_turns={:?}, max_price_usd={:?}",
            config.max_steps, config.max_turns, config.max_price_usd
        );

        Self {
            config,
            last_model: Mutex::new(None),
        }
    }

    /// Check if provider changed (for logging/debugging)
    fn check_provider_changed(&self, context: &ConversationContext) {
        let mut last = self.last_model.lock();
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
    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
        trace!(
            "LimitsMiddleware::on_step_start entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeLlmCall { ref context } => {
                self.check_provider_changed(context);

                if let Some(max_steps) = self.config.max_steps
                    && context.stats.steps >= max_steps
                {
                    debug!(
                        "LimitsMiddleware: stopping execution, {} steps >= {}",
                        context.stats.steps, max_steps
                    );
                    return Ok(ExecutionState::Stopped {
                        message: format!("Max steps ({}) reached", max_steps).into(),
                        stop_type: StopType::StepLimit,
                        context: Some(context.clone()),
                    });
                }

                if let Some(max_turns) = self.config.max_turns
                    && context.stats.turns >= max_turns
                {
                    debug!(
                        "LimitsMiddleware: stopping execution, {} turns >= {}",
                        context.stats.turns, max_turns
                    );
                    return Ok(ExecutionState::Stopped {
                        message: format!("Turn limit ({}) reached", max_turns).into(),
                        stop_type: StopType::TurnLimit,
                        context: Some(context.clone()),
                    });
                }

                // Shared cycle-accounting check: this cycle's committed budget
                // view (cache and auxiliary attempts included), never a
                // revaluation of token counters. Prior cycles do not count.
                if let Some(max_price) = self.config.max_price_usd
                    && context.stats.budget_cost_usd >= max_price
                {
                    let total_cost = context.stats.budget_cost_usd;
                    debug!(
                        "LimitsMiddleware: stopping execution, cost ${:.4} >= max ${}",
                        total_cost, max_price
                    );
                    return Ok(ExecutionState::Stopped {
                        message: format!(
                            "Price limit reached: ${:.4} >= ${:.2}",
                            total_cost, max_price
                        )
                        .into(),
                        stop_type: StopType::PriceLimit,
                        context: Some(context.clone()),
                    });
                }

                Ok(state)
            }
            _ => Ok(state),
        }
    }

    fn budget_pricing(&self) -> Option<ModelPricing> {
        // Configuration entry points validate via `try_new`; a directly
        // constructed middleware with an invalid rate yields no budget
        // valuation (logged at construction) rather than a bogus price.
        self.config
            .model_info_source
            .budget_pricing()
            .ok()
            .flatten()
    }

    fn reset(&self) {
        trace!("LimitsMiddleware::reset");
        let mut last = self.last_model.lock();
        *last = None;
    }

    fn name(&self) -> &'static str {
        "LimitsMiddleware"
    }

    fn get_limits(&self) -> Option<crate::events::SessionLimits> {
        Some(crate::events::SessionLimits {
            max_steps: self
                .config
                .max_steps
                .map(|v| u32_from_usize(v, "LimitsConfig.max_steps", None)),
            max_turns: self
                .config
                .max_turns
                .map(|v| u32_from_usize(v, "LimitsConfig.max_turns", None)),
            max_cost_usd: self.config.max_price_usd,
        })
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
    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
        trace!(
            "MaxStepsMiddleware::on_step_start entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeLlmCall { ref context } => {
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
                        message: format!("Max steps ({}) reached", self.max_steps).into(),
                        stop_type: StopType::StepLimit,
                        context: Some(context.clone()),
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

    fn get_limits(&self) -> Option<crate::events::SessionLimits> {
        Some(crate::events::SessionLimits {
            max_steps: Some(u32_from_usize(
                self.max_steps,
                "MaxStepsMiddleware.max_steps",
                None,
            )),
            max_turns: None,
            max_cost_usd: None,
        })
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
    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
        trace!(
            "TurnLimitMiddleware::on_step_start entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeLlmCall { ref context } => {
                let current_turns = context.stats.turns;
                trace!(
                    "TurnLimitMiddleware: current turns = {}, max = {}",
                    current_turns, self.max_turns
                );

                if current_turns >= self.max_turns {
                    debug!(
                        "TurnLimitMiddleware: stopping execution, {} turns >= {}",
                        current_turns, self.max_turns
                    );
                    Ok(ExecutionState::Stopped {
                        message: format!("Turn limit ({}) reached", self.max_turns).into(),
                        stop_type: StopType::TurnLimit,
                        context: Some(context.clone()),
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

    fn get_limits(&self) -> Option<crate::events::SessionLimits> {
        Some(crate::events::SessionLimits {
            max_steps: None,
            max_turns: Some(u32_from_usize(
                self.max_turns,
                "TurnLimitMiddleware.max_turns",
                None,
            )),
            max_cost_usd: None,
        })
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
}

#[async_trait]
impl MiddlewareDriver for PriceLimitMiddleware {
    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
        trace!(
            "PriceLimitMiddleware::on_step_start entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeLlmCall { ref context } => {
                let total_cost = context.stats.budget_cost_usd;

                if total_cost >= self.max_cost {
                    debug!(
                        "PriceLimitMiddleware: stopping execution, cost ${:.4} >= max ${}",
                        total_cost, self.max_cost
                    );
                    Ok(ExecutionState::Stopped {
                        message: format!(
                            "Price limit reached: ${:.4} >= ${:.2}",
                            total_cost, self.max_cost
                        )
                        .into(),
                        stop_type: StopType::PriceLimit,
                        context: Some(context.clone()),
                    })
                } else {
                    trace!(
                        "PriceLimitMiddleware: cost ${:.4} < max ${}, allowing execution",
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

    fn budget_pricing(&self) -> Option<ModelPricing> {
        // The configured rates are the manual budget valuation, snapshotted
        // onto each attempt. Cache rates stay unset, never inferred.
        Some(ModelPricing {
            input: Some(self.input_cost_per_million),
            output: Some(self.output_cost_per_million),
            source: Some(querymt::providers::PricingSource::Manual),
            ..Default::default()
        })
    }

    fn reset(&self) {
        trace!("PriceLimitMiddleware::reset");
    }

    fn name(&self) -> &'static str {
        "PriceLimitMiddleware"
    }

    fn get_limits(&self) -> Option<crate::events::SessionLimits> {
        Some(crate::events::SessionLimits {
            max_steps: None,
            max_turns: None,
            max_cost_usd: Some(self.max_cost),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{AgentStats, ConversationContext};
    use std::sync::Arc;

    #[test]
    fn try_new_rejects_invalid_manual_rates() {
        let bad = LimitsConfig::default()
            .model_info_source(ModelInfoSource::manual().cache_pricing(Some(-1.0), None, None));
        let err = LimitsMiddleware::try_new(bad).err().expect("rejected");
        assert_eq!(err.field, "cache_read_cost_per_million");
    }

    #[test]
    fn middleware_exposes_manual_budget_pricing_to_the_driver_chain() {
        let config = LimitsConfig::default().model_info_source(
            ModelInfoSource::manual().pricing(3.0, 15.0).cache_pricing(
                Some(0.3),
                Some(3.75),
                Some(6.0),
            ),
        );
        let driver: Arc<dyn MiddlewareDriver> =
            Arc::new(LimitsMiddleware::try_new(config).unwrap());
        let pricing = crate::middleware::CompositeDriver::new(vec![driver])
            .budget_pricing()
            .expect("budget pricing");
        assert_eq!(pricing.cache_write_1h, Some(6.0));

        let none: Arc<dyn MiddlewareDriver> =
            Arc::new(LimitsMiddleware::new(LimitsConfig::default()));
        assert!(
            crate::middleware::CompositeDriver::new(vec![none])
                .budget_pricing()
                .is_none()
        );
    }

    async fn factory_agent_config() -> (crate::agent::agent_config::AgentConfig, tempfile::TempDir)
    {
        use crate::agent::agent_config_builder::AgentConfigBuilder;
        use crate::session::sqlite_storage::SqliteStorage;
        use crate::test_utils::helpers::empty_plugin_registry;
        let (registry, temp_dir) = empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let llm = querymt::LLMParams::new().provider("mock").model("mock");
        (
            AgentConfigBuilder::new(Arc::new(registry), storage, llm).build(),
            temp_dir,
        )
    }

    #[tokio::test]
    async fn factory_parses_manual_rates_into_budget_pricing() {
        let (agent_config, _td) = factory_agent_config().await;
        let driver = LimitsFactory
            .create(
                &serde_json::json!({
                    "max_price_usd": 1.0,
                    "input_cost_per_million": 3.0,
                    "output_cost_per_million": 15.0,
                    "cache_read_cost_per_million": 0.3,
                    "cache_write_cost_per_million": 3.75,
                    "cache_write_1h_cost_per_million": 6.0,
                }),
                &agent_config,
            )
            .unwrap();
        let p = driver.budget_pricing().expect("budget pricing");
        assert_eq!(
            (
                p.input,
                p.output,
                p.cache_read,
                p.cache_write,
                p.cache_write_1h
            ),
            (Some(3.0), Some(15.0), Some(0.3), Some(3.75), Some(6.0))
        );
    }

    /// The `limits` example in `docs/docs/agent/middleware.md` must parse.
    #[tokio::test]
    async fn documented_price_limit_example_parses() {
        let docs = include_str!("../../../../docs/docs/agent/middleware.md");
        let start = docs
            .find("#### Price limits")
            .expect("price limits section");
        let block = docs[start..]
            .split("```toml")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("toml example");
        let value: toml::Value = toml::from_str(block).expect("example is valid TOML");
        let entry = value["middleware"][0].clone();
        assert_eq!(entry["type"].as_str(), Some("limits"));
        let json = serde_json::to_value(&entry).unwrap();

        let (agent_config, _td) = factory_agent_config().await;
        let driver = LimitsFactory.create(&json, &agent_config).unwrap();
        let p = driver.budget_pricing().expect("budget pricing");
        assert_eq!(
            (p.cache_read, p.cache_write, p.cache_write_1h),
            (Some(0.3), Some(3.75), Some(6.0))
        );
        assert_eq!(driver.get_limits().and_then(|l| l.max_cost_usd), Some(1.0));
    }

    #[tokio::test]
    async fn factory_without_manual_rates_has_no_budget_pricing() {
        let (agent_config, _td) = factory_agent_config().await;
        let driver = LimitsFactory
            .create(&serde_json::json!({"max_steps": 5}), &agent_config)
            .unwrap();
        assert!(driver.budget_pricing().is_none());
    }

    #[tokio::test]
    async fn factory_rejects_invalid_manual_rates() {
        let (agent_config, _td) = factory_agent_config().await;
        let result = LimitsFactory.create(
            &serde_json::json!({"cache_write_cost_per_million": -2.0}),
            &agent_config,
        );
        let error = result.err().expect("negative rate must be rejected");
        assert!(error.to_string().contains("cache_write_cost_per_million"));
    }

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
            fragments: Arc::from([]),
            stats,
            provider: "mock".into(),
            model: "mock-model".into(),
            session_mode: crate::agent::core::AgentMode::Build,
            request_overrides: Default::default(),
            profile: Default::default(),
            last_applied: Default::default(),
        });

        let state = ExecutionState::BeforeLlmCall { context };
        let result = middleware.on_step_start(state, None).await.unwrap();

        assert!(matches!(
            result,
            ExecutionState::Stopped {
                stop_type: StopType::StepLimit,
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
            fragments: Arc::from([]),
            stats,
            provider: "mock".into(),
            model: "mock-model".into(),
            session_mode: crate::agent::core::AgentMode::Build,
            request_overrides: Default::default(),
            profile: Default::default(),
            last_applied: Default::default(),
        });

        let state = ExecutionState::BeforeLlmCall { context };
        let result = middleware.on_step_start(state, None).await.unwrap();

        assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
    }
}

/// Factory for creating LimitsMiddleware from config
pub struct LimitsFactory;

/// Configuration structure for LimitsMiddleware
///
/// # TOML Example
///
/// ```toml
/// [[middleware]]
/// type = "limits"
/// max_steps = 100    # Total LLM API calls (including tool use)
/// max_turns = 20     # User/Assistant message pairs
/// max_price_usd = 1.0
///
/// # Optional manual budget rates (USD per million tokens). Valued separately
/// # from reported API spend; unset cache rates stay unset (never defaulted to
/// # the input rate), so used-but-unpriced cache categories are partial.
/// input_cost_per_million = 3.0
/// output_cost_per_million = 15.0
/// cache_read_cost_per_million = 0.3
/// cache_write_cost_per_million = 3.75
/// cache_write_1h_cost_per_million = 6.0
/// ```
#[derive(Debug, Deserialize)]
#[serde(default)]
struct LimitsFactoryConfig {
    enabled: bool,
    /// Maximum number of LLM calls (including tool invocations)
    max_steps: Option<usize>,
    /// Maximum number of user/assistant conversation turns
    max_turns: Option<usize>,
    /// Maximum total cost in USD
    max_price_usd: Option<f64>,
    input_cost_per_million: Option<f64>,
    output_cost_per_million: Option<f64>,
    cache_read_cost_per_million: Option<f64>,
    cache_write_cost_per_million: Option<f64>,
    cache_write_1h_cost_per_million: Option<f64>,
}

impl LimitsFactoryConfig {
    fn has_manual_rates(&self) -> bool {
        [
            self.input_cost_per_million,
            self.output_cost_per_million,
            self.cache_read_cost_per_million,
            self.cache_write_cost_per_million,
            self.cache_write_1h_cost_per_million,
        ]
        .iter()
        .any(Option::is_some)
    }
}

impl Default for LimitsFactoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_steps: None,
            max_turns: None,
            max_price_usd: None,
            input_cost_per_million: None,
            output_cost_per_million: None,
            cache_read_cost_per_million: None,
            cache_write_cost_per_million: None,
            cache_write_1h_cost_per_million: None,
        }
    }
}

impl MiddlewareFactory for LimitsFactory {
    fn type_name(&self) -> &'static str {
        "limits"
    }

    fn create(
        &self,
        config: &serde_json::Value,
        _agent_config: &crate::agent::agent_config::AgentConfig,
    ) -> anyhow::Result<Arc<dyn MiddlewareDriver>> {
        let cfg: LimitsFactoryConfig = serde_json::from_value(config.clone())?;

        if !cfg.enabled {
            return Err(anyhow::anyhow!("Middleware disabled"));
        }

        let mut limits_config = LimitsConfig::default();

        if let Some(max_steps) = cfg.max_steps {
            limits_config = limits_config.max_steps(max_steps);
        }

        if let Some(max_turns) = cfg.max_turns {
            limits_config = limits_config.max_turns(max_turns);
        }

        if let Some(max_price_usd) = cfg.max_price_usd {
            limits_config = limits_config.max_price_usd(max_price_usd);
        }

        if cfg.has_manual_rates() {
            let mut source = ModelInfoSource::manual().cache_pricing(
                cfg.cache_read_cost_per_million,
                cfg.cache_write_cost_per_million,
                cfg.cache_write_1h_cost_per_million,
            );
            if let ModelInfoSource::Manual {
                ref mut input_cost_per_million,
                ref mut output_cost_per_million,
                ..
            } = source
            {
                *input_cost_per_million = cfg.input_cost_per_million;
                *output_cost_per_million = cfg.output_cost_per_million;
            }
            limits_config = limits_config.model_info_source(source);
        }

        // Invalid (negative / non-finite) manual rates fail validation.
        Ok(Arc::new(LimitsMiddleware::try_new(limits_config)?))
    }
}

use crate::agent::utils::u32_from_usize;
use crate::middleware::factory::MiddlewareFactory;
use crate::middleware::{HookCx, Middleware, Result, StepVerdict, Stop, TurnInfo, TurnMiddleware};
use async_trait::async_trait;
use log::debug;
use querymt::providers::ModelPricing;
use querymt_agent_core::events::{SessionLimits, StopType};
use querymt_agent_services::model_info::{InvalidManualPricing, ModelInfoSource};
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
#[derive(Clone)]
pub struct LimitsMiddleware {
    config: LimitsConfig,
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

        Self { config }
    }
}

impl Middleware for LimitsMiddleware {
    fn name(&self) -> &'static str {
        "LimitsMiddleware"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(LimitsTurn {
            config: self.config.clone(),
        })
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

    fn limits(&self) -> Option<SessionLimits> {
        Some(SessionLimits {
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

/// The limits of one run. The counters it checks are the run's own stats.
struct LimitsTurn {
    config: LimitsConfig,
}

#[async_trait]
impl TurnMiddleware for LimitsTurn {
    async fn on_step_start(&mut self, cx: &mut HookCx<'_>) -> Result<StepVerdict> {
        let stats = cx.stats();

        if let Some(max_steps) = self.config.max_steps
            && stats.steps >= max_steps
        {
            debug!(
                "LimitsMiddleware: stopping execution, {} steps >= {}",
                stats.steps, max_steps
            );
            return Ok(StepVerdict::Stop(Stop::new(
                StopType::StepLimit,
                format!("Max steps ({max_steps}) reached"),
            )));
        }

        if let Some(max_turns) = self.config.max_turns
            && stats.turns >= max_turns
        {
            debug!(
                "LimitsMiddleware: stopping execution, {} turns >= {}",
                stats.turns, max_turns
            );
            return Ok(StepVerdict::Stop(Stop::new(
                StopType::TurnLimit,
                format!("Turn limit ({max_turns}) reached"),
            )));
        }

        // Shared cycle-accounting check: this cycle's committed budget
        // view (cache and auxiliary attempts included), never a
        // revaluation of token counters. Prior cycles do not count.
        if let Some(max_price) = self.config.max_price_usd
            && stats.budget_cost_usd >= max_price
        {
            let total_cost = stats.budget_cost_usd;
            debug!(
                "LimitsMiddleware: stopping execution, cost ${:.4} >= max ${}",
                total_cost, max_price
            );
            return Ok(StepVerdict::Stop(Stop::new(
                StopType::PriceLimit,
                format!("Price limit reached: ${total_cost:.4} >= ${max_price:.2}"),
            )));
        }

        Ok(StepVerdict::Proceed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::execution::{Stage, Step};
    use crate::middleware::Stop;
    use crate::test_utils::HookRig;

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
        let driver: Arc<dyn Middleware> = Arc::new(LimitsMiddleware::try_new(config).unwrap());
        let pricing = crate::middleware::CompositeDriver::new(vec![driver])
            .budget_pricing()
            .expect("budget pricing");
        assert_eq!(pricing.cache_write_1h, Some(6.0));

        let none: Arc<dyn Middleware> = Arc::new(LimitsMiddleware::new(LimitsConfig::default()));
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
            .unwrap()
            .expect("enabled middleware");
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
        let driver = LimitsFactory
            .create(&json, &agent_config)
            .unwrap()
            .expect("enabled middleware");
        let p = driver.budget_pricing().expect("budget pricing");
        assert_eq!(
            (p.cache_read, p.cache_write, p.cache_write_1h),
            (Some(0.3), Some(3.75), Some(6.0))
        );
        assert_eq!(driver.limits().and_then(|l| l.max_cost_usd), Some(1.0));
    }

    #[tokio::test]
    async fn factory_without_manual_rates_has_no_budget_pricing() {
        let (agent_config, _td) = factory_agent_config().await;
        let driver = LimitsFactory
            .create(&serde_json::json!({"max_steps": 5}), &agent_config)
            .unwrap()
            .expect("enabled middleware");
        assert!(driver.budget_pricing().is_none());
    }

    #[tokio::test]
    async fn factory_disabled_returns_none() {
        let (agent_config, _td) = factory_agent_config().await;
        let result = LimitsFactory
            .create(&serde_json::json!({"enabled": false}), &agent_config)
            .unwrap();
        assert!(result.is_none());
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
        let mut stack =
            HookRig::stack_of(LimitsMiddleware::new(LimitsConfig::default().max_steps(5)));
        let result = HookRig::at_step("test", 5)
            .step_start(&mut stack)
            .await
            .unwrap();

        assert!(matches!(
            result,
            Step::Continue(Stage::Stopping(Stop {
                stop_type: StopType::StepLimit,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn test_max_steps_continue() {
        let mut stack =
            HookRig::stack_of(LimitsMiddleware::new(LimitsConfig::default().max_steps(5)));
        let result = HookRig::at_step("test", 3)
            .step_start(&mut stack)
            .await
            .unwrap();

        assert!(matches!(result, Step::Continue(Stage::BeforeRequest)));
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
    ) -> anyhow::Result<Option<Arc<dyn Middleware>>> {
        let cfg: LimitsFactoryConfig = serde_json::from_value(config.clone())?;

        if !cfg.enabled {
            return Ok(None);
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
        Ok(Some(Arc::new(LimitsMiddleware::try_new(limits_config)?)))
    }
}

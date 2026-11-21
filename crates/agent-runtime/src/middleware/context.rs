use async_trait::async_trait;
use log::{debug, warn};
use std::sync::Arc;

use super::{HookCx, Middleware, Result, StepVerdict, Stop, TurnInfo, TurnMiddleware};
use crate::middleware::factory::MiddlewareFactory;
use querymt_agent_core::events::StopType;
use querymt_agent_core::model::NoticeSource;
use querymt_agent_services::model_info::{ModelInfoSource, get_model_info};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct ContextConfig {
    pub warn_at_percent: u32,
    /// Percentage of the context limit at which AI compaction is triggered preemptively.
    /// Must be greater than `warn_at_percent`. Defaults to 85.
    pub compact_at_percent: u32,
    pub auto_compact: bool,
    /// Source for context limit - default is FromSession (dynamic)
    pub context_source: ModelInfoSource,
    /// Fallback if model info not available
    pub fallback_max_tokens: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            warn_at_percent: 80,
            compact_at_percent: 85,
            auto_compact: true,
            context_source: ModelInfoSource::FromSession,
            fallback_max_tokens: 32_000,
        }
    }
}

impl ContextConfig {
    /// Use dynamic context limit from model constraints (default)
    pub fn with_dynamic_limits() -> Self {
        Self::default()
    }

    /// Use manual context limit
    pub fn with_manual_limit(max_tokens: usize) -> Self {
        Self {
            context_source: ModelInfoSource::manual().context_limit(max_tokens),
            ..Default::default()
        }
    }

    pub fn warn_at_percent(mut self, percent: u32) -> Self {
        self.warn_at_percent = percent;
        self
    }

    pub fn compact_at_percent(mut self, percent: u32) -> Self {
        assert!(
            self.warn_at_percent < percent,
            "compact_at_percent ({}) must be greater than warn_at_percent ({})",
            percent,
            self.warn_at_percent
        );
        self.compact_at_percent = percent;
        self
    }

    pub fn auto_compact(mut self, enabled: bool) -> Self {
        self.auto_compact = enabled;
        self
    }

    pub fn fallback_max_tokens(mut self, tokens: usize) -> Self {
        self.fallback_max_tokens = tokens;
        self
    }

    pub fn context_source(mut self, source: ModelInfoSource) -> Self {
        self.context_source = source;
        self
    }
}

/// Middleware that handles context warnings and optional auto-compaction
pub struct ContextMiddleware {
    config: ContextConfig,
}

impl ContextMiddleware {
    pub fn new(config: ContextConfig) -> Self {
        debug!(
            "Creating ContextMiddleware with warn_at_percent={}, auto_compact={}, context_source={:?}",
            config.warn_at_percent, config.auto_compact, config.context_source
        );
        Self { config }
    }
}

impl Middleware for ContextMiddleware {
    fn name(&self) -> &'static str {
        "ContextMiddleware"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(ContextTurn {
            config: self.config.clone(),
            warned: false,
        })
    }
}

/// The context checks of one run.
struct ContextTurn {
    config: ContextConfig,
    /// The usage warning was stored in this run.
    warned: bool,
}

impl ContextTurn {
    /// Get max tokens for this context, fetching dynamically if needed
    fn max_tokens(&self, provider: &str, model: &str) -> usize {
        match &self.config.context_source {
            ModelInfoSource::FromSession => {
                // Fetch from model constraints using new ModelInfo methods
                get_model_info(provider, model)
                    .and_then(|m| m.context_limit())
                    .map(|c| c as usize)
                    .unwrap_or_else(|| {
                        warn!(
                            "No context limit found for {}/{}, using fallback: {}",
                            provider, model, self.config.fallback_max_tokens
                        );
                        self.config.fallback_max_tokens
                    })
            }
            ModelInfoSource::Manual { context_limit, .. } => {
                context_limit.unwrap_or(self.config.fallback_max_tokens)
            }
        }
    }

    /// The usage warning, once per run, when usage reaches the threshold.
    fn warning(&mut self, current_tokens: usize, max_tokens: usize) -> Option<String> {
        if max_tokens == 0 || self.config.warn_at_percent == 0 || self.warned {
            return None;
        }

        let threshold =
            (max_tokens as f64 * (self.config.warn_at_percent.min(100) as f64 / 100.0)) as usize;
        if current_tokens < threshold {
            return None;
        }

        self.warned = true;
        let percent = (current_tokens as f64 / max_tokens as f64) * 100.0;
        Some(format!(
            "Warning: context usage is at {:.0}% ({} / {} tokens)",
            percent, current_tokens, max_tokens
        ))
    }
}

#[async_trait]
impl TurnMiddleware for ContextTurn {
    async fn on_step_start(&mut self, cx: &mut HookCx<'_>) -> Result<StepVerdict> {
        let max_tokens = self.max_tokens(cx.provider(), cx.model());
        if max_tokens == 0 {
            return Ok(StepVerdict::Proceed);
        }

        let current_tokens = cx.stats().context_tokens;
        let compact_threshold =
            (max_tokens as f64 * (self.config.compact_at_percent.min(100) as f64 / 100.0)) as usize;

        if self.config.auto_compact && current_tokens >= compact_threshold {
            debug!(
                "ContextMiddleware: requesting compaction, {} >= {} tokens ({}% of {})",
                current_tokens, compact_threshold, self.config.compact_at_percent, max_tokens
            );
            return Ok(StepVerdict::Stop(Stop::new(
                StopType::ContextThreshold,
                format!(
                    "Context token threshold ({} / {} tokens, {}%) reached, requesting compaction",
                    current_tokens, max_tokens, self.config.compact_at_percent
                ),
            )));
        }

        if let Some(warning) = self.warning(current_tokens, max_tokens) {
            debug!(
                "ContextMiddleware: usage warning queued for session {}",
                cx.session_id()
            );
            cx.queue_notice(NoticeSource::ContextUsage, warning);
        }

        Ok(StepVerdict::Proceed)
    }
}

/// Factory for creating ContextMiddleware from config
pub struct ContextFactory;

/// Configuration structure for ContextMiddleware
#[derive(Debug, Deserialize)]
#[serde(default)]
struct ContextFactoryConfig {
    enabled: bool,
    warn_at_percent: u32,
    compact_at_percent: u32,
    fallback_max_tokens: usize,
}

impl Default for ContextFactoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            warn_at_percent: 80,
            compact_at_percent: 85,
            fallback_max_tokens: 32_000,
        }
    }
}

impl MiddlewareFactory for ContextFactory {
    fn type_name(&self) -> &'static str {
        "context"
    }

    fn create(
        &self,
        config: &serde_json::Value,
        agent_config: &crate::agent::agent_config::AgentConfig,
    ) -> anyhow::Result<Option<Arc<dyn Middleware>>> {
        let cfg: ContextFactoryConfig = serde_json::from_value(config.clone())?;

        if !cfg.enabled {
            return Ok(None);
        }

        if cfg.warn_at_percent >= cfg.compact_at_percent {
            return Err(anyhow::anyhow!(
                "context middleware: warn_at_percent ({}) must be less than compact_at_percent ({})",
                cfg.warn_at_percent,
                cfg.compact_at_percent
            ));
        }

        // Read auto_compact from agent config's execution policy
        let auto_compact = agent_config.execution_policy.compaction.auto;

        let context_config = ContextConfig {
            warn_at_percent: cfg.warn_at_percent,
            compact_at_percent: cfg.compact_at_percent,
            auto_compact,
            context_source: ModelInfoSource::FromSession,
            fallback_max_tokens: cfg.fallback_max_tokens,
        };

        Ok(Some(Arc::new(ContextMiddleware::new(context_config))))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{ConversationContext, ExecutionState, TurnStack};
    use crate::test_utils::{HookRig, test_context};

    // ========================================================================
    // Fixtures for ContextMiddleware Tests
    // ========================================================================

    struct ContextMiddlewareFixture {
        middleware: ContextMiddleware,
        config: ContextConfig,
        rig: HookRig,
        stack: tokio::sync::Mutex<TurnStack>,
    }

    impl ContextMiddlewareFixture {
        fn from_config(config: ContextConfig) -> Self {
            let middleware = ContextMiddleware::new(config.clone());
            let stack = HookRig::stack_of(ContextMiddleware::new(config.clone()));
            Self {
                middleware,
                config,
                rig: HookRig::new(),
                stack: tokio::sync::Mutex::new(stack),
            }
        }

        fn with_defaults() -> Self {
            Self::from_config(ContextConfig::default())
        }

        fn with_warn_percent(warn_at_percent: u32) -> Self {
            Self::from_config(ContextConfig::default().warn_at_percent(warn_at_percent))
        }

        fn with_manual_limit_and_auto_compact(max_tokens: usize, auto_compact: bool) -> Self {
            Self::from_config(
                ContextConfig::with_manual_limit(max_tokens).auto_compact(auto_compact),
            )
        }

        fn with_manual_limit(max_tokens: usize) -> Self {
            Self::from_config(ContextConfig::with_manual_limit(max_tokens))
        }

        fn with_manual_limit_compact_and_warn_percent(
            max_tokens: usize,
            compact_percent: u32,
            warn_percent: u32,
        ) -> Self {
            Self::from_config(
                ContextConfig::with_manual_limit(max_tokens)
                    .compact_at_percent(compact_percent)
                    .warn_at_percent(warn_percent),
            )
        }

        fn with_manual_limit_and_warn_percent(max_tokens: usize, warn_percent: u32) -> Self {
            Self::from_config(
                ContextConfig::with_manual_limit(max_tokens).warn_at_percent(warn_percent),
            )
        }

        /// The state of one run's checks, for the unit tests of its helpers.
        fn turn(&self) -> ContextTurn {
            ContextTurn {
                config: self.config.clone(),
                warned: false,
            }
        }

        /// One step of the fixture's run.
        async fn run_step(&self, state: ExecutionState) -> Result<ExecutionState> {
            let mut stack = self.stack.lock().await;
            self.rig.step_start(&mut stack, state).await
        }
    }

    // ========================================================================
    // ContextConfig Tests
    // ========================================================================

    #[test]
    fn test_context_config_default_values() {
        let config = ContextConfig::default();
        assert_eq!(config.warn_at_percent, 80);
        assert_eq!(config.compact_at_percent, 85);
        assert!(config.auto_compact);
        assert_eq!(config.fallback_max_tokens, 32_000);
        // Verify it's FromSession
        assert!(matches!(
            config.context_source,
            ModelInfoSource::FromSession
        ));
    }

    #[test]
    fn test_context_config_with_dynamic_limits() {
        let config = ContextConfig::with_dynamic_limits();
        matches!(config.context_source, ModelInfoSource::FromSession);
        // Just verify it's FromSession (we can't use assert_eq without PartialEq)
    }

    #[test]
    fn test_context_config_with_manual_limit() {
        let config = ContextConfig::with_manual_limit(50_000);
        if let ModelInfoSource::Manual { context_limit, .. } = config.context_source {
            assert_eq!(context_limit, Some(50_000));
        } else {
            panic!("Expected Manual context source");
        }
    }

    #[test]
    fn test_context_config_builder_warn_at_percent() {
        let config = ContextConfig::default().warn_at_percent(50);
        assert_eq!(config.warn_at_percent, 50);
    }

    #[test]
    fn test_context_config_builder_auto_compact() {
        let config = ContextConfig::default().auto_compact(false);
        assert!(!config.auto_compact);
    }

    #[test]
    fn test_context_config_builder_fallback_max_tokens() {
        let config = ContextConfig::default().fallback_max_tokens(100_000);
        assert_eq!(config.fallback_max_tokens, 100_000);
    }

    #[test]
    fn test_context_config_builder_chaining() {
        let config = ContextConfig::default()
            .warn_at_percent(60)
            .compact_at_percent(90)
            .auto_compact(false)
            .fallback_max_tokens(64_000);

        assert_eq!(config.warn_at_percent, 60);
        assert_eq!(config.compact_at_percent, 90);
        assert!(!config.auto_compact);
        assert_eq!(config.fallback_max_tokens, 64_000);
    }

    // ========================================================================
    // ContextMiddleware Tests
    // ========================================================================

    #[test]
    fn test_should_warn_below_threshold() {
        let fixture = ContextMiddlewareFixture::with_warn_percent(80);
        let mut turn = fixture.turn();

        // 50% usage, threshold is 80% -> should not warn
        let result = turn.warning(5000, 10000);
        assert!(result.is_none(), "Should not warn below threshold");
    }

    #[test]
    fn test_should_warn_at_threshold() {
        let fixture = ContextMiddlewareFixture::with_warn_percent(80);
        let mut turn = fixture.turn();

        // 80% usage -> should warn
        let result = turn.warning(8000, 10000);
        assert!(result.is_some(), "Should warn at or above threshold");
        assert!(result.unwrap().contains("80"));
    }

    #[test]
    fn test_should_warn_only_once_per_session() {
        let fixture = ContextMiddlewareFixture::with_warn_percent(80);
        let mut turn = fixture.turn();

        // First call at 80% -> should warn
        let first_warning = turn.warning(8000, 10000);
        assert!(first_warning.is_some());

        // Second call at 90% -> should NOT warn again (already warned for session)
        let second_warning = turn.warning(9000, 10000);
        assert!(
            second_warning.is_none(),
            "Should only warn once per session"
        );
    }

    #[test]
    fn test_should_warn_zero_max_tokens_skips() {
        let fixture = ContextMiddlewareFixture::with_defaults();
        let mut turn = fixture.turn();

        let result = turn.warning(5000, 0);
        assert!(result.is_none(), "Should skip warning if max_tokens is 0");
    }

    #[test]
    fn test_should_warn_zero_percent_skips() {
        let fixture = ContextMiddlewareFixture::with_warn_percent(0);
        let mut turn = fixture.turn();

        let result = turn.warning(5000, 10000);
        assert!(
            result.is_none(),
            "Should skip warning if warn_at_percent is 0"
        );
    }

    #[tokio::test]
    async fn test_on_step_start_below_threshold_passes_through() {
        let fixture = ContextMiddlewareFixture::with_manual_limit(10000);
        let context = test_context("test-session", 0);
        // Modify to set lower token count
        let mut context_mut = (*context).clone();
        context_mut.stats = Arc::new(crate::middleware::AgentStats {
            context_tokens: 5000, // Below 80% threshold
            ..Default::default()
        });
        let context = Arc::new(context_mut);

        let state = ExecutionState::BeforeLlmCall {
            context: context.clone(),
        };

        let result = fixture.run_step(state).await.unwrap();
        assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
    }

    #[tokio::test]
    async fn test_on_step_start_at_compact_threshold_triggers_compaction() {
        // compact_at_percent=85, limit=10000 -> threshold=8500
        let fixture =
            ContextMiddlewareFixture::with_manual_limit_compact_and_warn_percent(10000, 85, 70);
        let context = test_context("test-session", 0);
        let mut context_mut = (*context).clone();
        // Exactly at 85% threshold
        context_mut.stats = Arc::new(crate::middleware::AgentStats {
            context_tokens: 8500,
            ..Default::default()
        });
        let context = Arc::new(context_mut);

        let state = ExecutionState::BeforeLlmCall {
            context: context.clone(),
        };

        let result = fixture.run_step(state).await.unwrap();

        assert!(
            matches!(result, ExecutionState::Stopped { stop_type, .. }
                                if stop_type == StopType::ContextThreshold),
            "Should stop with ContextThreshold at compact_at_percent threshold"
        );
    }

    #[tokio::test]
    async fn test_on_step_start_below_compact_threshold_does_not_compact() {
        // compact_at_percent=85, limit=10000 -> threshold=8500; tokens=8499 -> no compaction
        let fixture =
            ContextMiddlewareFixture::with_manual_limit_compact_and_warn_percent(10000, 85, 70);
        let context = test_context("test-session", 0);
        let mut context_mut = (*context).clone();
        context_mut.stats = Arc::new(crate::middleware::AgentStats {
            context_tokens: 8499,
            ..Default::default()
        });
        let context = Arc::new(context_mut);

        let state = ExecutionState::BeforeLlmCall {
            context: context.clone(),
        };

        let result = fixture.run_step(state).await.unwrap();

        assert!(
            !matches!(result, ExecutionState::Stopped { stop_type, .. }
                                if stop_type == StopType::ContextThreshold),
            "Should not compact below compact_at_percent threshold"
        );
    }

    #[tokio::test]
    async fn test_on_step_start_warn_fires_before_compact() {
        // warn=70%, compact=85%, limit=10000
        // At 75% (7500 tokens): warn fires, compact does not
        let fixture =
            ContextMiddlewareFixture::with_manual_limit_compact_and_warn_percent(10000, 85, 70);
        let context = test_context("test-session", 0);
        let mut context_mut = (*context).clone();
        context_mut.stats = Arc::new(crate::middleware::AgentStats {
            context_tokens: 7500,
            ..Default::default()
        });
        let context = Arc::new(context_mut);

        let state = ExecutionState::BeforeLlmCall {
            context: context.clone(),
        };

        let result = fixture.run_step(state).await.unwrap();

        // Should inject warning, not stop for compaction
        match result {
            ExecutionState::BeforeLlmCall {
                context: new_context,
            } => {
                assert_eq!(
                    new_context.pending_notices.len(),
                    1,
                    "Should queue warning notice before compact threshold"
                );
            }
            ExecutionState::Stopped {
                stop_type: StopType::ContextThreshold,
                ..
            } => {
                panic!("Should not compact below compact_at_percent threshold");
            }
            _ => panic!("Unexpected state"),
        }
    }

    #[tokio::test]
    async fn test_on_step_start_at_threshold_with_auto_compact() {
        // Use manual limit so we have predictable threshold behavior
        // Default compact_at_percent=85, limit=10000 -> threshold=8500
        // Set tokens to 10000 (above threshold) -> should compact
        let fixture = ContextMiddlewareFixture::with_manual_limit_and_auto_compact(10000, true);
        let context = test_context("test-session", 0);
        let mut context_mut = (*context).clone();
        context_mut.stats = Arc::new(crate::middleware::AgentStats {
            context_tokens: 10000,
            ..Default::default()
        });
        let context = Arc::new(context_mut);

        let state = ExecutionState::BeforeLlmCall {
            context: context.clone(),
        };

        let result = fixture.run_step(state).await.unwrap();

        assert!(
            matches!(result, ExecutionState::Stopped { stop_type, .. }
                                if stop_type == StopType::ContextThreshold),
            "Should stop with ContextThreshold when above compact_at_percent with auto_compact=true"
        );
    }

    #[tokio::test]
    async fn test_on_step_start_at_threshold_without_auto_compact() {
        // Use manual limit for predictable threshold, but auto_compact=false
        let fixture = ContextMiddlewareFixture::with_manual_limit_and_auto_compact(10000, false);
        let context = test_context("test-session", 0);
        let mut context_mut = (*context).clone();
        // Set to 9000 tokens (90% of 10000) -> triggers warning at 80% threshold
        context_mut.stats = Arc::new(crate::middleware::AgentStats {
            context_tokens: 9000,
            ..Default::default()
        });
        let context = Arc::new(context_mut);

        let state = ExecutionState::BeforeLlmCall {
            context: context.clone(),
        };

        let result = fixture.run_step(state).await.unwrap();

        // Should not stop, but should inject warning instead
        match result {
            ExecutionState::BeforeLlmCall {
                context: result_ctx,
            } => {
                // Warning should be queued as a notice
                assert_eq!(
                    result_ctx.pending_notices.len(),
                    1,
                    "Should queue warning notice"
                );
            }
            ExecutionState::Stopped { .. } => {
                panic!("Should not stop when auto_compact=false");
            }
            _ => panic!("Unexpected state"),
        }
    }

    #[tokio::test]
    async fn test_on_step_start_injects_warning_message() {
        // Use manual limit with 50% warning threshold
        let fixture = ContextMiddlewareFixture::with_manual_limit_and_warn_percent(10000, 50);
        let context = test_context("test-session", 0);
        let mut context_mut = (*context).clone();
        context_mut.stats = Arc::new(crate::middleware::AgentStats {
            context_tokens: 6000, // 60% of 10000, exceeds 50% threshold
            ..Default::default()
        });
        let context = Arc::new(context_mut);

        let state = ExecutionState::BeforeLlmCall {
            context: context.clone(),
        };

        let result = fixture.run_step(state).await.unwrap();

        match result {
            ExecutionState::BeforeLlmCall {
                context: new_context,
            } => {
                // The warning is queued as a `context_usage` notice.
                assert_eq!(new_context.pending_notices.len(), 1);
                assert_eq!(
                    new_context.pending_notices[0].source,
                    querymt_agent_core::model::NoticeSource::ContextUsage
                );
            }
            _ => panic!("Expected BeforeLlmCall with injected message"),
        }
    }

    #[test]
    fn test_middleware_name() {
        let fixture = ContextMiddlewareFixture::with_defaults();
        assert_eq!(fixture.middleware.name(), "ContextMiddleware");
    }

    #[test]
    #[should_panic(expected = "compact_at_percent")]
    fn test_compact_at_percent_equal_to_warn_panics() {
        // warn=80, compact=80 -> must panic
        ContextConfig::default()
            .warn_at_percent(80)
            .compact_at_percent(80);
    }

    #[test]
    #[should_panic(expected = "compact_at_percent")]
    fn test_compact_at_percent_less_than_warn_panics() {
        // warn=80, compact=70 -> must panic
        ContextConfig::default()
            .warn_at_percent(80)
            .compact_at_percent(70);
    }

    #[test]
    fn test_compact_at_percent_greater_than_warn_ok() {
        // warn=80, compact=85 -> valid
        let config = ContextConfig::default()
            .warn_at_percent(80)
            .compact_at_percent(85);
        assert_eq!(config.warn_at_percent, 80);
        assert_eq!(config.compact_at_percent, 85);
    }

    #[test]
    fn test_get_max_tokens_with_manual_source() {
        let fixture = ContextMiddlewareFixture::with_manual_limit(50_000);
        let context = test_context("test", 0);

        let max_tokens = fixture.turn().max_tokens(&context.provider, &context.model);
        assert_eq!(max_tokens, 50_000);
    }

    #[test]
    fn test_get_max_tokens_uses_fallback_when_model_info_missing() {
        let fixture = ContextMiddlewareFixture::with_defaults();
        let context = test_context("test", 0);

        let max_tokens = fixture.turn().max_tokens(&context.provider, &context.model);
        // Should use fallback since we don't have real model info
        assert_eq!(max_tokens, ContextConfig::default().fallback_max_tokens);
    }

    #[tokio::test]
    async fn a_new_run_warns_again() {
        let fixture = ContextMiddlewareFixture::with_manual_limit(10_000);
        let hot = || {
            let base = test_context("mine", 0);
            let mut stats = (*base.stats).clone();
            stats.context_tokens = 8_200;
            Arc::new(ConversationContext {
                stats: Arc::new(stats),
                ..(*base).clone()
            })
        };
        let queued =
            |state: &ExecutionState| state.context().expect("context").pending_notices.len();

        let first_run = fixture
            .run_step(ExecutionState::BeforeLlmCall { context: hot() })
            .await
            .unwrap();
        let same_run = fixture
            .run_step(ExecutionState::BeforeLlmCall { context: hot() })
            .await
            .unwrap();
        let next_run = ContextMiddlewareFixture::with_manual_limit(10_000)
            .run_step(ExecutionState::BeforeLlmCall { context: hot() })
            .await
            .unwrap();

        assert_eq!(
            (queued(&first_run), queued(&same_run), queued(&next_run)),
            (1, 0, 1)
        );
    }

    #[tokio::test]
    async fn model_change_within_a_run_does_not_warn_again() {
        let fixture = ContextMiddlewareFixture::with_manual_limit(10_000);
        let step_on = |model: &str| {
            let base = test_context("mine", 0);
            let mut stats = (*base.stats).clone();
            stats.context_tokens = 8_200;
            Arc::new(ConversationContext {
                model: model.into(),
                stats: Arc::new(stats),
                ..(*base).clone()
            })
        };

        let first = fixture
            .run_step(ExecutionState::BeforeLlmCall {
                context: step_on("model-1"),
            })
            .await
            .unwrap();
        let second = fixture
            .run_step(ExecutionState::BeforeLlmCall {
                context: step_on("model-2"),
            })
            .await
            .unwrap();

        let queued =
            |state: &ExecutionState| state.context().expect("context").pending_notices.len();
        assert_eq!(queued(&first), 1);
        assert_eq!(queued(&second), 0, "one warning per run");
    }
}

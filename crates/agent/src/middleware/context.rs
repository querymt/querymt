use async_trait::async_trait;
use log::{debug, trace, warn};
use parking_lot::Mutex;
use std::sync::Arc;

use super::{ExecutionState, MiddlewareDriver, Result};
use crate::events::StopType;
use crate::middleware::ConversationContext;
use crate::middleware::factory::MiddlewareFactory;
use crate::model_info::{ModelInfoSource, get_model_info};
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
    warned_sessions: Mutex<std::collections::HashSet<Arc<str>>>,
    last_model: Mutex<Option<(String, String)>>, // (provider, model)
}

impl ContextMiddleware {
    pub fn new(config: ContextConfig) -> Self {
        debug!(
            "Creating ContextMiddleware with warn_at_percent={}, auto_compact={}, context_source={:?}",
            config.warn_at_percent, config.auto_compact, config.context_source
        );
        Self {
            config,
            warned_sessions: Mutex::new(std::collections::HashSet::new()),
            last_model: Mutex::new(None),
        }
    }

    /// Get max tokens for this context, fetching dynamically if needed
    fn get_max_tokens(&self, context: &ConversationContext) -> usize {
        match &self.config.context_source {
            ModelInfoSource::FromSession => {
                // Fetch from model constraints using new ModelInfo methods
                get_model_info(&context.provider, &context.model)
                    .and_then(|m| m.context_limit())
                    .map(|c| c as usize)
                    .unwrap_or_else(|| {
                        warn!(
                            "No context limit found for {}/{}, using fallback: {}",
                            context.provider, context.model, self.config.fallback_max_tokens
                        );
                        self.config.fallback_max_tokens
                    })
            }
            ModelInfoSource::Manual { context_limit, .. } => {
                context_limit.unwrap_or(self.config.fallback_max_tokens)
            }
        }
    }

    /// Check if provider changed and reset state if needed
    fn check_provider_changed(&self, context: &ConversationContext) {
        let mut last = self.last_model.lock();
        let current = (context.provider.to_string(), context.model.to_string());

        if last.as_ref() != Some(&current) {
            // Provider changed - reset warned sessions
            let mut warned = self.warned_sessions.lock();
            warned.clear();
            *last = Some(current.clone());
            debug!(
                "Provider changed to {}/{}, reset context warnings",
                current.0, current.1
            );
        }
    }

    fn should_warn(
        &self,
        session_id: &Arc<str>,
        current_tokens: usize,
        max_tokens: usize,
    ) -> Option<String> {
        if max_tokens == 0 || self.config.warn_at_percent == 0 {
            return None;
        }

        let mut warned = self.warned_sessions.lock();
        if warned.contains(session_id) {
            return None;
        }

        let threshold =
            (max_tokens as f64 * (self.config.warn_at_percent.min(100) as f64 / 100.0)) as usize;
        if current_tokens < threshold {
            return None;
        }

        warned.insert(session_id.clone());
        let percent = (current_tokens as f64 / max_tokens as f64) * 100.0;
        Some(format!(
            "Warning: context usage is at {:.0}% ({} / {} tokens)",
            percent, current_tokens, max_tokens
        ))
    }
}

#[async_trait]
impl MiddlewareDriver for ContextMiddleware {
    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
        trace!(
            "ContextMiddleware::on_step_start entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeLlmCall { ref context } => {
                // Check if provider changed
                self.check_provider_changed(context);

                // Get max tokens dynamically
                let max_tokens = self.get_max_tokens(context);

                if max_tokens == 0 {
                    return Ok(state);
                }

                let current_tokens = context.stats.context_tokens;

                let compact_threshold = (max_tokens as f64
                    * (self.config.compact_at_percent.min(100) as f64 / 100.0))
                    as usize;

                if self.config.auto_compact && current_tokens >= compact_threshold {
                    debug!(
                        "ContextMiddleware: requesting compaction, {} >= {} tokens ({}% of {})",
                        current_tokens,
                        compact_threshold,
                        self.config.compact_at_percent,
                        max_tokens
                    );
                    return Ok(ExecutionState::Stopped {
                        message: format!(
                            "Context token threshold ({} / {} tokens, {}%) reached, requesting compaction",
                            current_tokens, max_tokens, self.config.compact_at_percent
                        )
                        .into(),
                        stop_type: StopType::ContextThreshold,
                        context: Some(context.clone()),
                    });
                }

                if let Some(warning) =
                    self.should_warn(&context.session_id, current_tokens, max_tokens)
                {
                    debug!(
                        "ContextMiddleware: usage warning injected for session {}",
                        context.session_id
                    );
                    let new_context = context.inject_message(warning);
                    return Ok(ExecutionState::BeforeLlmCall {
                        context: Arc::new(new_context),
                    });
                }

                Ok(state)
            }
            _ => Ok(state),
        }
    }

    fn reset(&self) {
        debug!("ContextMiddleware::reset - clearing warned sessions and model cache");
        let mut warned = self.warned_sessions.lock();
        warned.clear();
        let mut last = self.last_model.lock();
        *last = None;
    }

    fn name(&self) -> &'static str {
        "ContextMiddleware"
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
    ) -> anyhow::Result<Arc<dyn MiddlewareDriver>> {
        let cfg: ContextFactoryConfig = serde_json::from_value(config.clone())?;

        if !cfg.enabled {
            return Err(anyhow::anyhow!("Middleware disabled"));
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

        Ok(Arc::new(ContextMiddleware::new(context_config)))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::test_context;

    // ========================================================================
    // Fixtures for ContextMiddleware Tests
    // ========================================================================

    struct ContextMiddlewareFixture {
        middleware: ContextMiddleware,
    }

    impl ContextMiddlewareFixture {
        fn with_defaults() -> Self {
            Self {
                middleware: ContextMiddleware::new(ContextConfig::default()),
            }
        }

        fn with_warn_percent(warn_at_percent: u32) -> Self {
            Self {
                middleware: ContextMiddleware::new(
                    ContextConfig::default().warn_at_percent(warn_at_percent),
                ),
            }
        }

        fn with_manual_limit_and_auto_compact(max_tokens: usize, auto_compact: bool) -> Self {
            Self {
                middleware: ContextMiddleware::new(
                    ContextConfig::with_manual_limit(max_tokens).auto_compact(auto_compact),
                ),
            }
        }

        fn with_manual_limit(max_tokens: usize) -> Self {
            Self {
                middleware: ContextMiddleware::new(ContextConfig::with_manual_limit(max_tokens)),
            }
        }

        fn with_manual_limit_compact_and_warn_percent(
            max_tokens: usize,
            compact_percent: u32,
            warn_percent: u32,
        ) -> Self {
            Self {
                middleware: ContextMiddleware::new(
                    ContextConfig::with_manual_limit(max_tokens)
                        .compact_at_percent(compact_percent)
                        .warn_at_percent(warn_percent),
                ),
            }
        }

        fn with_manual_limit_and_warn_percent(max_tokens: usize, warn_percent: u32) -> Self {
            Self {
                middleware: ContextMiddleware::new(
                    ContextConfig::with_manual_limit(max_tokens).warn_at_percent(warn_percent),
                ),
            }
        }

        async fn run_step(&self, state: ExecutionState) -> Result<ExecutionState> {
            self.middleware.on_step_start(state, None).await
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
        let session_id: Arc<str> = "test-session".into();

        // 50% usage, threshold is 80% -> should not warn
        let result = fixture.middleware.should_warn(&session_id, 5000, 10000);
        assert!(result.is_none(), "Should not warn below threshold");
    }

    #[test]
    fn test_should_warn_at_threshold() {
        let fixture = ContextMiddlewareFixture::with_warn_percent(80);
        let session_id: Arc<str> = "test-session".into();

        // 80% usage -> should warn
        let result = fixture.middleware.should_warn(&session_id, 8000, 10000);
        assert!(result.is_some(), "Should warn at or above threshold");
        assert!(result.unwrap().contains("80"));
    }

    #[test]
    fn test_should_warn_only_once_per_session() {
        let fixture = ContextMiddlewareFixture::with_warn_percent(80);
        let session_id: Arc<str> = "test-session".into();

        // First call at 80% -> should warn
        let first_warning = fixture.middleware.should_warn(&session_id, 8000, 10000);
        assert!(first_warning.is_some());

        // Second call at 90% -> should NOT warn again (already warned for session)
        let second_warning = fixture.middleware.should_warn(&session_id, 9000, 10000);
        assert!(
            second_warning.is_none(),
            "Should only warn once per session"
        );
    }

    #[test]
    fn test_should_warn_reset_on_provider_change() {
        let fixture = ContextMiddlewareFixture::with_defaults();
        let session_id: Arc<str> = "test-session".into();

        // Warn for provider1/model1
        let ctx1 = test_context("test", 0);
        fixture
            .middleware
            .check_provider_changed(&Arc::new(ConversationContext {
                provider: "provider1".into(),
                model: "model1".into(),
                ..(*ctx1).clone()
            }));

        let first_warning = fixture.middleware.should_warn(&session_id, 8000, 10000);
        assert!(first_warning.is_some());

        // Check provider change to provider2/model2
        let ctx2 = test_context("test", 0);
        fixture
            .middleware
            .check_provider_changed(&Arc::new(ConversationContext {
                provider: "provider2".into(),
                model: "model2".into(),
                ..(*ctx2).clone()
            }));

        // Second call should warn again (session warned set was cleared)
        let second_warning = fixture.middleware.should_warn(&session_id, 8000, 10000);
        assert!(
            second_warning.is_some(),
            "Should reset warnings on provider change"
        );
    }

    #[test]
    fn test_should_warn_zero_max_tokens_skips() {
        let fixture = ContextMiddlewareFixture::with_defaults();
        let session_id: Arc<str> = "test-session".into();

        let result = fixture.middleware.should_warn(&session_id, 5000, 0);
        assert!(result.is_none(), "Should skip warning if max_tokens is 0");
    }

    #[test]
    fn test_should_warn_zero_percent_skips() {
        let fixture = ContextMiddlewareFixture::with_warn_percent(0);
        let session_id: Arc<str> = "test-session".into();

        let result = fixture.middleware.should_warn(&session_id, 5000, 10000);
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
                assert!(
                    new_context.messages.len() > context.messages.len(),
                    "Should inject warning message before compact threshold"
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
                // Warning should be injected in the messages
                assert_ne!(
                    context.messages.len(),
                    result_ctx.messages.len(),
                    "Should inject warning message"
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
                // Message should be injected - new context has more messages
                assert!(
                    new_context.messages.len() > context.messages.len(),
                    "Should inject warning message"
                );
            }
            _ => panic!("Expected BeforeLlmCall with injected message"),
        }
    }

    #[tokio::test]
    async fn test_on_step_start_ignores_non_before_llm_call_states() {
        let fixture = ContextMiddlewareFixture::with_defaults();
        let context = test_context("test-session", 0);

        let state = ExecutionState::CallLlm {
            context: context.clone(),
            tools: Arc::from([]),
        };

        let result = fixture.run_step(state).await.unwrap();
        assert!(matches!(result, ExecutionState::CallLlm { .. }));
    }

    #[test]
    fn test_reset_clears_warned_sessions() {
        let fixture = ContextMiddlewareFixture::with_defaults();
        let session_id: Arc<str> = "test-session".into();

        // Add a warning
        fixture.middleware.should_warn(&session_id, 8000, 10000);

        // Verify it was recorded
        assert!(
            fixture
                .middleware
                .warned_sessions
                .lock()
                .contains(&session_id)
        );

        // Reset
        fixture.middleware.reset();

        // Verify it was cleared
        assert!(
            !fixture
                .middleware
                .warned_sessions
                .lock()
                .contains(&session_id)
        );
    }

    #[test]
    fn test_reset_clears_model_cache() {
        let fixture = ContextMiddlewareFixture::with_defaults();

        // Set model info
        let ctx = test_context("test", 0);
        fixture
            .middleware
            .check_provider_changed(&Arc::new(ConversationContext {
                provider: "test-provider".into(),
                model: "test-model".into(),
                ..(*ctx).clone()
            }));

        // Verify it was set
        assert!(fixture.middleware.last_model.lock().is_some());

        // Reset
        fixture.middleware.reset();

        // Verify it was cleared
        assert!(fixture.middleware.last_model.lock().is_none());
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

        let max_tokens = fixture.middleware.get_max_tokens(&context);
        assert_eq!(max_tokens, 50_000);
    }

    #[test]
    fn test_get_max_tokens_uses_fallback_when_model_info_missing() {
        let fixture = ContextMiddlewareFixture::with_defaults();
        let context = test_context("test", 0);

        let max_tokens = fixture.middleware.get_max_tokens(&context);
        // Should use fallback since we don't have real model info
        assert_eq!(max_tokens, ContextConfig::default().fallback_max_tokens);
    }

    #[test]
    fn test_check_provider_changed_detects_change() {
        let fixture = ContextMiddlewareFixture::with_defaults();
        let ctx = test_context("test", 0);

        let ctx1 = Arc::new(ConversationContext {
            provider: "provider1".into(),
            model: "model1".into(),
            ..(*ctx).clone()
        });

        fixture.middleware.check_provider_changed(&ctx1);

        let ctx2 = Arc::new(ConversationContext {
            provider: "provider2".into(),
            model: "model2".into(),
            ..(*ctx).clone()
        });

        // Before calling check_provider_changed with ctx2, add a warned session
        let session_id: Arc<str> = "test".into();
        fixture.middleware.should_warn(&session_id, 8000, 10000);
        assert!(!fixture.middleware.warned_sessions.lock().is_empty());

        fixture.middleware.check_provider_changed(&ctx2);

        // Warned sessions should be cleared on provider change
        assert!(fixture.middleware.warned_sessions.lock().is_empty());
    }

    #[test]
    fn test_check_provider_not_changed_preserves_state() {
        let fixture = ContextMiddlewareFixture::with_defaults();
        let ctx = test_context("test", 0);

        let ctx1 = Arc::new(ConversationContext {
            provider: "provider1".into(),
            model: "model1".into(),
            ..(*ctx).clone()
        });

        fixture.middleware.check_provider_changed(&ctx1);

        let session_id: Arc<str> = "test".into();
        fixture.middleware.should_warn(&session_id, 8000, 10000);
        let warned_count_before = fixture.middleware.warned_sessions.lock().len();

        // Check same provider again
        fixture.middleware.check_provider_changed(&ctx1);

        let warned_count_after = fixture.middleware.warned_sessions.lock().len();

        assert_eq!(
            warned_count_before, warned_count_after,
            "State should be preserved if provider didn't change"
        );
    }
}

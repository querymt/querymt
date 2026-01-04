use async_trait::async_trait;
use log::{debug, trace, warn};
use std::sync::Arc;

use super::{ExecutionState, MiddlewareDriver, Result};
use crate::middleware::ConversationContext;
use crate::model_info::{ModelInfoSource, get_model_info};

#[derive(Debug, Clone)]
pub struct ContextConfig {
    pub warn_at_percent: u32,
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
    warned_sessions: std::sync::Mutex<std::collections::HashSet<Arc<str>>>,
    last_model: std::sync::Mutex<Option<(String, String)>>, // (provider, model)
}

impl ContextMiddleware {
    pub fn new(config: ContextConfig) -> Self {
        debug!(
            "Creating ContextMiddleware with warn_at_percent={}, auto_compact={}, context_source={:?}",
            config.warn_at_percent, config.auto_compact, config.context_source
        );
        Self {
            config,
            warned_sessions: std::sync::Mutex::new(std::collections::HashSet::new()),
            last_model: std::sync::Mutex::new(None),
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
        let mut last = self.last_model.lock().unwrap();
        let current = (context.provider.to_string(), context.model.to_string());

        if last.as_ref() != Some(&current) {
            // Provider changed - reset warned sessions
            let mut warned = self.warned_sessions.lock().unwrap();
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

        let mut warned = self.warned_sessions.lock().unwrap();
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
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "ContextMiddleware::next_state entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeTurn { ref context } => {
                // Check if provider changed
                self.check_provider_changed(context);

                // Get max tokens dynamically
                let max_tokens = self.get_max_tokens(context);

                if max_tokens == 0 {
                    return Ok(state);
                }

                let current_tokens = context.stats.context_tokens;

                if self.config.auto_compact && current_tokens >= max_tokens {
                    debug!(
                        "ContextMiddleware: requesting compaction, {} >= {} tokens",
                        current_tokens, max_tokens
                    );
                    return Ok(ExecutionState::Stopped {
                        reason: agent_client_protocol::StopReason::MaxTokens,
                        message: format!(
                            "Context token threshold ({} / {} tokens) reached, requesting compaction",
                            current_tokens, max_tokens
                        )
                        .into(),
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
                    return Ok(ExecutionState::BeforeTurn {
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
        let mut warned = self.warned_sessions.lock().unwrap();
        warned.clear();
        let mut last = self.last_model.lock().unwrap();
        *last = None;
    }

    fn name(&self) -> &'static str {
        "ContextMiddleware"
    }
}

/// Middleware that automatically triggers context compaction when token threshold is reached
pub struct AutoCompactMiddleware {
    threshold_tokens: usize,
}

impl AutoCompactMiddleware {
    pub fn new(threshold_tokens: usize) -> Self {
        debug!(
            "Creating AutoCompactMiddleware with threshold = {} tokens",
            threshold_tokens
        );
        Self { threshold_tokens }
    }
}

#[async_trait]
impl MiddlewareDriver for AutoCompactMiddleware {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "AutoCompactMiddleware::next_state entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeTurn { ref context } => {
                let current_tokens = context.stats.context_tokens;
                trace!(
                    "AutoCompactMiddleware: current tokens = {}, threshold = {}",
                    current_tokens, self.threshold_tokens
                );

                if current_tokens >= self.threshold_tokens {
                    debug!(
                        "AutoCompactMiddleware: requesting compaction, {} >= {} tokens",
                        current_tokens, self.threshold_tokens
                    );
                    // In the new system, we use a Stopped state to signal compaction
                    Ok(ExecutionState::Stopped {
                        reason: agent_client_protocol::StopReason::MaxTokens,
                        message: format!(
                            "Context token threshold ({} / {} tokens) reached, requesting compaction",
                            current_tokens, self.threshold_tokens
                        ).into(),
                    })
                } else {
                    trace!(
                        "AutoCompactMiddleware: below threshold, allowing execution to continue"
                    );
                    Ok(state)
                }
            }
            _ => {
                trace!(
                    "AutoCompactMiddleware: pass-through for state {}",
                    state.name()
                );
                Ok(state)
            }
        }
    }

    fn reset(&self) {
        trace!("AutoCompactMiddleware::reset");
    }

    fn name(&self) -> &'static str {
        "AutoCompactMiddleware"
    }
}

/// Middleware that warns when context usage reaches a threshold percentage
pub struct ContextWarningMiddleware {
    threshold_percent: f64,
    max_context_tokens: usize,
    warned_sessions: std::sync::Mutex<std::collections::HashSet<Arc<str>>>,
}

impl ContextWarningMiddleware {
    pub fn new(threshold_percent: f64, max_context_tokens: usize) -> Self {
        debug!(
            "Creating ContextWarningMiddleware: threshold = {:.0}%, max_tokens = {}",
            threshold_percent * 100.0,
            max_context_tokens
        );
        Self {
            threshold_percent,
            max_context_tokens,
            warned_sessions: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }
}

#[async_trait]
impl MiddlewareDriver for ContextWarningMiddleware {
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        trace!(
            "ContextWarningMiddleware::next_state entering state: {}",
            state.name()
        );

        match state {
            ExecutionState::BeforeTurn { ref context } => {
                let mut warned = self.warned_sessions.lock().unwrap();
                if warned.contains(&context.session_id) {
                    trace!(
                        "ContextWarningMiddleware: already warned for session {}, skipping",
                        context.session_id
                    );
                    return Ok(state);
                }

                let threshold = (self.max_context_tokens as f64 * self.threshold_percent) as usize;
                let current_tokens = context.stats.context_tokens;

                if current_tokens >= threshold {
                    warned.insert(context.session_id.clone());
                    let percent = (current_tokens as f64 / self.max_context_tokens as f64) * 100.0;

                    debug!(
                        "ContextWarningMiddleware: usage at {:.0}% ({} / {} tokens), injecting warning",
                        percent, current_tokens, self.max_context_tokens
                    );

                    let warning_message = format!(
                        "Warning: context usage is at {:.0}% ({} / {} tokens)",
                        percent, current_tokens, self.max_context_tokens
                    );

                    let new_context = context.inject_message(warning_message);
                    Ok(ExecutionState::BeforeTurn {
                        context: Arc::new(new_context),
                    })
                } else {
                    trace!(
                        "ContextWarningMiddleware: usage at {:.0}% ({} / {} tokens), below threshold",
                        (current_tokens as f64 / self.max_context_tokens as f64) * 100.0,
                        current_tokens,
                        self.max_context_tokens
                    );
                    Ok(state)
                }
            }
            _ => {
                trace!(
                    "ContextWarningMiddleware: pass-through for state {}",
                    state.name()
                );
                Ok(state)
            }
        }
    }

    fn reset(&self) {
        debug!("ContextWarningMiddleware::reset - clearing warned sessions");
        let mut warned = self.warned_sessions.lock().unwrap();
        warned.clear();
    }

    fn name(&self) -> &'static str {
        "ContextWarningMiddleware"
    }
}

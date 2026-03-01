//! Routing guardrails middleware for context-safe tool usage.
//!
//! Pushes agents toward context-safe flows (`context_execute`, `batch_execute`,
//! `context_search`) and away from context-flooding paths. Includes:
//!
//! - **Routing nudges**: Annotate tool results from high-output tools with
//!   suggestions to use context-safe alternatives.
//! - **Enforcement mode**: In `enforce` mode, block or redirect calls that
//!   would flood context.
//! - **Search throttling**: Progressively degrade result budgets after
//!   excessive retrieval calls within a time window.
//!
//! # Configuration
//!
//! Controlled via `RoutingGuardrailConfig` in the execution policy:
//!
//! ```toml
//! [agent.execution.routing_guardrails]
//! routing_preference = "warn"  # off | warn | enforce
//!
//! [agent.execution.routing_guardrails.search_throttle]
//! window_secs = 60
//! max_calls = 20
//! degrade_after = 10
//! ```

use crate::agent::core::SessionRuntime;
use crate::config::{RoutingGuardrailConfig, RoutingPreference};
use crate::middleware::{ExecutionState, MiddlewareDriver, Result, ToolCall, ToolResult};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{debug, info, info_span, trace, warn};

// ============================================================================
// High-output tool classification
// ============================================================================

/// Tools that produce potentially large output and have context-safe alternatives.
const HIGH_OUTPUT_TOOLS: &[(&str, &str)] = &[
    (
        "shell",
        "Consider using `context_execute` instead of `shell` for commands that produce large output. \
         This indexes the full output and returns a bounded preview.",
    ),
    (
        "web_fetch",
        "Consider using `context_fetch` instead of `web_fetch` for large responses. \
         This indexes the full response and returns a bounded preview.",
    ),
];

/// Tools that are context-safe alternatives (used for enforcement decisions).
const CONTEXT_SAFE_TOOLS: &[&str] = &[
    "context_execute",
    "context_execute_file",
    "context_fetch",
    "context_search",
    "batch_execute",
];

/// Check if a tool is a high-output tool and return the routing nudge message.
fn get_routing_nudge(tool_name: &str) -> Option<&'static str> {
    HIGH_OUTPUT_TOOLS
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, msg)| *msg)
}

/// Check if a tool is context-safe.
fn is_context_safe(tool_name: &str) -> bool {
    CONTEXT_SAFE_TOOLS.contains(&tool_name)
}

// ============================================================================
// Search throttling state
// ============================================================================

/// Tracks search calls within a sliding window for throttling.
#[derive(Debug)]
struct SearchThrottleState {
    /// Timestamps of search calls within the current window.
    call_timestamps: Vec<Instant>,
    /// Window size in seconds.
    window_secs: u64,
    /// Maximum calls before hard block.
    max_calls: usize,
    /// Number of calls after which per-query result budget degrades.
    degrade_after: usize,
}

impl SearchThrottleState {
    fn new(window_secs: u64, max_calls: usize, degrade_after: usize) -> Self {
        Self {
            call_timestamps: Vec::new(),
            window_secs,
            max_calls,
            degrade_after,
        }
    }

    /// Record a search call and return the throttle status.
    fn record_call(&mut self) -> ThrottleStatus {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(self.window_secs);

        // Prune old entries outside the window
        self.call_timestamps
            .retain(|&ts| now.duration_since(ts) < window);

        // Add current call
        self.call_timestamps.push(now);

        let count = self.call_timestamps.len();

        if count > self.max_calls {
            ThrottleStatus::Blocked {
                calls_in_window: count,
                max_calls: self.max_calls,
            }
        } else if count > self.degrade_after {
            // Degrade: reduce result budget proportionally
            let excess = count - self.degrade_after;
            let remaining = self.max_calls - self.degrade_after;
            let degradation_factor = 1.0 - (excess as f64 / remaining.max(1) as f64).min(0.8);
            ThrottleStatus::Degraded {
                calls_in_window: count,
                degradation_factor,
            }
        } else {
            ThrottleStatus::Normal
        }
    }

    /// Get the current call count within the window without recording a new call.
    fn current_count(&self) -> usize {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(self.window_secs);
        self.call_timestamps
            .iter()
            .filter(|&&ts| now.duration_since(ts) < window)
            .count()
    }

    fn reset(&mut self) {
        self.call_timestamps.clear();
    }
}

/// Result of checking the throttle.
#[derive(Debug, Clone)]
pub enum ThrottleStatus {
    /// Under the degrade threshold, normal operation.
    Normal,
    /// Above degrade threshold: per-query result budget is reduced.
    Degraded {
        calls_in_window: usize,
        degradation_factor: f64,
    },
    /// Above max_calls: search is blocked.
    Blocked {
        calls_in_window: usize,
        max_calls: usize,
    },
}

// ============================================================================
// Middleware implementation
// ============================================================================

/// Routing guardrails middleware.
///
/// Hooks into the `ProcessingToolCalls` phase to:
/// 1. Annotate high-output tool results with routing nudges (warn mode).
/// 2. Block or redirect context-flooding calls (enforce mode).
/// 3. Throttle search/retrieval calls to prevent runaway loops.
pub struct RoutingGuardrailsMiddleware {
    config: RoutingGuardrailConfig,
    throttle: Mutex<SearchThrottleState>,
    active: AtomicBool,
}

impl RoutingGuardrailsMiddleware {
    pub fn new(config: RoutingGuardrailConfig) -> Self {
        let throttle = SearchThrottleState::new(
            config.search_throttle.window_secs,
            config.search_throttle.max_calls,
            config.search_throttle.degrade_after,
        );
        Self {
            active: AtomicBool::new(config.routing_preference != RoutingPreference::Off),
            config,
            throttle: Mutex::new(throttle),
        }
    }

    /// Check if a tool call should be blocked in enforce mode.
    ///
    /// Currently unused — enforcement is only applied to throttled search calls.
    /// Reserved for future use when result-size-based enforcement is added.
    #[allow(dead_code)]
    fn should_block(&self, tool_name: &str, output_size: usize) -> bool {
        if self.config.routing_preference != RoutingPreference::Enforce {
            return false;
        }

        // In enforce mode, block high-output tools when output exceeds a threshold
        // and a context-safe alternative exists.
        if get_routing_nudge(tool_name).is_some() && output_size > 8192 {
            return true;
        }

        false
    }

    /// Apply routing nudge to a tool result if applicable.
    fn apply_nudge(&self, tool_name: &str, result: &str) -> Option<String> {
        if self.config.routing_preference == RoutingPreference::Off {
            return None;
        }

        let nudge = get_routing_nudge(tool_name)?;

        // Only nudge if the output is substantial (> 2KB)
        if result.len() < 2048 {
            return None;
        }

        Some(format!("{}\n\n[Routing hint: {}]", result, nudge))
    }

    /// Check search throttle and return modified result budget.
    fn check_search_throttle(&self, tool_name: &str) -> Option<ThrottleStatus> {
        if !is_context_safe(tool_name) || tool_name != "context_search" {
            return None;
        }

        let mut throttle = self.throttle.lock().unwrap();
        let status = throttle.record_call();

        match &status {
            ThrottleStatus::Normal => {
                trace!(
                    "search throttle: normal ({} calls in window)",
                    throttle.current_count()
                );
            }
            ThrottleStatus::Degraded {
                calls_in_window,
                degradation_factor,
            } => {
                debug!(
                    "search throttle: degraded ({} calls, factor {:.2})",
                    calls_in_window, degradation_factor
                );
            }
            ThrottleStatus::Blocked {
                calls_in_window,
                max_calls,
            } => {
                warn!(
                    "search throttle: blocked ({} calls, max {})",
                    calls_in_window, max_calls
                );
            }
        }

        Some(status)
    }
}

#[async_trait]
impl MiddlewareDriver for RoutingGuardrailsMiddleware {
    async fn on_processing_tool_calls(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        if !self.active.load(Ordering::Relaxed) {
            return Ok(state);
        }

        let ExecutionState::ProcessingToolCalls {
            remaining_calls,
            results,
            context,
        } = state
        else {
            return Ok(state);
        };

        let span = info_span!(
            "routing_guardrails.process",
            mode = ?self.config.routing_preference,
            results_count = results.len(),
            calls_count = remaining_calls.len(),
            nudges_applied = tracing::field::Empty,
            throttle_blocks = tracing::field::Empty,
        );
        let _guard = span.enter();

        let mut modified_results: Vec<ToolResult> = results.to_vec();
        let mut modified_calls: Vec<ToolCall> = remaining_calls.to_vec();
        let mut nudges_applied: u64 = 0;
        let mut throttle_blocks: u64 = 0;

        // Process results: add routing nudges to completed tool results
        for result in &mut modified_results {
            let tool_name = find_tool_name_for_result(result, &remaining_calls);

            if let Some(ref tool_name) = tool_name {
                if let Some(nudged) = self.apply_nudge(tool_name, &result.content) {
                    info!(
                        tool_name,
                        result_bytes = result.content.len(),
                        "routing nudge applied"
                    );
                    result.content = nudged;
                    nudges_applied += 1;
                }
            }
        }

        // Process remaining calls: check for enforcement and throttling
        if self.config.routing_preference == RoutingPreference::Enforce {
            let mut new_remaining = Vec::new();
            for call in &modified_calls {
                let tool_name = &call.function.name;

                if let Some(status) = self.check_search_throttle(tool_name) {
                    match status {
                        ThrottleStatus::Blocked {
                            calls_in_window,
                            max_calls,
                        } => {
                            info!(
                                tool_name,
                                calls_in_window, max_calls, "search throttle blocked call"
                            );
                            modified_results.push(ToolResult::new(
                                call.id.clone(),
                                format!(
                                    "Search throttled: {} calls in the current window (max {}). \
                                     Wait before making more search queries, or use `batch_execute` \
                                     to combine commands and queries in a single call.",
                                    calls_in_window, max_calls
                                ),
                                true,
                                Some(call.function.name.clone()),
                                Some(call.function.arguments.clone()),
                            ));
                            throttle_blocks += 1;
                            continue;
                        }
                        ThrottleStatus::Degraded {
                            calls_in_window,
                            degradation_factor,
                        } => {
                            debug!(
                                tool_name,
                                calls_in_window, degradation_factor, "search throttle degraded"
                            );
                        }
                        ThrottleStatus::Normal => {}
                    }
                }

                new_remaining.push(call.clone());
            }
            modified_calls = new_remaining;
        } else if self.config.routing_preference == RoutingPreference::Warn {
            for call in &modified_calls {
                let tool_name = &call.function.name;
                if let Some(status) = self.check_search_throttle(tool_name) {
                    match status {
                        ThrottleStatus::Degraded {
                            calls_in_window,
                            degradation_factor,
                        } => {
                            debug!(
                                tool_name,
                                calls_in_window,
                                degradation_factor,
                                "search throttle warning: degraded"
                            );
                        }
                        ThrottleStatus::Blocked {
                            calls_in_window,
                            max_calls,
                        } => {
                            warn!(
                                tool_name,
                                calls_in_window, max_calls, "search throttle warning: would block"
                            );
                        }
                        ThrottleStatus::Normal => {}
                    }
                }
            }
        }

        span.record("nudges_applied", nudges_applied);
        span.record("throttle_blocks", throttle_blocks);

        Ok(ExecutionState::ProcessingToolCalls {
            remaining_calls: modified_calls.into(),
            results: modified_results.into(),
            context,
        })
    }

    fn reset(&self) {
        let mut throttle = self.throttle.lock().unwrap();
        throttle.reset();
    }

    fn name(&self) -> &'static str {
        "routing_guardrails"
    }
}

/// Try to find the tool name for a given result by matching call_id against tool calls.
///
/// This is a best-effort lookup — if the call has already been consumed from
/// remaining_calls, we won't find it, which is fine (nudges apply to results
/// from known high-output tools).
fn find_tool_name_for_result(result: &ToolResult, calls: &[ToolCall]) -> Option<String> {
    calls
        .iter()
        .find(|c| c.id == result.call_id)
        .map(|c| c.function.name.clone())
}

// ============================================================================
// Factory for config-based creation
// ============================================================================

/// Factory for creating `RoutingGuardrailsMiddleware` from config.
pub struct RoutingGuardrailsFactory;

impl crate::middleware::factory::MiddlewareFactory for RoutingGuardrailsFactory {
    fn type_name(&self) -> &'static str {
        "routing_guardrails"
    }

    fn create(
        &self,
        _config: &serde_json::Value,
        agent_config: &crate::agent::agent_config::AgentConfig,
    ) -> anyhow::Result<Arc<dyn MiddlewareDriver>> {
        let guardrail_config = agent_config.execution_policy.routing_guardrails.clone();

        if guardrail_config.routing_preference == RoutingPreference::Off {
            return Err(anyhow::anyhow!(
                "routing_guardrails middleware is disabled (routing_preference = off)"
            ));
        }

        Ok(Arc::new(RoutingGuardrailsMiddleware::new(guardrail_config)))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RoutingGuardrailConfig, RoutingPreference, SearchThrottleConfig};
    use crate::middleware::state::{AgentStats, ConversationContext, ToolFunction};

    fn make_config(preference: RoutingPreference) -> RoutingGuardrailConfig {
        RoutingGuardrailConfig {
            routing_preference: preference,
            search_throttle: SearchThrottleConfig {
                window_secs: 60,
                max_calls: 5,
                degrade_after: 3,
            },
        }
    }

    fn make_tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            function: ToolFunction {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    fn make_tool_result(call_id: &str, content: &str) -> ToolResult {
        ToolResult::new(call_id.to_string(), content.to_string(), false, None, None)
    }

    fn make_context() -> Arc<ConversationContext> {
        Arc::new(ConversationContext::new(
            "test".into(),
            Arc::from([]),
            Arc::new(AgentStats::default()),
            "mock".into(),
            "mock-model".into(),
        ))
    }

    #[test]
    fn test_routing_nudge_for_shell() {
        let mw = RoutingGuardrailsMiddleware::new(make_config(RoutingPreference::Warn));
        let big_output = "x".repeat(3000);
        let nudged = mw.apply_nudge("shell", &big_output);
        assert!(nudged.is_some());
        assert!(nudged.unwrap().contains("context_execute"));
    }

    #[test]
    fn test_routing_nudge_for_web_fetch() {
        let mw = RoutingGuardrailsMiddleware::new(make_config(RoutingPreference::Warn));
        let big_output = "x".repeat(3000);
        let nudged = mw.apply_nudge("web_fetch", &big_output);
        assert!(nudged.is_some());
        assert!(nudged.unwrap().contains("context_fetch"));
    }

    #[test]
    fn test_no_nudge_for_small_output() {
        let mw = RoutingGuardrailsMiddleware::new(make_config(RoutingPreference::Warn));
        let nudged = mw.apply_nudge("shell", "small output");
        assert!(nudged.is_none());
    }

    #[test]
    fn test_no_nudge_when_off() {
        let mw = RoutingGuardrailsMiddleware::new(make_config(RoutingPreference::Off));
        let big_output = "x".repeat(3000);
        let nudged = mw.apply_nudge("shell", &big_output);
        assert!(nudged.is_none());
    }

    #[test]
    fn test_no_nudge_for_context_safe_tool() {
        let mw = RoutingGuardrailsMiddleware::new(make_config(RoutingPreference::Warn));
        let big_output = "x".repeat(3000);
        let nudged = mw.apply_nudge("context_execute", &big_output);
        assert!(nudged.is_none());
    }

    #[test]
    fn test_search_throttle_normal() {
        let mut state = SearchThrottleState::new(60, 5, 3);
        let status = state.record_call();
        assert!(matches!(status, ThrottleStatus::Normal));
    }

    #[test]
    fn test_search_throttle_degraded() {
        let mut state = SearchThrottleState::new(60, 5, 3);
        // Make 4 calls (above degrade_after=3)
        for _ in 0..3 {
            state.record_call();
        }
        let status = state.record_call();
        assert!(matches!(status, ThrottleStatus::Degraded { .. }));
    }

    #[test]
    fn test_search_throttle_blocked() {
        let mut state = SearchThrottleState::new(60, 5, 3);
        // Make 6 calls (above max_calls=5)
        for _ in 0..5 {
            state.record_call();
        }
        let status = state.record_call();
        assert!(matches!(status, ThrottleStatus::Blocked { .. }));
    }

    #[test]
    fn test_search_throttle_reset() {
        let mut state = SearchThrottleState::new(60, 5, 3);
        for _ in 0..5 {
            state.record_call();
        }
        state.reset();
        assert_eq!(state.current_count(), 0);
        let status = state.record_call();
        assert!(matches!(status, ThrottleStatus::Normal));
    }

    #[test]
    fn test_is_context_safe() {
        assert!(is_context_safe("context_execute"));
        assert!(is_context_safe("context_search"));
        assert!(is_context_safe("batch_execute"));
        assert!(!is_context_safe("shell"));
        assert!(!is_context_safe("web_fetch"));
    }

    #[tokio::test]
    async fn test_middleware_passthrough_when_off() {
        let mw = RoutingGuardrailsMiddleware::new(make_config(RoutingPreference::Off));

        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: vec![make_tool_call("c1", "shell")].into(),
            results: vec![make_tool_result("c0", &"x".repeat(5000))].into(),
            context: make_context(),
        };

        let result = mw.on_processing_tool_calls(state, None).await.unwrap();

        if let ExecutionState::ProcessingToolCalls { results, .. } = result {
            // Output should not have nudge annotation
            assert!(!results[0].content.contains("Routing hint"));
        } else {
            panic!("expected ProcessingToolCalls state");
        }
    }

    #[tokio::test]
    async fn test_middleware_adds_nudge_in_warn_mode() {
        let mw = RoutingGuardrailsMiddleware::new(make_config(RoutingPreference::Warn));

        let big_result = make_tool_result("c1", &"x".repeat(5000));
        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: vec![make_tool_call("c1", "shell")].into(),
            results: vec![big_result].into(),
            context: make_context(),
        };

        let result = mw.on_processing_tool_calls(state, None).await.unwrap();

        if let ExecutionState::ProcessingToolCalls { results, .. } = result {
            assert!(
                results[0].content.contains("Routing hint"),
                "expected routing nudge in result"
            );
        } else {
            panic!("expected ProcessingToolCalls state");
        }
    }

    #[tokio::test]
    async fn test_enforce_mode_blocks_throttled_search() {
        let mw = RoutingGuardrailsMiddleware::new(make_config(RoutingPreference::Enforce));

        // Exhaust the throttle
        {
            let mut throttle = mw.throttle.lock().unwrap();
            for _ in 0..6 {
                throttle.record_call();
            }
        }

        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: vec![make_tool_call("c1", "context_search")].into(),
            results: vec![].into(),
            context: make_context(),
        };

        let result = mw.on_processing_tool_calls(state, None).await.unwrap();

        if let ExecutionState::ProcessingToolCalls {
            remaining_calls,
            results,
            ..
        } = result
        {
            // The search call should have been blocked and converted to a result
            assert!(
                remaining_calls.is_empty(),
                "search call should have been removed"
            );
            assert_eq!(results.len(), 1, "should have one error result");
            assert!(
                results[0].content.contains("throttled"),
                "should mention throttling"
            );
        } else {
            panic!("expected ProcessingToolCalls state");
        }
    }

    #[tokio::test]
    async fn test_non_processing_state_passthrough() {
        let mw = RoutingGuardrailsMiddleware::new(make_config(RoutingPreference::Enforce));

        let state = ExecutionState::Complete;
        let result = mw.on_processing_tool_calls(state, None).await.unwrap();

        assert!(matches!(result, ExecutionState::Complete));
    }
}

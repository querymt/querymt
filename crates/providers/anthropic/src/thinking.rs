//! Thinking, effort and sampling-parameter decisions.

use crate::config::Anthropic;
use crate::model::{ThinkingClass, resolve_thinking_class};
use querymt::chat::ReasoningEffort;
use serde::Serialize;

/// Smallest thinking budget the API accepts.
pub(crate) const MIN_THINKING_BUDGET: u32 = 1_024;

/// Configuration for the thinking feature
#[derive(Serialize, Debug)]
pub(crate) struct ThinkingConfig {
    #[serde(rename = "type")]
    pub(crate) thinking_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) budget_tokens: Option<u32>,
}

/// Output configuration (thinking depth) for adaptive models.
#[derive(Serialize, Debug)]
pub(crate) struct OutputConfig {
    pub(crate) effort: &'static str,
}

impl Anthropic {
    /// Thinking class for this model, honouring `adaptive_thinking`.
    pub(crate) fn thinking_class(&self) -> ThinkingClass {
        resolve_thinking_class(self.parsed_model().as_ref(), self.adaptive_thinking)
    }

    /// Maps generic reasoning effort to Anthropic default budget tokens.
    pub(crate) fn effort_budget_tokens(effort: ReasoningEffort) -> u32 {
        match effort {
            ReasoningEffort::Low => 1_024,
            ReasoningEffort::Medium => 8_000,
            ReasoningEffort::High => 16_000,
            ReasoningEffort::Max => 31_999,
        }
    }

    /// Fit a thinking budget to `max_tokens`: at most half, at least 1,024.
    /// Returns `None` when the cap can't fit the minimum budget.
    pub(crate) fn fit_budget(budget: u32, max_tokens: u32) -> Option<u32> {
        if max_tokens < 2 * MIN_THINKING_BUDGET {
            return None;
        }
        Some(budget.clamp(MIN_THINKING_BUDGET, max_tokens / 2))
    }

    /// Wire name of an effort level for `output_config.effort`.
    pub(crate) fn effort_wire_name(effort: ReasoningEffort) -> &'static str {
        match effort {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    }

    /// Decide `thinking` and `output_config` from the model class and effort.
    pub(crate) fn thinking_for_request(&self) -> (Option<ThinkingConfig>, Option<OutputConfig>) {
        let adaptive = || ThinkingConfig {
            thinking_type: "adaptive".to_string(),
            budget_tokens: None,
        };
        match (self.thinking_class(), self.reasoning_effort) {
            (ThinkingClass::DefaultOnAdaptive, None) | (ThinkingClass::Budget, None) => {
                (None, None)
            }
            (ThinkingClass::OptInAdaptive, None) => (Some(adaptive()), None),
            (ThinkingClass::DefaultOnAdaptive | ThinkingClass::OptInAdaptive, Some(effort)) => (
                Some(adaptive()),
                Some(OutputConfig {
                    effort: Self::effort_wire_name(effort),
                }),
            ),
            (ThinkingClass::Budget, Some(effort)) => {
                let budget = self
                    .reasoning_budget_tokens
                    .unwrap_or_else(|| Self::effort_budget_tokens(effort));
                match Self::fit_budget(budget, self.max_tokens) {
                    Some(budget_tokens) => (
                        Some(ThinkingConfig {
                            thinking_type: "enabled".to_string(),
                            budget_tokens: Some(budget_tokens),
                        }),
                        None,
                    ),
                    None => {
                        log::debug!(target: "qmt_anthropic", "max_tokens={} too small for thinking; omitting thinking", self.max_tokens);
                        (None, None)
                    }
                }
            }
        }
    }

    /// Sampling parameters valid for this model and thinking choice.
    pub(crate) fn sampling_for(
        &self,
        thinking_sent: bool,
    ) -> (Option<f32>, Option<f32>, Option<u32>) {
        let reject_all = self
            .parsed_model()
            .is_some_and(|m| m.rejects_sampling_params());
        let log_drop = |name: &str, why: &str| {
            log::debug!(target: "qmt_anthropic", "dropping {name} for {}: {why}", self.model);
        };
        if reject_all {
            if self.temperature.is_some() {
                log_drop("temperature", "model rejects sampling parameters");
            }
            if self.top_p.is_some() {
                log_drop("top_p", "model rejects sampling parameters");
            }
            if self.top_k.is_some() {
                log_drop("top_k", "model rejects sampling parameters");
            }
            return (None, None, None);
        }
        if thinking_sent {
            if self.temperature.is_some() {
                log_drop("temperature", "incompatible with thinking");
            }
            if self.top_k.is_some() {
                log_drop("top_k", "incompatible with thinking");
            }
            let top_p = self.top_p.filter(|p| (0.95..=1.0).contains(p));
            if self.top_p.is_some() && top_p.is_none() {
                log_drop("top_p", "outside 0.95-1 with thinking");
            }
            return (None, top_p, None);
        }
        (self.temperature, self.top_p, self.top_k)
    }
}

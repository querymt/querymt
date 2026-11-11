//! Usage wire types and their conversion to `querymt::Usage`.

use querymt::{CacheWriteTiers, Usage};
use serde::Deserialize;

/// Thinking-token breakdown nested under `usage.output_tokens_details`.
#[derive(Deserialize, Debug, Clone, Default)]
pub(crate) struct AnthropicOutputTokensDetails {
    #[serde(default)]
    pub(crate) thinking_tokens: Option<u32>,
}

/// Cache-write duration breakdown nested under `usage.cache_creation`.
///
/// These partition `cache_creation_input_tokens`; they are never extra input.
#[derive(Deserialize, Debug, Clone, Default)]
pub(crate) struct AnthropicCacheCreation {
    #[serde(default)]
    pub(crate) ephemeral_5m_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) ephemeral_1h_input_tokens: Option<u32>,
}

/// Provider-local usage object.
///
/// Anthropic nests the thinking-token count under
/// `output_tokens_details.thinking_tokens`, which the shared `querymt::Usage`
/// cannot deserialize directly. `thinking_tokens` are already included in
/// `output_tokens`; the `Usage` conversion subtracts them so the shared
/// output and reasoning categories stay exclusive.
///
/// Every field is optional so a streaming event that omits a field is
/// distinguishable from one that reports an explicit zero: an omitted field
/// keeps the earlier event's value, an explicit zero replaces it.
#[derive(Deserialize, Debug, Clone, Default)]
pub(crate) struct AnthropicUsage {
    #[serde(default)]
    pub(crate) input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) output_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_creation: Option<AnthropicCacheCreation>,
    #[serde(default)]
    pub(crate) output_tokens_details: Option<AnthropicOutputTokensDetails>,
}

impl AnthropicUsage {
    /// Apply a later event's usage: fields it reports replace earlier ones,
    /// fields it omits are retained. Re-applying the same event is a no-op.
    pub(crate) fn overlay(&mut self, later: AnthropicUsage) {
        self.input_tokens = later.input_tokens.or(self.input_tokens);
        self.output_tokens = later.output_tokens.or(self.output_tokens);
        self.cache_creation_input_tokens = later
            .cache_creation_input_tokens
            .or(self.cache_creation_input_tokens);
        self.cache_read_input_tokens = later
            .cache_read_input_tokens
            .or(self.cache_read_input_tokens);
        self.cache_creation = match (self.cache_creation.take(), later.cache_creation) {
            (Some(earlier), Some(later)) => Some(AnthropicCacheCreation {
                ephemeral_5m_input_tokens: later
                    .ephemeral_5m_input_tokens
                    .or(earlier.ephemeral_5m_input_tokens),
                ephemeral_1h_input_tokens: later
                    .ephemeral_1h_input_tokens
                    .or(earlier.ephemeral_1h_input_tokens),
            }),
            (earlier, later) => later.or(earlier),
        };
        let thinking =
            |u: &Option<AnthropicOutputTokensDetails>| u.as_ref().and_then(|d| d.thinking_tokens);
        let thinking_tokens =
            thinking(&later.output_tokens_details).or(thinking(&self.output_tokens_details));
        self.output_tokens_details =
            thinking_tokens.map(|thinking_tokens| AnthropicOutputTokensDetails {
                thinking_tokens: Some(thinking_tokens),
            });
    }

    /// Usage as known at `message_start`: input and cache categories are
    /// final, but the output count is a provisional placeholder (typically
    /// 1), so it is withheld. Emitting it would let a downstream max-merge keep
    /// it as an extra ordinary token once the final inclusive output is split
    /// into ordinary output and reasoning.
    pub(crate) fn into_start_usage(self) -> Usage {
        let mut usage = Usage::from(self);
        usage.output_tokens = 0;
        usage.reasoning_tokens = 0;
        usage
    }
}

impl From<AnthropicUsage> for Usage {
    fn from(usage: AnthropicUsage) -> Self {
        // Anthropic's `output_tokens` includes thinking tokens, while QueryMT's
        // output and reasoning categories are exclusive (shared cost code adds
        // them back together), so subtract here to avoid double counting.
        let reasoning = usage
            .output_tokens_details
            .and_then(|d| d.thinking_tokens)
            .unwrap_or(0);
        let output = usage.output_tokens.unwrap_or(0);
        // A breakdown with neither tier is no breakdown.
        let tiers = usage
            .cache_creation
            .map(|c| CacheWriteTiers {
                five_minute: c.ephemeral_5m_input_tokens,
                one_hour: c.ephemeral_1h_input_tokens,
            })
            .filter(|t| t.five_minute.is_some() || t.one_hour.is_some());
        // The aggregate is authoritative when reported. If only the breakdown
        // arrived, its parts are the aggregate. Never both added.
        let cache_write = usage.cache_creation_input_tokens.unwrap_or_else(|| {
            tiers.map_or(0, |t| {
                t.five_minute
                    .unwrap_or(0)
                    .saturating_add(t.one_hour.unwrap_or(0))
            })
        });
        Usage {
            input_tokens: usage.input_tokens.unwrap_or(0),
            output_tokens: output.saturating_sub(reasoning),
            reasoning_tokens: reasoning,
            cache_read: usage.cache_read_input_tokens.unwrap_or(0),
            cache_write,
            cache_write_tiers: tiers,
        }
    }
}

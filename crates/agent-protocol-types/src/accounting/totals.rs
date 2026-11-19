//! Accumulated accounting totals carried in load-snapshot summaries.
//!
//! Plain data and read-only helpers. The reducer that mutates these totals
//! lives in `querymt-agent-services`.

use super::AccountingReason;
use serde::{Deserialize, Serialize};

/// Summed USD per category. Unknown components contribute nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ComponentSums {
    pub input_usd: f64,
    pub output_usd: f64,
    pub cache_read_usd: f64,
    pub cache_write_usd: f64,
    pub cache_write_1h_usd: f64,
}

/// Summed reported tokens per exclusive usage category. These are usage
/// totals for the scope only; they are never the main conversation's context
/// size or step counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSums {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// Accumulated accounting for one scope (session, cycle or purpose).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScopeTotals {
    /// Supported charges that are individually known.
    pub known_subtotal_usd: f64,
    /// Known subtotal of the budget view (manual-rate valuation where one was
    /// snapshotted, otherwise the same as `known_subtotal_usd`).
    pub budget_known_subtotal_usd: f64,
    pub components: ComponentSums,
    /// Tokens reported by settled attempts, including excluded billing.
    #[serde(default)]
    pub tokens: TokenSums,
    /// Settled metered or zero-priced attempts.
    pub priced_attempts: u32,
    /// Settled attempts excluded from metered API cost.
    pub excluded_attempts: u32,
    /// Started attempts with no settlement yet.
    pub unresolved_attempts: u32,
    /// Settled metered attempts that are not fully valued.
    pub incomplete_attempts: u32,
    /// Settled attempts whose budget view is not fully valued.
    pub budget_incomplete_attempts: u32,
    /// Retained local generation records with no accounting basis (legacy
    /// request-end events). Evidence of absent coverage, never a charge.
    #[serde(default)]
    pub unaccounted_generations: u32,
    pub settled_reasons: Vec<AccountingReason>,
    /// Settled reasons the budget view is not fully valued (manual-rate
    /// valuation where snapshotted, otherwise the reported valuation).
    #[serde(default)]
    pub budget_settled_reasons: Vec<AccountingReason>,
}

impl ScopeTotals {
    /// Why this scope's budget view is incomplete, for best-effort budget
    /// warnings. Attempts that are merely in flight are not a reason: an
    /// ordinary pending attempt is not an uncertainty to report. Interrupted
    /// or settled-incomplete attempts and unaccounted history are.
    pub fn budget_reasons(&self) -> Vec<AccountingReason> {
        let mut out = Vec::new();
        if self.unaccounted_generations > 0 {
            out.push(AccountingReason::UnaccountedHistory);
        }
        out.extend(self.budget_settled_reasons.iter().cloned());
        out
    }

    /// A complete supported-estimate total, or `None` when it cannot be
    /// claimed (nothing priced, unresolved or incomplete attempts).
    pub fn complete_total_usd(&self) -> Option<f64> {
        (self.priced_attempts > 0
            && self.unresolved_attempts == 0
            && self.incomplete_attempts == 0
            && self.unaccounted_generations == 0)
            .then_some(self.known_subtotal_usd)
    }

    /// Budget-view complete total under the same rules.
    pub fn budget_complete_total_usd(&self) -> Option<f64> {
        (self.priced_attempts > 0
            && self.unresolved_attempts == 0
            && self.budget_incomplete_attempts == 0
            && self.unaccounted_generations == 0)
            .then_some(self.budget_known_subtotal_usd)
    }

    /// Current uncertainty reasons: unresolved first, then settled ones in
    /// first-seen order.
    pub fn reasons(&self) -> Vec<AccountingReason> {
        let mut out = Vec::with_capacity(self.settled_reasons.len() + 1);
        if self.unresolved_attempts > 0 {
            out.push(AccountingReason::UnresolvedAttempt);
        }
        if self.unaccounted_generations > 0 {
            out.push(AccountingReason::UnaccountedHistory);
        }
        out.extend(self.settled_reasons.iter().cloned());
        out
    }
}

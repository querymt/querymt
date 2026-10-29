//! Warn-and-continue for incomplete cycle budgets.
//!
//! When a price cap is configured but the cycle's budget view cannot be fully
//! valued (unavailable usage, a missing rate, ...), execution continues
//! against the known subtotal and the user is told once per material reason.
//! Reaching the cap with known charges still stops dispatch (see
//! `AttemptOperation::begin`); this module only reports uncertainty.
//!
//! Reasons come from [`ScopeTotals::budget_reasons`], which excludes attempts
//! that are merely in flight, so a normal pending attempt never warns.
//! Accounting-storage failures are not reasons here: they are execution
//! errors raised by the recorder.

use super::{AccountingReason, ScopeTotals};
use parking_lot::Mutex;
use querymt::providers::IncompleteReason;
use std::collections::HashSet;

/// Remembers which budget-uncertainty reasons a cycle has already reported.
#[derive(Default)]
pub struct BudgetWarnings {
    seen: Mutex<HashSet<AccountingReason>>,
}

impl BudgetWarnings {
    /// Reasons in `totals` not yet reported for this cycle, recorded as
    /// reported. Repeated snapshots of the same state return nothing; a new
    /// missing category returns just the new reason.
    pub fn take_new(&self, totals: &ScopeTotals) -> Vec<AccountingReason> {
        let mut seen = self.seen.lock();
        totals
            .budget_reasons()
            .into_iter()
            .filter(|reason| seen.insert(reason.clone()))
            .collect()
    }
}

/// User-facing text for newly reported reasons.
pub fn budget_warning_message(
    reasons: &[AccountingReason],
    known_usd: f64,
    cap_usd: f64,
) -> String {
    let causes = reasons.iter().map(describe).collect::<Vec<_>>().join("; ");
    format!(
        "Price limit of ${cap_usd:.2} is enforced on a best-effort basis: {causes}. \
         Continuing with ${known_usd:.4} of known spend this cycle; in-flight requests can overshoot."
    )
}

fn describe(reason: &AccountingReason) -> String {
    match reason {
        AccountingReason::UnresolvedAttempt => "a request has no recorded outcome".into(),
        AccountingReason::PartialUsage => {
            "an interrupted request reported only partial usage".into()
        }
        AccountingReason::UsageUnavailable => "a request reported no usage".into(),
        AccountingReason::InvalidSettlement => "a recorded charge could not be trusted".into(),
        AccountingReason::UnaccountedHistory => {
            "earlier history has no recorded pricing basis".into()
        }
        AccountingReason::Valuation { reason } => match reason {
            IncompleteReason::UsageUnavailable => "a request reported no usage".into(),
            IncompleteReason::PricingUnavailable => "no price is available for the model".into(),
            IncompleteReason::MissingRate { category } => {
                format!("no {category:?} rate is configured")
            }
            IncompleteReason::InvalidRate { category } => {
                format!("the {category:?} rate is invalid")
            }
            IncompleteReason::ContradictoryWriteDetail => {
                "cache-write usage details are contradictory".into()
            }
            IncompleteReason::UnsupportedDimension { dimension } => {
                format!("pricing dimension '{dimension}' is not supported")
            }
            IncompleteReason::AmountOverflow => "a charge was not representable".into(),
            IncompleteReason::PricingInapplicable { effective_model } => {
                format!("no price applies to the served model '{effective_model}'")
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::{AccountingReducer, AttemptPurpose};
    use querymt::providers::PricingCategory;

    fn totals_with(reason: IncompleteReason) -> ScopeTotals {
        // Build real totals through the reducer: a settled metered attempt
        // whose reported valuation is incomplete for `reason`.
        let mut start = crate::accounting::tests::sample_start();
        start.attempt.purpose = AttemptPurpose::MainResponse;
        let mut settlement = crate::accounting::tests::sample_settlement();
        settlement.valuation.complete_total_usd = None;
        settlement.valuation.incomplete = vec![reason];
        let mut reducer = AccountingReducer::new(start.attempt.session_id.clone());
        reducer
            .apply(&crate::events::AgentEventKind::LlmAttemptStarted { start })
            .unwrap();
        reducer
            .apply(&crate::events::AgentEventKind::LlmAttemptSettled { settlement })
            .unwrap();
        reducer.session().clone()
    }

    #[test]
    fn complete_budget_never_warns() {
        let warnings = BudgetWarnings::default();
        assert!(warnings.take_new(&ScopeTotals::default()).is_empty());
    }

    #[test]
    fn repeated_snapshots_warn_once_per_reason() {
        let warnings = BudgetWarnings::default();
        let totals = totals_with(IncompleteReason::MissingRate {
            category: PricingCategory::CacheRead,
        });
        let first = warnings.take_new(&totals);
        assert_eq!(first.len(), 1);
        for _ in 0..5 {
            assert!(warnings.take_new(&totals).is_empty());
        }
    }

    #[test]
    fn a_new_missing_category_warns_again() {
        let warnings = BudgetWarnings::default();
        let read = totals_with(IncompleteReason::MissingRate {
            category: PricingCategory::CacheRead,
        });
        assert_eq!(warnings.take_new(&read).len(), 1);
        let write = totals_with(IncompleteReason::MissingRate {
            category: PricingCategory::CacheWrite,
        });
        assert_eq!(warnings.take_new(&write).len(), 1);
        assert!(warnings.take_new(&write).is_empty());
    }

    #[test]
    fn independent_cycles_track_independently() {
        let totals = totals_with(IncompleteReason::PricingUnavailable);
        assert_eq!(BudgetWarnings::default().take_new(&totals).len(), 1);
        assert_eq!(BudgetWarnings::default().take_new(&totals).len(), 1);
    }

    #[test]
    fn message_names_the_cause_and_best_effort_status() {
        let text = budget_warning_message(
            &[AccountingReason::Valuation {
                reason: IncompleteReason::MissingRate {
                    category: PricingCategory::CacheWrite,
                },
            }],
            0.0123,
            1.0,
        );
        assert!(text.contains("best-effort"), "{text}");
        assert!(text.contains("CacheWrite"), "{text}");
    }
}

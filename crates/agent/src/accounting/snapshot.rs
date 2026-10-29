//! Replacement accounting state published to clients.
//!
//! One typed DTO is carried by `_meta.querymt.accounting` on `usage_update`,
//! by the metadata-only `session_info_update` fallback, and by
//! `SessionLoadSnapshot.accounting`. Every field comes from the shared
//! [`AccountingReducer`]; request-end convenience costs are never an input.
//!
//! Replacement semantics: a payload replaces the previous accounting state
//! atomically. `completeTotalUsd` is serialized as an explicit `null` when no
//! complete total can be claimed, so a client can clear a prior complete
//! value. A message without the payload does not update accounting.

use super::{AccountingReason, AccountingReducer, ScopeTotals};
use crate::events::AgentEventKind;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

/// Schema version of [`AccountingSnapshot`].
pub const ACCOUNTING_SNAPSHOT_VERSION: u32 = 1;

/// Which kinds of billing a scope contains.
#[typeshare]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountingBillingScope {
    /// No settled attempt yet.
    #[default]
    Empty,
    /// Only metered (or explicitly zero-priced) API attempts.
    Metered,
    /// Only attempts excluded from API cost (subscription/OAuth). Never a
    /// known zero-dollar invoice.
    Excluded,
    /// Metered attempts alongside excluded ones; amounts cover the metered
    /// attempts only.
    Mixed,
}

/// API-spend accounting for one scope (session or cycle).
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountingScopeSnapshot {
    /// Supported charges that are individually known.
    pub known_subtotal_usd: f64,
    /// Complete supported estimate, or an explicit `null`.
    #[typeshare(serialized_as = "f64", typescript(type = "number | null"))]
    pub complete_total_usd: Option<f64>,
    /// Whether `complete_total_usd` is available.
    pub complete: bool,
    pub billing: AccountingBillingScope,
    /// Why a complete total is unavailable (unresolved attempts first).
    #[typeshare(typescript(type = "Array<Record<string, unknown>>"))]
    pub reasons: Vec<AccountingReason>,
    pub priced_attempts: u32,
    pub excluded_attempts: u32,
    pub unresolved_attempts: u32,
    pub incomplete_attempts: u32,
    /// Retained local generation with no accounting basis (legacy history).
    pub unaccounted_generations: u32,
}

/// Budget valuation of a cycle. It uses manual rates where configured, so it
/// is reported separately from API spend and never replaces it.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountingBudgetSnapshot {
    pub known_subtotal_usd: f64,
    #[typeshare(serialized_as = "f64", typescript(type = "number | null"))]
    pub complete_total_usd: Option<f64>,
    /// Settled reasons the budget valuation is incomplete. In-flight attempts
    /// are not a reason.
    #[typeshare(typescript(type = "Array<Record<string, unknown>>"))]
    pub reasons: Vec<AccountingReason>,
    pub incomplete_attempts: u32,
    /// The cap is enforced against the known subtotal only.
    pub best_effort: bool,
}

/// The active execution cycle's accounting.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountingCycleSnapshot {
    pub cycle_id: String,
    pub scope: AccountingScopeSnapshot,
    pub budget: AccountingBudgetSnapshot,
}

/// Versioned replacement accounting state.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountingSnapshot {
    pub version: u32,
    /// Session-lifetime API spend.
    pub session: AccountingScopeSnapshot,
    /// The cycle of the most recent main-response attempt, or an explicit
    /// `null` when none has been recorded. It is never inferred from
    /// whichever auxiliary attempt settled last.
    #[typeshare(
        serialized_as = "AccountingCycleSnapshot",
        typescript(type = "AccountingCycleSnapshot | null")
    )]
    pub cycle: Option<AccountingCycleSnapshot>,
}

impl Default for AccountingSnapshot {
    fn default() -> Self {
        Self {
            version: ACCOUNTING_SNAPSHOT_VERSION,
            session: AccountingScopeSnapshot::default(),
            cycle: None,
        }
    }
}

fn scope_snapshot(totals: &ScopeTotals) -> AccountingScopeSnapshot {
    let complete_total_usd = totals.complete_total_usd();
    let billing = match (totals.priced_attempts > 0, totals.excluded_attempts > 0) {
        (true, true) => AccountingBillingScope::Mixed,
        (true, false) => AccountingBillingScope::Metered,
        (false, true) => AccountingBillingScope::Excluded,
        (false, false) => AccountingBillingScope::Empty,
    };
    AccountingScopeSnapshot {
        known_subtotal_usd: totals.known_subtotal_usd,
        complete: complete_total_usd.is_some(),
        complete_total_usd,
        billing,
        reasons: totals.reasons(),
        priced_attempts: totals.priced_attempts,
        excluded_attempts: totals.excluded_attempts,
        unresolved_attempts: totals.unresolved_attempts,
        incomplete_attempts: totals.incomplete_attempts,
        unaccounted_generations: totals.unaccounted_generations,
    }
}

fn budget_snapshot(totals: &ScopeTotals) -> AccountingBudgetSnapshot {
    let reasons = totals.budget_reasons();
    AccountingBudgetSnapshot {
        known_subtotal_usd: totals.budget_known_subtotal_usd,
        complete_total_usd: totals.budget_complete_total_usd(),
        best_effort: !reasons.is_empty(),
        reasons,
        incomplete_attempts: totals.budget_incomplete_attempts,
    }
}

impl AccountingSnapshot {
    /// Project the reducer's current state.
    pub fn from_reducer(reducer: &AccountingReducer) -> Self {
        let cycle = reducer.latest_cycle_id().map(|id| {
            let totals = reducer.cycle(id);
            AccountingCycleSnapshot {
                cycle_id: id.to_string(),
                scope: scope_snapshot(&totals),
                budget: budget_snapshot(&totals),
            }
        });
        Self {
            version: ACCOUNTING_SNAPSHOT_VERSION,
            session: scope_snapshot(reducer.session()),
            cycle,
        }
    }

    /// Reduce recorded events for one session. Records the reducer rejects
    /// (another session's attempt, or a contradictory settlement) are skipped
    /// rather than failing the whole projection.
    pub fn from_events<'a>(
        session_id: &str,
        kinds: impl IntoIterator<Item = &'a AgentEventKind>,
    ) -> Self {
        let mut reducer = AccountingReducer::new(session_id);
        for kind in kinds {
            if let Err(conflict) = reducer.apply(kind) {
                tracing::warn!(
                    session.id = %session_id,
                    attempt.id = %conflict.attempt_id,
                    detail = conflict.detail,
                    "skipping conflicting accounting record in snapshot"
                );
            }
        }
        Self::from_reducer(&reducer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::tests::{sample_pricing, sample_settlement, sample_start};
    use querymt::providers::{UsageValuation, value_usage};

    fn settled(id: &str, cycle: &str) -> [AgentEventKind; 2] {
        let mut start = sample_start();
        start.attempt.attempt_id = id.into();
        start.attempt.cycle_id = cycle.into();
        let mut settlement = sample_settlement();
        settlement.attempt.attempt_id = id.into();
        settlement.attempt.cycle_id = cycle.into();
        [
            AgentEventKind::LlmAttemptStarted { start },
            AgentEventKind::LlmAttemptSettled { settlement },
        ]
    }

    fn missing_cache_write(id: &str, cycle: &str) -> AgentEventKind {
        let mut pricing = sample_pricing();
        pricing.cache_write = None;
        let mut s = sample_settlement();
        s.attempt.attempt_id = id.into();
        s.attempt.cycle_id = cycle.into();
        s.valuation = value_usage(Some(&pricing), s.usage.as_ref());
        s.pricing = Some(pricing);
        AgentEventKind::LlmAttemptSettled { settlement: s }
    }

    #[test]
    fn empty_snapshot_has_explicit_null_cycle_and_total() {
        let json = serde_json::to_value(AccountingSnapshot::default()).unwrap();
        assert_eq!(json["version"], 1);
        assert!(json["cycle"].is_null());
        assert!(json.as_object().unwrap().contains_key("cycle"));
        assert!(json["session"]["completeTotalUsd"].is_null());
        assert!(
            json["session"]
                .as_object()
                .unwrap()
                .contains_key("completeTotalUsd")
        );
        assert_eq!(json["session"]["billing"], "empty");
    }

    #[test]
    fn complete_total_is_cleared_with_an_explicit_null_when_it_turns_partial() {
        let mut kinds: Vec<AgentEventKind> = settled("a1", "c1").into();
        let complete = AccountingSnapshot::from_events("sess-1", &kinds);
        assert!(complete.session.complete);
        assert!(complete.session.complete_total_usd.is_some());

        kinds.push(missing_cache_write("a2", "c1"));
        let partial = AccountingSnapshot::from_events("sess-1", &kinds);
        let json = serde_json::to_value(&partial).unwrap();
        assert!(!partial.session.complete);
        assert!(json["session"]["completeTotalUsd"].is_null());
        assert!(
            partial.session.known_subtotal_usd > complete.session.known_subtotal_usd,
            "known subtotal keeps the priced components"
        );
        assert_eq!(partial.session.incomplete_attempts, 1);
        assert!(!partial.session.reasons.is_empty());
    }

    #[test]
    fn excluded_only_session_is_not_a_zero_dollar_total() {
        let mut start = sample_start();
        start.billing = querymt::providers::BillingClass::Excluded;
        let mut settlement = sample_settlement();
        settlement.valuation = UsageValuation::excluded();
        let snapshot = AccountingSnapshot::from_events(
            "sess-1",
            &[
                AgentEventKind::LlmAttemptStarted { start },
                AgentEventKind::LlmAttemptSettled { settlement },
            ],
        );
        assert_eq!(snapshot.session.billing, AccountingBillingScope::Excluded);
        assert_eq!(snapshot.session.complete_total_usd, None);
        assert_eq!(snapshot.session.excluded_attempts, 1);
    }

    #[test]
    fn mixed_billing_reports_metered_amount_and_exclusion() {
        let mut kinds: Vec<AgentEventKind> = settled("a1", "c1").into();
        let mut start = sample_start();
        start.attempt.attempt_id = "a2".into();
        let mut settlement = sample_settlement();
        settlement.attempt.attempt_id = "a2".into();
        settlement.valuation = UsageValuation::excluded();
        kinds.push(AgentEventKind::LlmAttemptStarted { start });
        kinds.push(AgentEventKind::LlmAttemptSettled { settlement });
        let snapshot = AccountingSnapshot::from_events("sess-1", &kinds);
        assert_eq!(snapshot.session.billing, AccountingBillingScope::Mixed);
        assert!(snapshot.session.complete_total_usd.is_some());
        assert_eq!(snapshot.session.excluded_attempts, 1);
    }

    #[test]
    fn active_cycle_follows_main_responses_not_delayed_auxiliary_settlements() {
        let mut kinds: Vec<AgentEventKind> = settled("a1", "c1").into();
        kinds.extend(settled("a2", "c2"));
        // A compaction attributed to the older cycle settles last.
        let mut start = sample_start();
        start.attempt.attempt_id = "a3".into();
        start.attempt.cycle_id = "c1".into();
        start.attempt.purpose = crate::accounting::AttemptPurpose::Compaction;
        let mut settlement = sample_settlement();
        settlement.attempt = start.attempt.clone();
        kinds.push(AgentEventKind::LlmAttemptStarted { start });
        kinds.push(AgentEventKind::LlmAttemptSettled { settlement });
        let snapshot = AccountingSnapshot::from_events("sess-1", &kinds);
        let cycle = snapshot.cycle.expect("active cycle");
        assert_eq!(cycle.cycle_id, "c2");
        assert_eq!(cycle.scope.priced_attempts, 1);
        assert_eq!(snapshot.session.priced_attempts, 3);
    }

    #[test]
    fn budget_view_is_separate_from_api_spend() {
        let mut kinds: Vec<AgentEventKind> = settled("a1", "c1").into();
        let mut start = sample_start();
        start.attempt.attempt_id = "a2".into();
        start.attempt.cycle_id = "c1".into();
        let mut manual = sample_pricing();
        manual.cache_write = None;
        start.budget_pricing = Some(manual.clone());
        let mut settlement = sample_settlement();
        settlement.attempt = start.attempt.clone();
        settlement.budget_valuation = Some(value_usage(Some(&manual), settlement.usage.as_ref()));
        kinds.push(AgentEventKind::LlmAttemptStarted { start });
        kinds.push(AgentEventKind::LlmAttemptSettled { settlement });
        let snapshot = AccountingSnapshot::from_events("sess-1", &kinds);
        assert!(snapshot.session.complete, "API estimate stays complete");
        let cycle = snapshot.cycle.unwrap();
        assert!(cycle.budget.best_effort);
        assert_eq!(cycle.budget.complete_total_usd, None);
        assert!(!cycle.budget.reasons.is_empty());
    }

    /// Every ```json block of the accounting section in the API reference
    /// whose root has `"version"` must deserialize into the real DTO and
    /// re-serialize to the same JSON, so the documented examples cannot drift.
    #[test]
    fn documented_examples_match_the_dto() {
        let docs = include_str!("../../../../docs/docs/agent/api_reference.md");
        let start = docs
            .find("#### Accounting metadata (ACP)")
            .expect("accounting section");
        let section = &docs[start..];
        let end = section.find("\n## ").unwrap_or(section.len());
        let section = &section[..end];
        let mut checked = 0;
        for block in section.split("```json").skip(1) {
            let json = block.split("```").next().unwrap();
            let value: serde_json::Value = serde_json::from_str(json).expect("valid JSON");
            let snapshot: AccountingSnapshot =
                serde_json::from_value(value.clone()).expect("example matches the DTO");
            assert_eq!(serde_json::to_value(&snapshot).unwrap(), value);
            checked += 1;
        }
        assert_eq!(checked, 2, "complete and cleared examples");
    }

    #[test]
    fn duplicate_records_do_not_change_the_snapshot() {
        let kinds: Vec<AgentEventKind> = settled("a1", "c1").into();
        let once = AccountingSnapshot::from_events("sess-1", &kinds);
        let twice = AccountingSnapshot::from_events("sess-1", kinds.iter().chain(kinds.iter()));
        assert_eq!(once, twice);
    }
}

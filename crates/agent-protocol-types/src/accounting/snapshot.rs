//! Replacement accounting state published to clients.
//!
//! One typed DTO is carried by `_meta.querymt.accounting` on `usage_update`,
//! by the metadata-only `session_info_update` fallback, and by
//! `SessionLoadSnapshot.accounting`. Replacement semantics: a payload replaces
//! the previous accounting state atomically. `completeTotalUsd` is serialized as
//! an explicit `null` when no complete total can be claimed. The reducer
//! projection lives in `querymt-agent-services`.

use super::AccountingReason;
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

impl AccountingSnapshot {
    /// Mark the session totals as covering only locally held events because
    /// older history of a remote session is not transferred yet: the complete
    /// total is withdrawn and the reason is recorded. Idempotent.
    pub fn mark_untransferred_history(&mut self) {
        self.session.complete = false;
        self.session.complete_total_usd = None;
        if !self
            .session
            .reasons
            .contains(&AccountingReason::UntransferredHistory)
        {
            self.session
                .reasons
                .push(AccountingReason::UntransferredHistory);
        }
    }
}

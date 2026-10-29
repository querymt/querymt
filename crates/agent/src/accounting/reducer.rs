//! Shared incremental reducer over attempt start/settlement records.
//!
//! One reducer implementation serves runtime budgets, ACP live state,
//! snapshot replay and exports. It is a rebuildable in-memory projection of the
//! journal: it never reprices (settlements carry their own valuations) and it
//! stores only a digest per attempt, not the full records.
//!
//! # Semantics
//!
//! * A start adds an *unresolved* attempt to its session, cycle and purpose
//!   scopes. Its settlement resolves it; it is never a second charge.
//! * An attempt contributes at most once. An identical duplicate (replay or
//!   remote redelivery) is a no-op; a contradictory record for the same
//!   attempt is an error and leaves state unchanged.
//! * Records may arrive in either order (a settlement is self-contained).
//! * Known subtotals are sums of finite, nonnegative amounts and only grow.
//!   Settled incompleteness persists; unresolved reasons disappear when the
//!   attempt settles.
//! * A complete total exists only when at least one metered/zero-priced
//!   attempt is present and nothing is unresolved or incomplete. Excluded
//!   attempts are counted but never produce a total, so an all-excluded scope
//!   has no dollar amount rather than a known zero.

use super::{AttemptPurpose, AttemptRef, AttemptSettlement, AttemptStart, UsageDisposition};
use crate::events::AgentEventKind;
use crate::hash::RapidHash;
use querymt::providers::{BillingClass, ComponentCharges, IncompleteReason};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Why a scope cannot claim a complete total.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountingReason {
    /// An attempt was started but no settlement has been recorded.
    UnresolvedAttempt,
    /// An interrupted attempt reported only part of its usage.
    PartialUsage,
    /// A dispatched attempt has no usage.
    UsageUnavailable,
    /// The valuation of a settled attempt was incomplete.
    Valuation { reason: IncompleteReason },
    /// A settlement was internally inconsistent (bad amounts or a complete
    /// claim its own fields contradict); it is never trusted as complete.
    InvalidSettlement,
    /// Retained history contains local generation records written before
    /// accounting existed. Their cost is unknown: old cumulative amounts are
    /// not trusted and no historical rate is applied.
    UnaccountedHistory,
}

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

impl TokenSums {
    fn add(&mut self, usage: &querymt::Usage) {
        self.input += u64::from(usage.input_tokens);
        self.output += u64::from(usage.output_tokens);
        self.reasoning += u64::from(usage.reasoning_tokens);
        self.cache_read += u64::from(usage.cache_read);
        // Aggregate writes only: tier detail partitions this and is not added.
        self.cache_write += u64::from(usage.cache_write);
    }
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
    settled_reasons: Vec<AccountingReason>,
    /// Settled reasons the budget view is not fully valued (manual-rate
    /// valuation where snapshotted, otherwise the reported valuation).
    #[serde(default)]
    budget_settled_reasons: Vec<AccountingReason>,
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

    fn add_unresolved(&mut self) {
        self.unresolved_attempts = self.unresolved_attempts.saturating_add(1);
    }

    fn resolve(&mut self) {
        self.unresolved_attempts = self.unresolved_attempts.saturating_sub(1);
    }

    fn settle(&mut self, c: &Contribution) {
        self.tokens.input += c.tokens.input;
        self.tokens.output += c.tokens.output;
        self.tokens.reasoning += c.tokens.reasoning;
        self.tokens.cache_read += c.tokens.cache_read;
        self.tokens.cache_write += c.tokens.cache_write;
        match c.kind {
            Kind::NoCharge => return,
            Kind::Excluded => {
                self.excluded_attempts = self.excluded_attempts.saturating_add(1);
                return;
            }
            Kind::Priced {
                complete,
                budget_complete,
            } => {
                self.priced_attempts = self.priced_attempts.saturating_add(1);
                if !complete {
                    self.incomplete_attempts = self.incomplete_attempts.saturating_add(1);
                }
                if !budget_complete {
                    self.budget_incomplete_attempts =
                        self.budget_incomplete_attempts.saturating_add(1);
                }
            }
        }
        self.known_subtotal_usd += c.known;
        self.budget_known_subtotal_usd += c.budget_known;
        self.components.input_usd += c.components.input_usd;
        self.components.output_usd += c.components.output_usd;
        self.components.cache_read_usd += c.components.cache_read_usd;
        self.components.cache_write_usd += c.components.cache_write_usd;
        self.components.cache_write_1h_usd += c.components.cache_write_1h_usd;
        for reason in &c.reasons {
            if !self.settled_reasons.contains(reason) {
                self.settled_reasons.push(reason.clone());
            }
        }
        for reason in &c.budget_reasons {
            if !self.budget_settled_reasons.contains(reason) {
                self.budget_settled_reasons.push(reason.clone());
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    /// Resolved without a charge (not sent, or provider-confirmed non-billable).
    NoCharge,
    Excluded,
    Priced {
        complete: bool,
        budget_complete: bool,
    },
}

struct Contribution {
    tokens: TokenSums,
    kind: Kind,
    known: f64,
    budget_known: f64,
    components: ComponentSums,
    reasons: Vec<AccountingReason>,
    budget_reasons: Vec<AccountingReason>,
}

fn valid_amount(x: f64) -> bool {
    x.is_finite() && x >= 0.0
}

/// Interpret a settlement without trusting its completeness claims.
fn contribution(s: &AttemptSettlement) -> Contribution {
    let mut tokens = TokenSums::default();
    if let Some(usage) = &s.usage {
        tokens.add(usage);
    }
    let none = |kind| Contribution {
        tokens,
        kind,
        known: 0.0,
        budget_known: 0.0,
        components: ComponentSums::default(),
        reasons: Vec::new(),
        budget_reasons: Vec::new(),
    };
    if matches!(
        s.disposition,
        UsageDisposition::NotSent | UsageDisposition::NotBillable
    ) {
        return none(Kind::NoCharge);
    }
    let v = &s.valuation;
    if v.billing == BillingClass::Excluded {
        return none(Kind::Excluded);
    }

    let mut invalid = false;
    let mut amount = |x: f64| {
        if valid_amount(x) {
            x
        } else {
            invalid = true;
            0.0
        }
    };
    let known = amount(v.known_subtotal_usd);
    let c: &ComponentCharges = &v.components;
    let components = ComponentSums {
        input_usd: c.input_usd.map_or(0.0, &mut amount),
        output_usd: c.output_usd.map_or(0.0, &mut amount),
        cache_read_usd: c.cache_read_usd.map_or(0.0, &mut amount),
        cache_write_usd: c.cache_write_usd.map_or(0.0, &mut amount),
        cache_write_1h_usd: c.cache_write_1h_usd.map_or(0.0, &mut amount),
    };
    let budget_known = match &s.budget_valuation {
        Some(b) => amount(b.known_subtotal_usd),
        None => known,
    };
    let budget_claims_complete = match &s.budget_valuation {
        Some(b) => b.complete_total_usd.is_some() && b.incomplete.is_empty(),
        None => true,
    };

    let mut reasons = Vec::new();
    match s.disposition {
        UsageDisposition::Partial => reasons.push(AccountingReason::PartialUsage),
        UsageDisposition::Unavailable => reasons.push(AccountingReason::UsageUnavailable),
        _ => {}
    }
    for reason in &v.incomplete {
        reasons.push(match reason {
            IncompleteReason::UsageUnavailable => AccountingReason::UsageUnavailable,
            other => AccountingReason::Valuation {
                reason: other.clone(),
            },
        });
    }
    reasons.dedup();

    let billing_is_priced = matches!(v.billing, BillingClass::Metered | BillingClass::ZeroPriced);
    let complete = !invalid
        && s.disposition == UsageDisposition::Final
        && billing_is_priced
        && v.complete_total_usd.is_some()
        && v.incomplete.is_empty();
    if !complete && reasons.is_empty() {
        // Claimed complete but contradicted by its own fields, or unknown
        // billing with no stated cause: never publish it as complete.
        reasons.push(AccountingReason::InvalidSettlement);
    }
    if invalid && !reasons.contains(&AccountingReason::InvalidSettlement) {
        reasons.push(AccountingReason::InvalidSettlement);
    }
    let budget_complete = complete && budget_claims_complete && valid_amount(budget_known);

    // Budget reasons: usage-level causes always apply. Valuation causes come
    // from the manual budget valuation when one was snapshotted, otherwise
    // from the reported valuation the budget view shares.
    let mut budget_reasons: Vec<AccountingReason> = reasons
        .iter()
        .filter(|r| !matches!(r, AccountingReason::Valuation { .. }))
        .cloned()
        .collect();
    let valuation_reasons: Vec<AccountingReason> = match &s.budget_valuation {
        Some(b) => b
            .incomplete
            .iter()
            .map(|reason| match reason {
                IncompleteReason::UsageUnavailable => AccountingReason::UsageUnavailable,
                other => AccountingReason::Valuation {
                    reason: other.clone(),
                },
            })
            .collect(),
        None => reasons
            .iter()
            .filter(|r| matches!(r, AccountingReason::Valuation { .. }))
            .cloned()
            .collect(),
    };
    for reason in valuation_reasons {
        if !budget_reasons.contains(&reason) {
            budget_reasons.push(reason);
        }
    }
    if budget_complete {
        budget_reasons.clear();
    } else if budget_reasons.is_empty() {
        // Incomplete budget view with no stated cause (for example a manual
        // valuation claiming completeness its own fields contradict).
        budget_reasons.push(AccountingReason::InvalidSettlement);
    }

    Contribution {
        tokens,
        kind: Kind::Priced {
            complete,
            budget_complete,
        },
        known,
        budget_known,
        components,
        reasons,
        budget_reasons,
    }
}

#[derive(Clone)]
enum AttemptState {
    Pending {
        attempt: AttemptRef,
        digest: RapidHash,
    },
    Settled {
        attempt: AttemptRef,
        digest: RapidHash,
    },
}

impl AttemptState {
    fn attempt(&self) -> &AttemptRef {
        match self {
            Self::Pending { attempt, .. } | Self::Settled { attempt, .. } => attempt,
        }
    }
}

/// Outcome of applying one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The record changed the projection.
    New,
    /// An identical record was already applied.
    Duplicate,
    /// Not an accounting record.
    NotAccounting,
}

/// A record contradicts what is already known about its attempt. The
/// reducer state is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("conflicting accounting record for attempt {attempt_id}: {detail}")]
pub struct AccountingConflict {
    pub attempt_id: String,
    pub detail: &'static str,
}

fn digest<T: Serialize>(record: &T) -> RapidHash {
    // Serialization of these plain DTOs cannot fail; an empty digest would
    // simply make a duplicate look like a conflict, which is the safe side.
    RapidHash::new(&serde_json::to_vec(record).unwrap_or_default())
}

/// Incremental projection of one session's accounting records.
#[derive(Clone)]
pub struct AccountingReducer {
    session_id: String,
    attempts: HashMap<String, AttemptState>,
    session: ScopeTotals,
    cycles: HashMap<String, ScopeTotals>,
    purposes: HashMap<AttemptPurpose, ScopeTotals>,
    /// Cycle of the most recently recorded main-response attempt. Derived
    /// purely from records (never from which attempt settled last), so live
    /// delivery and replay agree and a delayed auxiliary settlement cannot
    /// select an older cycle.
    latest_cycle: Option<String>,
    /// Journal sequence of the record that set `latest_cycle`, when known.
    latest_cycle_seq: Option<i64>,
    /// Sequence of the record currently being applied (see `apply_at`).
    applying_seq: Option<i64>,
}

impl std::fmt::Debug for AccountingReducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountingReducer")
            .field("session_id", &self.session_id)
            .field("attempts", &self.attempts.len())
            .field("session", &self.session)
            .finish()
    }
}

impl AccountingReducer {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            attempts: HashMap::new(),
            session: ScopeTotals::default(),
            cycles: HashMap::new(),
            purposes: HashMap::new(),
            latest_cycle: None,
            latest_cycle_seq: None,
            applying_seq: None,
        }
    }

    /// Cycle of the most recently recorded main-response attempt, if any.
    pub fn latest_cycle_id(&self) -> Option<&str> {
        self.latest_cycle.as_deref()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Lifetime totals for this session.
    pub fn session(&self) -> &ScopeTotals {
        &self.session
    }

    /// Totals for attempts attributed to one execution cycle (zero if none).
    pub fn cycle(&self, cycle_id: &str) -> ScopeTotals {
        self.cycles.get(cycle_id).cloned().unwrap_or_default()
    }

    /// Totals for one attempt purpose across the session.
    pub fn purpose(&self, purpose: AttemptPurpose) -> ScopeTotals {
        self.purposes.get(&purpose).cloned().unwrap_or_default()
    }

    /// Number of distinct attempts tracked.
    pub fn attempt_count(&self) -> usize {
        self.attempts.len()
    }

    /// Attempts started but not yet settled.
    pub fn unresolved(&self) -> Vec<&AttemptRef> {
        self.attempts
            .values()
            .filter_map(|s| match s {
                AttemptState::Pending { attempt, .. } => Some(attempt),
                AttemptState::Settled { .. } => None,
            })
            .collect()
    }

    /// Apply an event's accounting effect, if it has one.
    pub fn apply(&mut self, kind: &AgentEventKind) -> Result<Applied, AccountingConflict> {
        match kind {
            AgentEventKind::LlmAttemptStarted { start } => self.apply_start(start),
            AgentEventKind::LlmAttemptSettled { settlement } => self.apply_settlement(settlement),
            // A request-end derived by this contract always carries
            // `accounting`. One without it is a legacy generation record: it
            // contributes no amount (its cumulative cost is not trusted) and
            // only discloses that history is unaccounted.
            AgentEventKind::LlmRequestEnd {
                accounting: None, ..
            } => {
                self.session.unaccounted_generations =
                    self.session.unaccounted_generations.saturating_add(1);
                Ok(Applied::New)
            }
            _ => Ok(Applied::NotAccounting),
        }
    }

    fn conflict(attempt: &AttemptRef, detail: &'static str) -> AccountingConflict {
        AccountingConflict {
            attempt_id: attempt.attempt_id.clone(),
            detail,
        }
    }

    fn check_owner(&self, attempt: &AttemptRef) -> Result<(), AccountingConflict> {
        if attempt.session_id == self.session_id {
            Ok(())
        } else {
            Err(Self::conflict(
                attempt,
                "attempt belongs to another session",
            ))
        }
    }

    fn note_cycle(&mut self, attempt: &AttemptRef) {
        if attempt.purpose != AttemptPurpose::MainResponse {
            return;
        }
        // Records observed out of order (live events before seeded history)
        // must not move the active cycle backwards.
        if let (Some(seq), Some(current)) = (self.applying_seq, self.latest_cycle_seq)
            && seq < current
        {
            return;
        }
        self.latest_cycle = Some(attempt.cycle_id.clone());
        if self.applying_seq.is_some() {
            self.latest_cycle_seq = self.applying_seq;
        }
    }

    /// Like [`Self::apply`], for a record whose journal sequence is known so
    /// that out-of-order delivery cannot regress the active cycle.
    pub fn apply_at(
        &mut self,
        seq: i64,
        kind: &AgentEventKind,
    ) -> Result<Applied, AccountingConflict> {
        self.applying_seq = Some(seq);
        let result = self.apply(kind);
        self.applying_seq = None;
        result
    }

    fn each_scope(&mut self, attempt: &AttemptRef, f: impl Fn(&mut ScopeTotals)) {
        f(&mut self.session);
        f(self.cycles.entry(attempt.cycle_id.clone()).or_default());
        f(self.purposes.entry(attempt.purpose).or_default());
    }

    fn apply_start(&mut self, start: &AttemptStart) -> Result<Applied, AccountingConflict> {
        let attempt = &start.attempt;
        self.check_owner(attempt)?;
        match self.attempts.get(&attempt.attempt_id) {
            Some(existing) if existing.attempt() != attempt => {
                Err(Self::conflict(attempt, "attempt identity differs"))
            }
            Some(AttemptState::Pending { digest: d, .. }) if *d != digest(start) => {
                Err(Self::conflict(attempt, "start differs from recorded start"))
            }
            // Identical redelivery, or the settlement already arrived.
            Some(_) => Ok(Applied::Duplicate),
            None => {
                self.attempts.insert(
                    attempt.attempt_id.clone(),
                    AttemptState::Pending {
                        attempt: attempt.clone(),
                        digest: digest(start),
                    },
                );
                self.each_scope(attempt, ScopeTotals::add_unresolved);
                self.note_cycle(attempt);
                Ok(Applied::New)
            }
        }
    }

    fn apply_settlement(
        &mut self,
        settlement: &AttemptSettlement,
    ) -> Result<Applied, AccountingConflict> {
        let attempt = &settlement.attempt;
        self.check_owner(attempt)?;
        let new_digest = digest(settlement);
        let was_pending = match self.attempts.get(&attempt.attempt_id) {
            Some(existing) if existing.attempt() != attempt => {
                return Err(Self::conflict(attempt, "attempt identity differs"));
            }
            Some(AttemptState::Settled { digest: d, .. }) => {
                return if *d == new_digest {
                    Ok(Applied::Duplicate)
                } else {
                    Err(Self::conflict(attempt, "contradictory settlement"))
                };
            }
            Some(AttemptState::Pending { .. }) => true,
            None => false,
        };

        let c = contribution(settlement);
        if !was_pending {
            self.note_cycle(attempt);
        }
        self.attempts.insert(
            attempt.attempt_id.clone(),
            AttemptState::Settled {
                attempt: attempt.clone(),
                digest: new_digest,
            },
        );
        self.each_scope(attempt, |scope| {
            if was_pending {
                scope.resolve();
            }
            scope.settle(&c);
        });
        Ok(Applied::New)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::tests::{sample_pricing, sample_settlement, sample_start};
    use crate::accounting::{AttemptOutcome, PricingApplicability};
    use querymt::Usage;
    use querymt::providers::{UsageValuation, value_usage};

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-12, "{a} != {b}");
    }

    fn start(id: &str, cycle: &str, purpose: AttemptPurpose) -> AgentEventKind {
        let mut s = sample_start();
        s.attempt.attempt_id = id.into();
        s.attempt.cycle_id = cycle.into();
        s.attempt.purpose = purpose;
        AgentEventKind::LlmAttemptStarted { start: s }
    }

    fn settled_with(
        id: &str,
        cycle: &str,
        purpose: AttemptPurpose,
        edit: impl FnOnce(&mut AttemptSettlement),
    ) -> AgentEventKind {
        let mut s = sample_settlement();
        s.attempt.attempt_id = id.into();
        s.attempt.cycle_id = cycle.into();
        s.attempt.purpose = purpose;
        edit(&mut s);
        AgentEventKind::LlmAttemptSettled { settlement: s }
    }

    fn settled(id: &str, cycle: &str) -> AgentEventKind {
        settled_with(id, cycle, AttemptPurpose::MainResponse, |_| {})
    }

    /// sample usage at sample pricing: 100 in, 15 out, 50 read, 20 write.
    fn sample_cost() -> f64 {
        (100.0 * 3.0 + 15.0 * 15.0 + 50.0 * 0.3 + 20.0 * 3.75) / 1e6
    }

    fn reducer() -> AccountingReducer {
        AccountingReducer::new("sess-1")
    }

    #[test]
    fn settlement_resolves_its_start_without_a_second_charge() {
        let mut r = reducer();
        assert_eq!(
            r.apply(&start("a1", "c1", AttemptPurpose::MainResponse)),
            Ok(Applied::New)
        );
        assert_eq!(r.session().unresolved_attempts, 1);
        assert_eq!(r.session().complete_total_usd(), None);
        assert_eq!(
            r.session().reasons(),
            vec![AccountingReason::UnresolvedAttempt]
        );
        assert_eq!(r.unresolved().len(), 1);

        assert_eq!(r.apply(&settled("a1", "c1")), Ok(Applied::New));
        let s = r.session();
        assert_eq!(s.unresolved_attempts, 0);
        assert_eq!(s.priced_attempts, 1);
        approx(s.known_subtotal_usd, sample_cost());
        approx(s.complete_total_usd().unwrap(), sample_cost());
        assert!(s.reasons().is_empty());
        assert!(r.unresolved().is_empty());
        approx(s.components.cache_read_usd, 50.0 * 0.3 / 1e6);
    }

    #[test]
    fn identical_duplicates_and_remote_redelivery_add_nothing() {
        let mut r = reducer();
        let events = [
            start("a1", "c1", AttemptPurpose::MainResponse),
            settled("a1", "c1"),
        ];
        for e in &events {
            r.apply(e).unwrap();
        }
        let before = r.session().clone();
        // Replay then live relay of the same records, repeatedly.
        for _ in 0..3 {
            for e in &events {
                assert_eq!(r.apply(e), Ok(Applied::Duplicate));
            }
        }
        assert_eq!(r.session(), &before);
        assert_eq!(r.cycle("c1").priced_attempts, 1);
        assert_eq!(r.attempt_count(), 1);
    }

    #[test]
    fn duplicate_start_before_settlement_is_not_a_second_unresolved_attempt() {
        let mut r = reducer();
        let s = start("a1", "c1", AttemptPurpose::MainResponse);
        r.apply(&s).unwrap();
        assert_eq!(r.apply(&s), Ok(Applied::Duplicate));
        assert_eq!(r.session().unresolved_attempts, 1);
    }

    #[test]
    fn distinct_retry_attempts_add_independently() {
        let mut r = reducer();
        for (i, id) in ["a1", "a2"].into_iter().enumerate() {
            let mut s = sample_start();
            s.attempt.attempt_id = id.into();
            s.attempt.attempt_index = i as u32;
            r.apply(&AgentEventKind::LlmAttemptStarted { start: s })
                .unwrap();
            r.apply(&settled_with(
                id,
                "cycle-1",
                AttemptPurpose::MainResponse,
                |s| {
                    s.attempt.attempt_index = i as u32;
                },
            ))
            .unwrap();
        }
        assert_eq!(r.session().priced_attempts, 2);
        approx(r.session().known_subtotal_usd, 2.0 * sample_cost());
        approx(
            r.session().complete_total_usd().unwrap(),
            2.0 * sample_cost(),
        );
    }

    #[test]
    fn conflicting_settlement_fails_and_leaves_state_unchanged() {
        let mut r = reducer();
        r.apply(&settled("a1", "c1")).unwrap();
        let before = r.session().clone();
        let conflicting = settled_with("a1", "c1", AttemptPurpose::MainResponse, |s| {
            s.usage = Some(Usage {
                input_tokens: 1,
                ..Default::default()
            });
            s.valuation = value_usage(s.pricing.as_ref(), s.usage.as_ref());
        });
        let err = r.apply(&conflicting).unwrap_err();
        assert_eq!(err.attempt_id, "a1");
        assert_eq!(err.detail, "contradictory settlement");
        assert_eq!(r.session(), &before);
    }

    #[test]
    fn conflicting_start_identity_and_foreign_session_are_rejected() {
        let mut r = reducer();
        r.apply(&start("a1", "c1", AttemptPurpose::MainResponse))
            .unwrap();
        // Same attempt id, different cycle.
        assert!(
            r.apply(&start("a1", "c2", AttemptPurpose::MainResponse))
                .is_err()
        );
        // Settlement identity differs from the start's.
        assert!(
            r.apply(&settled_with(
                "a1",
                "c1",
                AttemptPurpose::Compaction,
                |_| {}
            ))
            .is_err()
        );
        assert_eq!(r.session().unresolved_attempts, 1);

        let mut foreign = sample_start();
        foreign.attempt.session_id = "other".into();
        foreign.attempt.attempt_id = "x".into();
        let err = r
            .apply(&AgentEventKind::LlmAttemptStarted { start: foreign })
            .unwrap_err();
        assert_eq!(err.detail, "attempt belongs to another session");
        assert_eq!(r.attempt_count(), 1);
    }

    #[test]
    fn settlement_may_arrive_before_or_without_its_start() {
        let mut r = reducer();
        r.apply(&settled("a1", "c1")).unwrap();
        // Late start for an already-settled attempt is not unresolved spend.
        assert_eq!(
            r.apply(&start("a1", "c1", AttemptPurpose::MainResponse)),
            Ok(Applied::Duplicate)
        );
        assert_eq!(r.session().unresolved_attempts, 0);
        assert_eq!(r.session().priced_attempts, 1);
    }

    #[test]
    fn excluded_only_scope_has_no_total() {
        let mut r = reducer();
        r.apply(&start("a1", "c1", AttemptPurpose::MainResponse))
            .unwrap();
        r.apply(&settled_with(
            "a1",
            "c1",
            AttemptPurpose::MainResponse,
            |s| {
                s.valuation = UsageValuation::excluded();
                s.pricing = None;
            },
        ))
        .unwrap();
        let s = r.session();
        assert_eq!(s.excluded_attempts, 1);
        assert_eq!(s.priced_attempts, 0);
        assert_eq!(s.known_subtotal_usd, 0.0);
        assert_eq!(
            s.complete_total_usd(),
            None,
            "excluded is not a known zero invoice"
        );
        assert!(
            s.reasons().is_empty(),
            "exclusion is scope, not uncertainty"
        );
    }

    #[test]
    fn mixed_billing_totals_cover_metered_attempts_and_disclose_exclusions() {
        let mut r = reducer();
        r.apply(&settled("metered", "c1")).unwrap();
        r.apply(&settled_with(
            "sub",
            "c1",
            AttemptPurpose::MainResponse,
            |s| {
                s.valuation = UsageValuation::excluded();
            },
        ))
        .unwrap();
        let s = r.session();
        assert_eq!((s.priced_attempts, s.excluded_attempts), (1, 1));
        approx(s.complete_total_usd().unwrap(), sample_cost());
    }

    #[test]
    fn unknown_or_contradicted_billing_cannot_fabricate_a_complete_total() {
        // Unknown billing that nevertheless claims a complete total.
        let mut r = reducer();
        r.apply(&settled_with(
            "a1",
            "c1",
            AttemptPurpose::MainResponse,
            |s| {
                s.valuation.billing = BillingClass::Unknown;
            },
        ))
        .unwrap();
        assert_eq!(r.session().complete_total_usd(), None);
        assert!(
            r.session()
                .reasons()
                .contains(&AccountingReason::InvalidSettlement)
        );

        // Valuation with no pricing basis: incomplete for a stated reason.
        let mut r = reducer();
        r.apply(&settled_with(
            "a1",
            "c1",
            AttemptPurpose::MainResponse,
            |s| {
                s.pricing = None;
                s.valuation = value_usage(None, s.usage.as_ref());
            },
        ))
        .unwrap();
        assert_eq!(r.session().complete_total_usd(), None);
        assert!(
            r.session()
                .reasons()
                .contains(&AccountingReason::Valuation {
                    reason: IncompleteReason::PricingUnavailable
                })
        );

        // Complete claim contradicted by its own incomplete list.
        let mut r = reducer();
        r.apply(&settled_with(
            "a1",
            "c1",
            AttemptPurpose::MainResponse,
            |s| {
                s.valuation.incomplete = vec![IncompleteReason::ContradictoryWriteDetail];
            },
        ))
        .unwrap();
        assert_eq!(r.session().complete_total_usd(), None);
    }

    #[test]
    fn non_finite_or_negative_amounts_are_invalid_not_summed() {
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            let mut r = reducer();
            r.apply(&settled_with(
                "a1",
                "c1",
                AttemptPurpose::MainResponse,
                |s| {
                    s.valuation.known_subtotal_usd = bad;
                },
            ))
            .unwrap();
            let s = r.session();
            assert!(s.known_subtotal_usd.is_finite() && s.known_subtotal_usd == 0.0);
            assert_eq!(s.complete_total_usd(), None);
            assert!(s.reasons().contains(&AccountingReason::InvalidSettlement));
        }
    }

    #[test]
    fn partial_and_unavailable_usage_keep_known_subtotal_but_no_total() {
        let mut r = reducer();
        r.apply(&start("p", "c1", AttemptPurpose::MainResponse))
            .unwrap();
        r.apply(&settled_with(
            "p",
            "c1",
            AttemptPurpose::MainResponse,
            |s| {
                s.disposition = UsageDisposition::Partial;
                s.outcome = AttemptOutcome::Cancelled;
            },
        ))
        .unwrap();
        r.apply(&settled_with(
            "u",
            "c1",
            AttemptPurpose::MainResponse,
            |s| {
                s.disposition = UsageDisposition::Unavailable;
                s.outcome = AttemptOutcome::TransportFailure;
                s.usage = None;
                s.valuation = value_usage(s.pricing.as_ref(), None);
            },
        ))
        .unwrap();
        let s = r.session();
        approx(s.known_subtotal_usd, sample_cost());
        assert_eq!(s.complete_total_usd(), None);
        assert_eq!(s.incomplete_attempts, 2);
        let reasons = s.reasons();
        assert!(reasons.contains(&AccountingReason::PartialUsage));
        assert!(reasons.contains(&AccountingReason::UsageUnavailable));
        // A later complete retry does not erase settled uncertainty.
        r.apply(&settled("ok", "c1")).unwrap();
        assert_eq!(r.session().complete_total_usd(), None);
        assert!(
            r.session()
                .reasons()
                .contains(&AccountingReason::PartialUsage)
        );
    }

    #[test]
    fn not_sent_and_not_billable_resolve_without_charge_or_uncertainty() {
        for disposition in [UsageDisposition::NotSent, UsageDisposition::NotBillable] {
            let mut r = reducer();
            r.apply(&start("a1", "c1", AttemptPurpose::MainResponse))
                .unwrap();
            r.apply(&settled_with(
                "a1",
                "c1",
                AttemptPurpose::MainResponse,
                |s| {
                    s.disposition = disposition;
                    s.outcome = AttemptOutcome::ProviderRejected;
                    s.usage = None;
                    s.valuation = value_usage(s.pricing.as_ref(), None);
                },
            ))
            .unwrap();
            let s = r.session();
            assert_eq!(s.unresolved_attempts, 0);
            assert_eq!(s.priced_attempts, 0);
            assert!(s.reasons().is_empty());
            assert_eq!(s.known_subtotal_usd, 0.0);
        }
    }

    #[test]
    fn session_cycle_and_purpose_scopes_are_separate_and_additive() {
        let mut r = reducer();
        r.apply(&settled("m1", "c1")).unwrap();
        r.apply(&settled_with(
            "k1",
            "c1",
            AttemptPurpose::Compaction,
            |_| {},
        ))
        .unwrap();
        r.apply(&settled("m2", "c2")).unwrap();

        approx(r.session().known_subtotal_usd, 3.0 * sample_cost());
        approx(r.cycle("c1").known_subtotal_usd, 2.0 * sample_cost());
        approx(r.cycle("c2").known_subtotal_usd, sample_cost());
        assert_eq!(r.cycle("unknown"), ScopeTotals::default());
        approx(
            r.purpose(AttemptPurpose::Compaction).known_subtotal_usd,
            sample_cost(),
        );
        approx(
            r.purpose(AttemptPurpose::MainResponse).known_subtotal_usd,
            2.0 * sample_cost(),
        );
    }

    #[test]
    fn purpose_token_totals_are_separate_and_sum_to_the_session() {
        let mut r = reducer();
        r.apply(&settled("m1", "c1")).unwrap();
        r.apply(&settled_with(
            "k1",
            "c1",
            AttemptPurpose::Compaction,
            |_| {},
        ))
        .unwrap();
        r.apply(&settled_with(
            "d1",
            "c1",
            AttemptPurpose::DelegationSummary,
            |_| {},
        ))
        .unwrap();
        let main = r.purpose(AttemptPurpose::MainResponse).tokens;
        let compaction = r.purpose(AttemptPurpose::Compaction).tokens;
        let summary = r.purpose(AttemptPurpose::DelegationSummary).tokens;
        // sample usage: 100 in, 10 out, 5 reasoning, 50 read, 20 write.
        for t in [main, compaction, summary] {
            assert_eq!(
                (t.input, t.output, t.reasoning, t.cache_read, t.cache_write),
                (100, 10, 5, 50, 20)
            );
        }
        assert_eq!(r.session().tokens.input, 300);
        assert_eq!(r.session().tokens.cache_write, 60);
    }

    #[test]
    fn cache_write_tier_detail_is_not_added_to_token_totals() {
        let mut r = reducer();
        r.apply(&settled_with(
            "a",
            "c1",
            AttemptPurpose::MainResponse,
            |s| {
                s.usage.as_mut().unwrap().cache_write_tiers = Some(querymt::CacheWriteTiers {
                    five_minute: Some(5),
                    one_hour: Some(15),
                });
            },
        ))
        .unwrap();
        assert_eq!(r.session().tokens.cache_write, 20);
    }

    #[test]
    fn delegated_child_generation_is_not_charged_to_the_parent() {
        // Parent records only its own summary; the child's generation is
        // recorded under the child session and rejected by the parent's reducer.
        let mut parent = AccountingReducer::new("parent");
        let mut child = AccountingReducer::new("child");
        let owned_by = |session: &str, id: &str, purpose| {
            let mut s = sample_settlement();
            s.attempt.session_id = session.into();
            s.attempt.attempt_id = id.into();
            s.attempt.purpose = purpose;
            AgentEventKind::LlmAttemptSettled { settlement: s }
        };
        parent
            .apply(&owned_by(
                "parent",
                "summary",
                AttemptPurpose::DelegationSummary,
            ))
            .unwrap();
        let child_event = owned_by("child", "child-main", AttemptPurpose::MainResponse);
        child.apply(&child_event).unwrap();
        // Remote/relay delivery of the child's record to the parent is refused.
        assert!(parent.apply(&child_event).is_err());

        assert_eq!(parent.session().priced_attempts, 1);
        assert_eq!(child.session().priced_attempts, 1);
        assert!((parent.session().known_subtotal_usd - sample_cost()).abs() < 1e-12);
        assert_eq!(
            parent.purpose(AttemptPurpose::MainResponse),
            ScopeTotals::default()
        );
    }

    #[test]
    fn delayed_settlement_charges_its_original_cycle() {
        let mut r = reducer();
        r.apply(&start("s1", "c1", AttemptPurpose::DelegationSummary))
            .unwrap();
        r.apply(&settled("m2", "c2")).unwrap();
        r.apply(&settled_with(
            "s1",
            "c1",
            AttemptPurpose::DelegationSummary,
            |_| {},
        ))
        .unwrap();
        assert_eq!(r.cycle("c1").priced_attempts, 1);
        assert_eq!(r.cycle("c1").unresolved_attempts, 0);
        assert_eq!(r.cycle("c2").priced_attempts, 1);
    }

    #[test]
    fn known_subtotal_never_decreases() {
        let mut r = reducer();
        let mut last = 0.0;
        let events = [
            start("a", "c", AttemptPurpose::MainResponse),
            settled("a", "c"),
            start("b", "c", AttemptPurpose::MainResponse),
            settled_with("b", "c", AttemptPurpose::MainResponse, |s| {
                s.disposition = UsageDisposition::Unavailable;
                s.usage = None;
                s.valuation = value_usage(s.pricing.as_ref(), None);
            }),
            settled("c", "c"),
        ];
        for e in &events {
            r.apply(e).unwrap();
            assert!(r.session().known_subtotal_usd >= last);
            last = r.session().known_subtotal_usd;
        }
    }

    #[test]
    fn budget_view_uses_its_own_snapshot_and_defaults_to_catalog() {
        let mut r = reducer();
        // No manual snapshot: budget view equals the catalog view.
        r.apply(&settled("a1", "c1")).unwrap();
        approx(r.session().budget_known_subtotal_usd, sample_cost());

        // Manual snapshot at a different rate, partial (no cache rates).
        let manual = querymt::providers::ModelPricing {
            input: Some(1.0),
            output: Some(2.0),
            ..Default::default()
        };
        r.apply(&settled_with(
            "a2",
            "c1",
            AttemptPurpose::MainResponse,
            |s| {
                s.budget_valuation = Some(value_usage(Some(&manual), s.usage.as_ref()));
            },
        ))
        .unwrap();
        let s = r.session();
        approx(s.known_subtotal_usd, 2.0 * sample_cost());
        approx(
            s.budget_known_subtotal_usd,
            sample_cost() + (100.0 + 15.0 * 2.0) / 1e6,
        );
        assert!(s.complete_total_usd().is_some(), "API view is unaffected");
        assert_eq!(
            s.budget_complete_total_usd(),
            None,
            "manual view lacks cache rates"
        );
        assert_eq!(s.budget_incomplete_attempts, 1);
    }

    #[test]
    fn replay_from_serialized_events_reproduces_live_totals() {
        let live_events = [
            start("a", "c1", AttemptPurpose::MainResponse),
            settled("a", "c1"),
            start("b", "c1", AttemptPurpose::Compaction),
        ];
        let mut live = reducer();
        for e in &live_events {
            live.apply(e).unwrap();
        }
        let mut replayed = reducer();
        for e in &live_events {
            let json = serde_json::to_string(e).unwrap();
            replayed
                .apply(&serde_json::from_str::<AgentEventKind>(&json).unwrap())
                .unwrap();
        }
        assert_eq!(live.session(), replayed.session());
        assert_eq!(live.cycle("c1"), replayed.cycle("c1"));
        assert_eq!(
            replayed.session().reasons(),
            vec![AccountingReason::UnresolvedAttempt]
        );
    }

    #[test]
    fn non_accounting_events_are_ignored() {
        let mut r = reducer();
        assert_eq!(
            r.apply(&AgentEventKind::SessionCreated),
            Ok(Applied::NotAccounting)
        );
        assert_eq!(r.attempt_count(), 0);
    }

    #[test]
    fn rebound_pricing_metadata_does_not_change_reduction() {
        let mut r = reducer();
        r.apply(&settled_with(
            "a1",
            "c1",
            AttemptPurpose::MainResponse,
            |s| {
                s.pricing_applicability = PricingApplicability::Rebound;
                s.pricing = Some(sample_pricing());
            },
        ))
        .unwrap();
        approx(r.session().known_subtotal_usd, sample_cost());
    }
}

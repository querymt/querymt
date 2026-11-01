//! Local delivery diagnostics.
//!
//! Counts how often automatic cards were matched, delivered, suppressed and
//! later fetched, with reasons, added tokens and selection latency. The
//! counters live on one [`RepositoryKnowledgeRegistry`](super::RepositoryKnowledgeRegistry)
//! (never in a process-global), in memory only:
//!
//! * nothing is written to repository files or shared statistics;
//! * no prompt text, card text, path or session identifier is recorded, only
//!   counts keyed by stage and a fixed reason vocabulary;
//! * "delivered" and "fetched" are independent observable events. Neither is
//!   evidence that the model followed the advice, and no such outcome exists.

use super::adapters::{INDEX_CONTEXT_HANDLER_ID, ORIENTATION_HANDLER_ID};
use super::budget::{DeliveryBudget, Refusal, estimate_tokens, is_native, serialized_len};
use crate::hooks::HookContextContribution;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Which automatic delivery stage produced a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Orientation,
    IndexContext,
}

impl Stage {
    /// The stage of a native handler id; `None` for foreign hook context.
    pub fn from_handler(handler_id: &str) -> Option<Self> {
        match handler_id {
            ORIENTATION_HANDLER_ID => Some(Self::Orientation),
            INDEX_CONTEXT_HANDLER_ID => Some(Self::IndexContext),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Orientation => "orientation",
            Self::IndexContext => "index_context",
        }
    }
}

/// Fixed reason vocabulary for a card that was matched but not delivered.
pub const fn reason_label(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::CardLimit => "card_limit",
        Refusal::TokenLimit => "token_limit",
        Refusal::ByteLimit => "byte_limit",
        Refusal::AlreadyDelivered => "already_delivered",
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Counters {
    matched: BTreeMap<Stage, u64>,
    delivered: BTreeMap<Stage, u64>,
    suppressed: BTreeMap<(Stage, &'static str), u64>,
    added_tokens: BTreeMap<Stage, u64>,
    latency_micros: BTreeMap<Stage, u64>,
    selections: BTreeMap<Stage, u64>,
    fetched: u64,
}

/// Point-in-time copy of the counters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiagnosticsSnapshot {
    counters: Counters,
}

impl DiagnosticsSnapshot {
    pub fn matched(&self, stage: Stage) -> u64 {
        self.counters.matched.get(&stage).copied().unwrap_or(0)
    }

    pub fn delivered(&self, stage: Stage) -> u64 {
        self.counters.delivered.get(&stage).copied().unwrap_or(0)
    }

    pub fn suppressed(&self, stage: Stage, reason: &str) -> u64 {
        self.counters
            .suppressed
            .iter()
            .filter(|((s, r), _)| *s == stage && *r == reason)
            .map(|(_, n)| *n)
            .sum()
    }

    pub fn suppressed_total(&self, stage: Stage) -> u64 {
        self.counters
            .suppressed
            .iter()
            .filter(|((s, _), _)| *s == stage)
            .map(|(_, n)| *n)
            .sum()
    }

    pub fn added_tokens(&self, stage: Stage) -> u64 {
        self.counters.added_tokens.get(&stage).copied().unwrap_or(0)
    }

    /// Total time spent selecting cards for `stage`.
    pub fn latency(&self, stage: Stage) -> Duration {
        Duration::from_micros(
            self.counters
                .latency_micros
                .get(&stage)
                .copied()
                .unwrap_or(0),
        )
    }

    pub fn selections(&self, stage: Stage) -> u64 {
        self.counters.selections.get(&stage).copied().unwrap_or(0)
    }

    pub fn fetched(&self) -> u64 {
        self.counters.fetched
    }

    /// True when nothing has been observed yet.
    pub fn is_empty(&self) -> bool {
        self.counters == Counters::default()
    }
}

impl fmt::Display for DiagnosticsSnapshot {
    /// Counts only. Matched, delivered, suppressed and fetched are reported as
    /// separate observations; there is deliberately no "followed" figure.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for stage in [Stage::Orientation, Stage::IndexContext] {
            write!(
                f,
                "{}: matched {}, delivered {}, suppressed {}, added tokens ~{}, selections {}, selection time {} ms",
                stage.as_str(),
                self.matched(stage),
                self.delivered(stage),
                self.suppressed_total(stage),
                self.added_tokens(stage),
                self.selections(stage),
                self.latency(stage).as_millis(),
            )?;
            let reasons: Vec<String> = self
                .counters
                .suppressed
                .iter()
                .filter(|((s, _), _)| *s == stage)
                .map(|((_, reason), n)| format!("{reason}={n}"))
                .collect();
            if !reasons.is_empty() {
                write!(f, " ({})", reasons.join(", "))?;
            }
            writeln!(f)?;
        }
        write!(f, "fetched (repo_knowledge_get calls): {}", self.fetched())
    }
}

/// Thread-safe, in-memory, local counters.
#[derive(Debug, Default)]
pub struct DeliveryDiagnostics {
    counters: Mutex<Counters>,
}

impl DeliveryDiagnostics {
    fn with<R>(&self, f: impl FnOnce(&mut Counters) -> R) -> R {
        f(&mut self.counters.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// A card was selected for delivery (before budget and duplicate checks).
    pub fn record_matched(&self, stage: Stage) {
        self.with(|c| *c.matched.entry(stage).or_default() += 1);
    }

    /// A card was included in the turn's context.
    pub fn record_delivered(&self, stage: Stage, tokens: usize) {
        self.with(|c| {
            *c.delivered.entry(stage).or_default() += 1;
            *c.added_tokens.entry(stage).or_default() += tokens as u64;
        });
    }

    /// A matched card was not delivered.
    pub fn record_suppressed(&self, stage: Stage, refusal: Refusal) {
        self.with(|c| {
            *c.suppressed
                .entry((stage, reason_label(refusal)))
                .or_default() += 1;
        });
    }

    /// One selection pass for `stage` took `elapsed`.
    pub fn record_selection(&self, stage: Stage, elapsed: Duration) {
        self.with(|c| {
            *c.selections.entry(stage).or_default() += 1;
            *c.latency_micros.entry(stage).or_default() +=
                u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        });
    }

    /// A successful explicit `repo_knowledge_get`. Independent of delivery.
    pub fn record_fetch(&self) {
        self.with(|c| c.fetched += 1);
    }

    pub fn snapshot(&self) -> DiagnosticsSnapshot {
        DiagnosticsSnapshot {
            counters: self.with(|c| c.clone()),
        }
    }
}

/// Charge `contributions` to the turn budget one at a time, in order, and
/// record each native card as matched and then delivered or suppressed (with
/// its reason). Foreign hook context passes through and is not counted.
/// Returns what the budget kept.
pub fn allocate_recorded(
    budget: &mut DeliveryBudget,
    diagnostics: &DeliveryDiagnostics,
    contributions: Vec<HookContextContribution>,
) -> Vec<HookContextContribution> {
    let mut kept_all = Vec::with_capacity(contributions.len());
    for contribution in contributions {
        let stage = is_native(&contribution)
            .then(|| Stage::from_handler(&contribution.handler_id))
            .flatten();
        if let Some(stage) = stage {
            diagnostics.record_matched(stage);
        }
        let (kept, refused) = budget.allocate(vec![contribution]);
        if let Some(stage) = stage {
            for card in &kept {
                diagnostics.record_delivered(stage, estimate_tokens(serialized_len(card)));
            }
            for refusal in refused {
                diagnostics.record_suppressed(stage, refusal);
            }
        }
        kept_all.extend(kept);
    }
    kept_all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepositoryKnowledgeDeliverySettings;

    fn native(handler: &str, id: &str) -> HookContextContribution {
        HookContextContribution {
            event_name: "user_prompt_submit".into(),
            handler_id: handler.into(),
            tool_use_id: None,
            content: format!("[repository-knowledge version=v card={id}]\nbody of {id}"),
        }
    }

    #[test]
    fn allocation_records_matched_delivered_and_suppressed_without_content() {
        let settings = RepositoryKnowledgeDeliverySettings {
            max_cards: 1,
            ..Default::default()
        };
        let mut budget = DeliveryBudget::new(&settings);
        let d = DeliveryDiagnostics::default();
        let foreign = HookContextContribution {
            event_name: "post_tool_use".into(),
            handler_id: "user-hook".into(),
            tool_use_id: None,
            content: "secret prompt text".into(),
        };
        let kept = allocate_recorded(
            &mut budget,
            &d,
            vec![
                native(ORIENTATION_HANDLER_ID, "a"),
                foreign,
                native(ORIENTATION_HANDLER_ID, "b"),
                native(INDEX_CONTEXT_HANDLER_ID, "a"),
            ],
        );
        assert_eq!(kept.len(), 2, "one native card plus the foreign context");
        let snap = d.snapshot();
        assert_eq!(snap.matched(Stage::Orientation), 2);
        assert_eq!(snap.delivered(Stage::Orientation), 1);
        assert_eq!(snap.suppressed(Stage::Orientation, "card_limit"), 1);
        assert_eq!(snap.matched(Stage::IndexContext), 1);
        assert_eq!(snap.delivered(Stage::IndexContext), 0);
        assert_eq!(snap.suppressed(Stage::IndexContext, "already_delivered"), 1);
        assert!(snap.added_tokens(Stage::Orientation) > 0);
        // Counts only: neither card text nor foreign content is retained.
        let rendered = format!("{snap} {snap:?}");
        assert!(!rendered.contains("secret prompt text"), "{rendered}");
        assert!(!rendered.contains("body of"), "{rendered}");
    }

    #[test]
    fn stages_come_only_from_native_handlers() {
        assert_eq!(
            Stage::from_handler(ORIENTATION_HANDLER_ID),
            Some(Stage::Orientation)
        );
        assert_eq!(
            Stage::from_handler(INDEX_CONTEXT_HANDLER_ID),
            Some(Stage::IndexContext)
        );
        assert_eq!(Stage::from_handler("user-hook"), None);
    }

    #[test]
    fn delivery_and_fetch_are_separate_events_with_no_followed_outcome() {
        let d = DeliveryDiagnostics::default();
        d.record_matched(Stage::Orientation);
        d.record_delivered(Stage::Orientation, 120);
        d.record_fetch();
        let snap = d.snapshot();
        assert_eq!(snap.matched(Stage::Orientation), 1);
        assert_eq!(snap.delivered(Stage::Orientation), 1);
        assert_eq!(snap.fetched(), 1);
        assert_eq!(snap.added_tokens(Stage::Orientation), 120);
        let text = snap.to_string();
        assert!(!text.to_lowercase().contains("follow"), "{text}");
    }

    #[test]
    fn suppression_keeps_stage_and_reason_and_never_counts_as_delivered() {
        let d = DeliveryDiagnostics::default();
        d.record_matched(Stage::IndexContext);
        d.record_suppressed(Stage::IndexContext, Refusal::ByteLimit);
        d.record_suppressed(Stage::IndexContext, Refusal::AlreadyDelivered);
        d.record_suppressed(Stage::IndexContext, Refusal::AlreadyDelivered);
        let snap = d.snapshot();
        assert_eq!(snap.delivered(Stage::IndexContext), 0);
        assert_eq!(snap.suppressed(Stage::IndexContext, "byte_limit"), 1);
        assert_eq!(snap.suppressed(Stage::IndexContext, "already_delivered"), 2);
        assert_eq!(snap.suppressed_total(Stage::IndexContext), 3);
        assert_eq!(snap.suppressed_total(Stage::Orientation), 0);
    }

    #[test]
    fn latency_accumulates_per_stage_and_instances_are_independent() {
        let a = DeliveryDiagnostics::default();
        let b = DeliveryDiagnostics::default();
        a.record_selection(Stage::Orientation, Duration::from_millis(3));
        a.record_selection(Stage::Orientation, Duration::from_millis(4));
        let snap = a.snapshot();
        assert_eq!(snap.selections(Stage::Orientation), 2);
        assert_eq!(snap.latency(Stage::Orientation), Duration::from_millis(7));
        assert!(b.snapshot().is_empty());
    }
}

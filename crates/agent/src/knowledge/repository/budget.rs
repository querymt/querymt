//! One shared per-turn budget for both automatic delivery stages.
//!
//! Orientation and index enrichment draw from the same allowance (cards,
//! estimated tokens, serialized bytes) instead of each receiving a full one.
//! Admission is first-come in a *stable* order chosen by the caller: orientation
//! first, then index cards in tool-call order (not completion order).
//!
//! * A card that does not fit is **omitted** whole and does not consume
//!   budget, so a later, smaller card may still fit.
//! * A card whose identity was already admitted this turn is not admitted
//!   again, whichever channel selected it.
//! * Sizes are measured on the serialized form the model receives (attribution
//!   wrapper included), so the ceiling holds for what is actually sent.

use super::adapters::{INDEX_CONTEXT_HANDLER_ID, ORIENTATION_HANDLER_ID, parse_mark, stamp_epoch};
use crate::config::RepositoryKnowledgeDeliverySettings;
use crate::hooks::HookContextContribution;
use std::collections::BTreeSet;

/// Why a card was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    CardLimit,
    TokenLimit,
    ByteLimit,
    AlreadyDelivered,
}

/// Whether a contribution was produced by a native delivery adapter. Only
/// those are subject to the budget; other hooks' context is never trimmed here.
pub fn is_native(contribution: &HookContextContribution) -> bool {
    is_native_handler(&contribution.handler_id)
}

/// Whether `handler_id` names a native delivery adapter. Persisted history is
/// only trusted for duplicate suppression when it carries one of these ids, so
/// a foreign hook that imitates a card marker cannot suppress real cards.
pub fn is_native_handler(handler_id: &str) -> bool {
    matches!(
        handler_id,
        ORIENTATION_HANDLER_ID | INDEX_CONTEXT_HANDLER_ID
    )
}

/// Bytes of the model-visible form of a contribution. Mirrors the
/// `<hook-context ...>` rendering used for stored hook-context parts.
pub fn serialized_len(contribution: &HookContextContribution) -> usize {
    let tool_label = contribution
        .tool_use_id
        .as_deref()
        .map(|id| format!(" tool_use_id={id}"))
        .unwrap_or_default();
    format!(
        "<hook-context event={} handler={}{tool_label}>\n{}\n</hook-context>",
        contribution.event_name, contribution.handler_id, contribution.content
    )
    .len()
}

/// Conservative token estimate (about four bytes per token, rounded up).
pub fn estimate_tokens(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

#[derive(Debug, Clone)]
pub struct DeliveryBudget {
    max_cards: usize,
    max_tokens: usize,
    max_bytes: usize,
    cards: usize,
    tokens: usize,
    bytes: usize,
    admitted: BTreeSet<String>,
    /// Card versions already represented in the session's effective history,
    /// rebuilt from persisted provenance at turn start.
    represented: BTreeSet<String>,
    epoch: u64,
    /// Whether `restore` has run for this budget, so history is read once per
    /// budget lifetime instead of once per tool batch.
    restored: bool,
}

impl DeliveryBudget {
    pub fn new(settings: &RepositoryKnowledgeDeliverySettings) -> Self {
        Self {
            max_cards: settings.effective_max_cards(),
            max_tokens: settings.effective_max_tokens(),
            max_bytes: settings.effective_max_bytes(),
            cards: 0,
            tokens: 0,
            bytes: 0,
            admitted: BTreeSet::new(),
            represented: BTreeSet::new(),
            epoch: 0,
            restored: false,
        }
    }

    /// Whether duplicate-suppression state was already rebuilt from history.
    pub fn is_restored(&self) -> bool {
        self.restored
    }

    /// Rebuild duplicate-suppression state from the effective history's
    /// persisted card provenance, and set the current compaction epoch.
    pub fn restore<'a>(
        &mut self,
        represented_contents: impl IntoIterator<Item = &'a str>,
        epoch: u64,
    ) {
        self.epoch = epoch;
        self.restored = true;
        self.represented.extend(
            represented_contents
                .into_iter()
                .filter_map(parse_mark)
                .map(|mark| mark.key()),
        );
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Try to admit one native contribution, charging the budget on success.
    pub fn admit(&mut self, contribution: &HookContextContribution) -> Result<(), Refusal> {
        let key = parse_mark(&contribution.content).map(|mark| mark.key());
        if key
            .as_ref()
            .is_some_and(|k| self.admitted.contains(k) || self.represented.contains(k))
        {
            return Err(Refusal::AlreadyDelivered);
        }
        if self.cards >= self.max_cards {
            return Err(Refusal::CardLimit);
        }
        let bytes = serialized_len(contribution);
        let tokens = estimate_tokens(bytes);
        if self.tokens + tokens > self.max_tokens {
            return Err(Refusal::TokenLimit);
        }
        if self.bytes + bytes > self.max_bytes {
            return Err(Refusal::ByteLimit);
        }
        self.cards += 1;
        self.tokens += tokens;
        self.bytes += bytes;
        if let Some(key) = key {
            self.admitted.insert(key);
        }
        Ok(())
    }

    /// Admit native contributions in order, passing everything else through.
    /// Returns the kept contributions and the refusals for the omitted ones.
    pub fn allocate(
        &mut self,
        contributions: Vec<HookContextContribution>,
    ) -> (Vec<HookContextContribution>, Vec<Refusal>) {
        let mut kept = Vec::with_capacity(contributions.len());
        let mut refused = Vec::new();
        for contribution in contributions {
            if !is_native(&contribution) {
                kept.push(contribution);
                continue;
            }
            match self.admit(&contribution) {
                Ok(()) => kept.push(HookContextContribution {
                    content: stamp_epoch(&contribution.content, self.epoch),
                    ..contribution
                }),
                Err(reason) => refused.push(reason),
            }
        }
        (kept, refused)
    }

    pub fn used(&self) -> (usize, usize, usize) {
        (self.cards, self.tokens, self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(
        handler: &str,
        id: &str,
        body_len: usize,
        tool: Option<&str>,
    ) -> HookContextContribution {
        HookContextContribution {
            event_name: "post_tool_use".into(),
            handler_id: handler.into(),
            tool_use_id: tool.map(str::to_string),
            content: format!(
                "[repository-knowledge version=v{body_len} card={id}]\n{}",
                "x".repeat(body_len)
            ),
        }
    }

    fn budget(cards: usize, tokens: usize, bytes: usize) -> DeliveryBudget {
        DeliveryBudget::new(&RepositoryKnowledgeDeliverySettings {
            max_cards: cards,
            max_tokens: tokens,
            max_bytes: bytes,
            ..Default::default()
        })
    }

    #[test]
    fn defaults_are_two_cards_600_tokens_and_4_kib() {
        let d = RepositoryKnowledgeDeliverySettings::default();
        assert_eq!(
            (
                d.effective_max_cards(),
                d.effective_max_tokens(),
                d.effective_max_bytes()
            ),
            (2, 600, 4096)
        );
    }

    #[test]
    fn the_card_limit_is_shared_across_stages() {
        let mut b = budget(2, 2000, 16 * 1024);
        assert!(
            b.admit(&card(ORIENTATION_HANDLER_ID, "o", 50, None))
                .is_ok()
        );
        assert!(
            b.admit(&card(INDEX_CONTEXT_HANDLER_ID, "a", 50, Some("1")))
                .is_ok()
        );
        assert_eq!(
            b.admit(&card(INDEX_CONTEXT_HANDLER_ID, "b", 50, Some("2"))),
            Err(Refusal::CardLimit)
        );
    }

    #[test]
    fn a_card_that_does_not_fit_is_omitted_and_costs_nothing() {
        let mut b = budget(4, 2000, 600);
        assert_eq!(
            b.admit(&card(INDEX_CONTEXT_HANDLER_ID, "big", 900, Some("1"))),
            Err(Refusal::ByteLimit)
        );
        assert_eq!(b.used(), (0, 0, 0));
        // A later, smaller card still fits, and the omitted one was never
        // recorded as delivered.
        assert!(
            b.admit(&card(INDEX_CONTEXT_HANDLER_ID, "small", 50, Some("2")))
                .is_ok()
        );
        assert!(
            b.admit(&card(INDEX_CONTEXT_HANDLER_ID, "big", 100, Some("3")))
                .is_ok()
        );
    }

    #[test]
    fn the_token_estimate_bounds_delivery() {
        let mut b = budget(4, 100, 16 * 1024);
        assert_eq!(
            b.admit(&card(ORIENTATION_HANDLER_ID, "t", 500, None)),
            Err(Refusal::TokenLimit)
        );
        assert!(
            b.admit(&card(ORIENTATION_HANDLER_ID, "t", 100, None))
                .is_ok()
        );
    }

    #[test]
    fn the_ceiling_counts_the_attribution_wrapper() {
        let c = card(INDEX_CONTEXT_HANDLER_ID, "w", 10, Some("call-1"));
        assert!(serialized_len(&c) > c.content.len() + INDEX_CONTEXT_HANDLER_ID.len());
        let mut b = budget(4, 2000, c.content.len());
        assert_eq!(b.admit(&c), Err(Refusal::ByteLimit));
    }

    #[test]
    fn the_same_card_is_admitted_once_per_turn_across_channels() {
        let mut b = budget(4, 2000, 16 * 1024);
        assert!(
            b.admit(&card(ORIENTATION_HANDLER_ID, "same", 20, None))
                .is_ok()
        );
        assert_eq!(
            b.admit(&card(INDEX_CONTEXT_HANDLER_ID, "same", 20, Some("1"))),
            Err(Refusal::AlreadyDelivered)
        );
    }

    #[test]
    fn restored_provenance_suppresses_the_same_version_only() {
        let mut b = budget(4, 2000, 16 * 1024);
        let old = card(INDEX_CONTEXT_HANDLER_ID, "n", 20, Some("1"));
        b.restore([old.content.as_str()], 3);
        assert_eq!(b.epoch(), 3);
        assert_eq!(
            b.admit(&card(ORIENTATION_HANDLER_ID, "n", 20, None)),
            Err(Refusal::AlreadyDelivered)
        );
        assert_eq!(b.used(), (0, 0, 0), "a suppressed card costs nothing");
        // Changed text is a new version and may be delivered.
        assert!(
            b.admit(&card(ORIENTATION_HANDLER_ID, "n", 25, None))
                .is_ok()
        );
    }

    #[test]
    fn allocation_follows_the_given_order_and_spares_foreign_context() {
        let mut b = budget(1, 2000, 16 * 1024);
        let foreign = HookContextContribution {
            event_name: "post_tool_use".into(),
            handler_id: "user-hook".into(),
            tool_use_id: None,
            content: "keep me".into(),
        };
        let (kept, refused) = b.allocate(vec![
            card(INDEX_CONTEXT_HANDLER_ID, "first", 20, Some("1")),
            foreign.clone(),
            card(INDEX_CONTEXT_HANDLER_ID, "second", 20, Some("2")),
        ]);
        assert_eq!(kept.len(), 2);
        assert!(kept[0].content.contains("card=first"));
        assert!(kept[0].content.contains("epoch=0"));
        assert_eq!(kept[1], foreign);
        assert_eq!(refused, vec![Refusal::CardLimit]);
    }
}

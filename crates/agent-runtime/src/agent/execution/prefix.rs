//! Cache prefix attribution.
//!
//! Every main model request records why its cache prefix differs from the
//! previous request's (`prefix_change`) and how long the session sat idle
//! (`idle_secs`). The previous request's fingerprint lives in session runtime
//! state, so the first request after a session is loaded reports
//! `first_request`.

use querymt::chat::ChatMessage;
use querymt_agent_core::hash::RapidHash;
use std::time::Instant;

pub(crate) use querymt_agent_core::events::CachePrefixChange as PrefixChange;

/// What the request-end event and the request span record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrefixAttribution {
    pub change: PrefixChange,
    /// Whole seconds since the previous main request ended; absent for the
    /// first request.
    pub idle_secs: Option<u64>,
}

/// The parts of a request that make up its cache prefix.
///
/// History is kept as one hash per durable message (serialized without cache
/// hints), so "the previous history is a prefix of this one" is a slice
/// comparison and the fingerprint costs eight bytes per message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrefixFingerprint {
    model: String,
    tools: RapidHash,
    system: RapidHash,
    history: Vec<RapidHash>,
}

impl PrefixFingerprint {
    pub(crate) fn new(
        provider: &str,
        model: &str,
        tools: RapidHash,
        system: RapidHash,
        durable_messages: &[ChatMessage],
    ) -> Self {
        Self {
            model: format!("{provider}/{model}"),
            tools,
            system,
            history: durable_messages.iter().map(message_hash).collect(),
        }
    }

    /// First difference from `previous`, in the order model, tools, system,
    /// history. Appended history is not a difference.
    fn change_from(&self, previous: &Self) -> PrefixChange {
        if self.model != previous.model {
            PrefixChange::Model
        } else if self.tools != previous.tools {
            PrefixChange::Tools
        } else if self.system != previous.system {
            PrefixChange::System
        } else if !self.history.starts_with(&previous.history) {
            PrefixChange::History
        } else {
            PrefixChange::None
        }
    }
}

/// Hash of one message as the provider sees it: the hint is not part of the
/// content, so moving the two cache breakpoints never reads as a change.
fn message_hash(message: &ChatMessage) -> RapidHash {
    let bytes = if message.cache.is_some() {
        let mut unhinted = message.clone();
        unhinted.cache = None;
        serde_json::to_vec(&unhinted)
    } else {
        serde_json::to_vec(message)
    }
    .unwrap_or_else(|error| error.to_string().into_bytes());
    RapidHash::new(&bytes)
}

/// Fingerprint of the last completed request plus the one being prepared.
///
/// A request becomes the reference only once it completed: a failed request
/// wrote nothing to the provider's cache.
#[derive(Debug, Default)]
pub(crate) struct CachePrefixState {
    committed: Option<(PrefixFingerprint, Instant)>,
    pending: Option<(PrefixFingerprint, PrefixAttribution)>,
}

impl CachePrefixState {
    /// Attribute the request being prepared at `now` and keep its fingerprint
    /// until [`commit`](Self::commit).
    pub(crate) fn begin(&mut self, next: PrefixFingerprint, now: Instant) -> PrefixAttribution {
        let attribution = match &self.committed {
            None => PrefixAttribution {
                change: PrefixChange::FirstRequest,
                idle_secs: None,
            },
            Some((previous, ended_at)) => PrefixAttribution {
                change: next.change_from(previous),
                idle_secs: Some(now.saturating_duration_since(*ended_at).as_secs()),
            },
        };
        self.pending = Some((next, attribution));
        attribution
    }

    /// Attribution of the request being prepared, if any.
    pub(crate) fn pending_attribution(&self) -> Option<PrefixAttribution> {
        self.pending.as_ref().map(|(_, attribution)| *attribution)
    }

    /// The prepared request completed at `ended_at`.
    pub(crate) fn commit(&mut self, ended_at: Instant) {
        if let Some((fingerprint, _)) = self.pending.take() {
            self.committed = Some((fingerprint, ended_at));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::CacheHint;
    use std::time::Duration;

    fn user(text: &str) -> ChatMessage {
        ChatMessage::user().text(text).build()
    }

    fn fingerprint(
        model: &str,
        tools: &[u8],
        system: &[u8],
        history: &[ChatMessage],
    ) -> PrefixFingerprint {
        PrefixFingerprint::new(
            "anthropic",
            model,
            RapidHash::new(tools),
            RapidHash::new(system),
            history,
        )
    }

    /// State after one completed request; returns the state and its end time.
    fn after_first(history: &[ChatMessage]) -> (CachePrefixState, Instant) {
        let mut state = CachePrefixState::default();
        let started = Instant::now();
        state.begin(fingerprint("m", b"t", b"s", history), started);
        let ended = started + Duration::from_secs(3);
        state.commit(ended);
        (state, ended)
    }

    #[test]
    fn first_request_has_no_idle() {
        let mut state = CachePrefixState::default();
        let a = state.begin(fingerprint("m", b"t", b"s", &[user("a")]), Instant::now());
        assert_eq!(a.change, PrefixChange::FirstRequest);
        assert_eq!(a.idle_secs, None);
    }

    #[test]
    fn unchanged_prefix_with_appended_history_is_none() {
        let first = [user("a"), user("b")];
        let (mut state, ended) = after_first(&first);
        let next = [user("a"), user("b"), user("c")];
        let a = state.begin(
            fingerprint("m", b"t", b"s", &next),
            ended + Duration::from_secs(12),
        );
        assert_eq!(a.change, PrefixChange::None);
        assert_eq!(a.idle_secs, Some(12));
    }

    #[test]
    fn long_wait_with_unchanged_prefix_is_none() {
        let first = [user("a")];
        let (mut state, ended) = after_first(&first);
        let a = state.begin(
            fingerprint("m", b"t", b"s", &first),
            ended + Duration::from_secs(1800),
        );
        assert_eq!(a.change, PrefixChange::None);
        assert_eq!(a.idle_secs, Some(1800));
    }

    #[test]
    fn model_switch_is_model() {
        let first = [user("a")];
        let (mut state, ended) = after_first(&first);
        let a = state.begin(fingerprint("other", b"t", b"s", &first), ended);
        assert_eq!(a.change, PrefixChange::Model);
    }

    #[test]
    fn provider_switch_is_model() {
        let first = [user("a")];
        let (mut state, ended) = after_first(&first);
        let switched = PrefixFingerprint::new(
            "openai",
            "m",
            RapidHash::new(b"t"),
            RapidHash::new(b"s"),
            &first,
        );
        assert_eq!(state.begin(switched, ended).change, PrefixChange::Model);
    }

    #[test]
    fn offered_tools_change_is_tools() {
        let first = [user("a")];
        let (mut state, ended) = after_first(&first);
        let a = state.begin(fingerprint("m", b"t+mcp", b"s", &first), ended);
        assert_eq!(a.change, PrefixChange::Tools);
    }

    #[test]
    fn system_prompt_change_is_system() {
        let first = [user("a")];
        let (mut state, ended) = after_first(&first);
        let a = state.begin(fingerprint("m", b"t", b"s2", &first), ended);
        assert_eq!(a.change, PrefixChange::System);
    }

    #[test]
    fn rewritten_history_is_history() {
        let first = [user("a"), user("b"), user("c")];
        let (mut state, ended) = after_first(&first);
        // Compaction replaced the older messages with a summary.
        let compacted = [user("summary"), user("c"), user("d")];
        let a = state.begin(fingerprint("m", b"t", b"s", &compacted), ended);
        assert_eq!(a.change, PrefixChange::History);
    }

    #[test]
    fn removed_history_is_history() {
        let first = [user("a"), user("b")];
        let (mut state, ended) = after_first(&first);
        let a = state.begin(fingerprint("m", b"t", b"s", &first[..1]), ended);
        assert_eq!(a.change, PrefixChange::History);
    }

    #[test]
    fn first_difference_wins_in_the_documented_order() {
        let first = [user("a")];
        let (mut state, ended) = after_first(&first);
        let all_changed = fingerprint("other", b"t2", b"s2", &[user("x")]);
        assert_eq!(state.begin(all_changed, ended).change, PrefixChange::Model);
        // `begin` replaced only the pending request; the reference is intact.
        let tools_and_history = fingerprint("m", b"t2", b"s", &[user("x")]);
        assert_eq!(
            state.begin(tools_and_history, ended).change,
            PrefixChange::Tools
        );
    }

    #[test]
    fn moving_cache_hints_is_not_a_change() {
        let mut hinted = user("b");
        hinted.cache = Some(CacheHint::Ephemeral { ttl_seconds: None });
        let (mut state, ended) = after_first(&[user("a"), hinted]);
        let a = state.begin(
            fingerprint("m", b"t", b"s", &[user("a"), user("b"), user("c")]),
            ended,
        );
        assert_eq!(a.change, PrefixChange::None);
    }

    #[test]
    fn failed_request_does_not_become_the_reference() {
        let first = [user("a")];
        let (mut state, ended) = after_first(&first);
        // A request that never completes: begun, not committed.
        state.begin(fingerprint("other", b"t", b"s", &first), ended);
        let a = state.begin(
            fingerprint("m", b"t", b"s", &first),
            ended + Duration::from_secs(5),
        );
        assert_eq!(a.change, PrefixChange::None);
        assert_eq!(a.idle_secs, Some(5));
    }

    #[test]
    fn commit_without_a_prepared_request_is_ignored() {
        let mut state = CachePrefixState::default();
        state.commit(Instant::now());
        assert_eq!(state.pending_attribution(), None);
        let a = state.begin(fingerprint("m", b"t", b"s", &[]), Instant::now());
        assert_eq!(a.change, PrefixChange::FirstRequest);
    }

    #[test]
    fn labels_match_the_spec() {
        let labels: Vec<_> = [
            PrefixChange::FirstRequest,
            PrefixChange::Model,
            PrefixChange::Tools,
            PrefixChange::System,
            PrefixChange::History,
            PrefixChange::None,
        ]
        .into_iter()
        .map(PrefixChange::as_str)
        .collect();
        assert_eq!(
            labels,
            [
                "first_request",
                "model",
                "tools",
                "system",
                "history",
                "none"
            ]
        );
    }
}

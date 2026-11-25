//! What a connection reads from a `session/load` snapshot while it answers the
//! load, decoded in one pass.
//!
//! The cut of the answer, the run the session is in and the history that
//! seeds the connection's translator used to be read by three separate walks
//! over the response's JSON. [`LoadSnapshotView`] reads them together, once,
//! and borrows the snapshot instead of cloning pieces of it.

use crate::acp::delivery::LoadCut;
use crate::acp::shared::SESSION_LOAD_SNAPSHOT_RESPONSE_KEY;
use crate::session::history_window::SessionHistorySummary;
use querymt_agent_core::events::AgentEvent;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Kinds of the snapshot's events that carry usage, compaction, model and
/// delegation state. The translator replays only these; the rest of a
/// (possibly large) window is never deserialized here.
const SEEDING_EVENT_KINDS: [&str; 10] = [
    "llm_request_end",
    "compaction_end",
    "provider_changed",
    "llm_attempt_started",
    "llm_attempt_settled",
    "delegation_requested",
    "session_forked",
    "delegation_completed",
    "delegation_failed",
    "delegation_cancelled",
];

/// The events of a snapshot that seed a translator.
#[derive(Debug, Default)]
pub(crate) struct SeedHistory {
    /// The seeding events that decoded, in snapshot order.
    pub(crate) events: Vec<AgentEvent>,
    /// An event of a seeding kind did not decode, so usage built from the
    /// rest is incomplete.
    pub(crate) partial: bool,
}

/// The parts of a load snapshot a connection reads.
#[derive(Debug)]
pub(crate) struct LoadSnapshotView {
    /// The last event the snapshot contains.
    pub(crate) cut: LoadCut,
    /// The run the session is in, per the snapshot's runtime status.
    pub(crate) active_run_id: Option<String>,
    /// The events that seed the translator; `None` when the snapshot carries
    /// no event list.
    pub(crate) history: Option<SeedHistory>,
    /// The whole-session summary, present only for a window that does not
    /// contain the complete history.
    pub(crate) summary: Option<SessionHistorySummary>,
    /// The snapshot's accounting reports history that was not transferred.
    pub(crate) untransferred_history: bool,
}

impl LoadSnapshotView {
    /// Reads the snapshot of a `session/load` response's `_meta`; `None` when
    /// the response carries none.
    pub(crate) fn from_meta(meta: Option<&Map<String, Value>>) -> Option<Self> {
        let snapshot = meta?.get(SESSION_LOAD_SNAPSHOT_RESPONSE_KEY)?;
        let local_stream_seq = snapshot
            .pointer("/cursor/local_seq")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let active_run_id = snapshot
            .pointer("/runtimeStatus/active_run_id")
            .and_then(Value::as_str)
            .map(str::to_owned);

        // One pass over the events: the newest `session_seq` of all of them,
        // and the decoded events the translator replays.
        let mut newest: Option<i64> = None;
        let history = snapshot
            .pointer("/audit/events")
            .and_then(Value::as_array)
            .map(|events| {
                let mut seed = SeedHistory::default();
                for event in events {
                    if let Some(seq) = event.get("session_seq").and_then(Value::as_i64) {
                        newest = newest.max(Some(seq));
                    }
                    let seeds = event
                        .pointer("/kind/type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| SEEDING_EVENT_KINDS.contains(&kind));
                    if !seeds {
                        continue;
                    }
                    match AgentEvent::deserialize(event) {
                        Ok(event) => seed.events.push(event),
                        Err(_) => seed.partial = true,
                    }
                }
                seed
            });

        let windowed = snapshot
            .pointer("/history/complete")
            .and_then(Value::as_bool)
            == Some(false);
        let summary = windowed
            .then(|| snapshot.get("summary"))
            .flatten()
            .and_then(|summary| SessionHistorySummary::deserialize(summary).ok());
        let untransferred_history = snapshot
            .pointer("/accounting/session/reasons")
            .and_then(Value::as_array)
            .is_some_and(|reasons| {
                reasons
                    .iter()
                    .any(|reason| reason["kind"] == "untransferred_history")
            });

        Some(Self {
            cut: LoadCut {
                session_seq: newest.unwrap_or(0),
                local_stream_seq,
            },
            active_run_id,
            history,
            summary,
            untransferred_history,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(snapshot: Value) -> Map<String, Value> {
        let mut meta = Map::new();
        meta.insert(SESSION_LOAD_SNAPSHOT_RESPONSE_KEY.to_owned(), snapshot);
        meta
    }

    fn event(session_seq: i64, kind: Value) -> Value {
        json!({
            "seq": session_seq,
            "session_seq": session_seq,
            "timestamp": 1,
            "session_id": "s",
            "kind": kind,
        })
    }

    fn compaction_end() -> Value {
        json!({ "type": "compaction_end", "data": { "summary": "s", "summary_len": 1 } })
    }

    fn prompt() -> Value {
        json!({ "type": "prompt_received", "data": { "content": "hi" } })
    }

    #[test]
    fn a_response_without_a_snapshot_has_no_view() {
        assert!(LoadSnapshotView::from_meta(None).is_none());
        assert!(LoadSnapshotView::from_meta(Some(&Map::new())).is_none());
    }

    #[test]
    fn the_cut_is_the_newest_event_of_every_kind() {
        let meta = meta(json!({
            "cursor": { "local_seq": 7 },
            "audit": { "events": [event(3, compaction_end()), event(9, prompt()), event(5, prompt())] },
        }));
        let view = LoadSnapshotView::from_meta(Some(&meta)).unwrap();
        assert_eq!(view.cut.session_seq, 9);
        assert_eq!(view.cut.local_stream_seq, 7);
    }

    #[test]
    fn only_seeding_kinds_are_decoded() {
        let meta = meta(json!({
            "audit": { "events": [event(1, prompt()), event(2, compaction_end())] },
        }));
        let history = LoadSnapshotView::from_meta(Some(&meta))
            .unwrap()
            .history
            .unwrap();
        assert_eq!(history.events.len(), 1);
        assert_eq!(history.events[0].session_seq, 2);
        assert!(!history.partial);
    }

    #[test]
    fn an_undecodable_seeding_event_makes_the_history_partial() {
        let broken = event(
            4,
            json!({ "type": "compaction_end", "data": { "summary_len": "no" } }),
        );
        let meta = meta(json!({ "audit": { "events": [broken, event(5, compaction_end())] } }));
        let view = LoadSnapshotView::from_meta(Some(&meta)).unwrap();
        let history = view.history.unwrap();
        assert!(history.partial);
        assert_eq!(history.events.len(), 1);
        // an event that does not decode still counts for the cut
        assert_eq!(view.cut.session_seq, 5);
    }

    #[test]
    fn a_snapshot_without_events_has_no_history_but_still_a_cut_and_run() {
        let meta = meta(json!({
            "cursor": { "local_seq": 2 },
            "runtimeStatus": { "active_run_id": "r1" },
        }));
        let view = LoadSnapshotView::from_meta(Some(&meta)).unwrap();
        assert!(view.history.is_none());
        assert_eq!(view.cut.session_seq, 0);
        assert_eq!(view.cut.local_stream_seq, 2);
        assert_eq!(view.active_run_id.as_deref(), Some("r1"));
    }

    #[test]
    fn a_run_id_is_absent_when_the_session_is_idle() {
        let meta = meta(json!({ "runtimeStatus": { "active_run_id": null } }));
        assert_eq!(
            LoadSnapshotView::from_meta(Some(&meta))
                .unwrap()
                .active_run_id,
            None
        );
    }

    #[test]
    fn the_summary_is_read_only_for_a_partial_window() {
        let summary = serde_json::to_value(SessionHistorySummary::default()).unwrap();
        for (complete, expected) in [(false, true), (true, false)] {
            let meta = meta(json!({
                "history": { "complete": complete },
                "summary": summary,
            }));
            let view = LoadSnapshotView::from_meta(Some(&meta)).unwrap();
            assert_eq!(view.summary.is_some(), expected, "complete: {complete}");
        }
        // a snapshot without a window has no `history` at all
        let meta = self::meta(json!({ "summary": summary }));
        assert!(
            LoadSnapshotView::from_meta(Some(&meta))
                .unwrap()
                .summary
                .is_none()
        );
    }

    #[test]
    fn untransferred_history_is_read_from_the_accounting_reasons() {
        let reasons = |kind: &str| {
            meta(json!({ "accounting": { "session": { "reasons": [{ "kind": kind }] } } }))
        };
        let flagged = reasons("untransferred_history");
        assert!(
            LoadSnapshotView::from_meta(Some(&flagged))
                .unwrap()
                .untransferred_history
        );
        let other = reasons("something_else");
        assert!(
            !LoadSnapshotView::from_meta(Some(&other))
                .unwrap()
                .untransferred_history
        );
    }
}

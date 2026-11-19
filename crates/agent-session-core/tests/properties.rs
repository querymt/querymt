//! Properties over generated traces (spec: Snapshot and live stream
//! equivalence, Deterministic output).
#![cfg(feature = "test-support")]

use proptest::prelude::*;
use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::input::SessionInput;
use querymt_agent_session_core::testing::{TraceSpec, generate};
use serde_json::{Value, json};

/// The whole view as JSON, with revisions zeroed: how many chunks carried a
/// text legitimately changes revisions, never content.
fn view_without_revisions(core: &SessionCore) -> Value {
    let ids = core.turn_ids();
    let mut turns = serde_json::to_value(core.turns(&ids)).expect("turns serialize");
    zero_revisions(&mut turns);
    json!({ "header": core.header(), "turns": turns })
}

fn zero_revisions(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(revision) = map.get_mut("revision") {
                *revision = json!(0);
            }
            map.values_mut().for_each(zero_revisions);
        }
        Value::Array(items) => items.iter_mut().for_each(zero_revisions),
        _ => {}
    }
}

fn chunk(text: &str) -> SessionInput {
    SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "agent_message_chunk", "messageId": "m1",
            "content": { "type": "text", "text": text } } }),
        replayed: false,
        at_ms: 0,
    }
}

/// `text` split into chunks of `size` characters.
fn chunks_of(text: &str, size: usize) -> Vec<SessionInput> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(size.max(1))
        .map(|piece| chunk(&piece.iter().collect::<String>()))
        .collect()
}

proptest! {
    #[test]
    fn chunk_size_does_not_change_the_view(
        text in "[a-zA-Z0-9 ,.é日]{1,300}",
        a in 1usize..70,
        b in 1usize..70,
    ) {
        let mut first = SessionCore::new("s1");
        first.apply(&chunks_of(&text, a));
        let mut second = SessionCore::new("s1");
        second.apply(&chunks_of(&text, b));
        prop_assert_eq!(view_without_revisions(&first), view_without_revisions(&second));
    }

    #[test]
    fn repeated_reduction_is_identical(seed in any::<u64>(), turns in 1usize..12) {
        let trace = generate(TraceSpec { turns, ..TraceSpec::small(seed) });
        let response = snapshot_response(&trace.events);
        let run = || {
            let mut core = SessionCore::new("s1");
            let report = core.apply(&[SessionInput::SnapshotLoaded { response: response.clone() }]);
            let ids = core.turn_ids();
            serde_json::to_string(&(core.header(), core.turns(&ids), report)).expect("serialize")
        };
        prop_assert_eq!(run(), run());
    }
}

fn snapshot_response(events: &[Value]) -> Value {
    json!({ "_meta": { "querymt/sessionLoadSnapshot.v1": {
        "runtimeStatus": { "phase": "idle", "active_run_id": null, "steerable": false,
            "pending_steering_count": 0, "queued_input_count": 0, "run_started_at_ms": null,
            "observation": { "availability": "live" } },
        "audit": { "session_id": "s1", "events": events, "tasks": [], "intent_snapshots": [],
            "decisions": [], "progress_entries": [], "artifacts": [], "delegations": [],
            "generated_at": "2026-01-01T00:00:00Z" },
        "cursor": { "local_seq": 0, "remote_seq_by_source": {} },
        "accounting": { "version": 1, "session": { "knownSubtotalUsd": 0.0,
            "completeTotalUsd": 0.0, "complete": true, "billing": "empty", "reasons": [],
            "pricedAttempts": 0, "excludedAttempts": 0, "unresolvedAttempts": 0,
            "incompleteAttempts": 0, "unaccountedGenerations": 0 }, "cycle": null },
        "delegationUpdates": []
    } } })
}

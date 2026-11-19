//! Fixture builders shared by the integration tests.
#![allow(dead_code)]

use querymt_agent_session_core::input::SessionInput;
use serde_json::{Value, json};

pub fn event(seq: i64, kind: &str, data: Value) -> Value {
    json!({
        "seq": seq, "session_seq": seq, "timestamp": 1000 + seq, "session_id": "s1",
        "origin": "local", "kind": { "type": kind, "data": data }
    })
}

pub fn prompt(seq: i64, id: &str, text: &str) -> Value {
    event(
        seq,
        "prompt_received",
        json!({ "content": text, "message_id": id }),
    )
}

pub fn reply(seq: i64, id: &str, text: &str) -> Value {
    event(
        seq,
        "assistant_message_stored",
        json!({ "content": text, "message_id": id }),
    )
}

pub fn tool_start(seq: i64, id: &str) -> Value {
    event(
        seq,
        "tool_call_start",
        json!({ "tool_call_id": id, "tool_name": "shell", "arguments": "{\"cmd\":\"ls\"}" }),
    )
}

pub fn tool_end(seq: i64, id: &str, result: &str) -> Value {
    event(
        seq,
        "tool_call_end",
        json!({ "tool_call_id": id, "tool_name": "shell", "is_error": false, "result": result }),
    )
}

pub fn scope(complete: bool, total: Option<f64>) -> Value {
    json!({ "knownSubtotalUsd": total.unwrap_or(0.0), "completeTotalUsd": total,
            "complete": complete, "billing": "metered", "reasons": [],
            "pricedAttempts": 0, "excludedAttempts": 0, "unresolvedAttempts": 0,
            "incompleteAttempts": 0, "unaccountedGenerations": 0 })
}

pub fn runtime(phase: &str) -> Value {
    json!({ "phase": phase, "active_run_id": null, "steerable": false,
            "pending_steering_count": 0, "queued_input_count": 0, "run_started_at_ms": null,
            "observation": { "availability": "live" } })
}

/// A load response carrying a snapshot of `events`; `extra` merges into the snapshot.
pub fn load_response(events: Vec<Value>, extra: Value) -> Value {
    let mut snapshot = json!({
        "runtimeStatus": runtime("idle"),
        "audit": { "session_id": "s1", "events": events, "tasks": [], "intent_snapshots": [],
                   "decisions": [], "progress_entries": [], "artifacts": [], "delegations": [],
                   "generated_at": "2026-01-01T00:00:00Z" },
        "cursor": { "local_seq": 0, "remote_seq_by_source": {} },
        "accounting": { "version": 1, "session": scope(true, Some(0.0)), "cycle": null },
        "delegationUpdates": []
    });
    if let (Some(base), Some(more)) = (snapshot.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            base.insert(k.clone(), v.clone());
        }
    }
    json!({ "_meta": { "querymt/sessionLoadSnapshot.v1": snapshot } })
}

pub fn loaded(response: Value) -> SessionInput {
    SessionInput::SnapshotLoaded { response }
}

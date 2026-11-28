//! Standard ACP context compaction (spec: Standard ACP context compaction).

use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::entry::{CompactionEntry, CompactionState, EntryKind};
use querymt_agent_session_core::input::SessionInput;
use serde_json::{Value, json};

fn live(update: Value) -> SessionInput {
    SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": update }),
        replayed: false,
        at_ms: 0,
    }
}

fn user(id: &str, text: &str) -> SessionInput {
    live(json!({
        "sessionUpdate": "user_message_chunk",
        "messageId": id,
        "content": { "type": "text", "text": text }
    }))
}

fn compaction(id: &str, status: &str) -> SessionInput {
    live(json!({ "sessionUpdate": "compaction_update", "compactionId": id, "status": status }))
}

fn summary_chunk(id: &str, text: &str) -> SessionInput {
    live(json!({
        "sessionUpdate": "compaction_summary_chunk",
        "compactionId": id,
        "content": { "type": "text", "text": text }
    }))
}

fn compactions(core: &SessionCore) -> Vec<(usize, CompactionEntry)> {
    let mut found = Vec::new();
    for (index, id) in core.turn_ids().iter().enumerate() {
        let turn = core.turn(id).expect("turn exists");
        for entry in turn.entries {
            if let EntryKind::Compaction(c) = entry.kind {
                found.push((index, c));
            }
        }
    }
    found
}

#[test]
fn first_compaction_update_fixes_the_timeline_position() {
    let mut core = SessionCore::new("s1");
    core.apply(&[user("u1", "a"), user("u2", "b"), user("u3", "c")]);
    core.apply(&[compaction("c1", "in_progress")]);
    core.apply(&[user("u4", "d"), user("u5", "e")]);
    core.apply(&[compaction("c1", "completed")]);
    let found = compactions(&core);
    assert_eq!(found.len(), 1);
    // Turns 0..=2 are the first three prompts; the boundary is turn index 3.
    assert_eq!(found[0].0, 3);
    assert_eq!(found[0].1.state, CompactionState::Completed);
}

#[test]
fn summary_chunks_append_and_create_a_compaction() {
    let mut core = SessionCore::new("s1");
    core.apply(&[summary_chunk("c9", "one "), summary_chunk("c9", "two")]);
    let found = compactions(&core);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1.state, CompactionState::InProgress);
    assert_eq!(found[0].1.summary.len(), 2);
}

#[test]
fn terminal_summary_replaces_streamed_chunks() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        compaction("c1", "in_progress"),
        summary_chunk("c1", "the "),
        summary_chunk("c1", "whole "),
        summary_chunk("c1", "text"),
    ]);
    assert_eq!(compactions(&core)[0].1.summary.len(), 3);
    core.apply(&[live(json!({
        "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "completed",
        "summary": [{ "type": "text", "text": "the whole text" }]
    }))]);
    let found = compactions(&core);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1.state, CompactionState::Completed);
    // One block, equal to what a load snapshot of the same session holds.
    assert_eq!(
        serde_json::to_value(&found[0].1.summary).unwrap(),
        json!([{ "type": "text", "text": "the whole text" }])
    );
}

#[test]
fn compaction_patch_semantics() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        compaction("c1", "in_progress"),
        summary_chunk("c1", "kept"),
        live(
            json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                     "status": "in_progress", "error": "boom" }),
        ),
    ]);
    // Omitted summary leaves the stored value.
    core.apply(&[compaction("c1", "in_progress")]);
    assert_eq!(compactions(&core)[0].1.summary.len(), 1);
    // null error clears it.
    core.apply(&[live(
        json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                              "status": "in_progress", "error": null }),
    )]);
    assert_eq!(compactions(&core)[0].1.error, None);
    // An empty summary clears the summary.
    core.apply(&[live(
        json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                              "status": "in_progress", "summary": [] }),
    )]);
    assert!(compactions(&core)[0].1.summary.is_empty());
}

#[test]
fn unknown_compaction_status_is_not_completed() {
    let mut core = SessionCore::new("s1");
    core.apply(&[compaction("c1", "_vendor_paused")]);
    let entry = &compactions(&core)[0].1;
    assert_eq!(entry.state, CompactionState::Other);
    assert_eq!(entry.other_status.as_deref(), Some("_vendor_paused"));
}

#[test]
fn settled_compaction_is_not_reopened() {
    let mut core = SessionCore::new("s1");
    core.apply(&[compaction("c1", "completed")]);
    core.apply(&[live(json!({
        "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "in_progress",
        "summary": [{ "type": "text", "text": "new" }]
    }))]);
    let entry = &compactions(&core)[0].1;
    assert_eq!(entry.state, CompactionState::Completed);
    assert_eq!(entry.summary.len(), 1);
}

mod common;

/// The agent's replay stamps the enclosing run on a compaction's `_meta`; a
/// snapshot builds the same entry, so a reload does not change it.
#[test]
fn snapshot_compaction_carries_the_run_id() {
    use common::{event, load_response, loaded};
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(
        vec![
            event(
                1,
                "run_started",
                json!({ "run_id": "r1", "origin": "interactive" }),
            ),
            event(
                2,
                "compaction_start",
                json!({ "token_estimate": 1, "compaction_id": "c1" }),
            ),
            event(
                3,
                "compaction_end",
                json!({ "summary": "short", "summary_len": 5, "compaction_id": "c1",
                        "context_tokens": null }),
            ),
            event(
                4,
                "run_completed",
                json!({ "run_id": "r1", "outcome": "completed", "stop_type": null,
                        "reason": null }),
            ),
        ],
        json!({}),
    ))]);
    let found = compactions(&core);
    assert_eq!(found.len(), 1);
    let meta = found[0].1.meta.as_ref().expect("the run id is stamped");
    assert_eq!(meta["querymt"]["runId"], "r1");
}

/// A request end reporting `context_tokens`, as the journal stores it.
fn request_end(seq: i64, context_tokens: u64) -> Value {
    common::event(
        seq,
        "llm_request_end",
        json!({ "usage": null, "tool_calls": 0, "finish_reason": null, "cost_usd": null,
                "cumulative_cost_usd": null, "context_tokens": context_tokens,
                "metrics": { "steps": 1, "turns": 1 } }),
    )
}

/// The `_meta.querymt.compaction` counts of the only compaction in `core`.
fn counts(core: &SessionCore) -> Value {
    let found = compactions(core);
    assert_eq!(found.len(), 1);
    found[0]
        .1
        .meta
        .as_ref()
        .and_then(|meta| meta.get("querymt"))
        .and_then(|querymt| querymt.get("compaction"))
        .cloned()
        .unwrap_or(Value::Null)
}

fn compaction_with_counts(id: &str, status: &str, counts: Value) -> SessionInput {
    live(json!({
        "sessionUpdate": "compaction_update", "compactionId": id, "status": status,
        "_meta": { "querymt": { "compaction": counts } }
    }))
}

#[test]
fn compaction_counts_after_a_reload() {
    use common::{event, load_response, loaded, prompt};

    // Watched live: the agent sends the counts on both updates.
    let mut live_core = SessionCore::new("s1");
    live_core.apply(&[
        user("u1", "hi"),
        compaction_with_counts("c1", "in_progress", json!({ "tokensBefore": 150000 })),
        compaction_with_counts(
            "c1",
            "completed",
            json!({ "tokensBefore": 150000, "tokensAfter": 20000 }),
        ),
    ]);

    // Loaded: the same events as a snapshot.
    let mut loaded_core = SessionCore::new("s1");
    loaded_core.apply(&[loaded(load_response(
        vec![
            prompt(1, "u1", "hi"),
            request_end(2, 150000),
            event(
                3,
                "compaction_start",
                json!({ "token_estimate": 90000, "compaction_id": "c1" }),
            ),
            event(
                4,
                "compaction_end",
                json!({ "summary": "short", "summary_len": 5, "compaction_id": "c1",
                        "context_tokens": 20000 }),
            ),
        ],
        json!({}),
    ))]);

    let expected = json!({ "tokensBefore": 150000, "tokensAfter": 20000 });
    assert_eq!(counts(&live_core), expected);
    assert_eq!(counts(&loaded_core), expected);
}

#[test]
fn no_measurement_before_the_compaction_uses_the_estimate() {
    use common::{event, load_response, loaded, prompt};
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(
        vec![
            prompt(1, "u1", "hi"),
            event(
                2,
                "compaction_start",
                json!({ "token_estimate": 90000, "compaction_id": "c1" }),
            ),
            event(
                3,
                "compaction_end",
                json!({ "summary": "short", "summary_len": 5, "compaction_id": "c1",
                        "context_tokens": null }),
            ),
        ],
        json!({}),
    ))]);
    // No count is known after the compaction: it is omitted, not zero.
    assert_eq!(counts(&core), json!({ "tokensBefore": 90000 }));
}

#[test]
fn a_later_update_keeps_the_counts_an_earlier_one_set() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        user("u1", "hi"),
        compaction_with_counts("c1", "in_progress", json!({ "tokensBefore": 150000 })),
        // Only the new count, and another key beside the compaction one.
        live(json!({
            "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "completed",
            "_meta": { "querymt": { "compaction": { "tokensAfter": 20000 }, "runId": "r1" } }
        })),
    ]);
    assert_eq!(
        counts(&core),
        json!({ "tokensBefore": 150000, "tokensAfter": 20000 })
    );
    let found = compactions(&core);
    let meta = found[0].1.meta.as_ref().expect("meta");
    assert_eq!(meta["querymt"]["runId"], "r1");
}

#[test]
fn a_second_compaction_is_measured_from_the_first_ones_result() {
    use common::{event, load_response, loaded, prompt};
    // The context a finished compaction reports is a measurement: the next
    // compaction starts from it when no request ran in between.
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(
        vec![
            prompt(1, "u1", "hi"),
            request_end(2, 150000),
            event(
                3,
                "compaction_start",
                json!({ "token_estimate": 1, "compaction_id": "c1" }),
            ),
            event(
                4,
                "compaction_end",
                json!({ "summary": "one", "summary_len": 3, "compaction_id": "c1",
                        "context_tokens": 20000 }),
            ),
            event(
                5,
                "compaction_start",
                json!({ "token_estimate": 1, "compaction_id": "c2" }),
            ),
            event(
                6,
                "compaction_end",
                json!({ "summary": "two", "summary_len": 3, "compaction_id": "c2",
                        "context_tokens": 9000 }),
            ),
        ],
        json!({}),
    ))]);
    let found = compactions(&core);
    assert_eq!(found.len(), 2);
    let second = found[1].1.meta.as_ref().expect("meta");
    assert_eq!(
        second["querymt"]["compaction"],
        json!({ "tokensBefore": 20000, "tokensAfter": 9000 })
    );
}

#[test]
fn compaction_keeps_its_record_through_later_updates() {
    use querymt_agent_protocol_types::events::{
        SlashCommandEffect, SlashCommandRecord, SlashCommandSource,
    };
    let mut core = SessionCore::new("s1");
    core.apply(&[
        live(json!({
            "sessionUpdate": "compaction_update",
            "compactionId": "k1",
            "status": "in_progress",
            "_meta": { "querymt": { "slashCommand": {
                "name": "compact",
                "arguments": "keep the API decisions",
                "source": "builtin",
                "effect": "action",
                "clientPromptId": "c2"
            } } }
        })),
        compaction("k1", "completed"),
    ]);
    let found = compactions(&core);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1.state, CompactionState::Completed);
    assert_eq!(
        found[0].1.slash_command,
        Some(SlashCommandRecord {
            name: "compact".into(),
            arguments: "keep the API decisions".into(),
            source: SlashCommandSource::Builtin,
            effect: SlashCommandEffect::Action,
            client_prompt_id: Some("c2".into()),
        })
    );

    // A compaction the agent started on its own has none.
    let mut auto = SessionCore::new("s1");
    auto.apply(&[compaction("k2", "in_progress")]);
    assert_eq!(compactions(&auto)[0].1.slash_command, None);
}

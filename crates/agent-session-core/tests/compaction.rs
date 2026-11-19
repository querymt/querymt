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

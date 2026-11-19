//! Replayed updates applied once (spec: Replayed updates applied once).

use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::entry::EntryKind;
use querymt_agent_session_core::input::SessionInput;
use serde_json::{Value, json};

fn note(update: Value, replayed: bool) -> SessionInput {
    SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": update }),
        replayed,
        at_ms: 0,
    }
}

fn text_chunk(text: &str) -> Value {
    json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m1",
            "content": { "type": "text", "text": text } })
}

#[test]
fn drained_duplicate_is_ignored() {
    let mut core = SessionCore::new("s1");
    let update = json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1",
                         "status": "completed" });
    core.apply(&[note(update.clone(), false)]);
    let before = core
        .turn_ids()
        .iter()
        .map(|id| core.turn(id))
        .collect::<Vec<_>>();
    let report = core.apply(&[note(update, true)]);
    assert_eq!(report.duplicates, 1);
    assert!(report.added.is_empty() && report.updated.is_empty() && !report.header_changed);
    let after = core
        .turn_ids()
        .iter()
        .map(|id| core.turn(id))
        .collect::<Vec<_>>();
    assert_eq!(before, after);
}

#[test]
fn repeated_live_chunk_text_is_kept() {
    let mut core = SessionCore::new("s1");
    core.apply(&[note(text_chunk("ab"), false), note(text_chunk("ab"), false)]);
    let id = core.turn_ids().pop().expect("turn");
    let text = core
        .turn(&id)
        .expect("turn")
        .entries
        .into_iter()
        .find_map(|e| match e.kind {
            EntryKind::AssistantMessage(m) => Some(m.text),
            _ => None,
        });
    assert_eq!(text.as_deref(), Some("abab"));
}

#[test]
fn debug_log_keeps_the_newest_entries() {
    let mut core = SessionCore::new("s1");
    let batch: Vec<_> = (0..1_500)
        .map(|n| note(text_chunk(&format!("c{n}")), false))
        .collect();
    core.apply(&batch);
    let log = core.debug_log();
    assert_eq!(log.entries.len(), 1_000);
    assert_eq!(log.dropped, 500);
    assert_eq!(log.entries[0].message_id.as_deref(), Some("m1"));
}

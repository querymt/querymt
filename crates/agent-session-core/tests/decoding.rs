//! Tolerant input handling (spec: Tolerant input handling, Live ACP updates).

use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::input::SessionInput;
use serde_json::{Value, json};

fn live(notification: Value) -> SessionInput {
    SessionInput::AcpNotification {
        notification,
        replayed: false,
        at_ms: 0,
    }
}

fn note(session: &str, update: Value) -> Value {
    json!({ "sessionId": session, "update": update })
}

fn chunk(session: &str, text: &str) -> Value {
    note(
        session,
        json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": "m1",
            "content": { "type": "text", "text": text }
        }),
    )
}

#[test]
fn unknown_update_kind_is_skipped() {
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[live(note("s1", json!({ "sessionUpdate": "zzz_new" })))]);
    assert_eq!(report.unknown, 1);
    assert!(report.errors.is_empty());
    assert!(report.added.is_empty() && report.updated.is_empty());
    assert!(!report.header_changed);
    assert!(core.turn_ids().is_empty());
}

#[test]
fn malformed_input_is_rejected() {
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[
        live(note("s1", json!("not an object"))),
        live(chunk("s1", "hello")),
    ]);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].input_index, 0);
    // The valid chunk after the bad input is still applied.
    assert_eq!(core.turn_ids().len(), 1);
}

#[test]
fn update_for_another_session_is_ignored() {
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[live(chunk("other", "hello"))]);
    assert_eq!(report.ignored, 1);
    assert!(report.added.is_empty());
    assert!(core.turn_ids().is_empty());
}

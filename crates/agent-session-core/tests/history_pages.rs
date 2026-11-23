//! Older history pages (spec: Older history pages).

mod common;

use common::*;
use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::entry::EntryKind;
use querymt_agent_session_core::input::SessionInput;
use serde_json::{Value, json};

fn page(
    events: Vec<Value>,
    complete: bool,
    before: Option<&str>,
    prompts: Option<Value>,
) -> SessionInput {
    let mut response = json!({
        "events": events,
        "history": { "complete": complete, "before": before },
        "truncated": []
    });
    if let Some(p) = prompts {
        response["userPrompts"] = p;
    }
    SessionInput::HistoryPage { response }
}

fn windowed(events: Vec<Value>) -> SessionInput {
    loaded(load_response(
        events,
        json!({ "history": { "complete": false, "before": "t1" } }),
    ))
}

#[test]
fn older_page_is_prepended() {
    let mut core = SessionCore::new("s1");
    core.apply(&[windowed(vec![
        prompt(20, "u20", "x"),
        reply(21, "a20", "y"),
    ])]);
    let existing = core.turn_ids();
    let older: Vec<_> = (0..10)
        .flat_map(|n| {
            let s = 1 + n * 2;
            vec![
                prompt(s, &format!("u{s}"), "q"),
                reply(s + 1, &format!("a{s}"), "r"),
            ]
        })
        .collect();
    let report = core.apply(&[page(older, true, None, None)]);
    let ids = core.turn_ids();
    assert_eq!(ids.len(), 11);
    assert_eq!(&ids[10..], existing.as_slice());
    assert_eq!(report.added.len(), 10);
    assert!(report.updated.is_empty());
    assert!(!core.header().history.has_older);
}

#[test]
fn prepending_during_a_streaming_turn_keeps_live_state() {
    let mut core = SessionCore::new("s1");
    core.apply(&[windowed(vec![prompt(20, "u20", "x")])]);
    let live = |t: &str| SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": { "sessionUpdate": "agent_message_chunk",
            "messageId": "live", "content": { "type": "text", "text": t } } }),
        replayed: false,
        at_ms: 0,
    };
    core.apply(&[live("ab")]);
    core.apply(&[page(
        vec![prompt(1, "u1", "q"), reply(2, "a1", "r")],
        true,
        None,
        None,
    )]);
    core.apply(&[live("cd")]);
    let last = core.turn_ids().pop().expect("turn");
    let text = core
        .turn(&last)
        .expect("turn")
        .entries
        .into_iter()
        .find_map(|e| match e.kind {
            EntryKind::AssistantMessage(m) => Some(m.text),
            _ => None,
        });
    assert_eq!(text.as_deref(), Some("abcd"));
    assert_eq!(core.turn_ids().len(), 2);
}

#[test]
fn overlapping_page_events_are_not_duplicated() {
    let mut core = SessionCore::new("s1");
    core.apply(&[windowed(vec![prompt(5, "u5", "x"), reply(6, "a5", "y")])]);
    core.apply(&[page(
        vec![
            prompt(3, "u3", "q"),
            prompt(5, "u5", "x"),
            reply(6, "a5", "y"),
        ],
        true,
        None,
        None,
    )]);
    assert_eq!(core.turn_ids(), vec!["turn-u3", "turn-u5"]);
    let users = core
        .turns(&core.turn_ids())
        .iter()
        .flat_map(|t| t.entries.clone())
        .filter(|e| matches!(e.kind, EntryKind::UserMessage(_)))
        .count();
    assert_eq!(users, 2);
}

#[test]
fn older_page_restores_prompt_attachments() {
    let mut core = SessionCore::new("s1");
    core.apply(&[windowed(vec![prompt(20, "u20", "x")])]);
    let prompts = json!([{ "messageId": "u1", "messageOrder": 0, "timestamp": 1, "blocks": [
        { "type": "text", "text": "look" },
        { "type": "image", "data": "AAAA", "mimeType": "image/png" } ] }]);
    core.apply(&[page(
        vec![prompt(1, "u1", "look")],
        true,
        None,
        Some(prompts),
    )]);
    let first = core.turn_ids().remove(0);
    let blocks = core
        .turn(&first)
        .expect("turn")
        .entries
        .into_iter()
        .find_map(|e| match e.kind {
            EntryKind::UserMessage(u) => Some(u.blocks),
            _ => None,
        })
        .expect("user");
    assert_eq!(blocks.len(), 2);
}

//! Borrowed and owned reads (spec: Native Rust library / Borrowed and owned
//! reads agree).

mod common;

use common::*;
use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::input::SessionInput;
use querymt_agent_session_core::testing::{
    TraceSpec, generate, live, snapshot_response, text_chunk,
};
use serde_json::json;

fn long_session() -> SessionCore {
    let trace = generate(TraceSpec::scaled(7, 4));
    let mut core = SessionCore::new("s1");
    core.apply(&[SessionInput::SnapshotLoaded {
        response: snapshot_response(&trace.events, serde_json::json!({})),
    }]);
    core.apply(&[
        text_chunk("m-live", "lorem "),
        text_chunk("m-live", "ipsum"),
    ]);
    core
}

#[test]
fn borrowed_and_owned_reads_agree() {
    let core = long_session();
    let ids = core.turn_ids();
    assert!(ids.len() > 10, "expected a long session, got {}", ids.len());

    let order: Vec<&str> = core.turn_order().collect();
    assert_eq!(core.turn_order().len(), ids.len());
    assert_eq!(order, ids.iter().map(String::as_str).collect::<Vec<_>>());

    let mut entries_seen = 0;
    for id in &ids {
        let owned = core.turn(id).expect("owned turn");
        let borrowed = core.turn_ref(id).expect("borrowed turn");
        assert_eq!(borrowed.id(), owned.id);
        assert_eq!(borrowed.revision(), owned.revision);
        let borrowed_entries: Vec<_> = borrowed.entries().cloned().collect();
        assert_eq!(borrowed_entries, owned.entries);
        for entry in &owned.entries {
            assert_eq!(core.entry(&entry.id), Some(entry), "entry {}", entry.id);
            entries_seen += 1;
        }
    }
    assert!(entries_seen > ids.len());
    assert!(core.turn_ref("no-such-turn").is_none());
    assert!(core.entry("no-such-entry").is_none());
}

#[test]
fn entry_index_follows_removal() {
    // An optimistic prompt replaced by the agent's echo leaves no stale entry.
    let mut core = SessionCore::new("s1");
    core.apply(&[SessionInput::PromptOptimistic {
        client_prompt_id: "c1".to_owned(),
        blocks: Vec::new(),
        at_ms: 1,
    }]);
    assert!(core.entry("user-pending-c1").is_some());
    core.apply(&[live(serde_json::json!({
        "sessionUpdate": "user_message_chunk",
        "messageId": "m1",
        "content": { "type": "text", "text": "hi" },
        "_meta": { "querymt": { "client_prompt_id": "c1" } }
    }))]);
    assert!(core.entry("user-pending-c1").is_none());
    assert!(core.entry("user-m1").is_some());
    assert!(core.turn_ref("pending-c1").is_none());
}

#[test]
fn entry_index_follows_an_older_page() {
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(
        vec![prompt(20, "u20", "x"), reply(21, "a20", "y")],
        json!({ "history": { "complete": false, "before": "t1" } }),
    ))]);
    assert!(core.entry("user-u20").is_some());
    assert!(core.entry("user-u1").is_none());
    core.apply(&[SessionInput::HistoryPage {
        response: json!({
            "events": [prompt(1, "u1", "q"), reply(2, "a1", "r")],
            "history": { "complete": true, "before": null },
            "truncated": []
        }),
    }]);
    // Old and new entries are both found, and each sits in the turn it was read from.
    for id in core.turn_order() {
        let turn = core.turn_ref(id).expect("turn");
        for entry in turn.entries() {
            assert_eq!(core.entry(&entry.id), Some(entry));
        }
    }
    assert!(core.entry("user-u1").is_some());
    assert!(core.entry("assistant-a1").is_some());
    assert!(core.entry("user-u20").is_some());
}

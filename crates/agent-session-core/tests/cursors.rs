//! Session cursors (spec: Session cursors, Cursors across loads).

mod common;

use common::{prompt, reply};
use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::input::SessionInput;
use querymt_agent_session_core::testing::{live, snapshot_response};
use serde_json::json;

fn agent_message(id: &str) -> SessionInput {
    live(json!({
        "sessionUpdate": "agent_message_chunk", "messageId": id,
        "content": { "type": "text", "text": "text" }
    }))
}

fn cursor(id: &str, extra: serde_json::Value) -> SessionInput {
    let mut update = json!({ "sessionUpdate": "cursor_update", "cursorId": id });
    for (key, value) in extra.as_object().expect("fields") {
        update[key] = value.clone();
    }
    live(update)
}

fn after_agent_message(id: &str) -> serde_json::Value {
    json!({ "type": "agent_message", "messageId": id, "position": "after" })
}

#[test]
fn cursor_is_placed_at_its_anchor() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        agent_message("a1"),
        cursor(
            "cur-1",
            json!({ "anchor": after_agent_message("a1"), "capabilities": { "fork": {} } }),
        ),
    ]);
    let cursors = &core.header().cursors;
    assert_eq!(cursors.len(), 1);
    assert_eq!(cursors[0].cursor_id, "cur-1");
    assert!(
        cursors[0]
            .capabilities
            .as_ref()
            .is_some_and(|c| c.get("fork").is_some()),
        "{:?}",
        cursors[0].capabilities
    );
    let placement = cursors[0].placement.as_ref().expect("placed");
    assert_eq!(placement.entry_id, "assistant-a1");
    assert_eq!(placement.position, "after");
}

#[test]
fn capabilities_are_patched_and_the_anchor_is_kept() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        agent_message("a1"),
        agent_message("a2"),
        cursor(
            "cur-1",
            json!({ "anchor": after_agent_message("a1"), "capabilities": { "fork": {} } }),
        ),
    ]);
    // Clears the capabilities and names another anchor.
    core.apply(&[cursor(
        "cur-1",
        json!({ "anchor": after_agent_message("a2"), "capabilities": null }),
    )]);
    let held = &core.header().cursors[0];
    assert_eq!(held.capabilities, None);
    assert_eq!(held.anchor["messageId"], "a1", "the first anchor is kept");
    assert_eq!(
        held.placement.as_ref().expect("placed").entry_id,
        "assistant-a1"
    );

    // Omitted capabilities keep what is held.
    core.apply(&[cursor_fork("cur-1")]);
    core.apply(&[cursor("cur-1", json!({}))]);
    assert!(core.header().cursors[0].capabilities.is_some());
}

fn cursor_fork(id: &str) -> SessionInput {
    cursor(id, json!({ "capabilities": { "fork": {} } }))
}

#[test]
fn cursor_without_its_entity_is_unplaced() {
    let mut core = SessionCore::new("s1");
    core.apply(&[cursor(
        "cur-1",
        json!({ "anchor": after_agent_message("missing") }),
    )]);
    let cursors = &core.header().cursors;
    assert_eq!(cursors.len(), 1, "the cursor is kept");
    assert_eq!(cursors[0].placement, None, "but not placed by guess");

    // The entity arrives later: now it is placed.
    core.apply(&[agent_message("missing")]);
    assert!(core.header().cursors[0].placement.is_some());
}

#[test]
fn a_cursor_without_a_first_anchor_is_skipped() {
    let mut core = SessionCore::new("s1");
    core.apply(&[cursor("cur-1", json!({ "capabilities": { "fork": {} } }))]);
    assert!(core.header().cursors.is_empty());
}

#[test]
fn load_rebuilds_cursors() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        agent_message("a0"),
        cursor("old-1", json!({ "anchor": after_agent_message("a0") })),
        cursor("old-2", json!({ "anchor": after_agent_message("a0") })),
        cursor("old-3", json!({ "anchor": after_agent_message("a0") })),
    ]);
    assert_eq!(core.header().cursors.len(), 3);

    let events = vec![prompt(1, "u1", "q"), reply(2, "a1", "r")];
    let response = snapshot_response(
        &events,
        json!({ "cursorUpdates": [{
            "cursorId": "new-1",
            "anchor": after_agent_message("a1"),
            "capabilities": { "fork": {} },
        }] }),
    );
    let report = core.apply(&[SessionInput::SnapshotLoaded { response }]);
    assert!(report.errors.is_empty(), "{report:?}");
    let cursors = &core.header().cursors;
    assert_eq!(cursors.len(), 1);
    assert_eq!(cursors[0].cursor_id, "new-1");
    assert_eq!(
        cursors[0].placement.as_ref().expect("placed").entry_id,
        "assistant-a1"
    );
}

#[test]
fn an_older_page_adds_its_cursors() {
    let mut core = SessionCore::new("s1");
    core.apply(&[SessionInput::SnapshotLoaded {
        response: snapshot_response(
            &[prompt(3, "u2", "q"), reply(4, "a2", "r")],
            json!({
                "history": { "complete": false, "before": "tok" },
                "cursorUpdates": [{ "cursorId": "kept", "anchor": after_agent_message("a2") }],
            }),
        ),
    }]);
    let page = json!({
        "events": [prompt(1, "u1", "q"), reply(2, "a1", "r")],
        "history": { "complete": true },
        "truncated": [],
        "cursorUpdates": [{ "cursorId": "older", "anchor": after_agent_message("a1") }],
    });
    let report = core.apply(&[SessionInput::HistoryPage { response: page }]);
    assert!(report.errors.is_empty(), "{report:?}");
    let ids: Vec<_> = core
        .header()
        .cursors
        .iter()
        .map(|c| c.cursor_id.as_str())
        .collect();
    assert_eq!(ids, ["kept", "older"]);
    assert!(core.header().cursors.iter().all(|c| c.placement.is_some()));
}

#[test]
fn a_cursor_update_that_does_not_decode_is_reported_and_skipped() {
    let mut core = SessionCore::new("s1");
    let response = snapshot_response(
        &[],
        json!({ "cursorUpdates": [{ "anchor": "nonsense" }, { "cursorId": "ok",
            "anchor": after_agent_message("a1") }] }),
    );
    let report = core.apply(&[SessionInput::SnapshotLoaded { response }]);
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert_eq!(core.header().cursors.len(), 1);
}

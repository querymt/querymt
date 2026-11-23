//! Subagents (spec: Subagent update patches the child, Tool call links a
//! child session, Snapshot restores subagents).
#![cfg(feature = "test-support")]

use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::entry::EntryKind;
use querymt_agent_session_core::input::SessionInput;
use querymt_agent_session_core::testing::{live, snapshot_response};
use serde_json::json;

fn subagent_update(fields: serde_json::Value) -> SessionInput {
    let mut update = json!({ "sessionUpdate": "subagent_update", "sessionId": "child-1" });
    for (key, value) in fields.as_object().expect("fields") {
        update[key] = value.clone();
    }
    live(update)
}

fn child_of(core: &SessionCore, tool_call_id: &str) -> Option<String> {
    core.turn_ids()
        .into_iter()
        .filter_map(|id| core.turn(&id))
        .flat_map(|turn| turn.entries)
        .find_map(|entry| match entry.kind {
            EntryKind::ToolCall(call) if call.tool_call_id == tool_call_id => call.child_session_id,
            _ => None,
        })
}

#[test]
fn subagent_update_patches_the_child() {
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[subagent_update(json!({
        "title": "Verifier",
        "description": "checks the diff",
        "state": { "state": "running" },
        "_meta": { "querymt": { "stage": "verifying" } },
    }))]);
    assert!(report.header_changed, "{report:?}");
    assert!(report.errors.is_empty(), "{report:?}");

    let subagents = &core.header().subagents;
    assert_eq!(subagents.len(), 1);
    let child = &subagents[0];
    assert_eq!(child.session_id, "child-1");
    assert_eq!(child.title.as_deref(), Some("Verifier"));
    assert_eq!(child.description.as_deref(), Some("checks the diff"));
    assert_eq!(child.state.as_deref(), Some("running"));
    assert_eq!(child.stage.as_deref(), Some("verifying"));

    // A patch keeps what it omits and replaces the state as a whole.
    core.apply(&[subagent_update(json!({ "state": { "state": "idle" } }))]);
    let child = &core.header().subagents[0];
    assert_eq!(core.header().subagents.len(), 1, "merged by session id");
    assert_eq!(
        child.title.as_deref(),
        Some("Verifier"),
        "omitted is unchanged"
    );
    assert_eq!(child.state.as_deref(), Some("idle"));
    assert_eq!(child.stage, None, "the idle snapshot carries no stage");

    // `null` clears a field.
    core.apply(&[subagent_update(json!({ "title": null, "state": null }))]);
    let child = &core.header().subagents[0];
    assert_eq!(child.title, None);
    assert_eq!(child.state, None);
    assert_eq!(child.description.as_deref(), Some("checks the diff"));
}

#[test]
fn subagents_keep_the_order_they_first_appeared_in() {
    let mut core = SessionCore::new("s1");
    let note = |id: &str| {
        live(json!({ "sessionUpdate": "subagent_update", "sessionId": id, "title": id }))
    };
    core.apply(&[note("b"), note("a"), note("b")]);
    let ids: Vec<_> = core
        .header()
        .subagents
        .iter()
        .map(|s| s.session_id.as_str())
        .collect();
    assert_eq!(ids, ["b", "a"]);
}

#[test]
fn tool_call_links_a_child_session() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        subagent_update(json!({ "title": "Verifier" })),
        live(json!({
            "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "delegate",
            "content": [{ "type": "session", "sessionId": "child-1" }]
        })),
    ]);
    assert_eq!(child_of(&core, "t1").as_deref(), Some("child-1"));

    // A tool call update that adds the reference links as well, and a later
    // update of other fields keeps the link.
    core.apply(&[
        live(json!({ "sessionUpdate": "tool_call", "toolCallId": "t2", "title": "delegate" })),
        live(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "t2",
            "content": [{ "type": "session", "sessionId": "child-2" }]
        })),
        live(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "completed"
        })),
    ]);
    assert_eq!(child_of(&core, "t2").as_deref(), Some("child-2"));
}

#[test]
fn snapshot_restores_subagents() {
    let mut core = SessionCore::new("s1");
    // A subagent seen live is replaced by what the snapshot lists.
    core.apply(&[subagent_update(json!({ "title": "stale" }))]);
    let response = snapshot_response(
        &[],
        json!({ "subagentUpdates": [
            { "sessionId": "child-1", "title": "Verifier", "state": { "state": "running" },
              "_meta": { "querymt": { "stage": "verifying" } } },
            { "sessionId": "child-2", "title": "Coder", "state": { "state": "idle" } },
        ] }),
    );
    let report = core.apply(&[SessionInput::SnapshotLoaded { response }]);
    assert!(report.errors.is_empty(), "{report:?}");

    let subagents = &core.header().subagents;
    assert_eq!(subagents.len(), 2);
    assert_eq!(subagents[0].session_id, "child-1");
    assert_eq!(subagents[0].stage.as_deref(), Some("verifying"));
    assert_eq!(subagents[1].state.as_deref(), Some("idle"));
}

#[test]
fn snapshot_delegation_updates_are_ignored() {
    // An agent that still reports the removed key: no subagents, no error,
    // everything else loads.
    let mut core = SessionCore::new("s1");
    let response = snapshot_response(
        &[],
        json!({ "delegationUpdates": [{ "version": 1, "delegationId": "d1" }] }),
    );
    let report = core.apply(&[SessionInput::SnapshotLoaded { response }]);
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(core.header().subagents.is_empty());
}

#[test]
fn snapshot_without_subagent_updates_loads() {
    let mut response = snapshot_response(&[], json!({}));
    response["_meta"]["querymt/sessionLoadSnapshot.v1"]
        .as_object_mut()
        .expect("snapshot")
        .remove("subagentUpdates");
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[SessionInput::SnapshotLoaded { response }]);
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(core.header().subagents.is_empty());
}

#[test]
fn a_subagent_update_that_does_not_decode_is_reported_and_skipped() {
    let mut core = SessionCore::new("s1");
    let response = snapshot_response(
        &[],
        json!({ "subagentUpdates": [
            { "title": "no session id" },
            { "sessionId": "child-1", "title": "kept" },
        ] }),
    );
    let report = core.apply(&[SessionInput::SnapshotLoaded { response }]);
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert!(report.errors[0].message.contains("subagentUpdates[0]"));
    assert_eq!(core.header().subagents.len(), 1);
}

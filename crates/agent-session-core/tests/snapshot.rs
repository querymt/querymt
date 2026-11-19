//! Session view from a load snapshot (spec: Session view from a load snapshot).

mod common;

use common::*;
use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::entry::EntryKind;

#[test]
fn full_snapshot_hydrates_the_conversation() {
    let events = vec![
        prompt(1, "u1", "one"),
        reply(2, "a1", "r1"),
        tool_start(3, "t1"),
        tool_end(4, "t1", "ok"),
        prompt(5, "u2", "two"),
        reply(6, "a2", "r2"),
        tool_start(7, "t2"),
        tool_end(8, "t2", "ok"),
        prompt(9, "u3", "three"),
        reply(10, "a3", "r3"),
        tool_start(11, "t3"),
        tool_end(12, "t3", "ok"),
        tool_start(13, "t4"),
        tool_end(14, "t4", "ok"),
    ];
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[loaded(load_response(events, serde_json::json!({})))]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let ids = core.turn_ids();
    assert_eq!(ids, vec!["turn-1", "turn-5", "turn-9"]);
    let turns = core.turns(&ids);
    let tools: Vec<usize> = turns
        .iter()
        .map(|t| {
            t.entries
                .iter()
                .filter(|e| matches!(&e.kind, EntryKind::ToolCall(c) if c.status == "completed"))
                .count()
        })
        .collect();
    assert_eq!(tools, vec![1, 1, 2]);
    assert!(!core.header().history.has_older);
}

fn tool_of(core: &SessionCore) -> querymt_agent_session_core::entry::ToolCallEntry {
    core.turn_ids()
        .iter()
        .flat_map(|id| core.turn(id).expect("turn").entries)
        .find_map(|e| match e.kind {
            EntryKind::ToolCall(c) => Some(c),
            _ => None,
        })
        .expect("a tool call")
}

#[test]
fn snapshot_with_an_idle_runtime_keeps_unfinished_tool_status() {
    let events = vec![prompt(1, "u1", "go"), tool_start(2, "t1")];
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(events, serde_json::json!({})))]);
    assert_eq!(tool_of(&core).status, "in_progress");
    assert_eq!(core.header().active_tool_call_id, None);
}

#[test]
fn snapshot_accounting_sets_cost() {
    let extra = serde_json::json!({
        "accounting": { "version": 1, "session": scope(true, Some(0.42)), "cycle": null }
    });
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(vec![prompt(1, "u1", "x")], extra))]);
    let cost = core.header().cost.clone().expect("cost");
    assert_eq!(
        (cost.amount, cost.currency.as_str(), cost.complete),
        (0.42, "USD", true)
    );
}

#[test]
fn truncated_result_is_marked() {
    let extra = serde_json::json!({
        "truncated": [{ "tool_call_id": "t1", "part": "result", "full_bytes": 400000 }]
    });
    let events = vec![
        prompt(1, "u1", "x"),
        tool_start(2, "t1"),
        tool_end(3, "t1", "pre"),
    ];
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(events, extra))]);
    assert_eq!(tool_of(&core).result_full_bytes, Some(400000));
}

#[test]
fn windowed_snapshot_takes_whole_session_state_from_the_summary() {
    let extra = serde_json::json!({
        "history": { "complete": false, "before": "tok-1" },
        "summary": {
            "usage": { "latestRequest": null,
                       "sessionTotals": { "inputTokens": 9, "outputTokens": 1, "reasoningTokens": 0,
                                          "cacheReadTokens": 0, "cacheWriteTokens": 0 },
                       "partial": true, "requestCount": 3,
                       "accounting": serde_json::to_value(
                           querymt_agent_protocol_types::accounting::ScopeTotals::default()).unwrap() },
            "provider": { "provider": "p", "model": "m", "contextLimit": 1000, "position": 1 },
            "pendingInputs": [],
            "childSessions": []
        }
    });
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[loaded(load_response(vec![prompt(5, "u1", "x")], extra))]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let h = core.header();
    assert!(h.history.has_older);
    assert_eq!(h.history.before.as_deref(), Some("tok-1"));
    assert_eq!(h.provider.as_ref().map(|p| p.model.as_str()), Some("m"));
    let tokens = h.tokens.clone().expect("tokens");
    assert!(tokens.partial);
    assert_eq!(tokens.session_totals.expect("totals").input_tokens, 9);
}

#[test]
fn bad_event_inside_a_snapshot_is_skipped() {
    let mut events: Vec<_> = (1..=200)
        .map(|n| prompt(n, &format!("u{n}"), "x"))
        .collect();
    events[99] = event(
        100,
        "tool_call_start",
        serde_json::json!({ "tool_call_id": 5 }),
    );
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[loaded(load_response(events, serde_json::json!({})))]);
    assert_eq!(core.turn_ids().len(), 199);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].seq, Some(100));
}

fn replayed_turn(n: usize) -> Vec<querymt_agent_session_core::input::SessionInput> {
    use querymt_agent_session_core::input::SessionInput;
    let note = |update: serde_json::Value| SessionInput::AcpNotification {
        notification: serde_json::json!({ "sessionId": "s1", "update": update,
                                          "_meta": { "querymtReplay": true } }),
        replayed: true,
        at_ms: 0,
    };
    vec![
        note(
            serde_json::json!({ "sessionUpdate": "user_message_chunk", "messageId": format!("u{n}"),
            "content": { "type": "text", "text": "q" } }),
        ),
        note(
            serde_json::json!({ "sessionUpdate": "agent_message_chunk", "messageId": format!("a{n}"),
            "content": { "type": "text", "text": "r" } }),
        ),
    ]
}

#[test]
fn replayed_updates_before_a_load_response_without_a_snapshot_are_kept() {
    let mut core = SessionCore::new("s1");
    let mut batch = replayed_turn(1);
    batch.extend(replayed_turn(2));
    core.apply(&batch);
    let response = serde_json::json!({
        "configOptions": [{ "id": "model", "name": "model", "type": "select",
                            "currentValue": "m", "options": [] }],
        "modes": { "currentModeId": "build", "availableModes": [] }
    });
    core.apply(&[loaded(response)]);
    assert_eq!(core.turn_ids().len(), 2);
    assert_eq!(core.header().config_options.len(), 1);
    assert_eq!(core.header().current_mode.as_deref(), Some("build"));
}

#[test]
fn snapshot_replaces_replayed_turns() {
    let mut core = SessionCore::new("s1");
    let mut batch = replayed_turn(1);
    batch.extend(replayed_turn(2));
    core.apply(&batch);
    let events = vec![
        prompt(1, "u1", "q"),
        reply(2, "a1", "r"),
        prompt(3, "u2", "q"),
        reply(4, "a2", "r"),
    ];
    let report = core.apply(&[loaded(load_response(events, serde_json::json!({})))]);
    assert_eq!(core.turn_ids(), vec!["turn-1", "turn-3"]);
    assert_eq!(
        report.removed.len(),
        2,
        "replayed live-id turns are replaced"
    );
    let texts: Vec<usize> = core
        .turns(&core.turn_ids())
        .iter()
        .map(|t| t.entries.len())
        .collect();
    assert_eq!(texts, vec![2, 2]);
}

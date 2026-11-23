//! Live ACP updates (spec: Live ACP session updates, Change reports, Turn entries).

use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::entry::EntryKind;
use querymt_agent_session_core::input::SessionInput;
use serde_json::{Value, json};

fn live(update: Value) -> SessionInput {
    SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": update }),
        replayed: false,
        at_ms: 0,
    }
}

fn chunk(message_id: &str, text: &str) -> SessionInput {
    live(json!({
        "sessionUpdate": "agent_message_chunk",
        "messageId": message_id,
        "content": { "type": "text", "text": text }
    }))
}

fn tool_call(id: &str, status: &str) -> SessionInput {
    live(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": id,
        "title": "Run shell",
        "status": status
    }))
}

fn tool_update(id: &str, status: &str, output: &str) -> SessionInput {
    live(json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": status,
        "rawOutput": output
    }))
}

#[test]
fn streamed_chunks_merge_into_one_assistant_message() {
    let mut core = SessionCore::new("s1");
    let batch: Vec<_> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|t| chunk("m1", t))
        .collect();
    core.apply(&batch);
    let ids = core.turn_ids();
    let turn = core.turn(ids.last().expect("a turn")).expect("turn exists");
    assert_eq!(turn.entries.len(), 1);
    match &turn.entries[0].kind {
        EntryKind::AssistantMessage(message) => assert_eq!(message.text, "abcde"),
        other => panic!("expected assistant message, got {other:?}"),
    }
}

#[test]
fn tool_call_update_replaces_the_tool_state() {
    let mut core = SessionCore::new("s1");
    core.apply(&[tool_call("t1", "in_progress")]);
    core.apply(&[tool_update("t1", "completed", "done")]);
    let ids = core.turn_ids();
    let turn = core.turn(ids.last().expect("a turn")).expect("turn exists");
    let tools: Vec<_> = turn
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].tool_call_id, "t1");
    assert_eq!(tools[0].status, "completed");
    assert_eq!(tools[0].raw_output, Some(json!("done")));
}

fn thought(message_id: &str, part_id: &str, text: &str) -> SessionInput {
    live(json!({
        "sessionUpdate": "agent_thought_chunk",
        "messageId": message_id,
        "content": { "type": "text", "text": text },
        "_meta": { "querymt": { "reasoning_part_id": part_id } }
    }))
}

#[test]
fn reasoning_parts_stay_separate() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        thought("m1", "p0", "one "),
        thought("m1", "p0", "two"),
        thought("m1", "p1", "three"),
    ]);
    let ids = core.turn_ids();
    let turn = core.turn(ids.last().expect("a turn")).expect("turn exists");
    let parts: Vec<_> = turn
        .entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Reasoning(part) => Some((part.part_id.as_str(), part.text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(parts, vec![("p0", "one two"), ("p1", "three")]);
}

fn plan_update(id: &str, entries: usize) -> SessionInput {
    let items: Vec<Value> = (0..entries)
        .map(|n| json!({ "content": format!("step {n}"), "priority": "medium", "status": "pending" }))
        .collect();
    live(json!({
        "sessionUpdate": "plan_update",
        "plan": { "type": "items", "planId": id, "entries": items }
    }))
}

#[test]
fn plan_update_replaces_and_plan_removal_clears() {
    let mut core = SessionCore::new("s1");
    core.apply(&[plan_update("p1", 3)]);
    assert_eq!(
        core.header().plan.as_ref().map(|p| p.entries.len()),
        Some(3)
    );
    let report = core.apply(&[plan_update("p1", 2)]);
    assert!(report.header_changed);
    assert_eq!(
        core.header().plan.as_ref().map(|p| p.entries.len()),
        Some(2)
    );
    core.apply(&[live(
        json!({ "sessionUpdate": "plan_removed", "planId": "p1" }),
    )]);
    assert!(core.header().plan.is_none());
}

fn usage(used: u64, size: u64, meta: Value) -> SessionInput {
    live(json!({ "sessionUpdate": "usage_update", "used": used, "size": size, "_meta": meta }))
}

#[test]
fn usage_update_replaces_context_usage() {
    let mut core = SessionCore::new("s1");
    core.apply(&[usage(1, 10, json!({}))]);
    core.apply(&[usage(12000, 200000, json!({}))]);
    let context = core.header().context.clone().expect("context usage");
    assert_eq!((context.used, context.size), (12000, 200000));
}

#[test]
fn model_change_hides_stale_context_live() {
    let mut core = SessionCore::new("s1");
    core.apply(&[usage(12000, 200000, json!({}))]);
    assert!(core.header().context.is_some());
    core.apply(&[live(json!({
        "sessionUpdate": "session_info_update",
        "_meta": { "querymt": { "contextInvalidated": true } }
    }))]);
    assert_eq!(core.header().context, None);
    // The signal changes neither the title nor anything else.
    assert_eq!(core.header().title, None);
    core.apply(&[usage(3000, 100000, json!({}))]);
    let context = core.header().context.clone().expect("context usage");
    assert_eq!((context.used, context.size), (3000, 100000));
}

#[test]
fn model_change_names_the_new_model_live() {
    let mut core = SessionCore::new("s1");
    assert!(core.header().provider.is_none());
    core.apply(&[live(json!({
        "sessionUpdate": "session_info_update",
        "_meta": { "querymt": { "contextInvalidated": true,
            "model": { "provider": "mock", "model": "mock-2", "node": "n1",
                       "contextLimit": 100000 } } }
    }))]);
    let provider = core.header().provider.clone().expect("the new model");
    assert_eq!(
        (provider.provider.as_str(), provider.model.as_str()),
        ("mock", "mock-2")
    );
    assert_eq!(provider.node.as_deref(), Some("n1"));
    assert_eq!(provider.context_limit, Some(100000));
    // A live update carries no journal position.
    assert_eq!(provider.position, 0);

    // A signal without a model leaves the header's model alone.
    core.apply(&[live(json!({
        "sessionUpdate": "session_info_update",
        "_meta": { "querymt": { "contextInvalidated": true } }
    }))]);
    assert_eq!(core.header().provider, Some(provider));
}

#[test]
fn usage_update_carries_the_token_breakdown() {
    let mut core = SessionCore::new("s1");
    let tokens = json!({ "inputTokens": 5, "outputTokens": 2, "reasoningTokens": 0,
                         "cacheReadTokens": 1, "cacheWriteTokens": 0 });
    core.apply(&[usage(
        10,
        100,
        json!({ "querymt": { "usage": {
            "version": 1, "latestRequest": tokens, "sessionTotals": tokens, "partial": true
        } } }),
    )]);
    let view = core.header().tokens.clone().expect("token view");
    assert!(view.partial);
    assert_eq!(view.latest_request.expect("latest").input_tokens, 5);
    assert_eq!(view.session_totals.expect("totals").cache_read_tokens, 1);
}

fn config_option(id: &str, current: &str) -> Value {
    json!({ "id": id, "name": id, "type": "select", "currentValue": current, "options": [] })
}

#[test]
fn config_option_update_merges_by_id() {
    let mut core = SessionCore::new("s1");
    core.apply(&[live(json!({ "sessionUpdate": "config_option_update",
        "configOptions": [config_option("mode", "build"), config_option("model", "a")] }))]);
    core.apply(&[live(json!({ "sessionUpdate": "config_option_update",
        "configOptions": [config_option("model", "b")] }))]);
    let options = &core.header().config_options;
    assert_eq!(options.len(), 2);
    let value_of = |id: &str| {
        options
            .iter()
            .find(|o| o["id"] == id)
            .map(|o| o["currentValue"].clone())
    };
    assert_eq!(value_of("model"), Some(json!("b")));
    assert_eq!(value_of("mode"), Some(json!("build")));
}

fn user_chunk(n: usize) -> SessionInput {
    live(json!({
        "sessionUpdate": "user_message_chunk",
        "messageId": format!("u{n}"),
        "content": { "type": "text", "text": format!("prompt {n}") }
    }))
}

/// A view of `turns` prompts, the last one followed by an assistant message.
fn long_view(turns: usize) -> SessionCore {
    let mut core = SessionCore::new("s1");
    let mut batch: Vec<_> = (0..turns).map(user_chunk).collect();
    batch.push(chunk("m-last", "start"));
    core.apply(&batch);
    core
}

#[test]
fn streaming_chunk_reports_only_the_last_turn() {
    let mut core = long_view(500);
    let last = core.turn_ids().pop().expect("a turn");
    let report = core.apply(&[chunk("m-last", " more")]);
    assert_eq!(report.updated, vec![last]);
    assert!(report.added.is_empty() && report.removed.is_empty());
}

#[test]
fn unchanged_turns_keep_their_content_and_id() {
    let mut core = long_view(50);
    let ids = core.turn_ids();
    let before = core.turns(&ids);
    core.apply(&[tool_call("t1", "in_progress")]);
    core.apply(&[tool_update("t1", "completed", "ok")]);
    assert_eq!(core.turn_ids(), ids);
    let after = core.turns(&ids);
    let last = ids.len() - 1;
    assert_eq!(before[..last], after[..last]);
    assert_ne!(before[last], after[last]);
}

#[test]
fn streaming_chunk_changes_one_entry() {
    let mut core = long_view(1);
    core.apply(&[
        tool_call("t1", "in_progress"),
        tool_call("t2", "in_progress"),
        tool_call("t3", "in_progress"),
    ]);
    let id = core.turn_ids().pop().expect("a turn");
    let before = core.turn(&id).expect("turn");
    core.apply(&[chunk("m-last", " more")]);
    let after = core.turn(&id).expect("turn");
    assert_eq!(before.entries.len(), after.entries.len());
    let changed: Vec<_> = before
        .entries
        .iter()
        .zip(&after.entries)
        .filter(|(a, b)| a.revision != b.revision)
        .collect();
    assert_eq!(changed.len(), 1);
    assert!(matches!(changed[0].1.kind, EntryKind::AssistantMessage(_)));
    assert!(after.revision > before.revision);
    for (a, b) in before.entries.iter().zip(&after.entries) {
        assert_eq!(a.id, b.id);
    }
}

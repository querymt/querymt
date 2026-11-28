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
    assert_eq!(ids, vec!["turn-u1", "turn-u2", "turn-u3"]);
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

/// An accounting scope that covers no model call.
fn empty_scope() -> serde_json::Value {
    serde_json::json!({
        "knownSubtotalUsd": 0.0, "completeTotalUsd": 0.0, "complete": true,
        "billing": "empty", "reasons": [],
        "pricedAttempts": 0, "excludedAttempts": 0, "unresolvedAttempts": 0,
        "incompleteAttempts": 0, "unaccountedGenerations": 0
    })
}

#[test]
fn session_without_model_calls_has_no_cost() {
    use querymt_agent_session_core::input::SessionInput;
    let accounting = serde_json::json!({ "version": 1, "session": empty_scope(), "cycle": null });

    // A snapshot.
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(
        vec![prompt(1, "u1", "x")],
        serde_json::json!({ "accounting": accounting }),
    ))]);
    assert_eq!(core.header().cost, None);

    // A snapshot replaces a cost the view held before.
    let priced = serde_json::json!({ "accounting": {
        "version": 1, "session": scope(true, Some(0.42)), "cycle": null } });
    core.apply(&[loaded(load_response(vec![prompt(1, "u1", "x")], priced))]);
    assert!(core.header().cost.is_some());
    core.apply(&[loaded(load_response(
        vec![prompt(1, "u1", "x")],
        serde_json::json!({ "accounting": accounting }),
    ))]);
    assert_eq!(core.header().cost, None);

    // The same rule for accounting in live `_meta`.
    let live = |update: serde_json::Value| SessionInput::AcpNotification {
        notification: serde_json::json!({ "sessionId": "s1", "update": update }),
        replayed: false,
        at_ms: 0,
    };
    let meta = serde_json::json!({ "querymt": { "accounting": accounting } });
    let mut core = SessionCore::new("s1");
    core.apply(&[live(serde_json::json!({
        "sessionUpdate": "usage_update", "used": 10, "size": 100, "_meta": meta }))]);
    assert_eq!(core.header().cost, None);
    core.apply(&[live(serde_json::json!({
        "sessionUpdate": "session_info_update", "_meta": meta }))]);
    assert_eq!(core.header().cost, None);
    assert_eq!(core.header().accounting, None);
}

fn live_update(update: serde_json::Value) -> querymt_agent_session_core::input::SessionInput {
    querymt_agent_session_core::input::SessionInput::AcpNotification {
        notification: serde_json::json!({ "sessionId": "s1", "update": update }),
        replayed: false,
        at_ms: 0,
    }
}

#[test]
fn subscription_only_accounting() {
    use querymt_agent_protocol_types::accounting::AccountingBillingScope;
    let excluded = serde_json::json!({
        "knownSubtotalUsd": 0.0, "completeTotalUsd": null, "complete": false,
        "billing": "excluded", "reasons": [],
        "pricedAttempts": 0, "excludedAttempts": 3, "unresolvedAttempts": 0,
        "incompleteAttempts": 0, "unaccountedGenerations": 0
    });
    let accounting = serde_json::json!({ "version": 1, "session": excluded, "cycle": null });
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(
        vec![prompt(1, "u1", "x")],
        serde_json::json!({ "accounting": accounting }),
    ))]);
    let held = core.header().accounting.clone().expect("accounting");
    assert_eq!(held.session.billing, AccountingBillingScope::Excluded);
    assert_eq!(held.session.excluded_attempts, 3);
    let cost = core.header().cost.clone().expect("cost");
    assert!(!cost.complete);
}

#[test]
fn live_accounting_replaces_the_snapshots() {
    let older = serde_json::json!({ "accounting": {
        "version": 1, "session": scope(true, Some(0.42)), "cycle": null } });
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(vec![prompt(1, "u1", "x")], older))]);
    assert_eq!(
        core.header()
            .accounting
            .as_ref()
            .map(|a| a.session.known_subtotal_usd),
        Some(0.42)
    );

    let newer =
        serde_json::json!({ "version": 1, "session": scope(true, Some(0.9)), "cycle": null });
    let meta = serde_json::json!({ "querymt": { "accounting": newer } });
    core.apply(&[live_update(serde_json::json!({
        "sessionUpdate": "usage_update", "used": 10, "size": 100, "_meta": meta }))]);
    assert_eq!(
        core.header()
            .accounting
            .as_ref()
            .map(|a| a.session.known_subtotal_usd),
        Some(0.9)
    );
    let cost = core.header().cost.clone().expect("cost");
    assert_eq!((cost.amount, cost.complete), (0.9, true));

    // The metadata-only fallback replaces it too.
    let newest =
        serde_json::json!({ "version": 1, "session": scope(true, Some(1.5)), "cycle": null });
    core.apply(&[live_update(serde_json::json!({
        "sessionUpdate": "session_info_update",
        "_meta": { "querymt": { "accounting": newest } } }))]);
    assert_eq!(core.header().cost.as_ref().map(|c| c.amount), Some(1.5));
    assert_eq!(
        core.header()
            .accounting
            .as_ref()
            .map(|a| a.session.known_subtotal_usd),
        Some(1.5)
    );
}

#[test]
fn budget_is_kept() {
    let accounting = serde_json::json!({
        "version": 1,
        "session": scope(true, Some(2.0)),
        "cycle": {
            "cycleId": "cycle-1",
            "scope": scope(false, None),
            "budget": {
                "knownSubtotalUsd": 1.25, "completeTotalUsd": null, "reasons": [],
                "incompleteAttempts": 2, "bestEffort": true
            }
        }
    });
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(load_response(
        vec![prompt(1, "u1", "x")],
        serde_json::json!({ "accounting": accounting }),
    ))]);
    let held = core.header().accounting.clone().expect("accounting");
    let cycle = held.cycle.expect("cycle");
    assert_eq!(cycle.cycle_id, "cycle-1");
    assert_eq!(cycle.budget.known_subtotal_usd, 1.25);
    assert_eq!(cycle.budget.complete_total_usd, None);
    assert_eq!(cycle.budget.incomplete_attempts, 2);
    assert!(cycle.budget.best_effort);
}

#[test]
fn todo_plan_keeps_its_id_after_a_reload() {
    use querymt_agent_session_core::input::SessionInput;
    let args =
        r#"{"todos":[{"content":"a","status":"pending"},{"content":"b","status":"in_progress"}]}"#;

    // Live: the agent sends the todo list as a plan_update under its id.
    let mut live = SessionCore::new("s1");
    live.apply(&[SessionInput::AcpNotification {
        notification: serde_json::json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "plan_update",
            "plan": { "type": "items", "planId": "querymt-todos", "entries": [
                { "content": "a", "priority": "medium", "status": "pending" },
                { "content": "b", "priority": "medium", "status": "in_progress" },
            ] } } }),
        replayed: false,
        at_ms: 0,
    }]);
    let live_plan = live.header().plan.clone().expect("live plan");
    assert_eq!(live_plan.plan_id.as_deref(), Some("querymt-todos"));

    // Reload: a snapshot holding the todowrite call.
    let events = vec![
        prompt(1, "u1", "go"),
        event(
            2,
            "tool_call_start",
            serde_json::json!({ "tool_call_id": "t1", "tool_name": "todowrite", "arguments": args }),
        ),
        tool_end(3, "t1", "ok"),
    ];
    let mut reloaded = SessionCore::new("s1");
    reloaded.apply(&[loaded(load_response(events, serde_json::json!({})))]);
    let plan = reloaded.header().plan.clone().expect("reloaded plan");
    assert_eq!(plan.plan_id, live_plan.plan_id);
    assert_eq!(plan.entries, live_plan.entries);
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

/// An unwindowed snapshot (no `history` key) whose summary has a context
/// measurement at `context_position` and a model selected at `model_position`.
fn context_snapshot(context_position: i64, model_position: i64) -> serde_json::Value {
    let extra = serde_json::json!({
        "summary": {
            "usage": { "latestRequest": null, "sessionTotals": null, "partial": false,
                       "requestCount": 1, "contextTokens": 12000,
                       "contextPosition": context_position,
                       "accounting": serde_json::to_value(
                           querymt_agent_protocol_types::accounting::ScopeTotals::default()).unwrap() },
            "provider": { "provider": "p", "model": "m", "contextLimit": 200000,
                          "position": model_position },
            "pendingInputs": [],
            "childSessions": []
        }
    });
    load_response(vec![prompt(5, "u1", "x")], extra)
}

#[test]
fn unwindowed_snapshot_reports_context() {
    let mut core = SessionCore::new("s1");
    let response = context_snapshot(7, 3);
    assert!(response["_meta"]["querymt/sessionLoadSnapshot.v1"]["history"].is_null());
    let report = core.apply(&[loaded(response)]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let context = core.header().context.clone().expect("context usage");
    assert_eq!((context.used, context.size), (12000, 200000));
}

#[test]
fn snapshot_ignores_a_measurement_older_than_the_model() {
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(context_snapshot(3, 7))]);
    assert_eq!(core.header().context, None);
    assert_eq!(
        core.header().provider.as_ref().map(|p| p.model.as_str()),
        Some("m")
    );
}

#[test]
fn measurement_after_the_model_change_is_kept() {
    let mut core = SessionCore::new("s1");
    core.apply(&[loaded(context_snapshot(8, 7))]);
    let context = core.header().context.clone().expect("context usage");
    assert_eq!((context.used, context.size), (12000, 200000));
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
    assert_eq!(core.turn_ids(), vec!["turn-u1", "turn-u2"]);
    // The replayed turns carry the agent's message ids, so the snapshot's
    // turns have the same ids and content: nothing is removed or reported.
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(
        report.added.is_empty() && report.updated.is_empty(),
        "{report:?}"
    );
    let texts: Vec<usize> = core
        .turns(&core.turn_ids())
        .iter()
        .map(|t| t.entries.len())
        .collect();
    assert_eq!(texts, vec![2, 2]);
}

// The snapshot's events are projected by the rules the agent's own replay
// uses (`querymt_agent_protocol_types::replay`); these pin the cases where a
// separate copy used to disagree.

fn compactions_of(core: &SessionCore) -> Vec<querymt_agent_session_core::entry::CompactionEntry> {
    core.turn_ids()
        .iter()
        .flat_map(|id| core.turn(id).expect("turn").entries)
        .filter_map(|e| match e.kind {
            EntryKind::Compaction(c) => Some(c),
            _ => None,
        })
        .collect()
}

fn load(events: Vec<serde_json::Value>) -> SessionCore {
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[loaded(load_response(events, serde_json::json!({})))]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    core
}

#[test]
fn todowrite_becomes_the_plan_and_not_a_tool_call() {
    let args = r#"{"todos":[{"content":"a","status":"pending","priority":"high"}]}"#;
    let core = load(vec![
        prompt(1, "u1", "go"),
        event(
            2,
            "tool_call_start",
            serde_json::json!({ "tool_call_id": "t1", "tool_name": "todowrite", "arguments": args }),
        ),
        event(
            3,
            "tool_call_end",
            serde_json::json!({ "tool_call_id": "t1", "tool_name": "todowrite",
                               "is_error": false, "result": "ok" }),
        ),
    ]);
    let plan = core.header().plan.clone().expect("a plan");
    assert_eq!(plan.entries.len(), 1);
    let tool_calls = core
        .turn_ids()
        .iter()
        .flat_map(|id| core.turn(id).expect("turn").entries)
        .filter(|e| matches!(e.kind, EntryKind::ToolCall(_)))
        .count();
    assert_eq!(tool_calls, 0);
}

#[test]
fn workspace_announcements_are_conversation_entries() {
    let core = load(vec![
        prompt(1, "u1", "go"),
        event(
            2,
            "worktree_archived",
            serde_json::json!({ "workspace_id": "w1" }),
        ),
        event(
            3,
            "worktree_command_completed",
            serde_json::json!({
                "id": "c1",
                "command": "remove",
                "code": "unpreserved_content",
                "archived": true,
                "text": "removal refused"
            }),
        ),
    ]);
    let messages = core
        .turn_ids()
        .iter()
        .flat_map(|id| core.turn(id).expect("turn").entries)
        .filter(|e| matches!(e.kind, EntryKind::AssistantMessage(_)))
        .count();
    assert_eq!(messages, 2);
}

#[test]
fn id_less_compaction_start_and_end_are_one_entry() {
    let core = load(vec![
        prompt(1, "u1", "go"),
        event(
            2,
            "compaction_start",
            serde_json::json!({ "token_estimate": 10 }),
        ),
        event(
            3,
            "compaction_end",
            serde_json::json!({ "summary": "done", "summary_len": 4 }),
        ),
    ]);
    let compactions = compactions_of(&core);
    assert_eq!(compactions.len(), 1, "{compactions:?}");
    assert_eq!(
        compactions[0].state,
        querymt_agent_session_core::entry::CompactionState::Completed
    );
}

#[test]
fn a_cancelled_compaction_carries_no_error() {
    let core = load(vec![
        prompt(1, "u1", "go"),
        event(
            2,
            "compaction_failed",
            serde_json::json!({ "compaction_id": "c1", "reason": "user cancelled", "cancelled": true }),
        ),
    ]);
    let compactions = compactions_of(&core);
    assert_eq!(compactions.len(), 1);
    assert_eq!(compactions[0].error, None);
}

fn advertise(commands: serde_json::Value) -> querymt_agent_session_core::input::SessionInput {
    querymt_agent_session_core::input::SessionInput::AcpNotification {
        notification: serde_json::json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "available_commands_update", "availableCommands": commands } }),
        replayed: false,
        at_ms: 0,
    }
}

fn compact_command() -> serde_json::Value {
    serde_json::json!([{ "name": "compact", "description": "Compact" }])
}

#[test]
fn snapshot_load_keeps_commands() {
    let mut core = SessionCore::new("s1");
    core.apply(&[advertise(compact_command())]);
    let events = vec![prompt(1, "u1", "q"), reply(2, "a1", "r")];
    let report = core.apply(&[loaded(load_response(events, serde_json::json!({})))]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(core.header().available_commands.len(), 1);
    assert_eq!(core.header().available_commands[0].name, "compact");
    // An update after the load still replaces the list.
    core.apply(&[advertise(serde_json::json!([]))]);
    assert!(core.header().available_commands.is_empty());
}

#[test]
fn load_without_snapshot_keeps_commands() {
    let mut core = SessionCore::new("s1");
    core.apply(&[advertise(compact_command())]);
    core.apply(&[loaded(serde_json::json!({
        "modes": { "currentModeId": "build", "availableModes": [] }
    }))]);
    assert_eq!(core.header().available_commands.len(), 1);
    assert_eq!(core.header().current_mode.as_deref(), Some("build"));
}

#[test]
fn loaded_prompt_command_exposes_its_record() {
    use querymt_agent_protocol_types::events::{
        SlashCommandEffect, SlashCommandRecord, SlashCommandSource,
    };
    let mut command = prompt(1, "u1", "The user invoked the slash command `/explain`.");
    command["kind"]["data"]["slash_command"] = serde_json::json!({
        "name": "explain",
        "arguments": "closures",
        "source": "project",
        "effect": "prompt",
        "client_prompt_id": "c1"
    });
    let prompts = serde_json::json!([{
        "messageId": "u1",
        "messageOrder": 0,
        "timestamp": 1001,
        "blocks": [
            { "type": "text", "text": "expansion text" },
            { "type": "resource_link", "name": "diagram.png", "uri": "file:///tmp/diagram.png" }
        ]
    }]);
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[loaded(load_response(
        vec![command, reply(2, "a1", "r1")],
        serde_json::json!({ "userPrompts": prompts }),
    ))]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let ids = core.turn_ids();
    let turn = core.turn(&ids[0]).expect("turn exists");
    let EntryKind::UserMessage(message) = &turn.entries[0].kind else {
        panic!("expected a user message, got {:?}", turn.entries[0].kind);
    };
    // The prompt record's blocks, with the record of the event.
    assert_eq!(message.blocks.len(), 2);
    assert_eq!(
        message.slash_command,
        Some(SlashCommandRecord {
            name: "explain".into(),
            arguments: "closures".into(),
            source: SlashCommandSource::Project,
            effect: SlashCommandEffect::Prompt,
            client_prompt_id: Some("c1".into()),
        })
    );
}

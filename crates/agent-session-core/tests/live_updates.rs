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

fn commands_update(commands: Value) -> SessionInput {
    live(json!({ "sessionUpdate": "available_commands_update", "availableCommands": commands }))
}

fn command_names(core: &SessionCore) -> Vec<String> {
    core.header()
        .available_commands
        .iter()
        .map(|command| command.name.clone())
        .collect()
}

#[test]
fn available_commands_replace_the_list() {
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[commands_update(json!([
        { "name": "compact", "description": "Compact" },
        { "name": "worktree", "description": "Worktree" }
    ]))]);
    assert!(report.header_changed);
    assert_eq!(command_names(&core), ["compact", "worktree"]);
    core.apply(&[commands_update(json!([
        { "name": "compact", "description": "Compact again" }
    ]))]);
    assert_eq!(command_names(&core), ["compact"]);
    assert_eq!(
        core.header().available_commands[0].description,
        "Compact again"
    );
}

#[test]
fn empty_update_clears_commands() {
    let mut core = SessionCore::new("s1");
    core.apply(&[commands_update(
        json!([{ "name": "compact", "description": "c" }]),
    )]);
    assert_eq!(command_names(&core), ["compact"]);
    core.apply(&[commands_update(json!([]))]);
    assert!(core.header().available_commands.is_empty());
}

#[test]
fn slash_prefix_is_trimmed() {
    let mut core = SessionCore::new("s1");
    core.apply(&[commands_update(json!([
        { "name": "/compact", "description": "first" },
        { "name": "compact", "description": "duplicate" },
        { "name": "/", "description": "empty after trimming" },
        { "name": "", "description": "empty" },
        { "name": "web", "description": "spec style" }
    ]))]);
    assert_eq!(command_names(&core), ["compact", "web"]);
    assert_eq!(core.header().available_commands[0].description, "first");
}

#[test]
fn input_hint_is_kept() {
    let mut core = SessionCore::new("s1");
    core.apply(&[commands_update(json!([
        { "name": "compact", "description": "c", "input": { "hint": "[instructions]" } },
        { "name": "worktree", "description": "w" }
    ]))]);
    let commands = &core.header().available_commands;
    assert_eq!(commands[0].input_hint.as_deref(), Some("[instructions]"));
    assert_eq!(commands[1].input_hint, None);
}

#[test]
fn malformed_command_entries_are_skipped() {
    let mut core = SessionCore::new("s1");
    core.apply(&[commands_update(json!([
        { "description": "no name" },
        { "name": "compact", "description": "c" }
    ]))]);
    assert_eq!(command_names(&core), ["compact"]);
}

#[test]
fn descriptor_is_read_from_the_catalog() {
    use querymt_agent_protocol_types::events::{SlashCommandEffect, SlashCommandSource};
    use querymt_agent_session_core::view::{CommandInput, CommandSubmission};
    let mut core = SessionCore::new("s1");
    core.apply(&[commands_update(json!([{
        "name": "compact",
        "description": "Compact",
        "_meta": { "querymt": {
            "source": "builtin",
            "effect": "action",
            "input": "optional",
            "accepts": ["prompt", "queue"]
        } }
    }]))]);
    let command = &core.header().available_commands[0];
    assert_eq!(command.source, Some(SlashCommandSource::Builtin));
    assert_eq!(command.effect, Some(SlashCommandEffect::Action));
    assert_eq!(command.input, Some(CommandInput::Optional));
    assert_eq!(
        command.accepts,
        Some(vec![CommandSubmission::Prompt, CommandSubmission::Queue])
    );
}

#[test]
fn catalog_without_descriptors() {
    let mut core = SessionCore::new("s1");
    core.apply(&[commands_update(json!([
        { "name": "compact", "description": "Compact", "input": { "hint": "[x]" } },
        { "name": "worktree", "description": "Worktree" }
    ]))]);
    for command in &core.header().available_commands {
        assert_eq!(command.source, None, "{}", command.name);
        assert_eq!(command.effect, None, "{}", command.name);
        assert_eq!(command.input, None, "{}", command.name);
        // Absent, not an empty list: a client picks its fallback from `None`.
        assert_eq!(command.accepts, None, "{}", command.name);
    }
    let commands = &core.header().available_commands;
    assert_eq!(commands[0].description, "Compact");
    assert_eq!(commands[0].input_hint.as_deref(), Some("[x]"));
}

#[test]
fn unknown_descriptor_value_is_absent() {
    use querymt_agent_protocol_types::events::SlashCommandSource;
    use querymt_agent_session_core::view::CommandSubmission;
    let mut core = SessionCore::new("s1");
    core.apply(&[commands_update(json!([{
        "name": "explain",
        "description": "Explain",
        "_meta": { "querymt": {
            "source": "project",
            "effect": "workflow",
            "accepts": ["prompt", "broadcast"]
        } }
    }]))]);
    let command = &core.header().available_commands[0];
    assert_eq!(command.source, Some(SlashCommandSource::Project));
    assert_eq!(command.effect, None);
    assert_eq!(command.input, None);
    // An unknown element is dropped; the list stays a list.
    assert_eq!(command.accepts, Some(vec![CommandSubmission::Prompt]));
    assert_eq!(command_names(&core), ["explain"]);
}

#[test]
fn user_message_exposes_its_record() {
    use querymt_agent_protocol_types::events::{
        SlashCommandEffect, SlashCommandRecord, SlashCommandSource,
    };
    let mut core = SessionCore::new("s1");
    core.apply(&[
        live(json!({
            "sessionUpdate": "user_message_chunk",
            "messageId": "u1",
            "content": { "type": "text", "text": "The user invoked the slash command `/explain`." },
            "_meta": { "querymt": { "slashCommand": {
                "name": "explain",
                "arguments": "closures",
                "source": "project",
                "effect": "prompt",
                "clientPromptId": "c1"
            } } }
        })),
        // A later chunk of the same message that repeats no record.
        live(json!({
            "sessionUpdate": "user_message_chunk",
            "messageId": "u1",
            "content": { "type": "text", "text": "attachment" }
        })),
    ]);
    let ids = core.turn_ids();
    let turn = core.turn(ids.last().expect("a turn")).expect("turn exists");
    let EntryKind::UserMessage(message) = &turn.entries[0].kind else {
        panic!("expected a user message, got {:?}", turn.entries[0].kind);
    };
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
    // The chunk's content stays the message's blocks.
    assert_eq!(message.blocks.len(), 2);
}

//! Client inputs (spec: Optimistic prompts, Run state).

use querymt_agent_protocol_types::protocol::{ContentBlock, StopReason, TextContent};
use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::entry::{EntryKind, PromptStatus};
use querymt_agent_session_core::input::SessionInput;
use querymt_agent_session_core::view::RunState;
use serde_json::json;

fn optimistic(id: &str) -> SessionInput {
    SessionInput::PromptOptimistic {
        client_prompt_id: id.into(),
        blocks: vec![ContentBlock::Text(TextContent::new("hi"))],
        at_ms: 5,
    }
}

fn echo(message_id: &str, prompt_id: &str) -> SessionInput {
    SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "user_message_chunk",
            "messageId": message_id,
            "content": { "type": "text", "text": "hi" },
            "_meta": { "querymt": { "client_prompt_id": prompt_id } }
        } }),
        replayed: false,
        at_ms: 6,
    }
}

fn users(core: &SessionCore) -> Vec<(String, querymt_agent_session_core::entry::UserMessage)> {
    let mut out = Vec::new();
    for id in core.turn_ids() {
        for e in core.turn(&id).expect("turn").entries {
            if let EntryKind::UserMessage(u) = e.kind {
                out.push((id.clone(), u));
            }
        }
    }
    out
}

#[test]
fn agent_echo_replaces_the_optimistic_prompt() {
    let mut core = SessionCore::new("s1");
    core.apply(&[optimistic("c1")]);
    let turn_before = core.turn_ids();
    core.apply(&[echo("m1", "c1")]);
    let found = users(&core);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1.status, PromptStatus::Sent);
    assert_eq!(found[0].1.message_id.as_deref(), Some("m1"));
    assert_eq!(core.turn_ids(), turn_before);
}

#[test]
fn failed_prompt_is_marked_failed() {
    let mut core = SessionCore::new("s1");
    core.apply(&[optimistic("c1")]);
    core.apply(&[SessionInput::PromptFailed {
        client_prompt_id: "c1".into(),
        message: "offline".into(),
    }]);
    let found = users(&core);
    assert_eq!(found[0].1.status, PromptStatus::Failed);
    assert_eq!(found[0].1.error.as_deref(), Some("offline"));
    assert_eq!(core.header().run_state, RunState::Failed);
}

#[test]
fn completed_prompt_returns_to_idle() {
    let mut core = SessionCore::new("s1");
    core.apply(&[optimistic("c1")]);
    core.apply(&[SessionInput::PromptCompleted {
        stop_reason: StopReason::EndTurn,
        at_ms: 9,
    }]);
    assert_eq!(core.header().run_state, RunState::Idle);
    assert_eq!(core.header().last_stop_reason.as_deref(), Some("end_turn"));
}

#[test]
fn running_tool_sets_tool_running_state() {
    let mut core = SessionCore::new("s1");
    core.apply(&[optimistic("c1")]);
    core.apply(&[SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "tool_call", "toolCallId": "t1",
            "title": "Run shell", "status": "in_progress"
        } }),
        replayed: false,
        at_ms: 7,
    }]);
    assert_eq!(core.header().run_state, RunState::ToolRunning);
    assert_eq!(core.header().active_tool_call_id.as_deref(), Some("t1"));
}

fn runtime(phase: &str) -> SessionInput {
    SessionInput::RuntimeObserved {
        status: serde_json::from_value(json!({
            "phase": phase, "active_run_id": null, "steerable": false,
            "pending_steering_count": 0, "queued_input_count": 0, "run_started_at_ms": null,
            "observation": { "availability": "live" }
        }))
        .expect("runtime status"),
    }
}

#[test]
fn idle_runtime_observation_ends_a_stale_run() {
    let mut core = SessionCore::new("s1");
    core.apply(&[SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "tool_call", "toolCallId": "t1",
            "title": "Run shell", "status": "in_progress"
        } }),
        replayed: false,
        at_ms: 1,
    }]);
    assert_eq!(core.header().run_state, RunState::ToolRunning);
    core.apply(&[runtime("idle")]);
    assert_eq!(core.header().run_state, RunState::Idle);
    assert_eq!(core.header().active_tool_call_id, None);
}

#[test]
fn undo_stack_is_replaced() {
    use querymt_agent_session_core::input::UndoFrame;
    let frame = |id: &str, failed: bool| UndoFrame {
        message_id: id.into(),
        restore_failed: failed,
    };
    let mut core = SessionCore::new("s1");
    core.apply(&[SessionInput::UndoStack {
        frames: vec![frame("old", false)],
    }]);
    core.apply(&[SessionInput::UndoStack {
        frames: vec![frame("a", false), frame("b", true)],
    }]);
    let stack = &core.header().undo_stack;
    assert_eq!(stack.len(), 2);
    assert!(!stack[0].restore_failed && stack[1].restore_failed);
}

fn input_note(id: &str, state: &str) -> SessionInput {
    SessionInput::InputState {
        notification: serde_json::from_value(json!({
            "version": 1, "session_id": "s1", "input_id": id,
            "delivery": "queue", "state": state
        }))
        .expect("input state"),
    }
}

#[test]
fn input_state_notification_updates_a_pending_input() {
    let mut core = SessionCore::new("s1");
    core.apply(&[input_note("i1", "queued"), input_note("i2", "queued")]);
    assert_eq!(core.header().pending_inputs.len(), 2);
    core.apply(&[input_note("i1", "started")]);
    let pending = &core.header().pending_inputs;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].input_id, "i2");
}

#[test]
fn delegation_update_links_a_child_session() {
    let mut core = SessionCore::new("s1");
    core.apply(&[SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "delegate"
        } }),
        replayed: false,
        at_ms: 1,
    }]);
    core.apply(&[SessionInput::DelegationUpdate {
        update: serde_json::from_value(json!({
            "version": 1, "sessionId": "s1", "delegationId": "d1", "toolCallId": "t1",
            "state": "forked", "targetAgentId": "coder", "objective": "x",
            "childSessionId": "child-1", "requestedAt": 1, "updatedAt": 2
        }))
        .expect("delegation"),
    }]);
    let id = core.turn_ids().pop().expect("turn");
    let linked = core.turn(&id).expect("turn").entries.into_iter().any(|e| {
        matches!(e.kind, EntryKind::ToolCall(c) if c.child_session_id.as_deref() == Some("child-1"))
    });
    assert!(linked);
    assert_eq!(core.header().delegations.len(), 1);
}

#[test]
fn full_content_replaces_the_prefix() {
    use querymt_agent_protocol_types::session::history::ToolContentPart;
    let mut core = SessionCore::new("s1");
    core.apply(&[SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "read",
            "rawOutput": "pre"
        } }),
        replayed: false,
        at_ms: 1,
    }]);
    let report = core.apply(&[SessionInput::ToolContent {
        tool_call_id: "t1".into(),
        part: ToolContentPart::Result,
        content: "the full result".into(),
    }]);
    assert_eq!(report.updated, core.turn_ids());
    let id = core.turn_ids().pop().expect("turn");
    let call = core
        .turn(&id)
        .expect("turn")
        .entries
        .into_iter()
        .find_map(|e| match e.kind {
            EntryKind::ToolCall(c) => Some(c),
            _ => None,
        })
        .expect("tool call");
    assert_eq!(call.raw_output, Some(json!("the full result")));
    assert_eq!(call.result_full_bytes, None);
}

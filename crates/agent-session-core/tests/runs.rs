//! Turns are runs, identity from the agent, run state from state updates
//! (spec: Turns are agent runs, Identity from the agent, Optimistic prompts,
//! Client-local prompts, Run state).

mod common;

use common::{prompt, reply};
use querymt_agent_protocol_types::events::AgentEventKind;
use querymt_agent_protocol_types::protocol::{ContentBlock, StopReason, TextContent};
use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::entry::{EntryKind, PromptStatus};
use querymt_agent_session_core::input::SessionInput;
use querymt_agent_session_core::testing::{event, live, snapshot_response};
use querymt_agent_session_core::view::RunState;
use serde_json::json;

fn user(message_id: &str, run: Option<&str>) -> SessionInput {
    let mut update = json!({
        "sessionUpdate": "user_message_chunk", "messageId": message_id,
        "content": { "type": "text", "text": message_id }
    });
    if let Some(run) = run {
        update["_meta"] = json!({ "querymt": { "runId": run } });
    }
    live(update)
}

fn assistant(message_id: &str, run: Option<&str>) -> SessionInput {
    let mut update = json!({
        "sessionUpdate": "agent_message_chunk", "messageId": message_id,
        "content": { "type": "text", "text": "text" }
    });
    if let Some(run) = run {
        update["_meta"] = json!({ "querymt": { "runId": run } });
    }
    live(update)
}

fn state(state: &str, run: &str) -> SessionInput {
    SessionInput::StateUpdate {
        notification: json!({ "sessionId": "s1", "state": state, "runId": run }),
        at_ms: 0,
    }
}

fn idle(run: &str, stop: &str) -> SessionInput {
    SessionInput::StateUpdate {
        notification: json!({
            "sessionId": "s1", "state": "idle", "runId": run, "stopReason": stop
        }),
        at_ms: 0,
    }
}

fn entry_ids(core: &SessionCore, turn: &str) -> Vec<String> {
    core.turn(turn)
        .expect("turn")
        .entries
        .into_iter()
        .map(|entry| entry.id)
        .collect()
}

fn optimistic(id: &str) -> SessionInput {
    SessionInput::PromptOptimistic {
        client_prompt_id: id.into(),
        blocks: vec![ContentBlock::Text(TextContent::new("hi"))],
        at_ms: 5,
    }
}

fn runtime(phase: &str) -> SessionInput {
    SessionInput::RuntimeObserved {
        status: serde_json::from_value(common::runtime(phase)).expect("runtime status"),
    }
}

// ── Turns are runs ─────────────────────────────────────────────────────────

#[test]
fn steering_stays_in_the_running_turn() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        user("u1", Some("r1")),
        assistant("a1", Some("r1")),
        user("steer", Some("r1")),
        assistant("a2", Some("r1")),
    ]);
    assert_eq!(core.turn_ids(), ["run-r1"]);
    assert_eq!(
        entry_ids(&core, "run-r1"),
        ["user-u1", "assistant-a1", "user-steer", "assistant-a2"]
    );
}

#[test]
fn compaction_during_a_run_stays_in_the_turn() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        user("u1", Some("r1")),
        live(json!({
            "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "in_progress",
            "_meta": { "querymt": { "runId": "r1" } }
        })),
    ]);
    assert_eq!(core.turn_ids(), ["run-r1"]);
    assert_eq!(entry_ids(&core, "run-r1"), ["user-u1", "compaction-c1"]);
}

#[test]
fn run_without_a_prompt_is_its_own_turn() {
    let mut core = SessionCore::new("s1");
    core.apply(&[user("u1", Some("r1")), assistant("a1", Some("r1"))]);
    // A scheduled run: the state reports it, its output carries no run id.
    core.apply(&[state("running", "r2"), assistant("a2", None)]);
    assert_eq!(core.turn_ids(), ["run-r1", "run-r2"]);
    assert_eq!(entry_ids(&core, "run-r2"), ["assistant-a2"]);
}

#[test]
fn older_agent_without_run_ids() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        user("u1", None),
        assistant("a1", None),
        user("u2", None),
        assistant("a2", None),
    ]);
    assert_eq!(core.turn_ids(), ["turn-u1", "turn-u2"]);
    assert_eq!(entry_ids(&core, "turn-u2"), ["user-u2", "assistant-a2"]);
}

#[test]
fn snapshot_events_between_run_start_and_end_belong_to_that_run() {
    let started = |seq, run: &str| {
        event(
            seq,
            AgentEventKind::RunStarted {
                run_id: run.into(),
                origin: "interactive".into(),
            },
        )
    };
    let completed = |seq, run: &str| {
        event(
            seq,
            AgentEventKind::RunCompleted {
                run_id: run.into(),
                outcome: "completed".into(),
                stop_type: None,
                reason: None,
            },
        )
    };
    let events = vec![
        started(1, "r1"),
        prompt(2, "u1", "first"),
        // A steering prompt of the same run.
        prompt(3, "steer", "also"),
        reply(4, "a1", "done"),
        completed(5, "r1"),
        // A run with no prompt.
        started(6, "r2"),
        reply(7, "a2", "scheduled"),
        completed(8, "r2"),
    ];
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[SessionInput::SnapshotLoaded {
        response: snapshot_response(&events, json!({})),
    }]);
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(core.turn_ids(), ["run-r1", "run-r2"]);
    assert_eq!(
        entry_ids(&core, "run-r1"),
        ["user-u1", "user-steer", "assistant-a1"]
    );
    assert_eq!(entry_ids(&core, "run-r2"), ["assistant-a2"]);
}

// ── Identity from the agent ────────────────────────────────────────────────

#[test]
fn prompt_response_links_the_prompt() {
    let mut core = SessionCore::new("s1");
    core.apply(&[optimistic("c1")]);
    assert_eq!(core.turn_ids(), ["pending-c1"]);
    // The agent's message arrives without the client prompt id.
    core.apply(&[user("m1", None)]);
    assert_eq!(core.turn_ids(), ["turn-m1", "pending-c1"]);
    core.apply(&[SessionInput::PromptCompleted {
        client_prompt_id: Some("c1".into()),
        message_id: Some("m1".into()),
        stop_reason: StopReason::EndTurn,
        at_ms: 9,
    }]);
    assert_eq!(core.turn_ids(), ["turn-m1"], "the pending prompt is gone");
}

#[test]
fn snapshot_keeps_client_local_prompts() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        optimistic("pending"),
        optimistic("failed"),
        SessionInput::PromptFailed {
            client_prompt_id: "failed".into(),
            message: "offline".into(),
        },
    ]);
    let events = vec![prompt(1, "u1", "q"), reply(2, "a1", "r")];
    core.apply(&[SessionInput::SnapshotLoaded {
        response: snapshot_response(&events, json!({})),
    }]);
    // The snapshot's turn first, then both client-local prompts.
    assert_eq!(
        core.turn_ids(),
        ["turn-u1", "pending-pending", "pending-failed"]
    );
    let status_of = |turn: &str| match &core.turn(turn).expect("turn").entries[0].kind {
        EntryKind::UserMessage(user) => (user.status, user.error.clone()),
        other => panic!("expected a user message, got {other:?}"),
    };
    assert_eq!(status_of("pending-pending").0, PromptStatus::Pending);
    assert_eq!(
        status_of("pending-failed"),
        (PromptStatus::Failed, Some("offline".into()))
    );
}

#[test]
fn reload_keeps_turn_and_entry_ids() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        optimistic("c1"),
        state("running", "r1"),
        live(json!({
            "sessionUpdate": "user_message_chunk", "messageId": "m1",
            "content": { "type": "text", "text": "hi" },
            "_meta": { "querymt": { "client_prompt_id": "c1", "runId": "r1" } }
        })),
        live(json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "a1",
            "content": { "type": "text", "text": "ok" },
            "_meta": { "querymt": { "runId": "r1" } }
        })),
        idle("r1", "end_turn"),
    ]);
    let ids = core.turn_ids();
    let before = core.turns(&ids);
    assert_eq!(ids, ["run-r1"]);

    let events = vec![
        event(
            1,
            AgentEventKind::RunStarted {
                run_id: "r1".into(),
                origin: "interactive".into(),
            },
        ),
        prompt(2, "m1", "hi"),
        reply(3, "a1", "ok"),
        event(
            4,
            AgentEventKind::RunCompleted {
                run_id: "r1".into(),
                outcome: "completed".into(),
                stop_type: None,
                reason: None,
            },
        ),
    ];
    let report = core.apply(&[SessionInput::SnapshotLoaded {
        response: snapshot_response(&events, json!({})),
    }]);
    assert_eq!(core.turn_ids(), ids);
    let after = core.turns(&ids);
    for (old, new) in before.iter().zip(&after) {
        let old_ids: Vec<_> = old.entries.iter().map(|e| &e.id).collect();
        let new_ids: Vec<_> = new.entries.iter().map(|e| &e.id).collect();
        assert_eq!(old_ids, new_ids);
        // Same content: the client's cached revisions stay valid.
        assert_eq!(old, new);
    }
    assert!(
        report.added.is_empty() && report.updated.is_empty() && report.removed.is_empty(),
        "no turn whose content is unchanged is reported: {report:?}"
    );
}

#[test]
fn same_id_on_different_entity_types() {
    let mut core = SessionCore::new("s1");
    core.apply(&[
        assistant("x", Some("r1")),
        live(json!({
            "sessionUpdate": "agent_thought_chunk", "messageId": "x",
            "content": { "type": "text", "text": "hm" },
            "_meta": { "querymt": { "runId": "r1", "reasoning_part_id": "p0" } }
        })),
    ]);
    let entries = core.turn("run-r1").expect("turn").entries;
    assert_eq!(entries.len(), 2);
    let ids: Vec<_> = entries.iter().map(|e| e.id.as_str()).collect();
    assert!(ids.contains(&"assistant-x"), "{ids:?}");
    assert!(ids.contains(&"reasoning-x-p0"), "{ids:?}");
}

// ── Run state ──────────────────────────────────────────────────────────────

#[test]
fn state_update_starts_and_ends_a_run() {
    let mut core = SessionCore::new("s1");
    core.apply(&[state("running", "r1")]);
    assert_eq!(core.header().run_state, RunState::Thinking);
    assert_eq!(core.header().active_run_id.as_deref(), Some("r1"));

    core.apply(&[idle("r1", "end_turn")]);
    assert_eq!(core.header().run_state, RunState::Idle);
    assert_eq!(core.header().active_run_id, None);
    assert_eq!(core.header().last_stop_reason.as_deref(), Some("end_turn"));

    core.apply(&[state("running", "r2"), idle("r2", "cancelled")]);
    assert_eq!(core.header().run_state, RunState::Cancelled);
}

#[test]
fn run_started_elsewhere_is_shown() {
    let mut core = SessionCore::new("s1");
    // Nothing was prompted from this client.
    core.apply(&[state("running", "scheduled-1")]);
    assert_ne!(core.header().run_state, RunState::Idle);
    assert_eq!(core.header().active_run_id.as_deref(), Some("scheduled-1"));
}

#[test]
fn waiting_for_the_user() {
    let mut core = SessionCore::new("s1");
    core.apply(&[state("running", "r1"), state("requires_action", "r1")]);
    assert_eq!(core.header().run_state, RunState::WaitingForUser);
    // A session update does not leave the wait; the agent's `running` does.
    core.apply(&[assistant("a1", Some("r1"))]);
    assert_eq!(core.header().run_state, RunState::WaitingForUser);
    core.apply(&[state("running", "r1")]);
    assert_ne!(core.header().run_state, RunState::WaitingForUser);
    assert_ne!(core.header().run_state, RunState::Idle);
}

#[test]
fn session_updates_only_refine_an_active_run() {
    let mut core = SessionCore::new("s1");
    core.apply(&[state("running", "r1"), idle("r1", "end_turn")]);
    // Late output after the run ended must not make the session busy.
    core.apply(&[assistant("a1", Some("r1"))]);
    assert_eq!(core.header().run_state, RunState::Idle);

    core.apply(&[state("running", "r2"), assistant("a2", Some("r2"))]);
    assert_eq!(core.header().run_state, RunState::Streaming);
}

#[test]
fn state_updates_take_the_decision_from_prompt_outcomes_and_runtime() {
    let mut core = SessionCore::new("s1");
    core.apply(&[state("running", "r1")]);
    core.apply(&[SessionInput::PromptCompleted {
        client_prompt_id: None,
        message_id: None,
        stop_reason: StopReason::EndTurn,
        at_ms: 1,
    }]);
    assert_ne!(
        core.header().run_state,
        RunState::Idle,
        "the response does not end a run the agent still reports"
    );
    core.apply(&[runtime("idle")]);
    assert_ne!(core.header().run_state, RunState::Idle);
    assert_eq!(core.header().active_run_id.as_deref(), Some("r1"));
}

#[test]
fn busy_runtime_observation_is_an_active_run() {
    for phase in [
        "starting",
        "model",
        "tools",
        "waiting",
        "closing",
        "cancel_requested",
    ] {
        let mut core = SessionCore::new("s1");
        core.apply(&[runtime(phase)]);
        assert_ne!(
            core.header().run_state,
            RunState::Idle,
            "phase {phase} is an active run"
        );
    }
}

#[test]
fn a_state_update_for_another_session_or_without_a_run_is_not_applied() {
    let mut core = SessionCore::new("s1");
    let report = core.apply(&[
        SessionInput::StateUpdate {
            notification: json!({ "sessionId": "other", "state": "running", "runId": "r9" }),
            at_ms: 0,
        },
        SessionInput::StateUpdate {
            notification: json!({ "sessionId": "s1", "state": "running" }),
            at_ms: 0,
        },
    ]);
    assert_eq!(report.ignored, 1);
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert_eq!(core.header().run_state, RunState::Idle);
}

//! Tests of the `SessionView` that the UI reads: input wiring (tasks 5.1-5.4)
//! and the turn tables (task 6.1), over QueryMT's golden traces and small
//! hand-built runs.

use std::sync::Arc;

use querymt_agent_session_core::input::SessionInput;
use serde_json::{Value, json};

use super::{ItemKind, SessionView, UndoFrame, WorkEntry};

/// The batches of one of QueryMT's golden cases, generated at test time.
fn trace(name: &str) -> Vec<Vec<SessionInput>> {
    querymt_agent_session_core::testing::golden_cases()
        .into_iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("no golden case {name}"))
        .batches
}

/// The `session/load` response of a trace's first snapshot input.
fn snapshot_response(name: &str) -> Value {
    let first = trace(name).remove(0).remove(0);
    let value = serde_json::to_value(first).unwrap();
    value["data"]["response"].clone()
}

fn view() -> SessionView {
    SessionView::new("agent", "s1", "/w", "t")
}

/// Text of the user and agent rows, in order.
fn texts(view: &SessionView) -> Vec<String> {
    view.items
        .iter()
        .filter_map(|i| match &i.kind {
            ItemKind::User { text, .. } | ItemKind::Agent { text } => Some(text.clone()),
            ItemKind::Notice { text, .. } => Some(format!("notice: {text}")),
            _ => None,
        })
        .collect()
}

fn state(view: &mut SessionView, run: &str, state: &str) {
    view.state_update(&json!({ "sessionId": "s1", "state": state, "runId": run,
        "stopReason": "end_turn" }));
}

/// A live update of run `run`.
fn live(view: &mut SessionView, run: &str, mut update: Value) {
    update["_meta"] = json!({ "querymt": { "runId": run } });
    view.session_update(&json!({ "sessionId": "s1", "update": update }));
}

fn user(view: &mut SessionView, run: &str, id: &str, text: &str) {
    live(
        view,
        run,
        json!({ "sessionUpdate": "user_message_chunk", "messageId": id,
            "content": { "type": "text", "text": text } }),
    );
}

fn agent(view: &mut SessionView, run: &str, id: &str, text: &str) {
    live(
        view,
        run,
        json!({ "sessionUpdate": "agent_message_chunk", "messageId": id,
            "content": { "type": "text", "text": text } }),
    );
}

fn edit(view: &mut SessionView, run: &str, id: &str, path: &str, new: &str) {
    live(
        view,
        run,
        json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": "edit",
            "kind": "edit", "status": "completed",
            "content": [{ "type": "diff", "path": path, "oldText": "", "newText": new }] }),
    );
}

/// One run `run` with a prompt, a steering message and an edit and a reply
/// on each side of it.
fn steered_run(view: &mut SessionView, run: &str) {
    state(view, run, "running");
    user(view, run, &format!("{run}-u1"), "first");
    agent(view, run, &format!("{run}-a1"), "one");
    edit(view, run, &format!("{run}-t1"), "a.rs", "a\n");
    user(view, run, &format!("{run}-u2"), "steer");
    agent(view, run, &format!("{run}-a2"), "two");
    edit(view, run, &format!("{run}-t2"), "b.rs", "b\n");
    state(view, run, "idle");
}

fn closing_row(view: &SessionView, turn: usize) -> usize {
    let (_, rows) = view.turn_bounds().remove(turn);
    view.turn_ends()
        .keys()
        .copied()
        .filter(|ix| rows.contains(ix))
        .max()
        .expect("the turn has a closing row")
}

// ---- 5.1 open and load ------------------------------------------------------

#[test]
fn update_for_another_session() {
    let mut view = view();
    view.session_update(&json!({ "sessionId": "other", "update": {
        "sessionUpdate": "agent_message_chunk", "messageId": "m",
        "content": { "type": "text", "text": "elsewhere" } } }));
    assert!(view.items.is_empty());
    agent(&mut view, "r1", "m1", "here");
    view.session_update(&json!({ "sessionId": "other", "update": {
        "sessionUpdate": "agent_message_chunk", "messageId": "m2",
        "content": { "type": "text", "text": "elsewhere" } } }));
    assert_eq!(texts(&view), ["here"]);
}

#[test]
fn replayed_updates_before_the_load_response() {
    let loaded_only = {
        let mut v = view();
        v.load(snapshot_response("small_snapshot"));
        texts(&v)
    };
    assert!(loaded_only.len() > 2);
    let mut v = view();
    // An agent that replays anyway sends the first turn before the response.
    v.session_update(&json!({ "sessionId": "s1", "_meta": { "querymtReplay": true },
        "update": { "sessionUpdate": "user_message_chunk", "messageId": "u0",
            "content": { "type": "text", "text": "sed labore ut labore" } } }));
    v.session_update(&json!({ "sessionId": "s1", "_meta": { "querymtReplay": true },
        "update": { "sessionUpdate": "agent_message_chunk", "messageId": "a0",
            "content": { "type": "text", "text": "partial" } } }));
    v.load(snapshot_response("small_snapshot"));
    assert_eq!(texts(&v), loaded_only);
    assert_eq!(
        texts(&v)
            .iter()
            .filter(|t| *t == "sed labore ut labore")
            .count(),
        1
    );
}

// ---- 5.2 run state ----------------------------------------------------------

#[test]
fn run_started_from_another_client() {
    let mut v = view();
    state(&mut v, "r9", "running");
    assert!(v.running());
    assert_eq!(v.run_id.as_deref(), Some("r9"));
    assert!(v.active_started_at.is_some(), "the work timer runs");
}

#[test]
fn run_ends() {
    let mut v = view();
    state(&mut v, "r9", "running");
    state(&mut v, "r9", "idle");
    assert!(!v.running());
    assert_eq!(v.run_id, None);
    assert_eq!(v.active_started_at, None, "the work timer stops");
}

#[test]
fn active_time_is_the_cores() {
    let mut v = view();
    v.load(snapshot_response("active_time"));
    // The golden case loads during a run: 120 s of ended runs, the run
    // started at host time 50 s.
    assert_eq!(v.active_work_ms, 120_000.);
    assert_eq!(v.active_started_at, Some(50_000.), "the host's start");
    v.state_update(&json!({ "sessionId": "s1", "state": "idle", "runId": "r-live",
        "stopReason": "end_turn", "atMs": 65_000 }));
    assert_eq!(v.active_work_ms, 135_000., "the ended run is added");
    assert_eq!(v.active_started_at, None);
}

// ---- 5.3 prompting ----------------------------------------------------------

fn echo(view: &mut SessionView, id: &str, client_id: &str, text: &str) {
    view.session_update(&json!({ "sessionId": "s1", "update": {
        "sessionUpdate": "user_message_chunk", "messageId": id,
        "content": { "type": "text", "text": text },
        "_meta": { "querymt": { "client_prompt_id": client_id } } } }));
}

fn text_block(text: &str) -> Vec<Value> {
    vec![json!({ "type": "text", "text": text })]
}

#[test]
fn prompt_replaced_by_its_echo() {
    let mut v = view();
    let blocks = vec![
        json!({ "type": "text", "text": "look" }),
        json!({ "type": "image", "data": "aGk=", "mimeType": "image/png",
            "_meta": { "querymt": { "filename": "a.png" } } }),
    ];
    v.prompt_optimistic("c1", &blocks);
    let ItemKind::User {
        local, attachments, ..
    } = &v.items[0].kind
    else {
        panic!("a pending prompt row");
    };
    assert!(*local);
    assert_eq!(attachments.len(), 1, "the image shows at once");
    v.session_update(&json!({ "sessionId": "s1", "update": {
        "sessionUpdate": "user_message_chunk", "messageId": "u1",
        "content": blocks[0],
        "_meta": { "querymt": { "client_prompt_id": "c1" } } } }));
    let users: Vec<_> = v
        .items
        .iter()
        .filter(|i| matches!(i.kind, ItemKind::User { .. }))
        .collect();
    assert_eq!(users.len(), 1, "the prompt shows once");
    assert_eq!(users[0].key, "user-u1");
    assert!(matches!(users[0].kind, ItemKind::User { local: false, .. }));
}

#[test]
fn two_identical_prompts() {
    let mut v = view();
    v.prompt_optimistic("c1", &text_block("again"));
    v.prompt_optimistic("c2", &text_block("again"));
    assert_eq!(texts(&v), ["again", "again"]);
    echo(&mut v, "u1", "c1", "again");
    // Only the first is confirmed; the second is still pending.
    let local: Vec<bool> = v
        .items
        .iter()
        .filter_map(|i| match &i.kind {
            ItemKind::User { local, .. } => Some(*local),
            _ => None,
        })
        .collect();
    assert_eq!(local, [false, true]);
    echo(&mut v, "u2", "c2", "again");
    assert_eq!(texts(&v), ["again", "again"]);
    assert!(v.items.iter().all(|i| !matches!(
        i.kind,
        ItemKind::User { local: true, .. }
    )));
}

#[test]
fn failed_prompt() {
    let mut v = view();
    v.prompt_optimistic("c1", &text_block("hello"));
    v.prompt_failed("c1", "boom");
    assert_eq!(texts(&v), ["hello", "notice: boom"]);
    assert!(matches!(
        v.items[1].kind,
        ItemKind::Notice { error: true, .. }
    ));
}

#[test]
fn pending_and_failed_prompts_survive_a_reload() {
    let mut v = view();
    v.load(snapshot_response("small_snapshot"));
    let loaded = texts(&v);
    v.prompt_optimistic("c1", &text_block("failed one"));
    v.prompt_failed("c1", "no connection");
    v.prompt_optimistic("c2", &text_block("pending one"));
    v.load(snapshot_response("small_snapshot"));
    let after = texts(&v);
    assert_eq!(after[..loaded.len()], loaded[..]);
    assert_eq!(
        after[loaded.len()..],
        ["failed one", "notice: no connection", "pending one"]
    );
}

// ---- 5.4 history, undo stack ------------------------------------------------

#[test]
fn older_page_keeps_the_view_stable() {
    let mut batches = trace("history_page");
    let page = batches.remove(1);
    let load = batches.remove(0);
    let mut v = view();
    v.feed(&load);
    let before: Vec<_> = v
        .items
        .iter()
        .map(|i| (i.key.clone(), i.rev, tool_ptrs(&i.kind)))
        .collect();
    assert!(v.history_more);
    v.feed(&page);
    let after: Vec<_> = v
        .items
        .iter()
        .map(|i| (i.key.clone(), i.rev, tool_ptrs(&i.kind)))
        .collect();
    assert!(after.len() > before.len(), "the page adds rows above");
    assert_eq!(
        after[after.len() - before.len()..],
        before[..],
        "rows already shown keep their key, revision and tool calls"
    );
    assert!(!v.history_more);
}

fn tool_ptrs(kind: &ItemKind) -> Vec<usize> {
    match kind {
        ItemKind::Work(entries) => entries
            .iter()
            .filter_map(|e| match e {
                WorkEntry::Tool(c) => Some(Arc::as_ptr(c) as usize),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn the_undo_stack_replaces_the_header_stack() {
    let mut v = view();
    v.set_undo_stack(&UndoFrame::parse_stack(&json!({ "undo_stack": [
        { "message_id": "u1", "restore_failed": false },
        { "message_id": "u2", "restore_failed": true },
    ] })));
    assert_eq!(v.undo_stack.len(), 2);
    assert!(v.undo_stack[1].restore_failed);
    v.set_undo_stack(&[]);
    assert!(v.undo_stack.is_empty());
}

// ---- 6.1 turn actions on runs -----------------------------------------------

fn frame(id: &str) -> UndoFrame {
    UndoFrame {
        message_id: id.into(),
        restore_failed: false,
    }
}

#[test]
fn a_steered_turn_is_one_turn() {
    let mut v = view();
    steered_run(&mut v, "r1");
    assert_eq!(v.turn_bounds().len(), 1);
    assert_eq!(v.turn_users().values().collect::<Vec<_>>(), ["r1-u1"]);
}

#[test]
fn undo_a_steered_turn() {
    let mut v = view();
    steered_run(&mut v, "r1");
    // The first prompt is the target; the whole run, steering included, goes.
    assert_eq!(v.undo_target().as_deref(), Some("r1-u1"));
    assert_eq!(v.undo_affected_turns("r1-u1"), Some(1));
    assert_eq!(v.undo_affected_turns("r1-u2"), None, "steering is not a target");
    v.set_undo_stack(&[frame("r1-u1")]);
    assert_eq!(v.reverted_from(), Some(0));
    assert_eq!(v.undo_target(), None);
    assert_eq!(v.undo_affected_turns("r1-u1"), None);
}

#[test]
fn pending_prompt_is_not_an_undo_target() {
    let mut v = view();
    steered_run(&mut v, "r1");
    steered_run(&mut v, "r2");
    v.prompt_optimistic("c1", &text_block("next"));
    assert_eq!(v.undo_target().as_deref(), Some("r2-u1"));
    assert_eq!(v.turn_bounds().len(), 3);
    assert!(
        !v.undoable().contains("c1") && v.undoable().len() == 2,
        "the pending turn is not undoable"
    );
}

#[test]
fn redo_restores_the_turn() {
    let mut v = view();
    steered_run(&mut v, "r1");
    steered_run(&mut v, "r2");
    v.set_undo_stack(&[frame("r2-u1")]);
    assert_eq!(v.redo_target(), Some("r2-u1"));
    assert_eq!(v.reverted_from(), Some(v.turn_bounds()[1].1.start));
    assert_eq!(v.undo_target().as_deref(), Some("r1-u1"));
    v.set_undo_stack(&[]);
    assert_eq!(v.reverted_from(), None);
    assert_eq!(v.redo_target(), None);
}

#[test]
fn fork_after_a_reply() {
    let mut v = view();
    steered_run(&mut v, "r1");
    let end = closing_row(&v, 0);
    let target = v.fork_target(end).expect("a settled turn forks");
    assert_eq!(target.message_id, "r1-a2", "after the turn's last reply");
    assert_eq!(target.prompt, "first");
    assert_eq!(target.response, "onetwo");
    assert!(target.includes_response);
}

#[test]
fn turn_still_streaming_without_ids() {
    let mut v = view();
    v.prompt_optimistic("c1", &text_block("hello"));
    assert_eq!(v.fork_point(0), None);
    assert!(v.fork_target(0).is_none());
}

#[test]
fn diff_of_a_steered_turn() {
    let mut v = view();
    steered_run(&mut v, "r1");
    let rows = v.turn_range(v.turn_key(0).unwrap()).unwrap();
    let files = v.turn_diffs(rows);
    let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a.rs", "b.rs"], "edits on both sides of the steering");
}

#[test]
fn copy_a_steered_turn() {
    let mut v = view();
    steered_run(&mut v, "r1");
    assert_eq!(v.turn_text(0), "one\n\ntwo");
}

#[test]
fn a_fork_branches_after_the_turns_last_reply() {
    let mut v = view();
    steered_run(&mut v, "r1");
    steered_run(&mut v, "r2");
    let first = v.fork_target(closing_row(&v, 0)).unwrap();
    let second = v.fork_target(closing_row(&v, 1)).unwrap();
    assert_eq!(first.message_id, "r1-a2");
    assert_eq!(second.message_id, "r2-a2");
}

#[test]
fn undo_targets_the_latest_turn_before_the_undo_frontier() {
    let mut v = view();
    for run in ["r1", "r2", "r3"] {
        steered_run(&mut v, run);
    }
    assert_eq!(v.undo_target().as_deref(), Some("r3-u1"));
    v.set_undo_stack(&[frame("r3-u1")]);
    assert_eq!(v.undo_target().as_deref(), Some("r2-u1"));
    assert_eq!(v.undo_affected_turns("r2-u1"), Some(1));
    v.set_undo_stack(&[frame("r3-u1"), frame("r2-u1")]);
    assert_eq!(v.undo_target().as_deref(), Some("r1-u1"));
    assert_eq!(v.undo_affected_turns("r1-u1"), Some(1));
}

// ---- 5.5 batching and background hydration ----------------------------------

#[test]
fn inputs_during_background_hydration_keep_their_order() {
    // The same inputs on a view that loads inline, for comparison.
    let inline = {
        let mut v = view();
        v.load(snapshot_response("small_snapshot"));
        agent(&mut v, "live", "m-live-1", "first live chunk");
        agent(&mut v, "live", "m-live-2", "second live chunk");
        v.prompt_optimistic("c1", &text_block("typed while loading"));
        texts(&v)
    };

    let mut v = view();
    let job = v
        .begin_load(snapshot_response("small_snapshot"))
        .unwrap_or_else(|_| panic!("a view with no turns hydrates"));
    assert!(v.hydrating());
    // Live notifications and a prompt arrive while the snapshot decodes.
    v.queue_session_update(&json!({ "sessionId": "s1", "update": {
        "sessionUpdate": "agent_message_chunk", "messageId": "m-live-1",
        "content": { "type": "text", "text": "first live chunk" },
        "_meta": { "querymt": { "runId": "live" } } } }));
    agent(&mut v, "live", "m-live-2", "second live chunk");
    v.prompt_optimistic("c1", &text_block("typed while loading"));
    assert!(v.items.is_empty(), "nothing is applied before the load");
    assert!(v.has_queued());

    // Decoded elsewhere, then handed back.
    let hydrated = std::thread::spawn(move || job.hydrate()).join().unwrap();
    v.finish_load(hydrated);
    assert!(!v.hydrating());
    assert!(!v.has_queued());
    assert_eq!(texts(&v), inline, "the inputs follow the load, in order");
}

#[test]
fn a_burst_of_chunks_is_one_batch() {
    let mut v = view();
    state(&mut v, "r1", "running");
    for i in 0..50 {
        v.queue_session_update(&json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "agent_message_chunk", "messageId": "m1",
            "content": { "type": "text", "text": format!("{i} ") },
            "_meta": { "querymt": { "runId": "r1" } } } }));
    }
    assert!(v.items.is_empty(), "queued, not applied");
    let report = v.flush();
    assert_eq!(report.updated.len() + report.added.len(), 1, "one turn, one report");
    assert_eq!(v.items.len(), 1);
    assert!(!v.has_queued());
}

/// A load response of `turns` runs (prompt, reasoning, reply, two tool calls
/// each), on the shape of the golden `small_snapshot` trace.
fn long_snapshot(turns: usize) -> Value {
    let mut response = snapshot_response("small_snapshot");
    let mut events = Vec::new();
    let mut seq = 0u64;
    let mut push = |kind: &str, data: Value| {
        seq += 1;
        events.push(json!({ "seq": seq, "session_seq": seq, "timestamp": 1_000_000 + seq,
            "session_id": "s1", "origin": "local", "kind": { "type": kind, "data": data } }));
    };
    for t in 0..turns {
        let run = format!("r{t}");
        push("run_started", json!({ "run_id": run, "origin": "interactive" }));
        push("prompt_received", json!({ "content": format!("prompt {t}"), "message_id": format!("u{t}") }));
        push("assistant_message_stored", json!({
            "content": format!("reply {t} {}", "lorem ipsum ".repeat(40)),
            "reasoning_parts": [{ "id": format!("a{t}:summary:0"), "text": "thinking about it" }],
            "message_id": format!("a{t}") }));
        for k in 0..2 {
            let id = format!("t{t}-{k}");
            push("tool_call_start", json!({ "tool_call_id": id, "tool_name": "shell",
                "arguments": "{\"cmd\":\"ls\"}" }));
            push("tool_call_end", json!({ "tool_call_id": id, "tool_name": "shell",
                "is_error": false, "result": "file_a\nfile_b\n".repeat(20) }));
        }
        push("run_completed", json!({ "run_id": run, "outcome": "completed" }));
    }
    response["_meta"]["querymt/sessionLoadSnapshot.v1"]["audit"]["events"] = json!(events);
    response
}

#[test]
fn a_long_snapshot_loads_into_one_turn_per_run() {
    let mut v = view();
    v.load(long_snapshot(200));
    assert_eq!(v.turn_bounds().len(), 200);
}

/// `session.apply_ms` of a 200-turn load with the session core and its
/// projection (run by hand:
/// `cargo nextest run --bins --run-ignored only -E 'test(/apply_ms/)' --no-capture`).
/// The old reducer measured 23.3-23.8 ms on this snapshot in the same debug
/// build, before it was removed (task 5.5).
#[test]
#[ignore = "timing measurement, run by hand"]
fn apply_ms_of_a_200_turn_load() {
    let response = long_snapshot(200);
    // The response is moved into the load, as the store does: clones are made
    // before the clock starts.
    let mut responses: Vec<Value> = (0..5).map(|_| response.clone()).collect();
    let new = (0..5)
        .map(|_| {
            let response = responses.pop().unwrap();
            let start = std::time::Instant::now();
            let mut v = view();
            v.load(response);
            start.elapsed().as_secs_f64() * 1000.
        })
        .fold(f64::MAX, f64::min);
    eprintln!("session.apply_ms, 200 turns: core + projection {new:.1} ms");
}

#[test]
fn a_turn_diff_groups_changes_by_file() {
    let mut v = view();
    state(&mut v, "r1", "running");
    user(&mut v, "r1", "u1", "change things");
    for (id, path, new) in [
        ("t1", "./src/a.rs", "a\nB\n"),
        ("t2", "src/b.rs", "x\ny\n"),
        ("t3", "src/a.rs", "a\nB\nc\n"),
    ] {
        edit(&mut v, "r1", id, path, new);
    }
    state(&mut v, "r1", "idle");
    state(&mut v, "r2", "running");
    user(&mut v, "r2", "u2", "next");
    edit(&mut v, "r2", "t4", "src/c.rs", "z\n");
    state(&mut v, "r2", "idle");
    let first = v.turn_bounds()[0].1.clone();
    let files = v.turn_diffs(first);
    let summary: Vec<_> = files
        .iter()
        .map(|f| (f.path.as_str(), f.changes.len()))
        .collect();
    // First-touched order, `./` dropped, each tool's change kept apart; the
    // next turn's edit isn't included.
    assert_eq!(summary, [("src/a.rs", 2), ("src/b.rs", 1)]);
}

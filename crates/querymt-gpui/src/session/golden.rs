//! Projection tests over whole scenarios (design D8, after the old
//! reducer's removal).
//!
//! - The gpui scenarios ported from the old reducer's tests are fed to the
//!   projection and the rows it shows are compared with the files under
//!   `tests/golden/session/`. A deliberate change of what the rows show is
//!   made by running the test with `UPDATE_GOLDEN=1` and reviewing the diff.
//! - QueryMT's own golden cases (`testing::golden_cases()`, generated from a
//!   seed at test time, nothing stored here) are fed batch by batch; the core
//!   must report no errors, every turn must show rows, and the rows built
//!   incrementally must equal a rebuild of every turn from scratch.

use std::path::PathBuf;

use querymt_agent_session_core::input::SessionInput;

use super::projection::Projection;
use super::{CompactionStatus, Item, ItemKind, WorkEntry};

/// What a row shows, independent of keys and revisions.
#[derive(Clone, Debug, PartialEq)]
enum Row {
    User {
        text: String,
        local: bool,
    },
    Agent(String),
    Thought(String),
    Tool {
        status: String,
        output: String,
        child: Option<String>,
    },
    Compaction {
        status: &'static str,
        summary: String,
        before: Option<u64>,
        after: Option<u64>,
    },
    Notice(String),
}

fn rows(items: &[Item]) -> Vec<Row> {
    let mut out = Vec::new();
    for item in items {
        match &item.kind {
            ItemKind::User { text, local, .. } => out.push(Row::User {
                text: text.clone(),
                local: *local,
            }),
            ItemKind::Agent { text } => out.push(Row::Agent(text.clone())),
            ItemKind::Work(entries) => {
                for entry in entries {
                    out.push(match entry {
                        WorkEntry::Thought(t) => Row::Thought(t.text.clone()),
                        WorkEntry::Tool(c) => Row::Tool {
                            status: c.status.clone(),
                            output: c.output().to_owned(),
                            child: c.child_session.clone(),
                        },
                    });
                }
            }
            ItemKind::Notice { text, .. } => out.push(Row::Notice(text.clone())),
            ItemKind::Compaction(c) => out.push(Row::Compaction {
                status: match c.status {
                    CompactionStatus::InProgress => "in_progress",
                    CompactionStatus::Completed => "completed",
                    CompactionStatus::Failed => "failed",
                    CompactionStatus::Cancelled => "cancelled",
                },
                summary: c.summary.clone(),
                before: c.tokens_before,
                after: c.tokens_after,
            }),
        }
    }
    out
}

/// QueryMT's golden cases at the pinned rev, by name.
fn core_cases() -> Vec<(String, Vec<Vec<SessionInput>>)> {
    querymt_agent_session_core::testing::golden_cases()
        .into_iter()
        .map(|case| (case.name, case.batches))
        .collect()
}

/// File name of a scenario's golden: lowercase words joined by `-`, portable
/// to every file system.
fn slug(name: &str) -> String {
    name.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Scenarios ported from the old reducer's own tests (`history_tests`,
/// `compaction_tests`, `snapshot_tests`, `undo_tests`, `input_tests`).
mod gpui_fixtures {
    use serde_json::{Value, json};

    use super::SessionInput;

    type Batches = Vec<Vec<SessionInput>>;

    pub fn live(update: Value) -> SessionInput {
        SessionInput::AcpNotification {
            notification: json!({ "sessionId": "s1", "update": update }),
            replayed: false,
            at_ms: 0,
        }
    }

    fn input(value: Value) -> SessionInput {
        serde_json::from_value(value).expect("input")
    }

    pub fn hev(seq: i64, kind: &str, data: Value) -> Value {
        json!({ "seq": seq, "session_seq": seq, "timestamp": seq * 100, "session_id": "s1",
                "origin": "local", "kind": { "type": kind, "data": data } })
    }

    pub fn snapshot(events: Vec<Value>, extra: Value) -> SessionInput {
        let mut snapshot = json!({
            "runtimeStatus": { "phase": "idle", "active_run_id": null, "steerable": false,
                "pending_steering_count": 0, "queued_input_count": 0, "run_started_at_ms": null,
                "observation": { "availability": "live" } },
            "audit": { "session_id": "s1", "events": events, "tasks": [], "intent_snapshots": [],
                "decisions": [], "progress_entries": [], "artifacts": [], "delegations": [],
                "generated_at": "2026-01-01T00:00:00Z" },
            "cursor": { "local_seq": 0, "remote_seq_by_source": {} },
            "accounting": { "version": 1, "session": { "knownSubtotalUsd": 0.0,
                "completeTotalUsd": 0.0, "complete": true, "billing": "empty", "reasons": [],
                "pricedAttempts": 0, "excludedAttempts": 0, "unresolvedAttempts": 0,
                "incompleteAttempts": 0, "unaccountedGenerations": 0 }, "cycle": null },
            "subagentUpdates": []
        });
        for (key, value) in extra.as_object().into_iter().flatten() {
            snapshot[key] = value.clone();
        }
        input(json!({ "type": "snapshot_loaded", "data": { "response": {
            "_meta": { "querymt/sessionLoadSnapshot.v1": snapshot } } } }))
    }

    fn user(id: &str, text: &str) -> SessionInput {
        live(
            json!({ "sessionUpdate": "user_message_chunk", "messageId": id,
                     "content": { "type": "text", "text": text } }),
        )
    }

    fn agent(id: &str, text: &str) -> SessionInput {
        live(
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": id,
                     "content": { "type": "text", "text": text } }),
        )
    }

    fn thought(id: &str, text: &str, part: Option<&str>) -> SessionInput {
        let mut update = json!({ "sessionUpdate": "agent_thought_chunk", "messageId": id,
                                 "content": { "type": "text", "text": text } });
        if let Some(part) = part {
            update["_meta"] = json!({ "querymt": { "reasoning_part_id": part } });
        }
        live(update)
    }

    fn optimistic(id: &str, text: &str) -> SessionInput {
        input(json!({ "type": "prompt_optimistic", "data": {
            "client_prompt_id": id, "blocks": [{ "type": "text", "text": text }], "at_ms": 1 } }))
    }

    fn runtime(phase: &str, run: Option<&str>) -> SessionInput {
        input(json!({ "type": "runtime_observed", "data": { "status": {
            "phase": phase, "active_run_id": run, "steerable": false,
            "pending_steering_count": 0, "queued_input_count": 0, "run_started_at_ms": null,
            "observation": { "availability": "live" } } } }))
    }

    fn edit(id: &str, path: &str, old: &str, new: &str) -> SessionInput {
        live(
            json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": "edit",
            "kind": "edit", "status": "completed",
            "content": [{ "type": "diff", "path": path, "oldText": old, "newText": new }] }),
        )
    }

    fn delegation_json(tool: &str, public: &str) -> Value {
        json!({ "tool_call_id": tool, "delegation": {
            "public_id": public, "target_agent_id": "coder", "objective": "o",
            "objective_hash": "779a65e7023cd2e7", "context": null, "constraints": null,
            "expected_output": null, "planning_summary": null, "status": "requested",
            "retry_count": 0, "created_at": "2026-01-01T00:00:00Z", "completed_at": null } })
    }

    fn forked_json(child: &str, public: &str) -> Value {
        json!({ "parent_session_id": "s1", "child_session_id": child,
                "target_agent_id": "coder", "origin": "delegation",
                "fork_point_type": "progress_entry", "fork_point_ref": public,
                "instructions": null })
    }

    fn transcript_batches() -> Batches {
        vec![vec![snapshot(
            vec![
                hev(
                    1,
                    "prompt_received",
                    json!({ "message_id": "u1", "content": "hi" }),
                ),
                hev(
                    2,
                    "assistant_thinking_delta",
                    json!({ "message_id": "a1", "content": "hmm" }),
                ),
                hev(
                    3,
                    "tool_call_start",
                    json!({ "tool_call_id": "t1", "tool_name": "shell",
                            "arguments": "{\"command\":\"ls\"}" }),
                ),
                hev(
                    4,
                    "tool_call_end",
                    json!({ "tool_call_id": "t1", "tool_name": "shell",
                            "is_error": false, "result": "ok" }),
                ),
                hev(
                    5,
                    "tool_call_start",
                    json!({ "tool_call_id": "td", "tool_name": "todowrite",
                            "arguments": "{\"todos\":[{\"content\":\"a\",\"status\":\"pending\"},{\"content\":\"b\",\"status\":\"cancelled\"}]}" }),
                ),
                hev(
                    6,
                    "assistant_content_delta",
                    json!({ "message_id": "a1", "content": "partial" }),
                ),
                hev(
                    7,
                    "assistant_message_stored",
                    json!({ "message_id": "a1",
                            "content": "final reply", "reasoning_parts": [{ "id": "p", "text": "hmm" }] }),
                ),
                hev(
                    8,
                    "tool_call_start",
                    json!({ "tool_call_id": "t2", "tool_name": "read_tool",
                            "arguments": "{}" }),
                ),
                hev(
                    9,
                    "assistant_message_stored",
                    json!({ "message_id": "a2", "content": "later" }),
                ),
            ],
            json!({ "userPrompts": [
                        { "messageId": "u1", "messageOrder": 0, "timestamp": 100, "blocks": [
                            { "type": "text", "text": "hi" },
                            { "type": "image", "mimeType": "image/png", "data": "AAA" } ] },
                        { "messageId": "s1", "messageOrder": 3, "timestamp": 850, "blocks": [
                            { "type": "text", "text": "steer" } ] } ] }),
        )]]
    }

    /// A current agent journals a steering prompt as `prompt_received`; the
    /// legacy fixture only listed it in `userPrompts`.
    fn steering_as_event(mut batches: Batches) -> Batches {
        for input in batches.iter_mut().flatten() {
            let mut value = serde_json::to_value(&*input).expect("input serializes");
            let events = value
                .pointer_mut("/data/response/_meta/querymt~1sessionLoadSnapshot.v1/audit/events")
                .and_then(Value::as_array_mut);
            if let Some(events) = events {
                if let Some(last) = events.last_mut() {
                    last["seq"] = json!(10);
                    last["session_seq"] = json!(10);
                    last["timestamp"] = json!(1000);
                }
                let at = events.len() - 1;
                events.insert(
                    at,
                    hev(
                        9,
                        "prompt_received",
                        json!({ "message_id": "s1", "content": "steer" }),
                    ),
                );
                *input = serde_json::from_value(value).expect("input");
            }
        }
        batches
    }

    pub fn all() -> Vec<(&'static str, Batches)> {
        let tool = |id: &str, name: &str| {
            hev(
                1,
                "tool_call_start",
                json!({ "tool_call_id": id, "tool_name": name, "arguments": "{}" }),
            )
        };
        let turns = |ids: &[&str]| -> Vec<SessionInput> {
            ids.iter()
                .flat_map(|id| [user(id, "prompt"), agent(&format!("r-{id}"), "reply")])
                .collect()
        };
        vec![
            (
                "history: delegation events link children",
                vec![vec![snapshot(
                    vec![
                        tool("t1", "delegate"),
                        hev(2, "delegation_requested", delegation_json("t1", "d1")),
                        hev(3, "session_forked", forked_json("c1", "d1")),
                        hev(
                            4,
                            "tool_call_start",
                            json!({ "tool_call_id": "t2", "tool_name": "delegate", "arguments": "{}" }),
                        ),
                        hev(5, "session_forked", forked_json("c2", "d2")),
                        hev(6, "delegation_requested", delegation_json("t2", "d2")),
                    ],
                    json!({}),
                )]],
            ),
            (
                "history: late reasoning does not split the reply",
                vec![vec![
                    user("u1", "go"),
                    agent("m1", "in qmac"),
                    thought(
                        "m1",
                        "**Inspecting replay test mocks**",
                        Some("rs:summary:0"),
                    ),
                    agent("m1", "p."),
                ]],
            ),
            (
                "history: late reasoning joins the work group",
                vec![vec![
                    user("u1", "go"),
                    live(json!({ "sessionUpdate": "tool_call", "toolCallId": "t1",
                        "title": "Shell", "kind": "execute", "status": "completed" })),
                    agent("m1", "Done"),
                    live(json!({ "sessionUpdate": "tool_call", "toolCallId": "t2",
                        "title": "Shell", "kind": "execute", "status": "in_progress" })),
                    thought("m1", "late", None),
                    live(
                        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t2",
                        "status": "completed" }),
                    ),
                ]],
            ),
            (
                "history: live reasoning summaries stay separate",
                vec![vec![
                    thought("m1", "**Inspecting ", Some("rs_1:summary:0")),
                    thought("m1", "fixtures**", Some("rs_1:summary:0")),
                    thought("m1", "**Adding checks**", Some("rs_1:summary:1")),
                    thought("m1", "raw", None),
                ]],
            ),
            (
                "history: stored reasoning parts keep their ids",
                vec![vec![snapshot(
                    vec![hev(
                        1,
                        "assistant_message_stored",
                        json!({ "message_id": "m1", "content": "done", "reasoning_parts": [
                            { "id": "rs_1:summary:0", "text": "**One**" },
                            { "id": "rs_1:summary:1", "text": "**Two**" } ] }),
                    )],
                    json!({}),
                )]],
            ),
            (
                "history: echoed images are not duplicated",
                vec![vec![
                    optimistic("p1", "hi"),
                    // The agent echoes the client prompt id (never matched by text).
                    live(
                        json!({ "sessionUpdate": "user_message_chunk", "messageId": "u1",
                        "content": { "type": "text", "text": "hi" },
                        "_meta": { "querymt": { "client_prompt_id": "p1" } } }),
                    ),
                    live(
                        json!({ "sessionUpdate": "user_message_chunk", "messageId": "u1",
                        "content": { "type": "image", "mimeType": "image/png", "data": "x",
                                     "_meta": { "querymt": { "filename": "a.png" } } } }),
                    ),
                ]],
            ),
            (
                "history: older page is prepended in order",
                vec![
                    vec![snapshot(
                        vec![
                            hev(
                                5,
                                "prompt_received",
                                json!({ "content": "second", "message_id": "u2" }),
                            ),
                            hev(
                                6,
                                "assistant_message_stored",
                                json!({ "content": "reply 2", "message_id": "a2" }),
                            ),
                        ],
                        json!({ "history": { "complete": false, "before": "t1" } }),
                    )],
                    vec![input(
                        json!({ "type": "history_page", "data": { "response": {
                        "events": [
                            hev(1, "prompt_received", json!({ "content": "first", "message_id": "u1" })),
                            hev(2, "tool_call_start", json!({ "tool_call_id": "t1", "tool_name": "shell",
                                "arguments": "{\"command\":\"ls\"}" })),
                            hev(3, "tool_call_end", json!({ "tool_call_id": "t1", "tool_name": "shell",
                                "is_error": false, "result": "ok" })),
                            hev(4, "assistant_message_stored",
                                json!({ "content": "reply 1", "message_id": "a1" })),
                        ],
                        "history": { "complete": true, "before": null },
                        "truncated": [] } } }),
                    )],
                ],
            ),
            (
                "compaction: live updates stream the summary",
                vec![vec![
                    live(
                        json!({ "sessionUpdate": "usage_update", "used": 120000, "size": 200000 }),
                    ),
                    live(
                        json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                        "status": "in_progress" }),
                    ),
                    live(
                        json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "c1",
                        "content": { "type": "text", "text": "Summary " } }),
                    ),
                    live(
                        json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "c1",
                        "content": { "type": "text", "text": "text" } }),
                    ),
                    live(
                        json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                        "status": "completed" }),
                    ),
                    live(json!({ "sessionUpdate": "usage_update", "used": 18000, "size": 200000 })),
                    live(
                        json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                        "status": "in_progress" }),
                    ),
                ]],
            ),
            (
                "compaction: a failed compaction shows its reason",
                vec![vec![live(json!({ "sessionUpdate": "compaction_update",
                    "compactionId": "c1", "status": "failed", "error": "model refused" }))]],
            ),
            (
                "compaction: load snapshot places compactions after their turn",
                vec![
                    vec![user("u1", "hi"), agent("a1", "reply"), user("u2", "next")],
                    vec![snapshot(
                        vec![
                            hev(
                                1,
                                "prompt_received",
                                json!({ "message_id": "u1", "content": "hi" }),
                            ),
                            hev(
                                2,
                                "llm_request_end",
                                json!({ "usage": null, "tool_calls": 0,
                                "finish_reason": null, "cost_usd": null, "cumulative_cost_usd": null,
                                "context_tokens": 90000, "metrics": { "steps": 1, "turns": 1 } }),
                            ),
                            hev(
                                3,
                                "assistant_message_stored",
                                json!({ "message_id": "a1", "content": "reply" }),
                            ),
                            hev(4, "compaction_start", json!({ "token_estimate": 10 })),
                            hev(
                                6,
                                "compaction_end",
                                json!({ "summary": "Final summary", "summary_len": 13, "context_tokens": 9000 }),
                            ),
                            hev(
                                7,
                                "compaction_start",
                                json!({ "compaction_id": "c2", "token_estimate": 1 }),
                            ),
                            hev(
                                8,
                                "prompt_received",
                                json!({ "message_id": "u2", "content": "next" }),
                            ),
                        ],
                        json!({}),
                    )],
                ],
            ),
            (
                "snapshot: transcript from the load snapshot (legacy steering prompt without an event)",
                transcript_batches(),
            ),
            (
                "snapshot: replayed transcript is not duplicated",
                vec![
                    vec![user("u1", "hi")],
                    vec![snapshot(
                        vec![hev(
                            1,
                            "prompt_received",
                            json!({ "message_id": "u1", "content": "hi" }),
                        )],
                        json!({}),
                    )],
                ],
            ),
            (
                "undo: a turn with edits",
                vec![{
                    let mut batch = turns(&["u1"]);
                    batch.push(edit("t1", "./src/a.rs", "a\nb\n", "a\nB\n"));
                    batch.push(edit("t2", "src/b.rs", "", "x\ny\n"));
                    batch.push(user("u2", "next"));
                    batch.push(edit("t4", "src/c.rs", "", "z\n"));
                    batch
                }],
            ),
            (
                "undo: fork with an anonymous reply and a pending prompt",
                vec![{
                    let mut batch = turns(&["u1", "u2"]);
                    batch.push(user("u3", "third"));
                    batch.push(live(json!({ "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "anonymous" } })));
                    batch.push(optimistic("p1", "pending"));
                    batch
                }],
            ),
            (
                "snapshot: transcript from the load snapshot (steering prompt journaled as an event)",
                steering_as_event(transcript_batches()),
            ),
            (
                "input: runtime phase and run id",
                vec![vec![runtime("tools", Some("r1")), runtime("idle", None)]],
            ),
        ]
    }
}

/// What the projection shows after every batch, as text: rows, then the plan.
fn shown(batches: &[Vec<SessionInput>]) -> String {
    let mut p = Projection::new("s1");
    let mut errors = Vec::new();
    for batch in batches {
        errors.extend(p.feed(batch).errors.iter().map(|e| e.message.clone()));
    }
    assert!(errors.is_empty(), "the core reported errors: {errors:?}");
    let mut out = String::new();
    for row in rows(&p.items) {
        out.push_str(&format!("{row:?}\n"));
    }
    for entry in p.header_fields().plan {
        out.push_str(&format!("plan: {} [{}]\n", entry.content, entry.status));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every ported gpui scenario matches its golden file.
    #[test]
    fn golden_fixtures() {
        let ported = gpui_fixtures::all();
        assert_eq!(ported.len(), 16, "every ported gpui scenario runs");
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/session");
        let update = std::env::var_os("UPDATE_GOLDEN").is_some();
        let mut failures = Vec::new();
        for (name, batches) in ported {
            let shown = shown(&batches);
            let path = dir.join(format!("{}.txt", slug(name)));
            if update {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &shown).unwrap();
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(golden) if golden == shown => {}
                Ok(golden) => failures.push(format!(
                    "{name}: rows changed\n--- golden\n{golden}--- shown\n{shown}"
                )),
                Err(_) => failures.push(format!(
                    "{name}: no golden file {} (run with UPDATE_GOLDEN=1)",
                    path.display()
                )),
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// QueryMT's golden cases, fed batch by batch: no core errors, rows for
    /// every turn, and incremental rows equal to a rebuild from scratch.
    #[test]
    fn core_cases_project_consistently() {
        let cases = core_cases();
        assert!(!cases.is_empty(), "the pinned core has golden cases");
        let mut failures = Vec::new();
        for (name, batches) in cases {
            let mut p = Projection::new("s1");
            for (at, batch) in batches.iter().enumerate() {
                let report = p.feed(batch);
                if !report.errors.is_empty() {
                    failures.push(format!("{name}: batch {at}: core errors {:?}", report.errors));
                }
            }
            for (turn, rows) in p.turn_bounds() {
                if rows.is_empty() {
                    failures.push(format!("{name}: turn {turn} shows no rows"));
                }
            }
            let incremental = rows(&p.items);
            p.rebuild_all();
            let rebuilt = rows(&p.items);
            if incremental != rebuilt {
                failures.push(format!(
                    "{name}: incremental rows differ from a rebuild\n  incremental {incremental:#?}\n  rebuilt {rebuilt:#?}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn slugs_are_portable_file_names() {
        assert_eq!(
            slug("compaction: a failed compaction shows its reason"),
            "compaction-a-failed-compaction-shows-its-reason"
        );
        assert_eq!(
            slug("snapshot: transcript from the load snapshot (legacy steering prompt without an event)"),
            "snapshot-transcript-from-the-load-snapshot-legacy-steering-prompt-without-an-event"
        );
    }
}

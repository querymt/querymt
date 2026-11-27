//! Conformance of the agent's output with the session view reducer
//! (`querymt-agent-session-core`).
//!
//! A generated journal reaches a client three ways: as a load snapshot, as
//! the live stream the connection forwarder sends, and as a history replay.
//! A fresh `SessionCore` must end with the same view from all three, so a
//! change to the agent's translation that the reducer cannot follow fails
//! here, without a running agent.

use super::shared::{
    AcpLiveEventTranslator, AcpSessionUpdateCapabilities, replay_agent_events_materialized,
};
use crate::session::backend::StorageBackend as _;
use crate::session::history_window::LoadWindow;
use crate::session::load_snapshot::{load_session_snapshot, load_session_snapshot_windowed};
use crate::session::store::SessionStore as _;
use querymt_agent_core::events::{
    AgentEvent, AgentEventKind, Durability, EphemeralEvent, EventEnvelope, EventOrigin,
    classify_durability,
};
use querymt_agent_core::protocol::{ContentBlock, TextContent};
use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::input::SessionInput;
use querymt_agent_session_core::testing::{TraceSpec, generate};
use querymt_agent_session_core::view::RunState;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const SESSION: &str = "s1";

/// The capabilities querymt-gpui negotiates.
pub(super) fn gpui_capabilities() -> AcpSessionUpdateCapabilities {
    AcpSessionUpdateCapabilities {
        plan_operations: true,
        notices: true,
        compaction: true,
        cursors: true,
        subagents: true,
    }
}

/// The generator's journal as the agent's own event type.
pub(super) fn journal(spec: TraceSpec) -> Vec<AgentEvent> {
    generate(spec)
        .events
        .into_iter()
        .map(|event| serde_json::from_value(event).expect("a generated event decodes"))
        .collect()
}

/// The `session/load` responses of an agent that journaled `events`, built
/// by the agent's own snapshot builder from a real session: first the way
/// gpui receives it (a load window that covers the whole session, so the
/// summary comes from the agent's summarizer), then without a window. The
/// session's id is returned with them.
pub(super) async fn agent_snapshots(events: &[AgentEvent]) -> (String, Value, Value) {
    let (sid, windowed, unwindowed, _) = agent_snapshots_with_journal(events).await;
    (sid, windowed, unwindowed)
}

/// [`agent_snapshots`], plus the events as the journal stored them (the
/// storage stamps its own timestamps), under the id `SESSION`.
pub(super) async fn agent_snapshots_with_journal(
    events: &[AgentEvent],
) -> (String, Value, Value, Vec<AgentEvent>) {
    let fixture = crate::session::run_reconcile::tests::fixture().await;
    let sid = fixture
        .storage
        .create_session(None, None, None, None)
        .await
        .expect("a session")
        .public_id;
    for event in events {
        // Streamed-only events are never journaled.
        if classify_durability(&event.kind) == Durability::Ephemeral {
            continue;
        }
        let mut kind = event.kind.clone();
        // Attempts are charged to the session that journals them.
        querymt_agent_session_core::testing::charge_to_session(&mut kind, &sid);
        crate::session::run_reconcile::tests::seed(&fixture.storage, &sid, kind).await;
    }
    let agent = crate::agent::LocalAgentHandle::from_config(fixture.config.clone());
    let view_store = fixture.storage.view_store().expect("a view store");
    let windowed = load_session_snapshot_windowed(
        &agent,
        view_store.clone(),
        &sid,
        Some(LoadWindow {
            max_turns: 100,
            ..LoadWindow::default()
        }),
    )
    .await
    .expect("the windowed snapshot builds");
    assert!(
        windowed.history.as_ref().is_some_and(|h| h.complete),
        "the window covers the session"
    );
    let unwindowed = load_session_snapshot(&agent, view_store, &sid)
        .await
        .expect("the snapshot builds");
    // What the transport sends: the snapshot scoped to gpui's capabilities.
    let response = |snapshot: &crate::session::load_snapshot::SessionLoadSnapshot| {
        let mut response = json!({ "_meta": { "querymt/sessionLoadSnapshot.v1": snapshot } });
        crate::acp::shared::scope_load_cursor_updates(
            response["_meta"].as_object_mut(),
            gpui_capabilities(),
        );
        response
    };
    let stored = crate::session::projection::EventJournal::load_session_stream(
        fixture.storage.as_ref(),
        &sid,
        None,
        None,
    )
    .await
    .expect("the stored journal")
    .into_iter()
    .map(AgentEvent::from)
    .map(|mut event| {
        event.session_id = SESSION.to_owned();
        event
    })
    .collect();
    (sid, response(&windowed), response(&unwindowed), stored)
}

fn notification(update: &impl serde::Serialize) -> SessionInput {
    SessionInput::AcpNotification {
        notification: json!({ "sessionId": SESSION, "update": update }),
        replayed: false,
        at_ms: 0,
    }
}

fn ephemeral(seq: i64, kind: AgentEventKind) -> EventEnvelope {
    EventEnvelope::Ephemeral(EphemeralEvent {
        session_id: SESSION.to_owned(),
        session_seq: seq,
        timestamp: 1_000_000 + seq,
        origin: EventOrigin::Local,
        source_node: None,
        kind,
    })
}

/// A message's text split into three pieces (the last one possibly empty).
fn split_text(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let third = chars.len().div_ceil(3).max(1);
    chars
        .chunks(third)
        .map(|piece| piece.iter().collect())
        .collect()
}

/// The envelopes the agent publishes for one journal event: the journal event
/// itself, preceded by what is only streamed live (the structured blocks of a
/// prompt, the deltas of a message).
pub(super) fn live_envelopes(
    event: &AgentEvent,
    blocked: &mut HashSet<String>,
) -> Vec<EventEnvelope> {
    let mut envelopes = Vec::new();
    match &event.kind {
        AgentEventKind::UserPromptBlock { message_id, .. } => {
            blocked.insert(message_id.clone());
        }
        AgentEventKind::PromptReceived {
            content,
            message_id: Some(message_id),
        } if !blocked.contains(message_id) => {
            envelopes.push(ephemeral(
                event.seq,
                AgentEventKind::UserPromptBlock {
                    message_id: message_id.clone(),
                    client_prompt_id: None,
                    block: ContentBlock::Text(TextContent::new(content.clone())),
                },
            ));
        }
        AgentEventKind::AssistantMessageStored {
            content,
            reasoning_parts,
            message_id: Some(message_id),
            ..
        } => {
            for part in reasoning_parts {
                for piece in split_text(&part.text) {
                    envelopes.push(ephemeral(
                        event.seq,
                        AgentEventKind::AssistantThinkingDelta {
                            content: piece,
                            message_id: message_id.clone(),
                            part_id: Some(part.id.clone()),
                        },
                    ));
                }
            }
            for piece in split_text(content) {
                envelopes.push(ephemeral(
                    event.seq,
                    AgentEventKind::AssistantContentDelta {
                        content: piece,
                        message_id: message_id.clone(),
                    },
                ));
            }
        }
        // The agent streams a compaction's summary as chunks before the end.
        AgentEventKind::CompactionEnd {
            summary,
            compaction_id: Some(compaction_id),
            ..
        } => {
            for piece in split_text(summary) {
                envelopes.push(ephemeral(
                    event.seq,
                    AgentEventKind::CompactionSummaryChunk {
                        compaction_id: compaction_id.clone(),
                        content: piece,
                    },
                ));
            }
        }
        _ => {}
    }
    envelopes.push(EventEnvelope::from(event.clone()));
    envelopes
}

/// What a connection forwards for `envelopes`, in order: state updates and
/// session updates.
pub(super) fn forwarded(
    translator: &mut AcpLiveEventTranslator,
    envelopes: &[EventEnvelope],
) -> Vec<SessionInput> {
    let mut inputs = Vec::new();
    for envelope in envelopes {
        if let Some(state) = translator.translate_state_update(envelope) {
            inputs.push(SessionInput::StateUpdate {
                notification: serde_json::to_value(&state).expect("a state update serializes"),
                at_ms: 0,
            });
        }
        for update in translator.translate_updates(envelope) {
            inputs.push(notification(&update));
        }
    }
    inputs
}

/// The whole journal as the live stream of a connection that saw all of it.
pub(super) fn live_stream(events: &[AgentEvent]) -> Vec<SessionInput> {
    let mut translator = AcpLiveEventTranslator::new();
    translator.set_capabilities(gpui_capabilities());
    let mut blocked = HashSet::new();
    let mut inputs = Vec::new();
    for event in events {
        let envelopes = live_envelopes(event, &mut blocked);
        inputs.extend(forwarded(&mut translator, &envelopes));
    }
    inputs
}

/// The journal as the replayed `session/update`s of a load without a snapshot.
fn replay_stream(events: &[AgentEvent]) -> Vec<SessionInput> {
    replay_agent_events_materialized(
        SESSION,
        events.iter().cloned(),
        &HashMap::new(),
        &gpui_capabilities(),
    )
    .into_iter()
    .map(|notification| SessionInput::AcpNotification {
        notification: serde_json::to_value(&notification).expect("a notification serializes"),
        replayed: true,
        at_ms: 0,
    })
    .collect()
}

fn without_revisions(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("revision");
            map.values_mut().for_each(without_revisions);
        }
        Value::Array(items) => items.iter_mut().for_each(without_revisions),
        _ => {}
    }
}

/// What the three paths must agree on, with no exceptions: turns (ids
/// included) with their entries (compaction summary blocks included), the
/// plan (plan id included), context usage, token breakdown, cost, the
/// accounting detail, the title, the session cursors (ids, anchors and
/// placement) and the subagents (state and stage; the
/// timestamps in their meta are the journal's own and differ by path). The
/// run state is checked on its own, since a replay carries no state updates.
pub(super) fn comparable(core: &SessionCore) -> Value {
    let ids = core.turn_ids();
    let header = core.header();
    let subagents: Vec<Value> = header
        .subagents
        .iter()
        .map(|child| {
            let namespace = &child
                .meta
                .as_ref()
                .map_or(Value::Null, |m| m["querymt"].clone());
            json!({
                "sessionId": child.session_id,
                "title": child.title,
                "description": child.description,
                "state": child.state,
                "stage": child.stage,
                "capabilities": child.capabilities,
                "delegationId": namespace["delegationId"],
            })
        })
        .collect();
    let mut view = json!({
        "subagents": subagents,
        "turns": core.turns(&ids),
        "plan": header.plan,
        "context": header.context,
        "tokens": header.tokens,
        "cost": header.cost,
        "accounting": header.accounting,
        "title": header.title,
        "cursors": header.cursors,
    });
    without_revisions(&mut view);
    view
}

/// The session's current model as a header names it: provider, model and
/// context limit. The journal position and node are not compared: a live
/// update carries no position. A replay names the model only through its
/// config option, so only the live stream and the snapshot are compared.
fn current_model(core: &SessionCore) -> Value {
    core.header().provider.as_ref().map_or(
        Value::Null,
        |p| json!({ "provider": p.provider, "model": p.model, "contextLimit": p.context_limit }),
    )
}

/// The JSON paths where two views differ (at most `limit`), with short values.
fn differences(path: &str, left: &Value, right: &Value, out: &mut Vec<String>) {
    const LIMIT: usize = 12;
    if out.len() >= LIMIT || left == right {
        return;
    }
    let short = |value: &Value| {
        let text = value.to_string();
        if text.len() > 90 {
            format!("{}...", text.chars().take(90).collect::<String>())
        } else {
            text
        }
    };
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let at = format!("{path}.{key}");
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => differences(&at, x, y, out),
                    (x, y) => out.push(format!(
                        "{at}: {} != {}",
                        x.map_or("<missing>".to_owned(), short),
                        y.map_or("<missing>".to_owned(), short)
                    )),
                }
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                differences(&format!("{path}[{index}]"), x, y, out);
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            out.push(format!("{path}: {} items != {} items", a.len(), b.len()));
        }
        _ => out.push(format!("{path}: {} != {}", short(left), short(right))),
    }
}

/// Asserts two views are equal, naming where they differ instead of
/// printing both.
pub(super) fn assert_same_view(name: &str, got: &Value, expected: &Value) {
    let mut out = Vec::new();
    differences("", got, expected, &mut out);
    assert!(out.is_empty(), "{name}: views differ:\n{}", out.join("\n"));
}

fn apply_all(session: &str, inputs: &[SessionInput]) -> SessionCore {
    let mut core = SessionCore::new(session);
    let report = core.apply(inputs);
    assert!(report.errors.is_empty(), "inputs were rejected: {report:?}");
    core
}

/// The generated sessions: plain usage, a request without usage with model
/// changes (one after the last request), no model call at all, and a scaled
/// one.
fn specs() -> Vec<(String, TraceSpec)> {
    vec![
        ("small seed 1".to_owned(), TraceSpec::small(1)),
        (
            "usage gaps".to_owned(),
            TraceSpec {
                usage_gaps: true,
                ..TraceSpec::small(2)
            },
        ),
        (
            "no model call".to_owned(),
            TraceSpec {
                model_calls: false,
                ..TraceSpec::small(3)
            },
        ),
        ("small seed 4".to_owned(), TraceSpec::small(4)),
        (
            "delegations".to_owned(),
            TraceSpec {
                delegations: true,
                ..TraceSpec::small(5)
            },
        ),
        ("scaled".to_owned(), TraceSpec::scaled(9, 3)),
    ]
}

/// What the usage of a trace must look like on every path.
fn assert_usage_shape(name: &str, spec: &TraceSpec, view: &Value) {
    let reports_context = spec.model_calls && !spec.usage_gaps;
    assert_eq!(
        !view["context"].is_null(),
        reports_context,
        "{name}: context usage {}",
        view["context"]
    );
    assert_eq!(
        !view["cost"].is_null(),
        spec.model_calls,
        "{name}: cost {}",
        view["cost"]
    );
    assert_eq!(
        !view["accounting"].is_null(),
        spec.model_calls,
        "{name}: accounting {}",
        view["accounting"]
    );
    // Every compaction reports the context around it (compared with the
    // rest of the turns): tokens before, and after once known.
    let mut counted = 0;
    for turn in view["turns"].as_array().into_iter().flatten() {
        for entry in turn["entries"].as_array().into_iter().flatten() {
            if entry["kind"]["type"] == "compaction" {
                let counts = &entry["kind"]["data"]["meta"]["querymt"]["compaction"];
                assert!(
                    counts["tokensBefore"].is_u64() && counts["tokensAfter"].is_u64(),
                    "{name}: compaction counts {counts}"
                );
                counted += 1;
            }
        }
    }
    assert!(counted > 0, "{name}: the trace should hold a compaction");
    assert_eq!(
        !view["tokens"].is_null(),
        spec.model_calls,
        "{name}: tokens {}",
        view["tokens"]
    );
}

#[tokio::test]
async fn snapshot_and_live_stream_produce_the_same_turns() {
    for (name, spec) in specs() {
        let events = journal(spec);
        let (sid, windowed, unwindowed) = agent_snapshots(&events).await;
        let from_snapshot = apply_all(&sid, &[SessionInput::SnapshotLoaded { response: windowed }]);
        let from_unwindowed = apply_all(
            &sid,
            &[SessionInput::SnapshotLoaded {
                response: unwindowed,
            }],
        );
        let from_live = apply_all(SESSION, &live_stream(&events));
        let expected = comparable(&from_snapshot);
        assert!(
            expected["turns"].as_array().is_some_and(|t| t.len() > 3),
            "{name}: the trace should have several turns"
        );
        assert_usage_shape(&name, &spec, &expected);
        assert_same_view(&name, &comparable(&from_live), &expected);
        assert_same_view(
            &format!("{name} (no window)"),
            &comparable(&from_unwindowed),
            &expected,
        );
        // The model the header names: the last one selected, live as loaded.
        let model = current_model(&from_snapshot);
        assert_eq!(model.is_null(), !spec.model_calls, "{name}: model {model}");
        assert_eq!(current_model(&from_live), model, "{name}: live model");
        assert_eq!(
            current_model(&from_unwindowed),
            model,
            "{name}: unwindowed model"
        );
        // The state updates ended the last run, and the snapshot says the
        // same.
        assert_eq!(from_live.header().run_state, RunState::Idle, "{name}");
        assert_eq!(from_live.header().active_run_id, None, "{name}");
        assert_eq!(
            from_snapshot.header().run_state,
            from_live.header().run_state,
            "{name}"
        );
        assert_eq!(
            from_snapshot.header().active_run_id,
            from_live.header().active_run_id,
            "{name}"
        );
    }
}

/// The run time a journal reports, read the way the agent's summary does.
fn summary_active_ms(events: &[AgentEvent]) -> u64 {
    querymt_agent_services::session::history_window::summarize_events(
        SESSION,
        events.iter().map(|event| (event.seq, event)),
    )
    .summary
    .active_ms
}

/// Active time agrees live and after a load: the live stream's state updates
/// carry the event time as `atMs`, and the snapshot's summary sums the same
/// run boundaries. Run on the journal as storage stamped it (real
/// timestamps, all three views of one journal) and on the generated journal
/// (timestamps that make every run last a known time).
#[tokio::test]
async fn active_time_agrees_live_and_after_a_load() {
    for (name, spec) in specs() {
        let events = journal(spec);
        let (sid, windowed, unwindowed, stored) = agent_snapshots_with_journal(&events).await;
        let from_snapshot = apply_all(&sid, &[SessionInput::SnapshotLoaded { response: windowed }]);
        let from_unwindowed = apply_all(
            &sid,
            &[SessionInput::SnapshotLoaded {
                response: unwindowed,
            }],
        );
        let from_stored_live = apply_all(SESSION, &live_stream(&stored));
        let expected = from_snapshot.header().active_time.clone();
        assert!(
            expected.is_some(),
            "{name}: the snapshot reports active time"
        );
        assert_eq!(expected.as_ref().and_then(|t| t.active_since_ms), None);
        assert_eq!(
            expected.as_ref().map(|t| t.total_ms),
            Some(summary_active_ms(&stored))
        );
        assert_eq!(from_unwindowed.header().active_time, expected, "{name}");
        assert_eq!(from_stored_live.header().active_time, expected, "{name}");

        // Generated timestamps: each run lasts its events' span in seconds.
        let generated = summary_active_ms(&events);
        assert!(generated > 0, "{name}: the generated runs take time");
        let from_live = apply_all(SESSION, &live_stream(&events));
        assert_eq!(
            from_live.header().active_time.as_ref().map(|t| t.total_ms),
            Some(generated),
            "{name}: live total"
        );
        assert_eq!(
            from_live
                .header()
                .active_time
                .as_ref()
                .and_then(|t| t.active_since_ms),
            None,
            "{name}: no run is active at the end"
        );
    }
}

#[tokio::test]
async fn snapshot_and_replay_produce_the_same_turns() {
    for (name, spec) in specs() {
        let events = journal(spec);
        let (sid, windowed, _) = agent_snapshots(&events).await;
        let from_snapshot = apply_all(&sid, &[SessionInput::SnapshotLoaded { response: windowed }]);
        let from_replay = apply_all(SESSION, &replay_stream(&events));
        assert_usage_shape(&name, &spec, &comparable(&from_replay));
        assert_same_view(
            &name,
            &comparable(&from_replay),
            &comparable(&from_snapshot),
        );
        // The comparison above covers the cursors only if there are some.
        assert!(
            !from_snapshot.header().cursors.is_empty(),
            "{name}: the snapshot announced no cursors"
        );
        assert!(
            from_snapshot
                .header()
                .cursors
                .iter()
                .all(|cursor| cursor.placement.is_some()),
            "{name}: every announced cursor is placed in the view"
        );
    }
}

/// The delegate tool calls of a compared view: id, status and child session.
fn delegate_calls(view: &Value) -> Vec<(String, String, Option<String>)> {
    let mut calls = Vec::new();
    for turn in view["turns"].as_array().into_iter().flatten() {
        for entry in turn["entries"].as_array().into_iter().flatten() {
            let data = &entry["kind"]["data"];
            if entry["kind"]["type"] == "tool_call"
                && data["toolCallId"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("del"))
            {
                calls.push((
                    data["toolCallId"].as_str().unwrap_or_default().to_owned(),
                    data["status"].as_str().unwrap_or_default().to_owned(),
                    data["childSessionId"].as_str().map(str::to_owned),
                ));
            }
        }
    }
    calls
}

#[tokio::test]
async fn delegations_agree_live_and_after_a_load() {
    let spec = TraceSpec {
        delegations: true,
        ..TraceSpec::small(11)
    };
    let events = journal(spec);
    let (sid, windowed, unwindowed) = agent_snapshots(&events).await;
    let from_snapshot = apply_all(&sid, &[SessionInput::SnapshotLoaded { response: windowed }]);
    let from_unwindowed = apply_all(
        &sid,
        &[SessionInput::SnapshotLoaded {
            response: unwindowed,
        }],
    );
    let live = live_stream(&events);
    let from_live = apply_all(SESSION, &live);
    let from_replay = apply_all(SESSION, &replay_stream(&events));
    let expected = comparable(&from_snapshot);

    // Two children: the completed and the failed delegation, both finished.
    let subagents = expected["subagents"].as_array().expect("subagents");
    assert_eq!(subagents.len(), 2, "subagents {subagents:?}");
    for (index, child) in subagents.iter().enumerate() {
        assert_eq!(child["sessionId"], format!("child{index}"));
        assert_eq!(child["delegationId"], format!("d{index}"));
        assert_eq!(child["state"], "idle", "child {index} is finished");
    }
    // Each delegate tool call points at its child; the failed one is failed.
    assert_eq!(
        delegate_calls(&expected),
        vec![
            (
                "del0".to_owned(),
                "completed".to_owned(),
                Some("child0".to_owned())
            ),
            (
                "del1".to_owned(),
                "failed".to_owned(),
                Some("child1".to_owned())
            ),
        ]
    );

    assert_same_view("delegations live", &comparable(&from_live), &expected);
    assert_same_view(
        "delegations no window",
        &comparable(&from_unwindowed),
        &expected,
    );
    assert_same_view("delegations replay", &comparable(&from_replay), &expected);

    // While the first delegation runs, the live view shows its stage; the
    // journal holds no stage, so only the live stream can.
    let stage_at = events
        .iter()
        .position(|event| matches!(event.kind, AgentEventKind::DelegationStageChanged { .. }))
        .expect("the trace holds a stage change");
    let mut translator = AcpLiveEventTranslator::new();
    translator.set_capabilities(gpui_capabilities());
    let mut blocked = HashSet::new();
    let mut inputs = Vec::new();
    for event in &events[..=stage_at] {
        inputs.extend(forwarded(
            &mut translator,
            &live_envelopes(event, &mut blocked),
        ));
    }
    let midway = apply_all(SESSION, &inputs);
    let child = &midway.header().subagents[0];
    assert_eq!(child.state.as_deref(), Some("running"));
    assert_eq!(child.stage.as_deref(), Some("verifying"));
}

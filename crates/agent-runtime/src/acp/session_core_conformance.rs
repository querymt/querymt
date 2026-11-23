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
use crate::session::load_snapshot::{SessionLoadSnapshot, cursor_from_events};
use querymt_agent_core::events::{
    AgentEvent, AgentEventKind, EphemeralEvent, EventEnvelope, EventOrigin,
};
use querymt_agent_core::protocol::{ContentBlock, TextContent};
use querymt_agent_core::runtime_status::SessionRuntimeStatus;
use querymt_agent_core::session::projection::AuditView;
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

/// The `session/load` response of an agent whose snapshot holds `events`.
pub(super) fn snapshot_response(events: &[AgentEvent]) -> Value {
    let snapshot = SessionLoadSnapshot {
        runtime_status: SessionRuntimeStatus::Idle,
        audit: AuditView {
            session_id: SESSION.to_owned(),
            events: events.to_vec(),
            tasks: Vec::new(),
            intent_snapshots: Vec::new(),
            decisions: Vec::new(),
            progress_entries: Vec::new(),
            artifacts: Vec::new(),
            delegations: Vec::new(),
            generated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        cursor: cursor_from_events(events),
        accounting: querymt_agent_services::accounting::snapshot_from_events(
            SESSION,
            events.iter().map(|event| &event.kind),
        ),
        delegation_updates: Vec::new(),
        user_prompts: None,
        history: None,
        summary: None,
        truncated: None,
    };
    json!({ "_meta": { "querymt/sessionLoadSnapshot.v1": snapshot } })
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

/// What the three paths must agree on: turns (ids included) with their
/// entries, the plan's items, context usage, token breakdown and the
/// title. Cost is not compared yet: the generated journals have no LLM
/// attempts, and then a snapshot reports an incomplete zero cost where the
/// live stream and a replay send no accounting at all (see tasks.md 6.1).
///
/// A compaction's summary is compared as its text: live, the agent streams
/// it as chunks and the view keeps one block per chunk (as the ACP contract
/// says), while a snapshot or replay holds the whole summary in one block.
///
/// The plan's id is left out: the agent names the todo plan only for clients
/// that announce plan operations, and a snapshot has no such client. The run
/// state is checked on its own, since a replay carries no state updates.
pub(super) fn comparable(core: &SessionCore) -> Value {
    let ids = core.turn_ids();
    let header = core.header();
    let mut view = json!({
        "turns": core.turns(&ids),
        "plan": header.plan.as_ref().map(|plan| (&plan.entries, &plan.markdown)),
        "context": header.context,
        "tokens": header.tokens,
        "title": header.title,
    });
    without_revisions(&mut view);
    join_compaction_summaries(&mut view);
    view
}

fn join_compaction_summaries(view: &mut Value) {
    let Some(turns) = view["turns"].as_array_mut() else {
        return;
    };
    for entry in turns
        .iter_mut()
        .filter_map(|turn| turn["entries"].as_array_mut())
        .flatten()
    {
        if entry["kind"]["type"] != "compaction" {
            continue;
        }
        let text: String = entry["kind"]["data"]["summary"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| block["text"].as_str())
            .collect();
        entry["kind"]["data"]["summary"] = Value::String(text);
    }
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

fn apply_all(inputs: &[SessionInput]) -> SessionCore {
    let mut core = SessionCore::new(SESSION);
    let report = core.apply(inputs);
    assert!(report.errors.is_empty(), "inputs were rejected: {report:?}");
    core
}

fn specs() -> Vec<(String, TraceSpec)> {
    let mut specs = Vec::new();
    for seed in 1..=4 {
        specs.push((format!("small seed {seed}"), TraceSpec::small(seed)));
    }
    specs.push(("scaled".to_owned(), TraceSpec::scaled(9, 3)));
    specs
}

#[test]
fn snapshot_and_live_stream_produce_the_same_turns() {
    for (name, spec) in specs() {
        let events = journal(spec);
        let from_snapshot = apply_all(&[SessionInput::SnapshotLoaded {
            response: snapshot_response(&events),
        }]);
        let from_live = apply_all(&live_stream(&events));
        let expected = comparable(&from_snapshot);
        assert!(
            expected["turns"].as_array().is_some_and(|t| t.len() > 3),
            "{name}: the trace should have several turns"
        );
        assert_same_view(&name, &comparable(&from_live), &expected);
        // The state updates ended the last run.
        assert_eq!(from_live.header().run_state, RunState::Idle, "{name}");
        assert_eq!(from_live.header().active_run_id, None, "{name}");
    }
}

#[test]
fn snapshot_and_replay_produce_the_same_turns() {
    for (name, spec) in specs() {
        let events = journal(spec);
        let from_snapshot = apply_all(&[SessionInput::SnapshotLoaded {
            response: snapshot_response(&events),
        }]);
        let from_replay = apply_all(&replay_stream(&events));
        assert_same_view(
            &name,
            &comparable(&from_replay),
            &comparable(&from_snapshot),
        );
    }
}

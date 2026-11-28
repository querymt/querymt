//! Session cursors announced to clients that advertise `session.cursors`:
//! live delivery, history replay and capability gating.

use crate::acp::shared::{
    AcpLiveEventTranslator, AcpSessionUpdateCapabilities, replay_agent_events_materialized,
};
use querymt_agent_core::events::{
    AgentEvent, AgentEventKind, DurableEvent, EventEnvelope, EventOrigin, ReasoningPartStored,
};
use serde_json::Value;
use std::collections::HashMap;

const SESSION: &str = "s-1";

fn cursors() -> AcpSessionUpdateCapabilities {
    AcpSessionUpdateCapabilities {
        cursors: true,
        ..Default::default()
    }
}

fn agent_event(seq: i64, kind: AgentEventKind) -> AgentEvent {
    AgentEvent {
        session_seq: seq,
        seq,
        timestamp: seq,
        session_id: SESSION.into(),
        origin: EventOrigin::Local,
        source_node: None,
        kind,
    }
}

fn envelope(event: &AgentEvent) -> EventEnvelope {
    EventEnvelope::Durable(DurableEvent {
        session_seq: event.session_seq,
        event_id: format!("evt-{}", event.seq),
        stream_seq: event.seq,
        timestamp: event.timestamp,
        session_id: event.session_id.clone(),
        origin: EventOrigin::Local,
        source_node: None,
        kind: event.kind.clone(),
    })
}

/// A turn: the user prompt, a tool call, an agent message with a thought.
fn turn() -> Vec<AgentEvent> {
    vec![
        agent_event(
            1,
            AgentEventKind::PromptReceived {
                slash_command: None,
                content: "hello".into(),
                message_id: Some("u1".into()),
            },
        ),
        agent_event(
            2,
            AgentEventKind::ToolCallStart {
                tool_call_id: "call-1".into(),
                tool_name: "shell".into(),
                arguments: "{}".into(),
            },
        ),
        agent_event(
            3,
            AgentEventKind::ToolCallEnd {
                tool_call_id: "call-1".into(),
                tool_name: "shell".into(),
                is_error: false,
                result: "{}".into(),
            },
        ),
        agent_event(
            4,
            AgentEventKind::AssistantMessageStored {
                content: "done".into(),
                thinking: None,
                reasoning_parts: vec![ReasoningPartStored {
                    id: "r1".into(),
                    text: "thinking".into(),
                }],
                message_id: Some("a1".into()),
            },
        ),
    ]
}

fn compaction() -> Vec<AgentEvent> {
    vec![
        agent_event(
            5,
            AgentEventKind::CompactionStart {
                slash_command: None,
                token_estimate: 100,
                compaction_id: Some("cmp-1".into()),
            },
        ),
        agent_event(
            6,
            AgentEventKind::CompactionEnd {
                summary: "summary".into(),
                summary_len: 7,
                compaction_id: Some("cmp-1".into()),
                context_tokens: None,
            },
        ),
    ]
}

/// The events a live connection sees for `events`: an agent message is
/// streamed (thinking and content deltas, which are never persisted) before
/// its stored event.
fn streamed(events: &[AgentEvent]) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    for event in events {
        if let AgentEventKind::AssistantMessageStored {
            content,
            reasoning_parts,
            message_id: Some(message_id),
            ..
        } = &event.kind
        {
            for part in reasoning_parts {
                out.push(agent_event(
                    event.seq,
                    AgentEventKind::AssistantThinkingDelta {
                        content: part.text.clone(),
                        message_id: message_id.clone(),
                        part_id: Some(part.id.clone()),
                    },
                ));
            }
            out.push(agent_event(
                event.seq,
                AgentEventKind::AssistantContentDelta {
                    content: content.clone(),
                    message_id: message_id.clone(),
                },
            ));
        }
        out.push(event.clone());
    }
    out
}

fn kind(update: &Value) -> &str {
    update["sessionUpdate"].as_str().unwrap_or_default()
}

/// Every update of the live stream, one entry per event, as JSON.
fn live(capabilities: AcpSessionUpdateCapabilities, events: &[AgentEvent]) -> Vec<Vec<Value>> {
    let mut translator = AcpLiveEventTranslator::new();
    translator.set_capabilities(capabilities);
    events
        .iter()
        .map(|event| {
            translator
                .translate_updates(&envelope(event))
                .into_iter()
                .map(|update| serde_json::to_value(update).expect("update serializes"))
                .collect()
        })
        .collect()
}

fn replayed(capabilities: AcpSessionUpdateCapabilities, events: Vec<AgentEvent>) -> Vec<Value> {
    replay_agent_events_materialized(SESSION, events, &HashMap::new(), &capabilities)
        .into_iter()
        .map(|notification| serde_json::to_value(notification.update).expect("update serializes"))
        .collect()
}

fn cursor_ids(updates: impl IntoIterator<Item = Value>) -> Vec<String> {
    updates
        .into_iter()
        .filter(|update| kind(update) == "cursor_update")
        .map(|update| update["cursorId"].as_str().expect("cursor id").to_owned())
        .collect()
}

#[test]
fn live_turn_announces_cursors_after_its_entities() {
    let events = streamed(&turn());
    let per_event = live(cursors(), &events);

    // The user message: chunk, then its cursor.
    let user: Vec<&str> = per_event[0].iter().map(kind).collect();
    assert_eq!(user, ["user_message_chunk", "cursor_update"]);
    let cursor = &per_event[0][1];
    assert_eq!(cursor["cursorId"], "m:u1");
    assert_eq!(cursor["anchor"]["type"], "user_message");
    assert_eq!(cursor["anchor"]["position"], "after");

    // The tool call is announced only once it ended: update, then cursor.
    let started: Vec<&str> = per_event[1].iter().map(kind).collect();
    assert_eq!(started, ["tool_call"], "no cursor before the call ends");
    let ended: Vec<&str> = per_event[2].iter().map(kind).collect();
    assert_eq!(ended, ["tool_call_update", "cursor_update"]);
    let cursor = &per_event[2][1];
    assert_eq!(cursor["cursorId"], "c:call-1");
    assert_eq!(cursor["anchor"]["type"], "tool_call");
    assert_eq!(cursor["anchor"]["toolCallId"], "call-1");

    // The agent message streams as a thought chunk and a message chunk;
    // neither is a cursor, and the stored event closes both entities.
    assert_eq!(
        per_event[3].iter().map(kind).collect::<Vec<_>>(),
        ["agent_thought_chunk"]
    );
    assert_eq!(
        per_event[4].iter().map(kind).collect::<Vec<_>>(),
        ["agent_message_chunk"]
    );
    let stored: Vec<&str> = per_event[5].iter().map(kind).collect();
    assert_eq!(stored, ["cursor_update", "cursor_update"]);
    assert_eq!(per_event[5][0]["cursorId"], "t:a1");
    assert_eq!(per_event[5][0]["anchor"]["type"], "agent_thought");
    assert_eq!(per_event[5][1]["cursorId"], "m:a1");
    assert_eq!(per_event[5][1]["anchor"]["type"], "agent_message");
    for position in [0, 1] {
        assert_eq!(per_event[5][position]["anchor"]["position"], "after");
    }
}

#[test]
fn compaction_announces_a_cursor() {
    let events = compaction();
    let capabilities = AcpSessionUpdateCapabilities {
        compaction: true,
        ..cursors()
    };
    let per_event = live(capabilities, &events);

    assert!(cursor_ids(per_event[0].clone()).is_empty(), "not at start");
    let end: Vec<&str> = per_event[1].iter().map(kind).collect();
    assert_eq!(end, ["compaction_update", "cursor_update"]);
    let cursor = &per_event[1][1];
    assert_eq!(cursor["cursorId"], "k:cmp-1");
    assert_eq!(cursor["anchor"]["type"], "compaction");
    assert_eq!(cursor["anchor"]["compactionId"], "cmp-1");
}

#[test]
fn client_without_the_cursors_capability() {
    let mut events = turn();
    events.extend(compaction());
    let all = AcpSessionUpdateCapabilities {
        compaction: true,
        ..Default::default()
    };

    let live_updates: Vec<Value> = live(all, &streamed(&events))
        .into_iter()
        .flatten()
        .collect();
    assert!(!live_updates.is_empty());
    assert!(cursor_ids(live_updates).is_empty(), "no live cursor_update");

    let replay = replayed(all, events);
    assert!(!replay.is_empty());
    assert!(cursor_ids(replay).is_empty(), "no replayed cursor_update");
}

#[test]
fn replay_reproduces_the_same_cursor_ids() {
    let mut events = turn();
    events.extend(compaction());
    let capabilities = AcpSessionUpdateCapabilities {
        compaction: true,
        ..cursors()
    };

    let live_ids = cursor_ids(live(capabilities, &streamed(&events)).into_iter().flatten());
    let replay = replayed(capabilities, events);
    let replay_ids = cursor_ids(replay.clone());

    assert_eq!(
        live_ids,
        ["m:u1", "c:call-1", "t:a1", "m:a1", "k:cmp-1"],
        "live announces every anchor"
    );
    assert_eq!(replay_ids, live_ids, "replay announces the same ids");

    // Each replayed cursor follows the update that introduced its entity.
    let position = |wanted: &str| {
        replay
            .iter()
            .position(|update| update["cursorId"] == wanted)
            .unwrap_or_else(|| panic!("{wanted} replayed"))
    };
    assert_eq!(kind(&replay[position("m:u1") - 1]), "user_message_chunk");
    assert_eq!(kind(&replay[position("c:call-1") - 1]), "tool_call_update");
    assert_eq!(kind(&replay[position("m:a1") - 1]), "cursor_update");
    assert_eq!(kind(&replay[position("m:a1") - 2]), "agent_message_chunk");
    assert_eq!(kind(&replay[position("k:cmp-1") - 1]), "compaction_update");
}

#[test]
fn replay_announces_only_the_returned_window() {
    let events = turn();
    // A window that starts after the first prompt.
    let window = events[1..].to_vec();

    let ids = cursor_ids(replayed(cursors(), window));

    assert_eq!(ids, ["c:call-1", "t:a1", "m:a1"]);
}

#[test]
fn an_anchor_is_announced_once() {
    let events = turn();
    let mut translator = AcpLiveEventTranslator::new();
    translator.set_capabilities(cursors());
    let stored = envelope(&events[3]);
    assert!(matches!(
        stored.kind(),
        AgentEventKind::AssistantMessageStored { .. }
    ));

    let first = translator.translate_updates(&stored);
    let second = translator.translate_updates(&stored);

    let count = |updates: &[crate::acp::protocol::SessionUpdate]| {
        updates
            .iter()
            .filter(|update| matches!(update, crate::acp::protocol::SessionUpdate::CursorUpdate(_)))
            .count()
    };
    assert_eq!(count(&first), 2);
    assert_eq!(count(&second), 0, "already announced to this connection");
}

#[tokio::test]
async fn cursor_ids_survive_an_agent_restart() {
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;
    use querymt_agent_services::event_fanout::EventFanout;
    use querymt_agent_services::session_event_stream::SessionEventStream;
    use std::sync::Arc;

    let mut events = turn();
    events.extend(compaction());
    let capabilities = AcpSessionUpdateCapabilities {
        compaction: true,
        ..cursors()
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cursors.db");

    // Before the restart: the events are journaled and announced live.
    let before = {
        let storage = SqliteStorage::connect(path.clone()).await.expect("store");
        let stream = SessionEventStream::new(
            SESSION,
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        );
        for event in &events {
            stream
                .emit_persisted(event.kind.clone())
                .await
                .expect("event is journaled");
        }
        cursor_ids(live(capabilities, &streamed(&events)).into_iter().flatten())
    };
    assert_eq!(before.len(), 5, "every anchor was announced: {before:?}");

    // After the restart: a fresh connection to the same file; the journal is
    // all that is left, and a reload derives the same ids.
    let storage = SqliteStorage::connect(path).await.expect("reopened store");
    let journal: Vec<AgentEvent> = storage
        .event_journal()
        .load_session_stream(SESSION, None, None)
        .await
        .expect("journal loads")
        .into_iter()
        .map(AgentEvent::from)
        .collect();
    let after = cursor_ids(replayed(capabilities, journal));

    assert_eq!(after, before, "cursor ids are derived, not stored");
}

/// A client with `cursors` but without `compaction` is never sent a compaction
/// update, so it gets no cursor anchored to one, live or replayed.
#[test]
fn compaction_cursor_needs_the_compaction_capability() {
    let mut events = turn();
    events.extend(compaction());

    let live_ids = cursor_ids(live(cursors(), &streamed(&events)).into_iter().flatten());
    assert_eq!(live_ids, ["m:u1", "c:call-1", "t:a1", "m:a1"]);
    let replay_ids = cursor_ids(replayed(cursors(), events));
    assert_eq!(replay_ids, live_ids);
}

/// The dedupe memory is bounded: an unbounded session does not grow it, and
/// an anchor that fell out is merely announced again (the update is idempotent).
#[test]
fn announced_cursor_memory_is_bounded() {
    let mut translator = AcpLiveEventTranslator::new();
    translator.set_capabilities(cursors());
    let prompt = |n: usize| {
        envelope(&agent_event(
            n as i64,
            AgentEventKind::PromptReceived {
                slash_command: None,
                content: "hi".into(),
                message_id: Some(format!("u{n}")),
            },
        ))
    };
    let count = |updates: Vec<crate::acp::protocol::SessionUpdate>| {
        updates
            .iter()
            .filter(|u| matches!(u, crate::acp::protocol::SessionUpdate::CursorUpdate(_)))
            .count()
    };

    assert_eq!(count(translator.translate_updates(&prompt(0))), 1);
    assert_eq!(
        count(translator.translate_updates(&prompt(0))),
        0,
        "deduped"
    );
    for n in 1..=5000 {
        assert_eq!(count(translator.translate_updates(&prompt(n))), 1);
    }
    assert_eq!(
        count(translator.translate_updates(&prompt(0))),
        1,
        "an evicted anchor is announced again"
    );
}

/// A structured (multi-block) prompt streams as one chunk per block; its
/// cursor follows the last of them, once, at `PromptReceived`.
#[test]
fn structured_prompt_cursor_follows_all_of_its_blocks() {
    use crate::acp::protocol::{ContentBlock, TextContent};

    let block = |text: &str| {
        agent_event(
            1,
            AgentEventKind::UserPromptBlock {
                slash_command: None,
                message_id: "u1".into(),
                client_prompt_id: None,
                block: ContentBlock::Text(TextContent::new(text)),
            },
        )
    };
    let events = vec![
        block("first"),
        block("second"),
        agent_event(
            2,
            AgentEventKind::PromptReceived {
                slash_command: None,
                content: "first second".into(),
                message_id: Some("u1".into()),
            },
        ),
    ];

    let per_event = live(cursors(), &events);

    let kinds: Vec<Vec<&str>> = per_event
        .iter()
        .map(|updates| updates.iter().map(kind).collect())
        .collect();
    assert_eq!(
        kinds,
        [
            vec!["user_message_chunk"],
            vec!["user_message_chunk"],
            vec!["cursor_update"]
        ],
        "no cursor before the last block, none duplicated"
    );
    assert_eq!(per_event[2][0]["cursorId"], "m:u1");

    // Replay with the stored structured blocks: chunks, then the cursor.
    let blocks = HashMap::from([(
        "u1".to_owned(),
        vec![
            ContentBlock::Text(TextContent::new("first")),
            ContentBlock::Text(TextContent::new("second")),
        ],
    )]);
    let replay: Vec<Value> =
        replay_agent_events_materialized(SESSION, vec![events[2].clone()], &blocks, &cursors())
            .into_iter()
            .map(|n| serde_json::to_value(n.update).expect("update serializes"))
            .collect();
    assert_eq!(
        replay.iter().map(kind).collect::<Vec<_>>(),
        ["user_message_chunk", "user_message_chunk", "cursor_update"]
    );
}

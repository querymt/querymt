//! Seeded lorem-ipsum trace generator (feature `test-support`).
//!
//! Emits `AgentEvent` JSON sequences and the equivalent ACP notification
//! stream. No clock, no OS randomness: the same seed gives the same trace.

use querymt_agent_protocol_types::events::{
    AgentEvent, AgentEventKind, EventOrigin, ReasoningPartStored,
};
use serde_json::{Value, json};

const WORDS: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
];

/// Splitmix64: small, seedable and good enough for test data.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (`bound` of 0 gives 0).
    pub fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        usize::try_from(self.next_u64() % bound as u64).unwrap_or(0)
    }

    /// `count` lorem-ipsum words joined by spaces.
    pub fn lorem(&mut self, count: usize) -> String {
        let words: Vec<&str> = (0..count).map(|_| WORDS[self.below(WORDS.len())]).collect();
        words.join(" ")
    }
}

/// Shape of a generated session.
#[derive(Debug, Clone, Copy)]
pub struct TraceSpec {
    pub seed: u64,
    /// Number of prompt/reply exchanges.
    pub turns: usize,
    /// Upper bound of tool calls after each reply.
    pub max_tools: usize,
    /// A compaction is inserted after every n-th turn (0 for never).
    pub compaction_every: usize,
    /// Words in a tool result.
    pub result_words: usize,
}

impl TraceSpec {
    pub fn small(seed: u64) -> Self {
        Self {
            seed,
            turns: 6,
            max_tools: 2,
            compaction_every: 4,
            result_words: 40,
        }
    }

    /// `factor` times the size of [`TraceSpec::small`] in turns.
    pub fn scaled(seed: u64, factor: usize) -> Self {
        Self {
            turns: 6 * factor,
            ..Self::small(seed)
        }
    }
}

/// One generated session: the journal events and what a client would see.
#[derive(Debug, Clone)]
pub struct Trace {
    pub session_id: String,
    pub events: Vec<Value>,
}

/// Appends `kind` as the next journal event. Events are built as
/// [`AgentEventKind`] values and serialized, so a renamed or reshaped event
/// stops this generator compiling instead of drifting from the agent.
fn push(events: &mut Vec<Value>, seq: &mut i64, kind: AgentEventKind) {
    *seq += 1;
    let event = AgentEvent {
        seq: *seq,
        session_seq: *seq,
        timestamp: 1_000_000 + *seq,
        session_id: "s1".to_owned(),
        origin: EventOrigin::Local,
        source_node: None,
        kind,
    };
    events.push(serde_json::to_value(&event).expect("an agent event serializes"));
}

/// Generate a session as the agent journals it: per turn a run start (before
/// the prompt is stored), the prompt, a reply with reasoning, tool calls (one
/// failing, a todo-list update every third turn), steering, the run end and,
/// every n-th turn, a compaction. Steering and run events are kinds the
/// reducer ignores; run state comes from updates.
pub fn generate(spec: TraceSpec) -> Trace {
    let mut rng = Rng::new(spec.seed);
    let mut events = Vec::new();
    let mut seq = 0i64;
    for turn in 0..spec.turns {
        let run_id = format!("r{turn}");
        push(
            &mut events,
            &mut seq,
            AgentEventKind::RunStarted {
                run_id: run_id.clone(),
                origin: "interactive".to_owned(),
            },
        );
        let words = 3 + rng.below(8);
        let prompt = rng.lorem(words);
        push(
            &mut events,
            &mut seq,
            AgentEventKind::PromptReceived {
                content: prompt,
                message_id: Some(format!("u{turn}")),
            },
        );
        if turn % 2 == 1 {
            let input_id = format!("i{turn}");
            push(
                &mut events,
                &mut seq,
                AgentEventKind::SteeringAccepted {
                    run_id: run_id.clone(),
                    input_id: input_id.clone(),
                    position: 0,
                    blocks: Vec::new(),
                    accepted_at_ms: Some(1),
                },
            );
            push(
                &mut events,
                &mut seq,
                AgentEventKind::SteeringApplied {
                    run_id: run_id.clone(),
                    input_id,
                    boundary: "before_model_request".to_owned(),
                    latency_ms: 5,
                },
            );
        }
        let reasoning_words = 4 + rng.below(6);
        let reasoning = rng.lorem(reasoning_words);
        let reply_words = 20 + rng.below(60);
        let reply = rng.lorem(reply_words);
        push(
            &mut events,
            &mut seq,
            AgentEventKind::AssistantMessageStored {
                content: reply,
                thinking: None,
                reasoning_parts: vec![ReasoningPartStored {
                    id: format!("a{turn}:summary:0"),
                    text: reasoning,
                }],
                message_id: Some(format!("a{turn}")),
            },
        );
        if turn % 3 == 2 {
            let id = format!("todo{turn}");
            push(
                &mut events,
                &mut seq,
                AgentEventKind::ToolCallStart {
                    tool_call_id: id.clone(),
                    tool_name: "todowrite".to_owned(),
                    arguments: r#"{"todos":[{"content":"lorem","status":"in_progress","priority":"high"}]}"#
                        .to_owned(),
                },
            );
            push(
                &mut events,
                &mut seq,
                AgentEventKind::ToolCallEnd {
                    tool_call_id: id,
                    tool_name: "todowrite".to_owned(),
                    is_error: false,
                    result: "ok".to_owned(),
                },
            );
        }
        for tool in 0..rng.below(spec.max_tools + 1) {
            let id = format!("t{turn}-{tool}");
            push(
                &mut events,
                &mut seq,
                AgentEventKind::ToolCallStart {
                    tool_call_id: id.clone(),
                    tool_name: "shell".to_owned(),
                    arguments: "{\"cmd\":\"ls\"}".to_owned(),
                },
            );
            let result = rng.lorem(spec.result_words);
            push(
                &mut events,
                &mut seq,
                AgentEventKind::ToolCallEnd {
                    tool_call_id: id,
                    tool_name: "shell".to_owned(),
                    is_error: tool == 1,
                    result,
                },
            );
        }
        if spec.compaction_every > 0 && (turn + 1) % spec.compaction_every == 0 {
            let id = format!("c{turn}");
            push(
                &mut events,
                &mut seq,
                AgentEventKind::CompactionStart {
                    token_estimate: 1000,
                    compaction_id: Some(id.clone()),
                },
            );
            let summary = rng.lorem(12);
            push(
                &mut events,
                &mut seq,
                AgentEventKind::CompactionEnd {
                    summary,
                    summary_len: 12,
                    compaction_id: Some(id),
                    context_tokens: Some(200),
                },
            );
        }
        push(
            &mut events,
            &mut seq,
            AgentEventKind::RunCompleted {
                run_id,
                outcome: "completed".to_owned(),
                stop_type: None,
                reason: None,
            },
        );
    }
    Trace {
        session_id: "s1".to_owned(),
        events,
    }
}

use crate::SessionCore;
use crate::input::SessionInput;

/// A snapshot-bearing `session/load` response for `events`.
pub fn snapshot_response(events: &[Value], extra: Value) -> Value {
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
        "delegationUpdates": []
    });
    if let (Some(base), Some(more)) = (snapshot.as_object_mut(), extra.as_object()) {
        for (key, value) in more {
            base.insert(key.clone(), value.clone());
        }
    }
    json!({ "_meta": { "querymt/sessionLoadSnapshot.v1": snapshot } })
}

/// A live notification input.
pub fn live(update: Value) -> SessionInput {
    SessionInput::AcpNotification {
        notification: json!({ "sessionId": "s1", "update": update }),
        replayed: false,
        at_ms: 0,
    }
}

/// An `agent_message_chunk` input.
pub fn text_chunk(message_id: &str, text: &str) -> SessionInput {
    live(
        json!({ "sessionUpdate": "agent_message_chunk", "messageId": message_id,
                 "content": { "type": "text", "text": text } }),
    )
}

/// One named, replayable scenario: batches of inputs for a fresh view.
#[derive(Debug, Clone)]
pub struct GoldenCase {
    pub name: String,
    pub batches: Vec<Vec<SessionInput>>,
}

/// What applying a case produced; this is what golden files store.
pub fn evaluate(case: &GoldenCase) -> Value {
    let mut core = SessionCore::new("s1");
    let reports: Vec<Value> = case
        .batches
        .iter()
        .map(|batch| serde_json::to_value(core.apply(batch)).unwrap_or(Value::Null))
        .collect();
    let ids = core.turn_ids();
    json!({
        "reports": reports,
        "header": core.header(),
        "turnIds": ids,
        "turns": core.turns(&ids),
    })
}

fn case(name: &str, batches: Vec<Vec<SessionInput>>) -> GoldenCase {
    GoldenCase {
        name: name.to_owned(),
        batches,
    }
}

fn loaded(spec: TraceSpec) -> SessionInput {
    let trace = generate(spec);
    SessionInput::SnapshotLoaded {
        response: snapshot_response(&trace.events, json!({})),
    }
}

/// The golden scenarios: small, 1x and 4x sessions; streaming, history page,
/// optimistic prompt, compaction and delegation cases.
pub fn golden_cases() -> Vec<GoldenCase> {
    let mut rng = Rng::new(99);
    let stream: Vec<SessionInput> = (0..40)
        .map(|_| {
            let words = 1 + rng.below(3);
            text_chunk("m-stream", &rng.lorem(words))
        })
        .collect();
    let older = generate(TraceSpec {
        turns: 3,
        ..TraceSpec::small(5)
    });
    let older_events: Vec<Value> = older.events.iter().take(8).cloned().collect();
    let windowed = SessionInput::SnapshotLoaded {
        response: snapshot_response(
            &generate(TraceSpec::small(6))
                .events
                .iter()
                .skip(20)
                .cloned()
                .collect::<Vec<_>>(),
            json!({ "history": { "complete": false, "before": "tok-1" } }),
        ),
    };
    let page = SessionInput::HistoryPage {
        response: json!({ "events": older_events,
            "history": { "complete": true }, "truncated": [] }),
    };
    let optimistic = SessionInput::PromptOptimistic {
        client_prompt_id: "c1".into(),
        blocks: serde_json::from_value(json!([{ "type": "text", "text": "lorem ipsum" }]))
            .unwrap_or_default(),
        at_ms: 10,
    };
    let echo = live(
        json!({ "sessionUpdate": "user_message_chunk", "messageId": "u-echo",
        "content": { "type": "text", "text": "lorem ipsum" },
        "_meta": { "querymt": { "client_prompt_id": "c1" } } }),
    );
    let compaction = vec![
        live(
            json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                     "status": "in_progress" }),
        ),
        live(
            json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "c1",
                     "content": { "type": "text", "text": "dolor sit" } }),
        ),
        live(
            json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                     "status": "completed" }),
        ),
    ];
    let delegation = vec![
        live(
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "delegate",
                     "status": "in_progress" }),
        ),
        SessionInput::DelegationUpdate {
            update: serde_json::from_value(json!({
                "version": 1, "sessionId": "s1", "delegationId": "d1", "toolCallId": "t1",
                "state": "forked", "targetAgentId": "coder", "objective": "lorem",
                "childSessionId": "child-1", "requestedAt": 1, "updatedAt": 2 }))
            .expect("delegation fixture"),
        },
    ];
    vec![
        case("small_snapshot", vec![vec![loaded(TraceSpec::small(1))]]),
        case("snapshot_1x", vec![vec![loaded(TraceSpec::scaled(2, 4))]]),
        case("snapshot_4x", vec![vec![loaded(TraceSpec::scaled(2, 16))]]),
        case(
            "streaming",
            vec![stream[..20].to_vec(), stream[20..].to_vec()],
        ),
        case("history_page", vec![vec![windowed], vec![page]]),
        case("optimistic_prompt", vec![vec![optimistic], vec![echo]]),
        case("compaction", vec![compaction]),
        case("delegation", vec![delegation]),
    ]
}

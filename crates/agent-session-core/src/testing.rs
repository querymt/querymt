//! Seeded lorem-ipsum trace generator (feature `test-support`).
//!
//! Emits `AgentEvent` JSON sequences and the equivalent ACP notification
//! stream. No clock, no OS randomness: the same seed gives the same trace.

use querymt_agent_protocol_types::accounting::{
    AttemptPurpose, AttemptRef, RequestAccounting, samples,
};
use querymt_agent_protocol_types::delegation_update::DelegationStage;
use querymt_agent_protocol_types::events::{
    AgentEvent, AgentEventKind, EventOrigin, ExecutionMetrics, ReasoningPartStored,
};
use querymt_agent_protocol_types::protocol::{ContentBlock, TextContent};
use querymt_agent_protocol_types::session::domain::{Delegation, ForkOrigin, ForkPointType};
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
    /// Each turn makes one model call, journaled as the agent does:
    /// `ProviderChanged` once at the start, then `LlmAttemptStarted`,
    /// `LlmAttemptSettled` and `LlmRequestEnd` per call. Off: the session has
    /// no model call at all (no usage, no cost, no context).
    pub model_calls: bool,
    /// With model calls: the third request reports no usage, the model
    /// changes halfway, and it changes once more after the last request (its
    /// context measurement is then stale).
    pub usage_gaps: bool,
    /// The first two turns each delegate to a child session: the first
    /// delegation completes, the second fails (see [`delegation_ids`]).
    pub delegations: bool,
}

/// The ids the generator gives a delegation made in `turn` (0 completes, 1
/// fails): the delegate tool call, the delegation and the child session.
/// `None` for any other turn.
pub fn delegation_ids(turn: usize) -> Option<DelegationIds> {
    (turn < 2).then(|| DelegationIds {
        tool_call_id: format!("del{turn}"),
        delegation_id: format!("d{turn}"),
        child_session_id: format!("child{turn}"),
        completes: turn == 0,
    })
}

/// See [`delegation_ids`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationIds {
    pub tool_call_id: String,
    pub delegation_id: String,
    pub child_session_id: String,
    /// Completes, or fails.
    pub completes: bool,
}

impl TraceSpec {
    pub fn small(seed: u64) -> Self {
        Self {
            seed,
            turns: 6,
            max_tools: 2,
            compaction_every: 4,
            result_words: 40,
            model_calls: true,
            usage_gaps: false,
            delegations: false,
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

/// A model selection with a context limit.
fn provider_changed(config_id: i64, model: &str, context_limit: u64) -> AgentEventKind {
    AgentEventKind::ProviderChanged {
        provider: "mock".to_owned(),
        model: model.to_owned(),
        config_id,
        context_limit: Some(context_limit),
        provider_node_id: None,
    }
}

/// One model call: the attempt start, its settlement and the request end.
/// `usage` is the (input, output) tokens, none for a call that reported no
/// usage.
fn model_call(
    events: &mut Vec<Value>,
    seq: &mut i64,
    turn: usize,
    model: &str,
    usage: Option<(u32, u32)>,
) {
    let attempt = AttemptRef {
        operation_id: format!("op{turn}"),
        attempt_id: format!("att{turn}"),
        attempt_index: 0,
        session_id: "s1".to_owned(),
        cycle_id: format!("cycle{turn}"),
        purpose: AttemptPurpose::MainResponse,
    };
    push(
        events,
        seq,
        AgentEventKind::LlmAttemptStarted {
            start: samples::start_for(attempt.clone(), "mock", model),
        },
    );
    let settlement = samples::settlement_for(
        attempt.clone(),
        usage.map(|(input, output)| samples::sample_usage(input, output)),
    );
    let accounting = RequestAccounting {
        operation_id: attempt.operation_id.clone(),
        attempt_ids: vec![attempt.attempt_id],
        known_subtotal_usd: settlement.valuation.known_subtotal_usd,
        complete: settlement.valuation.complete_total_usd.is_some(),
        reasons: Vec::new(),
    };
    let cost_usd = settlement.valuation.complete_total_usd;
    let request_usage = settlement.usage.clone();
    push(
        events,
        seq,
        AgentEventKind::LlmAttemptSettled { settlement },
    );
    let (input, output) = usage.unwrap_or((1_000 + 300 * turn as u32, 40));
    push(
        events,
        seq,
        AgentEventKind::LlmRequestEnd {
            usage: request_usage,
            tool_calls: 0,
            finish_reason: None,
            cost_usd,
            cumulative_cost_usd: None,
            context_tokens: u64::from(input) + u64::from(output),
            metrics: ExecutionMetrics { steps: 1, turns: 1 },
            accounting: Some(accounting),
            prefix_change: None,
            idle_secs: None,
        },
    );
}

/// The events of one delegation as the agent journals them: the delegate tool
/// call, the request, the child's fork, a live-only stage change and the
/// outcome, with the tool call failed when the delegation failed.
fn delegation(events: &mut Vec<Value>, seq: &mut i64, ids: &DelegationIds) {
    let delegation: Delegation = serde_json::from_value(json!({
        "public_id": ids.delegation_id,
        "target_agent_id": "coder",
        "objective": format!("lorem {}", ids.delegation_id),
        "objective_hash": "0000000000000000",
        "context": null,
        "constraints": null,
        "expected_output": null,
        "planning_summary": null,
        "status": "requested",
        "retry_count": 0,
        "created_at": "2026-01-01T00:00:00Z",
        "completed_at": null,
    }))
    .expect("a generated delegation decodes");
    push(
        events,
        seq,
        AgentEventKind::ToolCallStart {
            tool_call_id: ids.tool_call_id.clone(),
            tool_name: "delegate".to_owned(),
            arguments: "{\"target_agent_id\":\"coder\"}".to_owned(),
        },
    );
    push(
        events,
        seq,
        AgentEventKind::DelegationRequested {
            delegation,
            batch_id: None,
            tool_call_id: Some(ids.tool_call_id.clone()),
            cycle_id: None,
            handoff_from: None,
        },
    );
    push(
        events,
        seq,
        AgentEventKind::SessionForked {
            parent_session_id: "s1".to_owned(),
            child_session_id: ids.child_session_id.clone(),
            target_agent_id: "coder".to_owned(),
            origin: ForkOrigin::Delegation,
            fork_point_type: ForkPointType::ProgressEntry,
            fork_point_ref: ids.delegation_id.clone(),
            instructions: None,
            selected_model_id: None,
            selected_provider_node_id: None,
        },
    );
    push(
        events,
        seq,
        AgentEventKind::DelegationStageChanged {
            delegation_id: ids.delegation_id.clone(),
            stage: Some(DelegationStage::Verifying),
            source_epoch: "epoch-1".to_owned(),
        },
    );
    if ids.completes {
        push(
            events,
            seq,
            AgentEventKind::DelegationCompleted {
                delegation_id: ids.delegation_id.clone(),
                result: Some("ipsum dolor".to_owned()),
            },
        );
    } else {
        push(
            events,
            seq,
            AgentEventKind::DelegationFailed {
                delegation_id: ids.delegation_id.clone(),
                error: "sit amet".to_owned(),
            },
        );
    }
    push(
        events,
        seq,
        AgentEventKind::ToolCallEnd {
            tool_call_id: ids.tool_call_id.clone(),
            tool_name: "delegate".to_owned(),
            is_error: !ids.completes,
            result: if ids.completes {
                "ipsum dolor".to_owned()
            } else {
                "sit amet".to_owned()
            },
        },
    );
}

/// Charge the generated model-call events to `session_id`. The generator
/// names its own session (`s1`); a rig that journals the events under another
/// session id calls this on each event kind before journaling it, so the
/// attempt records belong to the session that holds them. This is the one
/// place that knows which event kinds carry a session id.
pub fn charge_to_session(kind: &mut AgentEventKind, session_id: &str) {
    match kind {
        AgentEventKind::LlmAttemptStarted { start } => {
            start.attempt.session_id = session_id.to_owned();
        }
        AgentEventKind::LlmAttemptSettled { settlement } => {
            settlement.attempt.session_id = session_id.to_owned();
        }
        AgentEventKind::SessionForked {
            parent_session_id, ..
        } => {
            *parent_session_id = session_id.to_owned();
        }
        _ => {}
    }
}

/// One journal event as JSON, with history position `seq`.
pub fn event(seq: i64, kind: AgentEventKind) -> Value {
    let mut events = Vec::new();
    let mut at = seq - 1;
    push(&mut events, &mut at, kind);
    events.remove(0)
}

/// Generate a session as the agent journals it: per turn a run start (before
/// the prompt is stored), the prompt, a reply with reasoning, tool calls (one
/// failing, a todo-list update every third turn), steering, the run end and,
/// every n-th turn, a compaction. Every other turn a tool waits for a
/// permission, every fifth turn a scheduled run without a prompt follows, and
/// applied steering is stored as a user message of its run. Steering, permission
/// and run events other than the run boundaries are kinds the reducer ignores;
/// run state comes from state updates.
pub fn generate(spec: TraceSpec) -> Trace {
    let mut rng = Rng::new(spec.seed);
    let mut events = Vec::new();
    let mut seq = 0i64;
    let mut model = "mock-model";
    if spec.model_calls {
        push(&mut events, &mut seq, provider_changed(1, model, 200_000));
    }
    for turn in 0..spec.turns {
        let run_id = format!("r{turn}");
        if spec.model_calls && spec.usage_gaps && turn == spec.turns / 2 {
            model = "mock-model-2";
            push(&mut events, &mut seq, provider_changed(2, model, 100_000));
        }
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
                slash_command: None,
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
            // An applied steering prompt becomes a user message of the
            // running run: its blocks (live only), then the stored prompt.
            let steer_id = format!("s{turn}");
            let steer_text = rng.lorem(3);
            push(
                &mut events,
                &mut seq,
                AgentEventKind::UserPromptBlock {
                    slash_command: None,
                    message_id: steer_id.clone(),
                    client_prompt_id: None,
                    block: ContentBlock::Text(TextContent::new(steer_text.clone())),
                },
            );
            push(
                &mut events,
                &mut seq,
                AgentEventKind::PromptReceived {
                    slash_command: None,
                    content: steer_text,
                    message_id: Some(steer_id),
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
        if spec.model_calls {
            let usage = (!(spec.usage_gaps && turn == 2))
                .then(|| (1_000 + 300 * turn as u32, 50 + turn as u32));
            model_call(&mut events, &mut seq, turn, model, usage);
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
            // Every other turn the first tool waits for a permission decision.
            if tool == 0 && turn % 2 == 0 {
                let permission_id = format!("p{turn}");
                push(
                    &mut events,
                    &mut seq,
                    AgentEventKind::PermissionRequested {
                        permission_id: permission_id.clone(),
                        task_id: None,
                        tool_name: "shell".to_owned(),
                        reason: "needs approval".to_owned(),
                        tool_call_id: id.clone(),
                        request: None,
                    },
                );
                push(
                    &mut events,
                    &mut seq,
                    AgentEventKind::PermissionGranted {
                        permission_id,
                        granted: true,
                    },
                );
            }
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
        if spec.delegations
            && let Some(ids) = delegation_ids(turn)
        {
            delegation(&mut events, &mut seq, &ids);
        }
        if spec.compaction_every > 0 && (turn + 1) % spec.compaction_every == 0 {
            let id = format!("c{turn}");
            push(
                &mut events,
                &mut seq,
                AgentEventKind::CompactionStart {
                    slash_command: None,
                    // With a model call just before, the compaction's
                    // `tokensBefore` is that request's context; the estimate
                    // is the fallback of a session with no measurement.
                    token_estimate: 1_000 + 10 * turn as u32,
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
                    context_tokens: Some(200 + turn as u64),
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
        // Every fifth turn a scheduled run follows: it has output and no prompt.
        if turn % 5 == 4 {
            let run_id = format!("sched{turn}");
            push(
                &mut events,
                &mut seq,
                AgentEventKind::RunStarted {
                    run_id: run_id.clone(),
                    origin: format!("scheduled:s1:t{turn}"),
                },
            );
            let words = 8 + rng.below(8);
            let output = rng.lorem(words);
            push(
                &mut events,
                &mut seq,
                AgentEventKind::AssistantMessageStored {
                    content: output,
                    thinking: None,
                    reasoning_parts: Vec::new(),
                    message_id: Some(format!("sa{turn}")),
                },
            );
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
    }
    if spec.model_calls && spec.usage_gaps {
        // The model changes after the last request: its context measurement
        // does not describe the new model.
        push(
            &mut events,
            &mut seq,
            provider_changed(3, "mock-model-3", 50_000),
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
        "subagentUpdates": []
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

/// A `querymt/session/stateUpdate` input.
fn state_update(state: &str, run_id: &str, stop_reason: Option<&str>) -> SessionInput {
    let mut notification = json!({ "sessionId": "s1", "state": state, "runId": run_id });
    if let Some(stop) = stop_reason {
        notification["stopReason"] = json!(stop);
    }
    SessionInput::StateUpdate {
        notification,
        at_ms: 0,
    }
}

/// A state update with the host's time (`atMs`).
fn state_update_at(
    state: &str,
    run_id: &str,
    stop_reason: Option<&str>,
    host_at_ms: Option<i64>,
) -> SessionInput {
    let SessionInput::StateUpdate {
        mut notification,
        at_ms,
    } = state_update(state, run_id, stop_reason)
    else {
        unreachable!("state_update builds a StateUpdate")
    };
    if let Some(at) = host_at_ms {
        notification["atMs"] = json!(at);
    }
    SessionInput::StateUpdate {
        notification,
        at_ms,
    }
}

fn loaded(spec: TraceSpec) -> SessionInput {
    let trace = generate(spec);
    SessionInput::SnapshotLoaded {
        response: snapshot_response(&trace.events, json!({})),
    }
}

/// The `subagent_update` the agent sends for the delegation of `turn`: with
/// `finished` false while it runs (stage verifying), true once it ended.
fn subagent_update_for(ids: &DelegationIds, finished: bool) -> Value {
    let mut update = json!({
        "sessionUpdate": "subagent_update",
        "sessionId": ids.child_session_id,
        "title": "coder",
        "description": format!("lorem {}", ids.delegation_id),
        "state": { "state": if finished { "idle" } else { "running" } },
        "_meta": { "querymt": { "delegationId": ids.delegation_id } },
    });
    if !finished {
        update["_meta"]["querymt"]["stage"] = json!("verifying");
    }
    update
}

/// The delegate tool call update that points at its child session.
fn session_reference_for(ids: &DelegationIds) -> Value {
    json!({ "sessionUpdate": "tool_call_update", "toolCallId": ids.tool_call_id,
             "content": [{ "type": "session", "sessionId": ids.child_session_id }] })
}

/// The golden scenarios: small, 1x and 4x sessions; streaming, history page,
/// optimistic prompt, compaction and delegation (subagent) cases.
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
    // The delegations of the generated session, as its live stream tells
    // them: each delegate tool call starts, its child is announced and the
    // call is linked to it, then the child finishes and the call ends.
    let delegation: Vec<SessionInput> = (0..2)
        .filter_map(delegation_ids)
        .flat_map(|ids| {
            let status = if ids.completes { "completed" } else { "failed" };
            vec![
                live(
                    json!({ "sessionUpdate": "tool_call", "toolCallId": ids.tool_call_id,
                         "title": "delegate", "status": "in_progress" }),
                ),
                live(subagent_update_for(&ids, false)),
                live(session_reference_for(&ids)),
                live(subagent_update_for(&ids, true)),
                live(json!({ "sessionUpdate": "tool_call_update",
                         "toolCallId": ids.tool_call_id, "status": status })),
            ]
        })
        .collect();
    let run_state = vec![
        vec![
            state_update("running", "r1", None),
            live(
                json!({ "sessionUpdate": "user_message_chunk", "messageId": "u1",
                         "content": { "type": "text", "text": "lorem" },
                         "_meta": { "querymt": { "runId": "r1" } } }),
            ),
            live(
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "shell",
                         "status": "in_progress",
                         "_meta": { "querymt": { "runId": "r1" } } }),
            ),
        ],
        vec![
            state_update("requires_action", "r1", None),
            state_update("running", "r1", None),
            state_update("idle", "r1", Some("end_turn")),
        ],
        // A scheduled run: reported by the state, its output carries no run id.
        vec![
            state_update("running", "sched-1", None),
            text_chunk("m-sched", "ipsum"),
            state_update("idle", "sched-1", Some("end_turn")),
        ],
    ];
    // A load of the generated session with its delegations: the agent lists
    // each child as a `subagent_update` and links it to its tool call in the
    // summary; a live update then patches one child.
    let delegation_ids: Vec<DelegationIds> = (0..2).filter_map(delegation_ids).collect();
    let subagent = vec![
        vec![SessionInput::SnapshotLoaded {
            response: snapshot_response(
                &generate(TraceSpec {
                    delegations: true,
                    ..TraceSpec::small(3)
                })
                .events,
                json!({
                    "subagentUpdates": delegation_ids.iter().map(|ids| {
                        let mut update = subagent_update_for(ids, true);
                        update.as_object_mut().map(|o| o.remove("sessionUpdate"));
                        update
                    }).collect::<Vec<_>>(),
                    "summary": { "usage": { "latestRequest": null, "sessionTotals": null,
                            "partial": false, "requestCount": 0, "contextTokens": null,
                            "contextPosition": null,
                            "accounting": serde_json::to_value(
                                querymt_agent_protocol_types::accounting::ScopeTotals::default())
                                .unwrap_or(Value::Null) },
                        "provider": null, "pendingInputs": [],
                        "childSessions": delegation_ids.iter().map(|ids| json!({
                            "childSessionId": ids.child_session_id,
                            "delegationId": ids.delegation_id,
                            "toolCallId": ids.tool_call_id,
                        })).collect::<Vec<_>>() },
                }),
            ),
        }],
        vec![live(
            json!({ "sessionUpdate": "subagent_update", "sessionId": "child0",
                     "title": "reviewer" }),
        )],
    ];
    let cursor = vec![
        vec![
            text_chunk("m-cur", "dolor"),
            live(
                json!({ "sessionUpdate": "cursor_update", "cursorId": "cur-1",
                         "anchor": { "type": "agent_message", "messageId": "m-cur",
                                      "position": "after" },
                         "capabilities": { "fork": {} } }),
            ),
            live(
                json!({ "sessionUpdate": "cursor_update", "cursorId": "cur-2",
                         "anchor": { "type": "agent_message", "messageId": "not-here",
                                      "position": "after" } }),
            ),
        ],
        // A load discards the cursors and rebuilds them from its own.
        vec![SessionInput::SnapshotLoaded {
            response: snapshot_response(
                &generate(TraceSpec::small(4)).events,
                json!({ "cursorUpdates": [{
                    "cursorId": "cur-3",
                    "anchor": { "type": "agent_message", "messageId": "a0", "position": "after" },
                    "capabilities": { "fork": {} },
                }] }),
            ),
        }],
    ];
    // Usage: a measurement, the model change that makes it stale, a new
    // measurement; then snapshots whose summary is stale and fresh, and one
    // whose accounting covers no model call.
    let totals = json!({ "inputTokens": 9, "outputTokens": 1, "reasoningTokens": 0,
                         "cacheReadTokens": 0, "cacheWriteTokens": 0 });
    let summary = |context_position: i64| {
        json!({ "summary": {
            "usage": { "latestRequest": totals, "sessionTotals": totals, "partial": false,
                       "requestCount": 2, "contextTokens": 12000,
                       "contextPosition": context_position,
                       "accounting": serde_json::to_value(
                           querymt_agent_protocol_types::accounting::ScopeTotals::default())
                           .unwrap_or(Value::Null) },
            "provider": { "provider": "mock", "model": "mock-model-2",
                          "contextLimit": 100000, "position": 10 },
            "pendingInputs": [], "childSessions": [] } })
    };
    let usage = vec![
        vec![
            live(
                json!({ "sessionUpdate": "usage_update", "used": 12000, "size": 200000,
                          "_meta": { "querymt": { "usage": { "version": 1,
                              "latestRequest": totals, "sessionTotals": totals,
                              "partial": false } } } }),
            ),
            live(json!({ "sessionUpdate": "session_info_update",
                          "_meta": { "querymt": { "contextInvalidated": true } } })),
        ],
        vec![live(
            json!({ "sessionUpdate": "usage_update", "used": 3000, "size": 100000 }),
        )],
        vec![SessionInput::SnapshotLoaded {
            response: snapshot_response(&generate(TraceSpec::small(7)).events, summary(4)),
        }],
        vec![SessionInput::SnapshotLoaded {
            response: snapshot_response(&generate(TraceSpec::small(7)).events, summary(12)),
        }],
    ];
    let usage_gaps = vec![vec![loaded(TraceSpec {
        usage_gaps: true,
        ..TraceSpec::small(8)
    })]];
    let no_model_calls = vec![vec![loaded(TraceSpec {
        model_calls: false,
        ..TraceSpec::small(9)
    })]];
    // Active time: a load during a run (total from the summary, start from
    // the runtime status), the sync that follows it, the run ending, a
    // second run with a permission wait, and a reload that sets the total.
    let active_load = |active_ms: u64, running: bool| {
        let mut status = json!({ "phase": "idle", "active_run_id": null, "steerable": false,
            "pending_steering_count": 0, "queued_input_count": 0, "run_started_at_ms": null,
            "observation": { "availability": "live" } });
        if running {
            status["phase"] = json!("model");
            status["active_run_id"] = json!("r-live");
            status["run_started_at_ms"] = json!(50_000);
        }
        SessionInput::SnapshotLoaded {
            response: snapshot_response(
                &generate(TraceSpec::small(10)).events,
                json!({ "runtimeStatus": status,
                    "summary": { "usage": { "latestRequest": null, "sessionTotals": null,
                        "partial": false, "requestCount": 0, "contextTokens": null,
                        "contextPosition": null,
                        "accounting": serde_json::to_value(
                            querymt_agent_protocol_types::accounting::ScopeTotals::default())
                            .unwrap_or(Value::Null) },
                        "provider": null, "pendingInputs": [], "childSessions": [],
                        "activeMs": active_ms } }),
            ),
        }
    };
    let active_time = vec![
        vec![
            active_load(120_000, true),
            state_update_at("running", "r-live", None, None),
        ],
        vec![state_update_at(
            "idle",
            "r-live",
            Some("end_turn"),
            Some(65_000),
        )],
        vec![
            state_update_at("running", "r-next", None, Some(70_000)),
            state_update_at("requires_action", "r-next", None, Some(71_000)),
            state_update_at("running", "r-next", None, Some(72_000)),
            state_update_at("idle", "r-next", Some("end_turn"), Some(80_000)),
        ],
        vec![active_load(145_000, false)],
    ];
    // Commands: an update, a load that keeps them, a replacement with a
    // `/`-prefixed name and an input, then an empty update that clears them.
    let commands = |list: Value| {
        live(json!({ "sessionUpdate": "available_commands_update",
                      "availableCommands": list }))
    };
    let available_commands = vec![
        vec![commands(json!([
            { "name": "compact", "description": "Compact the conversation" },
            { "name": "worktree", "description": "Work in a worktree" },
        ]))],
        vec![loaded(TraceSpec::small(11))],
        vec![commands(json!([
            { "name": "/compact", "description": "Compact the conversation",
              "input": { "hint": "[optional instructions]" } },
        ]))],
        vec![commands(json!([]))],
    ];
    vec![
        case("available_commands", available_commands),
        case("active_time", active_time),
        case("usage", usage),
        case("usage_gaps", usage_gaps),
        case("no_model_calls", no_model_calls),
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
        case("run_state", run_state),
        case("subagent", subagent),
        case("cursor", cursor),
    ]
}

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use querymt_agent::acp::protocol::{LoadSessionRequest, SessionId};
use querymt_agent::acp::{
    load_session_with_replay, shared::replay_agent_events_to_session_notifications,
};
use querymt_agent::agent::{AgentConfig, AgentConfigBuilder, LocalAgentHandle};
use querymt_agent::events::{AgentEventKind, EventOrigin, ExecutionMetrics};
use querymt_agent::session::domain::{
    Artifact, IntentSnapshot, ProgressEntry, ProgressKind, Task, TaskKind, TaskStatus,
};
use querymt_agent::session::projection::{EventJournal, NewDurableEvent};
use querymt_agent::session::store::SessionStore;
use querymt_agent::session::{SqliteStorage, StorageBackend, ViewStore, load_session_snapshot};
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::runtime::Runtime;

const REFERENCE_EVENTS: usize = 4_183;
const REFERENCE_PROGRESS: usize = 1_061;
const REFERENCE_TOOL_CALLS: usize = 732;
const REFERENCE_LLM_REQUEST_STARTS: usize = 330;
const REFERENCE_LLM_REQUEST_ENDS: usize = 329;
const REFERENCE_PROMPTS: usize = 16;
const REFERENCE_ASSISTANT_MESSAGES: usize = 329;
const REFERENCE_SNAPSHOTS: usize = 221;
const REFERENCE_TASKS: usize = 8;
const REFERENCE_INTENTS: usize = 9;
const REFERENCE_ARTIFACTS: usize = 2;

#[derive(Clone, Copy)]
struct SyntheticSessionSpec {
    scale: f64,
}

impl SyntheticSessionSpec {
    fn scaled(self, reference: usize) -> usize {
        ((reference as f64 * self.scale).round() as usize).max(1)
    }

    fn event_count(self) -> usize {
        self.scaled(REFERENCE_EVENTS)
    }

    fn label(self) -> String {
        format!("scale_{}x", self.scale)
    }
}

struct Scenario {
    spec: SyntheticSessionSpec,
    store: Arc<SqliteStorage>,
    view_store: Arc<dyn ViewStore>,
    config: Arc<AgentConfig>,
    snapshot_handle: Arc<LocalAgentHandle>,
    session_id: String,
    payload_bytes: usize,
    _tmp_dir: TempDir,
}

fn repeated_payload(prefix: &str, index: usize, bytes: usize) -> String {
    let header = format!("{prefix}-{index:06}: ");
    if header.len() >= bytes {
        return header;
    }
    let mut value = String::with_capacity(bytes);
    value.push_str(&header);
    value.extend(std::iter::repeat_n('x', bytes - header.len()));
    value
}

fn durable(session_id: &str, kind: AgentEventKind) -> NewDurableEvent {
    NewDurableEvent {
        session_id: session_id.to_string(),
        origin: EventOrigin::Local,
        source_node: None,
        kind,
    }
}

async fn append_event(store: &SqliteStorage, session_id: &str, kind: AgentEventKind) -> usize {
    let payload_bytes = serde_json::to_vec(&kind)
        .expect("serialize synthetic event")
        .len();
    store
        .append_durable(&durable(session_id, kind))
        .await
        .expect("append synthetic event");
    payload_bytes
}

fn make_agent_config(tmp_dir: &TempDir, store: Arc<SqliteStorage>) -> Arc<AgentConfig> {
    let providers_path = tmp_dir.path().join("providers.toml");
    std::fs::write(&providers_path, "providers = []\n")
        .expect("write synthetic benchmark provider config");
    let registry = Arc::new(
        PluginRegistry::from_path(&providers_path)
            .expect("create empty plugin registry for session load benchmark"),
    );
    let backend: Arc<dyn StorageBackend> = store;
    Arc::new(AgentConfigBuilder::new(registry, backend, LLMParams::default()).build())
}

async fn seed_scenario(spec: SyntheticSessionSpec) -> Scenario {
    let tmp_dir = tempfile::tempdir().expect("create temp dir for session load benchmark");
    let db_path = tmp_dir
        .path()
        .join(format!("session_load_{}.sqlite3", spec.label()));
    let store = Arc::new(
        SqliteStorage::connect(db_path)
            .await
            .expect("create synthetic benchmark sqlite store"),
    );
    let session = store
        .create_session(
            Some(format!("synthetic-{}", spec.label())),
            Some(PathBuf::from("/tmp/querymt-session-load-benchmark")),
            None,
            None,
        )
        .await
        .expect("create synthetic benchmark session");
    let session_id = session.public_id.clone();
    let now = OffsetDateTime::now_utc();
    let mut payload_bytes = 0;
    let mut event_count = 0;

    payload_bytes +=
        append_event(store.as_ref(), &session_id, AgentEventKind::SessionCreated).await;
    event_count += 1;

    let mut tasks = Vec::new();
    for index in 0..spec.scaled(REFERENCE_TASKS) {
        let task = store
            .create_task(Task {
                id: 0,
                public_id: String::new(),
                session_id: session.id,
                kind: TaskKind::Finite,
                status: if index + 1 == spec.scaled(REFERENCE_TASKS) {
                    TaskStatus::Active
                } else {
                    TaskStatus::Done
                },
                expected_deliverable: Some(repeated_payload("deliverable", index, 320)),
                acceptance_criteria: Some(repeated_payload("criteria", index, 220)),
                revision: 1,
                creation_key: Some(format!("synthetic-task-{index}")),
                completion_evidence: None,
                completed_at: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .expect("seed synthetic task");
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::TaskCreated { task: task.clone() },
        )
        .await;
        event_count += 1;
        tasks.push(task);
    }

    for index in 0..spec.scaled(REFERENCE_INTENTS) {
        let intent = IntentSnapshot {
            id: 0,
            session_id: session.id,
            task_id: tasks.get(index % tasks.len()).map(|task| task.id),
            summary: repeated_payload("intent", index, 360),
            constraints: Some(repeated_payload("constraints", index, 180)),
            next_step_hint: Some(repeated_payload("next-step", index, 140)),
            revision: index as u64 + 1,
            source: "benchmark".to_string(),
            source_ref: None,
            created_at: now,
        };
        store
            .create_intent_snapshot(intent.clone())
            .await
            .expect("seed synthetic intent");
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::IntentUpdated {
                intent_snapshot: intent,
            },
        )
        .await;
        event_count += 1;
    }

    for index in 0..spec.scaled(REFERENCE_PROGRESS) {
        let progress = ProgressEntry {
            id: 0,
            session_id: session.id,
            task_id: tasks.get(index % tasks.len()).map(|task| task.id),
            kind: if index % 8 == 0 {
                ProgressKind::Checkpoint
            } else {
                ProgressKind::ToolCall
            },
            content: repeated_payload("progress", index, 360),
            metadata: Some(repeated_payload("metadata", index, 80)),
            created_at: now,
        };
        store
            .append_progress_entry(progress.clone())
            .await
            .expect("seed synthetic progress projection");
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::ProgressRecorded {
                progress_entry: progress,
            },
        )
        .await;
        event_count += 1;
    }

    for index in 0..spec.scaled(REFERENCE_ARTIFACTS) {
        let artifact = Artifact {
            id: 0,
            session_id: session.id,
            task_id: tasks.get(index % tasks.len()).map(|task| task.id),
            kind: "source-file".to_string(),
            uri: None,
            path: Some(format!("src/generated_{index}.rs")),
            summary: Some(repeated_payload("artifact", index, 180)),
            created_at: now,
        };
        store
            .record_artifact(artifact.clone())
            .await
            .expect("seed synthetic artifact projection");
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::ArtifactRecorded { artifact },
        )
        .await;
        event_count += 1;
    }

    for index in 0..spec.scaled(REFERENCE_TOOL_CALLS) {
        let call_id = format!("synthetic-call-{index:06}");
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::ToolCallStart {
                tool_call_id: call_id.clone(),
                tool_name: "read_tool".to_string(),
                arguments: serde_json::json!({
                    "path": format!("src/generated_{index}.rs"),
                    "padding": repeated_payload("arguments", index, 400),
                })
                .to_string(),
            },
        )
        .await;
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::ToolCallEnd {
                tool_call_id: call_id,
                tool_name: "read_tool".to_string(),
                is_error: false,
                result: if index % 4 == 0 {
                    serde_json::json!({
                        "content": repeated_payload("tool-result", index, 3_450),
                    })
                    .to_string()
                } else {
                    repeated_payload("tool-result", index, 3_550)
                },
            },
        )
        .await;
        event_count += 2;
    }

    for index in 0..spec.scaled(REFERENCE_LLM_REQUEST_STARTS) {
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::LlmRequestStart {
                message_count: index as u32 + 1,
            },
        )
        .await;
        event_count += 1;
    }
    for index in 0..spec.scaled(REFERENCE_LLM_REQUEST_ENDS) {
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::LlmRequestEnd {
                usage: None,
                tool_calls: 2,
                finish_reason: None,
                cost_usd: Some(0.001),
                cumulative_cost_usd: Some(index as f64 * 0.001),
                context_tokens: 4_096 + index as u64,
                metrics: ExecutionMetrics {
                    steps: index as u32 + 1,
                    turns: index as u32 + 1,
                },
            },
        )
        .await;
        event_count += 1;
    }

    for index in 0..spec.scaled(REFERENCE_PROMPTS) {
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::PromptReceived {
                content: repeated_payload("prompt", index, 420),
                message_id: Some(format!("prompt-message-{index:06}")),
            },
        )
        .await;
        event_count += 1;
    }

    for index in 0..spec.scaled(REFERENCE_ASSISTANT_MESSAGES) {
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::AssistantMessageStored {
                content: repeated_payload("assistant", index, 500),
                thinking: None,
                message_id: Some(format!("assistant-message-{index:06}")),
            },
        )
        .await;
        event_count += 1;
    }

    for index in 0..spec.scaled(REFERENCE_SNAPSHOTS) {
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::SnapshotStart {
                policy: "turn".to_string(),
            },
        )
        .await;
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::SnapshotEnd {
                summary: Some(format!("synthetic snapshot {index}")),
            },
        )
        .await;
        event_count += 2;
    }

    let target_events = spec.event_count();
    assert!(
        event_count <= target_events,
        "synthetic event mix exceeds requested scale"
    );
    while event_count < target_events {
        payload_bytes += append_event(
            store.as_ref(),
            &session_id,
            AgentEventKind::HookNotice {
                event_name: "synthetic-lifecycle".to_string(),
                message: repeated_payload("notice", event_count, 180),
                is_error: false,
            },
        )
        .await;
        event_count += 1;
    }

    let audit = store
        .get_audit_view(&session_id, false)
        .await
        .expect("validate synthetic session fixture");
    assert_eq!(audit.events.len(), target_events);
    assert_eq!(
        audit.progress_entries.len(),
        spec.scaled(REFERENCE_PROGRESS)
    );
    assert_eq!(audit.tasks.len(), spec.scaled(REFERENCE_TASKS));
    assert_eq!(audit.intent_snapshots.len(), spec.scaled(REFERENCE_INTENTS));
    assert_eq!(audit.artifacts.len(), spec.scaled(REFERENCE_ARTIFACTS));
    let replay_count =
        replay_agent_events_to_session_notifications(&session_id, audit.events.clone()).len();
    assert_eq!(
        replay_count,
        spec.scaled(REFERENCE_TOOL_CALLS) * 2
            + spec.scaled(REFERENCE_ASSISTANT_MESSAGES)
            + spec.scaled(REFERENCE_PROMPTS)
    );

    let config = make_agent_config(&tmp_dir, store.clone());
    let snapshot_handle = Arc::new(LocalAgentHandle::from_config(config.clone()));
    let view_store: Arc<dyn ViewStore> = store.clone();

    eprintln!(
        "seeded {}: {} events, {:.2} MiB JSON, {} progress rows",
        spec.label(),
        target_events,
        payload_bytes as f64 / (1024.0 * 1024.0),
        audit.progress_entries.len()
    );

    Scenario {
        spec,
        store,
        view_store,
        config,
        snapshot_handle,
        session_id,
        payload_bytes,
        _tmp_dir: tmp_dir,
    }
}

fn scales_from_env() -> Vec<f64> {
    let raw = std::env::var("QMT_BENCH_SESSION_SCALES")
        .unwrap_or_else(|_| "0.125,0.25,0.5,1,2".to_string());
    let scales: Vec<f64> = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<f64>()
                .expect("invalid session benchmark scale")
        })
        .filter(|scale| *scale > 0.0)
        .collect();
    assert!(
        !scales.is_empty(),
        "at least one benchmark scale is required"
    );
    scales
}

async fn remove_loaded_actor(handle: &LocalAgentHandle, session_id: &str) {
    let actor = {
        let mut registry = handle.registry.lock().await;
        registry.remove(session_id)
    };
    if let Some(actor) = actor {
        actor
            .shutdown()
            .await
            .expect("shut down benchmark session actor");
    }
}

fn load_request(session_id: &str) -> LoadSessionRequest {
    LoadSessionRequest::new(SessionId::from(session_id.to_string()), PathBuf::new())
}

fn bench_session_load(c: &mut Criterion) {
    let rt = Runtime::new().expect("create tokio runtime for session load benchmark");
    let scenarios: Vec<Scenario> = scales_from_env()
        .into_iter()
        .map(|scale| rt.block_on(seed_scenario(SyntheticSessionSpec { scale })))
        .collect();

    let mut group = c.benchmark_group("session_load");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    for scenario in &scenarios {
        let parameter = format!(
            "{}_events_{}_mib_{:.2}",
            scenario.spec.label(),
            scenario.spec.event_count(),
            scenario.payload_bytes as f64 / (1024.0 * 1024.0)
        );
        group.throughput(Throughput::Elements(scenario.spec.event_count() as u64));

        group.bench_with_input(
            BenchmarkId::new("primitive/audit_view", &parameter),
            scenario,
            |b, scenario| {
                b.to_async(&rt).iter(|| async {
                    let audit = scenario
                        .view_store
                        .get_audit_view(&scenario.session_id, false)
                        .await
                        .expect("load synthetic audit view");
                    black_box(audit);
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("primitive/snapshot", &parameter),
            scenario,
            |b, scenario| {
                b.to_async(&rt).iter(|| async {
                    let snapshot = load_session_snapshot(
                        &scenario.snapshot_handle,
                        scenario.view_store.clone(),
                        &scenario.session_id,
                    )
                    .await
                    .expect("load synthetic session snapshot");
                    black_box(snapshot);
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("primitive/snapshot_serialize", &parameter),
            scenario,
            |b, scenario| {
                b.to_async(&rt).iter(|| async {
                    let snapshot = load_session_snapshot(
                        &scenario.snapshot_handle,
                        scenario.view_store.clone(),
                        &scenario.session_id,
                    )
                    .await
                    .expect("load synthetic session snapshot");
                    black_box(serde_json::to_vec(&snapshot).expect("serialize session snapshot"));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("acp/core_cold", &parameter),
            scenario,
            |b, scenario| {
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iterations {
                            let handle = LocalAgentHandle::from_config(scenario.config.clone());
                            let request = load_request(&scenario.session_id);
                            let started = Instant::now();
                            let response = handle
                                .load_session(request)
                                .await
                                .expect("cold-load synthetic session actor");
                            elapsed += started.elapsed();
                            black_box(response);
                            remove_loaded_actor(&handle, &scenario.session_id).await;
                        }
                        elapsed
                    })
                });
            },
        );

        let warm_handle = Arc::new(LocalAgentHandle::from_config(scenario.config.clone()));
        rt.block_on(warm_handle.load_session(load_request(&scenario.session_id)))
            .expect("warm synthetic session actor");
        group.bench_with_input(
            BenchmarkId::new("acp/core_warm", &parameter),
            scenario,
            |b, scenario| {
                b.to_async(&rt).iter(|| async {
                    let response = warm_handle
                        .load_session(load_request(&scenario.session_id))
                        .await
                        .expect("warm-load synthetic session actor");
                    black_box(response);
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("acp/replay_cold", &parameter),
            scenario,
            |b, scenario| {
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iterations {
                            let handle =
                                Arc::new(LocalAgentHandle::from_config(scenario.config.clone()));
                            let started = Instant::now();
                            let outcome = load_session_with_replay(
                                &handle,
                                load_request(&scenario.session_id),
                            )
                            .await
                            .expect("ACP-load synthetic session with replay");
                            elapsed += started.elapsed();
                            black_box(outcome);
                            remove_loaded_actor(&handle, &scenario.session_id).await;
                        }
                        elapsed
                    })
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("acp/replay_serialize_cold", &parameter),
            scenario,
            |b, scenario| {
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iterations {
                            let handle =
                                Arc::new(LocalAgentHandle::from_config(scenario.config.clone()));
                            let started = Instant::now();
                            let outcome = load_session_with_replay(
                                &handle,
                                load_request(&scenario.session_id),
                            )
                            .await
                            .expect("ACP-load synthetic session with replay");
                            let response = serde_json::to_vec(&outcome.response)
                                .expect("serialize ACP load response");
                            let notifications: usize = outcome
                                .notifications
                                .iter()
                                .map(|notification| {
                                    serde_json::to_vec(notification)
                                        .expect("serialize ACP replay notification")
                                        .len()
                                })
                                .sum();
                            elapsed += started.elapsed();
                            black_box((response, notifications));
                            remove_loaded_actor(&handle, &scenario.session_id).await;
                        }
                        elapsed
                    })
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("acp/wire_codec_cold", &parameter),
            scenario,
            |b, scenario| {
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let mut elapsed = Duration::ZERO;
                        for iteration in 0..iterations {
                            let handle =
                                Arc::new(LocalAgentHandle::from_config(scenario.config.clone()));
                            let started = Instant::now();
                            let outcome = load_session_with_replay(
                                &handle,
                                load_request(&scenario.session_id),
                            )
                            .await
                            .expect("ACP-load synthetic session for wire codec");
                            let mut wire_bytes = 0usize;
                            for notification in outcome.notifications {
                                let line = serde_json::to_vec(&serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "session/update",
                                    "params": notification,
                                }))
                                .expect("encode ACP notification wire line");
                                wire_bytes += line.len() + 1;
                                black_box(
                                    serde_json::from_slice::<serde_json::Value>(&line)
                                        .expect("decode ACP notification wire line"),
                                );
                            }
                            let response_line = serde_json::to_vec(&serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": iteration,
                                "result": outcome.response,
                            }))
                            .expect("encode ACP response wire line");
                            wire_bytes += response_line.len() + 1;
                            black_box(
                                serde_json::from_slice::<serde_json::Value>(&response_line)
                                    .expect("decode ACP response wire line"),
                            );
                            elapsed += started.elapsed();
                            black_box(wire_bytes);
                            remove_loaded_actor(&handle, &scenario.session_id).await;
                        }
                        elapsed
                    })
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("dashboard/core_cold", &parameter),
            scenario,
            |b, scenario| {
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iterations {
                            let handle = LocalAgentHandle::from_config(scenario.config.clone());
                            let started = Instant::now();
                            let snapshot = load_session_snapshot(
                                &handle,
                                scenario.view_store.clone(),
                                &scenario.session_id,
                            )
                            .await
                            .expect("load desktop-style session snapshot");
                            let session = scenario
                                .store
                                .get_session(&scenario.session_id)
                                .await
                                .expect("read synthetic session")
                                .expect("synthetic session exists");
                            let response = handle
                                .load_session(load_request(&scenario.session_id))
                                .await
                                .expect("load desktop-style session actor");
                            let serialized = serde_json::to_vec(&snapshot)
                                .expect("serialize desktop-style session snapshot");
                            elapsed += started.elapsed();
                            black_box((session, response, serialized));
                            remove_loaded_actor(&handle, &scenario.session_id).await;
                        }
                        elapsed
                    })
                });
            },
        );

        rt.block_on(remove_loaded_actor(&warm_handle, &scenario.session_id));
    }

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .with_plots()
        .nresamples(100_000)
        .warm_up_time(Duration::from_secs(3));
    targets = bench_session_load
);
criterion_main!(benches);

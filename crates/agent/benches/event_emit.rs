//! Event emission benchmark: today's per-event spawn vs. ordered writers.
//!
//! Motivation (openspec change `unify-session-interactions`): a prompt response
//! has no causal link to the session events that precede it. One candidate fix
//! is a per-session "event watermark", which requires events of a session to be
//! written and published *in order* and to support a "drain, then tell me the
//! tip" operation. This benchmark measures what that ordering costs before any
//! production code is changed. The candidate writers live in this file only.
//!
//! Variants
//! - `baseline`    : `AgentConfig::emit_event` as it exists today
//!   (one spawned task per durable event, ephemeral inline).
//! - `per_session` : one ordered writer task per session; durable and
//!   ephemeral events share the queue; supports `drain`.
//! - `global`      : a single ordered writer task for all sessions.
//!
//! Workload per session: for every durable event, `DELTAS_PER_DURABLE`
//! ephemeral streaming deltas precede it (a model streaming a message).
//!
//! Output
//! - Criterion throughput numbers (time until every durable event was written
//!   *and* observed on the fanout).
//! - A custom report (printed once, unless `EVENT_EMIT_SKIP_REPORT` is set)
//!   with emit-to-fanout latency, caller-side emit cost, drain time and the
//!   number of events delivered out of order.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use querymt_agent::agent::{AgentConfig, AgentConfigBuilder};
use querymt_agent::event_fanout::EventFanout;
use querymt_agent::event_sink::EventSink;
use querymt_agent::events::{AgentEventKind, Durability, EventEnvelope, classify_durability};
use querymt_agent::session::store::SessionStore;
use querymt_agent::session::{SqliteStorage, StorageBackend};
use std::collections::HashMap;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tokio::sync::{broadcast, mpsc, oneshot};

/// Ephemeral streaming deltas emitted before each durable event.
const DELTAS_PER_DURABLE: usize = 4;
/// The emitter yields to the scheduler every `PACE` events, like an async
/// stream producer would. Without it a tight loop outruns the 1024-slot fanout.
const PACE: usize = 4;
/// Upper bound for waiting on a run to be observed completely.
const WAIT_LIMIT: Duration = Duration::from_secs(60);
/// Runs per configuration for the custom report.
const REPORT_RUNS: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    Baseline,
    PerSession,
    Global,
}

impl Variant {
    const ALL: [Variant; 3] = [Variant::Baseline, Variant::PerSession, Variant::Global];

    fn name(self) -> &'static str {
        match self {
            Variant::Baseline => "baseline",
            Variant::PerSession => "per_session",
            Variant::Global => "global",
        }
    }
}

#[derive(Clone, Copy)]
enum Storage {
    File,
    Memory,
}

impl Storage {
    const ALL: [Storage; 2] = [Storage::File, Storage::Memory];

    fn name(self) -> &'static str {
        match self {
            Storage::File => "file",
            Storage::Memory => "memory",
        }
    }
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    sessions: usize,
    durable_per_session: usize,
}

const SCENARIOS: [Scenario; 3] = [
    Scenario {
        name: "single_100",
        sessions: 1,
        durable_per_session: 100,
    },
    Scenario {
        name: "single_1000",
        sessions: 1,
        durable_per_session: 1000,
    },
    Scenario {
        name: "multi_8x125",
        sessions: 8,
        durable_per_session: 125,
    },
];

impl Scenario {
    fn events_per_session(self) -> usize {
        self.durable_per_session * (1 + DELTAS_PER_DURABLE)
    }

    fn total_events(self) -> u64 {
        (self.sessions * self.events_per_session()) as u64
    }

    fn total_durable(self) -> usize {
        self.sessions * self.durable_per_session
    }
}

/// Event `j` of a session is durable when it closes a delta group.
fn is_durable(j: usize) -> bool {
    (j + 1).is_multiple_of(1 + DELTAS_PER_DURABLE)
}

fn make_event(session_index: usize, j: usize) -> AgentEventKind {
    let message_id = format!("{session_index}:{j}");
    if is_durable(j) {
        AgentEventKind::AssistantMessageStored {
            content: "x".repeat(200),
            thinking: None,
            reasoning_parts: Vec::new(),
            message_id: Some(message_id),
        }
    } else {
        AgentEventKind::AssistantContentDelta {
            content: "tok".to_string(),
            message_id,
        }
    }
}

/// Extract `(session_index, j, durable)` from an event produced by `make_event`.
fn parse_event(envelope: &EventEnvelope) -> Option<(usize, usize, bool)> {
    let message_id = match envelope.kind() {
        AgentEventKind::AssistantContentDelta { message_id, .. } => message_id.as_str(),
        AgentEventKind::AssistantMessageStored {
            message_id: Some(message_id),
            ..
        } => message_id.as_str(),
        _ => return None,
    };
    let (session_index, j) = message_id.split_once(':')?;
    let j: usize = j.parse().ok()?;
    Some((session_index.parse().ok()?, j, envelope.is_durable()))
}

// ── Candidate ordered writers (benchmark-local) ─────────────────────────────

/// `AgentEventKind` is large, but boxing it here would add an allocation to
/// the exact caller-side emit cost this benchmark measures.
#[allow(clippy::large_enum_variant)]
enum Cmd {
    Event(String, AgentEventKind),
    Drain(String, oneshot::Sender<i64>),
}

async fn writer_loop(sink: Arc<EventSink>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let mut tips: HashMap<String, i64> = HashMap::new();
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Event(session_id, kind) => match classify_durability(&kind) {
                Durability::Durable => match sink.emit_durable(&session_id, kind).await {
                    Ok(event) => {
                        tips.insert(session_id, event.stream_seq);
                    }
                    Err(err) => eprintln!("event_emit bench: durable write failed: {err}"),
                },
                Durability::Ephemeral => sink.emit_ephemeral(&session_id, kind),
            },
            Cmd::Drain(session_id, reply) => {
                let _ = reply.send(tips.get(&session_id).copied().unwrap_or(0));
            }
        }
    }
}

struct Writers {
    sink: Arc<EventSink>,
    global: Option<mpsc::UnboundedSender<Cmd>>,
    per_session: Mutex<HashMap<String, mpsc::UnboundedSender<Cmd>>>,
}

impl Writers {
    fn new(sink: Arc<EventSink>, global: bool) -> Self {
        let global = global.then(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(writer_loop(sink.clone(), rx));
            tx
        });
        Self {
            sink,
            global,
            per_session: Mutex::new(HashMap::new()),
        }
    }

    fn sender_for(&self, session_id: &str) -> mpsc::UnboundedSender<Cmd> {
        if let Some(global) = &self.global {
            return global.clone();
        }
        let mut writers = self.per_session.lock().unwrap();
        writers
            .entry(session_id.to_string())
            .or_insert_with(|| {
                let (tx, rx) = mpsc::unbounded_channel();
                tokio::spawn(writer_loop(self.sink.clone(), rx));
                tx
            })
            .clone()
    }

    fn emit(&self, session_id: &str, kind: AgentEventKind) {
        let _ = self
            .sender_for(session_id)
            .send(Cmd::Event(session_id.to_string(), kind));
    }

    async fn drain(&self, session_id: &str) -> i64 {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .sender_for(session_id)
            .send(Cmd::Drain(session_id.to_string(), tx));
        rx.await.unwrap_or(0)
    }
}

enum Emitter {
    Baseline(Arc<AgentConfig>),
    Ordered(Writers),
}

impl Emitter {
    fn emit(&self, session_id: &str, kind: AgentEventKind) {
        match self {
            Emitter::Baseline(config) => config.emit_event(session_id, kind),
            Emitter::Ordered(writers) => writers.emit(session_id, kind),
        }
    }

    /// `None` for the baseline, which has no drain operation.
    async fn drain(&self, session_id: &str) -> Option<i64> {
        match self {
            Emitter::Baseline(_) => None,
            Emitter::Ordered(writers) => Some(writers.drain(session_id).await),
        }
    }
}

// ── Environment ──────────────────────────────────────────────────────────────

struct Env {
    emitter: Arc<Emitter>,
    fanout: Arc<EventFanout>,
    session_ids: Vec<String>,
    _tmp: TempDir,
}

fn make_agent_config(tmp_dir: &TempDir, store: Arc<SqliteStorage>) -> Arc<AgentConfig> {
    let providers_path = tmp_dir.path().join("providers.toml");
    std::fs::write(&providers_path, "providers = []\n").expect("write provider config");
    let registry =
        Arc::new(PluginRegistry::from_path(&providers_path).expect("empty plugin registry"));
    let backend: Arc<dyn StorageBackend> = store;
    Arc::new(AgentConfigBuilder::new(registry, backend, LLMParams::default()).build())
}

async fn build_env(variant: Variant, storage: Storage, sessions: usize) -> Env {
    let tmp = tempfile::tempdir().expect("temp dir");
    let db_path = match storage {
        Storage::File => tmp.path().join("event_emit.sqlite3"),
        Storage::Memory => PathBuf::from(":memory:"),
    };
    let store = Arc::new(SqliteStorage::connect(db_path).await.expect("sqlite store"));

    let mut session_ids = Vec::new();
    for index in 0..sessions {
        let session = store
            .create_session(
                Some(format!("event-emit-{index}")),
                Some(PathBuf::from("/tmp/querymt-event-emit-benchmark")),
                None,
                None,
            )
            .await
            .expect("create benchmark session");
        session_ids.push(session.public_id);
    }

    let (emitter, fanout) = match variant {
        Variant::Baseline => {
            let config = make_agent_config(&tmp, store.clone());
            let fanout = config.event_sink.fanout().clone();
            (Emitter::Baseline(config), fanout)
        }
        Variant::PerSession | Variant::Global => {
            let backend: Arc<dyn StorageBackend> = store.clone();
            let fanout = Arc::new(EventFanout::new());
            let sink = Arc::new(EventSink::new(backend.event_journal(), fanout.clone()));
            (
                Emitter::Ordered(Writers::new(sink, variant == Variant::Global)),
                fanout,
            )
        }
    };

    Env {
        emitter: Arc::new(emitter),
        fanout,
        session_ids,
        _tmp: tmp,
    }
}

// ── One measured run ─────────────────────────────────────────────────────────

struct RecvStats {
    times: Vec<Vec<Option<Instant>>>,
    durable_inversions: usize,
    all_inversions: usize,
    lagged: u64,
    done_at: Option<Instant>,
}

struct RunResult {
    /// First emit until every durable event was observed on the fanout.
    total: Duration,
    emit_cost: Duration,
    emit_calls: usize,
    /// Last emit call until the run is settled: the drain acknowledgement for
    /// ordered writers, full delivery for the baseline (which has no drain).
    settle: Duration,
    lat_durable: Vec<Duration>,
    lat_ephemeral: Vec<Duration>,
    durable_inversions: usize,
    all_inversions: usize,
    lagged: u64,
    complete: bool,
}

async fn receive(
    mut rx: broadcast::Receiver<EventEnvelope>,
    sessions: usize,
    events_per_session: usize,
    expected_durable: usize,
    deadline: tokio::time::Instant,
) -> RecvStats {
    let mut stats = RecvStats {
        times: vec![vec![None; events_per_session]; sessions],
        durable_inversions: 0,
        all_inversions: 0,
        lagged: 0,
        done_at: None,
    };
    let mut last_any: Vec<i64> = vec![-1; sessions];
    let mut last_durable: Vec<i64> = vec![-1; sessions];
    let mut durable_seen = 0usize;

    loop {
        let envelope = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(envelope)) => envelope,
            Ok(Err(broadcast::error::RecvError::Lagged(missed))) => {
                stats.lagged += missed;
                continue;
            }
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => break,
        };
        let Some((session_index, j, durable)) = parse_event(&envelope) else {
            continue;
        };
        if session_index >= sessions || j >= events_per_session {
            continue;
        }
        stats.times[session_index][j] = Some(Instant::now());
        if (j as i64) < last_any[session_index] {
            stats.all_inversions += 1;
        } else {
            last_any[session_index] = j as i64;
        }
        if durable {
            durable_seen += 1;
            if (j as i64) < last_durable[session_index] {
                stats.durable_inversions += 1;
            } else {
                last_durable[session_index] = j as i64;
            }
            if durable_seen == expected_durable {
                stats.done_at = Some(Instant::now());
                break;
            }
        }
    }
    stats
}

async fn run_on(env: &Env, scenario: Scenario) -> RunResult {
    let events_per_session = scenario.events_per_session();
    let rx = env.fanout.subscribe();
    let start = Instant::now();
    let deadline = tokio::time::Instant::now() + WAIT_LIMIT;
    let receiver = tokio::spawn(receive(
        rx,
        scenario.sessions,
        events_per_session,
        scenario.total_durable(),
        deadline,
    ));

    let mut emitters = Vec::new();
    for (session_index, session_id) in env.session_ids.iter().enumerate() {
        let emitter = env.emitter.clone();
        let session_id = session_id.clone();
        emitters.push(tokio::spawn(async move {
            let mut emitted_at = Vec::with_capacity(events_per_session);
            let mut cost = Duration::ZERO;
            for j in 0..events_per_session {
                let kind = make_event(session_index, j);
                let before = Instant::now();
                emitter.emit(&session_id, kind);
                let after = Instant::now();
                emitted_at.push(before);
                cost += after - before;
                if (j + 1) % PACE == 0 {
                    tokio::task::yield_now().await;
                }
            }
            (emitted_at, cost, Instant::now())
        }));
    }

    let mut emitted_at = Vec::new();
    let mut emit_cost = Duration::ZERO;
    let mut last_emit = start;
    for emitter in emitters {
        let (times, cost, finished) = emitter.await.expect("emitter task");
        emitted_at.push(times);
        emit_cost += cost;
        last_emit = last_emit.max(finished);
    }

    let mut drained_at = None;
    for session_id in &env.session_ids {
        if let Some(tip) = env.emitter.drain(session_id).await {
            black_box(tip);
            drained_at = Some(Instant::now());
        }
    }

    let stats = receiver.await.expect("receiver task");
    let complete = stats.done_at.is_some();
    let done_at = stats.done_at.unwrap_or_else(Instant::now);

    let mut lat_durable = Vec::new();
    let mut lat_ephemeral = Vec::new();
    for (session_index, times) in stats.times.iter().enumerate() {
        for (j, received) in times.iter().enumerate() {
            if let Some(received) = received {
                let latency = received.saturating_duration_since(emitted_at[session_index][j]);
                if is_durable(j) {
                    lat_durable.push(latency);
                } else {
                    lat_ephemeral.push(latency);
                }
            }
        }
    }

    let settled_at = drained_at.unwrap_or(done_at);
    RunResult {
        total: done_at.saturating_duration_since(start),
        emit_cost,
        emit_calls: scenario.sessions * events_per_session,
        settle: settled_at.saturating_duration_since(last_emit),
        lat_durable,
        lat_ephemeral,
        durable_inversions: stats.durable_inversions,
        all_inversions: stats.all_inversions,
        lagged: stats.lagged,
        complete,
    }
}

// ── Criterion: throughput ────────────────────────────────────────────────────

fn bench_throughput(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("benchmark runtime");

    let mut group = c.benchmark_group("event_emit");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(8));

    for storage in Storage::ALL {
        for scenario in SCENARIOS {
            group.throughput(Throughput::Elements(scenario.total_events()));
            for variant in Variant::ALL {
                let id = BenchmarkId::new(
                    format!("{}/{}", storage.name(), scenario.name),
                    variant.name(),
                );
                group.bench_function(id, |b| {
                    b.iter_custom(|iterations| {
                        rt.block_on(async {
                            let env = build_env(variant, storage, scenario.sessions).await;
                            let mut total = Duration::ZERO;
                            for _ in 0..iterations {
                                let result = run_on(&env, scenario).await;
                                if !result.complete {
                                    eprintln!(
                                        "event_emit bench: incomplete run ({} {} {}), lagged={}",
                                        storage.name(),
                                        scenario.name,
                                        variant.name(),
                                        result.lagged
                                    );
                                }
                                total += result.total;
                            }
                            total
                        })
                    })
                });
            }
        }
    }
    group.finish();
}

// ── Custom report ────────────────────────────────────────────────────────────

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index]
}

fn median(values: &mut [Duration]) -> Duration {
    values.sort();
    percentile(values, 0.5)
}

fn fmt(duration: Duration) -> String {
    let micros = duration.as_secs_f64() * 1e6;
    if micros >= 1000.0 {
        format!("{:.2}ms", micros / 1000.0)
    } else {
        format!("{micros:.0}us")
    }
}

fn report(rt: &Runtime) {
    println!();
    println!("event_emit report ({REPORT_RUNS} runs per row; latencies pooled across runs)");
    println!(
        "  workload: {DELTAS_PER_DURABLE} ephemeral deltas before each durable event, \
         yield every {PACE} emits"
    );
    for storage in Storage::ALL {
        for scenario in SCENARIOS {
            println!();
            println!(
                "== storage={} scenario={} ({} sessions x {} durable) ==",
                storage.name(),
                scenario.name,
                scenario.sessions,
                scenario.durable_per_session
            );
            println!(
                "{:<12} {:>9} {:>11} {:>10} {:>10} {:>10} {:>10} {:>9} {:>9} {:>7} {:>7}",
                "variant",
                "total",
                "events/s",
                "dur p50",
                "dur p99",
                "eph p50",
                "eph p99",
                "emit/ev",
                "settle",
                "inv.dur",
                "inv.all"
            );
            for variant in Variant::ALL {
                let mut totals = Vec::new();
                let mut settles = Vec::new();
                let mut lat_durable = Vec::new();
                let mut lat_ephemeral = Vec::new();
                let mut emit_cost = Duration::ZERO;
                let mut emit_calls = 0usize;
                let mut durable_inversions = 0usize;
                let mut all_inversions = 0usize;
                let mut lagged = 0u64;
                let mut incomplete = 0usize;
                rt.block_on(async {
                    let env = build_env(variant, storage, scenario.sessions).await;
                    for _ in 0..REPORT_RUNS {
                        let result = run_on(&env, scenario).await;
                        totals.push(result.total);
                        settles.push(result.settle);
                        lat_durable.extend(result.lat_durable);
                        lat_ephemeral.extend(result.lat_ephemeral);
                        emit_cost += result.emit_cost;
                        emit_calls += result.emit_calls;
                        durable_inversions += result.durable_inversions;
                        all_inversions += result.all_inversions;
                        lagged += result.lagged;
                        incomplete += usize::from(!result.complete);
                    }
                });
                lat_durable.sort();
                lat_ephemeral.sort();
                let total = median(&mut totals);
                let settle = median(&mut settles);
                let events_per_second = scenario.total_events() as f64 / total.as_secs_f64();
                println!(
                    "{:<12} {:>9} {:>11.0} {:>10} {:>10} {:>10} {:>10} {:>9} {:>9} {:>7} {:>7}{}{}",
                    variant.name(),
                    fmt(total),
                    events_per_second,
                    fmt(percentile(&lat_durable, 0.5)),
                    fmt(percentile(&lat_durable, 0.99)),
                    fmt(percentile(&lat_ephemeral, 0.5)),
                    fmt(percentile(&lat_ephemeral, 0.99)),
                    format!("{}ns", (emit_cost.as_nanos() / emit_calls.max(1) as u128)),
                    fmt(settle),
                    durable_inversions / REPORT_RUNS,
                    all_inversions / REPORT_RUNS,
                    if lagged > 0 {
                        format!("  LAGGED={lagged}")
                    } else {
                        String::new()
                    },
                    if incomplete > 0 {
                        format!("  INCOMPLETE={incomplete}")
                    } else {
                        String::new()
                    },
                );
            }
        }
    }
    println!();
    println!(
        "legend: total=first emit -> all durable observed (median); dur/eph = emit->fanout \
         latency of durable/ephemeral events; emit/ev = caller-side cost of one emit call; \
         settle = last emit -> drain ack (ordered) or full delivery (baseline); \
         inv.* = events delivered after a later event of the same session (per run)."
    );
    println!();
}

fn bench_report(_c: &mut Criterion) {
    if std::env::var_os("EVENT_EMIT_SKIP_REPORT").is_some() {
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("report runtime");
    report(&rt);
}

criterion_group!(benches, bench_report, bench_throughput);
criterion_main!(benches);

//! Event emission benchmark: today's per-event spawn vs. ordered writers.
//!
//! Motivation (openspec change `ordered-session-events`): a session's events
//! must reach clients in production order, and the journal must store that
//! same order. This benchmark measures what ordering costs. The candidate
//! writers live in this file only; production code adopts the winner.
//!
//! Variants
//! - `baseline`      : `AgentConfig::emit_event` as it exists in the crate.
//! - `per_session`   : one ordered writer task per session; durable and
//!   ephemeral events share the queue; supports `drain`.
//! - `global`        : a single ordered writer task for all sessions.
//! - `split`         : design decision 2. A per-session sequence is assigned
//!   at emit time; durable events go through an ordered per-session writer
//!   (persist, then publish); ephemeral events publish inline unless an
//!   earlier durable event is still unpublished (per-session reorder buffer).
//! - `split_batched` : `split`, but the writer persists every queued durable
//!   event in one transaction.
//! - `production`    : the shipped implementation, driven directly through
//!   `EventSink::session_stream` (per-session `SessionEventStream`).
//!
//! Since `ordered-session-events`, `baseline` (`AgentConfig::emit_event`) also
//! runs the shipped implementation. The pre-change numbers are recorded in the
//! openspec change's design.md.
//!
//! Storage
//! - `file`     : on-disk SQLite with the crate's connection settings.
//! - `file_wal` : on-disk SQLite with `journal_mode=WAL`, `synchronous=NORMAL`.
//! - `memory`   : `:memory:` SQLite.
//!
//! Workloads
//! - burst scenarios: every durable event is preceded by `BURST_DELTAS`
//!   ephemeral deltas, emitted as fast as possible (yield every `PACE`).
//! - paced scenarios: a model streams `PACED_DELTAS` deltas per turn, one
//!   every `TOKEN_INTERVAL`, then emits `PACED_DURABLE` durable events back to
//!   back (tool calls, stored message) at the end of the turn.
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
use querymt_agent::events::{
    AgentEventKind, Durability, DurableEvent, EphemeralEvent, EventEnvelope, EventOrigin,
    classify_durability,
};
use querymt_agent::session::projection::{EventJournal, NewDurableEvent};
use querymt_agent::session::store::SessionStore;
use querymt_agent::session::{SqliteStorage, StorageBackend};
use std::collections::{BTreeMap, HashMap};
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tokio::sync::{broadcast, mpsc, oneshot};

/// Burst workload: ephemeral streaming deltas emitted before each durable event.
const BURST_DELTAS: usize = 4;
/// Paced workload: deltas streamed per turn.
const PACED_DELTAS: usize = 50;
/// Paced workload: durable events emitted back to back at the end of a turn.
const PACED_DURABLE: usize = 8;
/// Paced workload: interval between two streamed deltas (~500 tokens/s).
const TOKEN_INTERVAL: Duration = Duration::from_millis(2);
/// The burst emitter yields to the scheduler every `PACE` events, like an
/// async stream producer would. Without it a tight loop outruns the fanout.
const PACE: usize = 4;
/// Most durable events the batched writer persists in one transaction.
const BATCH_LIMIT: usize = 64;
/// Upper bound for waiting on a run to be observed completely.
const WAIT_LIMIT: Duration = Duration::from_secs(60);
/// Runs per configuration for the custom report.
const REPORT_RUNS: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    Baseline,
    PerSession,
    Global,
    Split,
    SplitBatched,
    Production,
}

impl Variant {
    const ALL: [Variant; 6] = [
        Variant::Baseline,
        Variant::PerSession,
        Variant::Global,
        Variant::Split,
        Variant::SplitBatched,
        Variant::Production,
    ];

    fn name(self) -> &'static str {
        match self {
            Variant::Baseline => "baseline",
            Variant::PerSession => "per_session",
            Variant::Global => "global",
            Variant::Split => "split",
            Variant::SplitBatched => "split_batched",
            Variant::Production => "production",
        }
    }
}

#[derive(Clone, Copy)]
enum Storage {
    File,
    FileWal,
    Memory,
}

impl Storage {
    const ALL: [Storage; 3] = [Storage::File, Storage::FileWal, Storage::Memory];

    fn name(self) -> &'static str {
        match self {
            Storage::File => "file",
            Storage::FileWal => "file_wal",
            Storage::Memory => "memory",
        }
    }
}

/// A session emits `groups` groups of `deltas` ephemeral events followed by
/// `durable` durable events.
#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    sessions: usize,
    groups: usize,
    deltas: usize,
    durable: usize,
    paced: bool,
}

const SCENARIOS: [Scenario; 5] = [
    Scenario {
        name: "single_100",
        sessions: 1,
        groups: 100,
        deltas: BURST_DELTAS,
        durable: 1,
        paced: false,
    },
    Scenario {
        name: "single_1000",
        sessions: 1,
        groups: 1000,
        deltas: BURST_DELTAS,
        durable: 1,
        paced: false,
    },
    Scenario {
        name: "multi_8x125",
        sessions: 8,
        groups: 125,
        deltas: BURST_DELTAS,
        durable: 1,
        paced: false,
    },
    Scenario {
        name: "paced_1x4",
        sessions: 1,
        groups: 4,
        deltas: PACED_DELTAS,
        durable: PACED_DURABLE,
        paced: true,
    },
    Scenario {
        name: "paced_4x4",
        sessions: 4,
        groups: 4,
        deltas: PACED_DELTAS,
        durable: PACED_DURABLE,
        paced: true,
    },
];

impl Scenario {
    fn group_len(self) -> usize {
        self.deltas + self.durable
    }

    fn events_per_session(self) -> usize {
        self.groups * self.group_len()
    }

    fn total_events(self) -> u64 {
        (self.sessions * self.events_per_session()) as u64
    }

    fn total_durable(self) -> usize {
        self.sessions * self.groups * self.durable
    }

    /// Event `j` of a session is durable when it is in the group's tail.
    fn is_durable(self, j: usize) -> bool {
        j % self.group_len() >= self.deltas
    }

    fn make_event(self, session_index: usize, j: usize) -> AgentEventKind {
        let message_id = format!("{session_index}:{j}");
        if self.is_durable(j) {
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

// ── Candidate: single ordered queue (durable + ephemeral) ────────────────────

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

// ── Candidate: split (durable ordered writer, ephemeral inline) ──────────────

/// Per-session publish state shared by emitters and the durable writer.
struct SplitState {
    next_seq: u64,
    next_publish: u64,
    /// Events whose turn to publish has not come yet, keyed by sequence.
    /// `None` marks a durable event whose write failed (skipped, not stuck).
    ready: BTreeMap<u64, Option<EventEnvelope>>,
}

impl SplitState {
    fn complete(&mut self, seq: u64, envelope: Option<EventEnvelope>, fanout: &EventFanout) {
        if seq == self.next_publish && self.ready.is_empty() {
            if let Some(envelope) = envelope {
                fanout.publish(envelope);
            }
            self.next_publish += 1;
            return;
        }
        self.ready.insert(seq, envelope);
        while let Some(entry) = self.ready.remove(&self.next_publish) {
            if let Some(envelope) = entry {
                fanout.publish(envelope);
            }
            self.next_publish += 1;
        }
    }
}

#[allow(clippy::large_enum_variant)]
enum WriteCmd {
    Durable(u64, AgentEventKind),
    Drain(oneshot::Sender<u64>),
}

struct SplitSession {
    session_id: String,
    fanout: Arc<EventFanout>,
    state: Arc<Mutex<SplitState>>,
    writer: mpsc::UnboundedSender<WriteCmd>,
}

impl SplitSession {
    fn emit(&self, kind: AgentEventKind) {
        let mut state = self.state.lock().unwrap();
        let seq = state.next_seq;
        state.next_seq += 1;
        match classify_durability(&kind) {
            // Sent under the lock so the writer queue is in sequence order.
            Durability::Durable => {
                let _ = self.writer.send(WriteCmd::Durable(seq, kind));
            }
            Durability::Ephemeral => {
                let envelope = EventEnvelope::Ephemeral(EphemeralEvent {
                    session_seq: 0,
                    session_id: self.session_id.clone(),
                    timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
                    origin: EventOrigin::Local,
                    source_node: None,
                    kind,
                });
                state.complete(seq, Some(envelope), &self.fanout);
            }
        }
    }
}

struct SplitCtx {
    store: Arc<SqliteStorage>,
    journal: Arc<dyn EventJournal>,
    fanout: Arc<EventFanout>,
    batched: bool,
}

/// Persist one batch of durable events in a single transaction. Mirrors the
/// crate's journal insert (global `stream_seq` allocation) for comparison.
fn insert_batch(
    conn: &mut rusqlite::Connection,
    session_id: &str,
    kinds: &[AgentEventKind],
) -> rusqlite::Result<Vec<DurableEvent>> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut persisted = Vec::with_capacity(kinds.len());
    {
        let mut next_seq = tx.prepare_cached(
            "UPDATE event_journal_seq SET next_seq = next_seq + 1 WHERE id = 1 \
             RETURNING next_seq - 1",
        )?;
        let mut insert = tx.prepare_cached(
            "INSERT INTO event_journal \
             (event_id, stream_seq, session_id, timestamp, origin, kind, payload_json) \
             VALUES (?, ?, ?, ?, 'local', ?, ?)",
        )?;
        for kind in kinds {
            let value = serde_json::to_value(kind)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let tag = value
                .get("type")
                .and_then(|tag| tag.as_str())
                .unwrap_or("unknown")
                .to_string();
            let payload = value.to_string();
            let stream_seq: i64 = next_seq.query_row([], |row| row.get(0))?;
            let event_id = uuid::Uuid::now_v7().to_string();
            let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
            insert.execute(rusqlite::params![
                event_id, stream_seq, session_id, timestamp, tag, payload
            ])?;
            persisted.push(DurableEvent {
                session_seq: 0,
                event_id,
                stream_seq,
                session_id: session_id.to_string(),
                timestamp,
                origin: EventOrigin::Local,
                source_node: None,
                kind: kind.clone(),
            });
        }
    }
    tx.commit()?;
    Ok(persisted)
}

async fn split_writer(
    ctx: Arc<SplitCtx>,
    session_id: String,
    state: Arc<Mutex<SplitState>>,
    mut rx: mpsc::UnboundedReceiver<WriteCmd>,
) {
    let limit = if ctx.batched { BATCH_LIMIT } else { 1 };
    let mut seqs = Vec::with_capacity(limit);
    let mut kinds = Vec::with_capacity(limit);
    let mut drains = Vec::new();
    while let Some(cmd) = rx.recv().await {
        let mut next = Some(cmd);
        while let Some(cmd) = next.take() {
            match cmd {
                WriteCmd::Durable(seq, kind) => {
                    seqs.push(seq);
                    kinds.push(kind);
                }
                WriteCmd::Drain(reply) => drains.push(reply),
            }
            if seqs.len() < limit {
                next = rx.try_recv().ok();
            }
        }

        let results: Vec<Option<DurableEvent>> = if ctx.batched && !kinds.is_empty() {
            let conn = ctx.store.conn();
            let sid = session_id.clone();
            let batch = std::mem::take(&mut kinds);
            let count = batch.len();
            match tokio::task::spawn_blocking(move || {
                insert_batch(&mut conn.lock().unwrap(), &sid, &batch)
            })
            .await
            {
                Ok(Ok(events)) => events.into_iter().map(Some).collect(),
                Ok(Err(err)) => {
                    eprintln!("event_emit bench: batch write failed: {err}");
                    vec![None; count]
                }
                Err(err) => {
                    eprintln!("event_emit bench: batch task failed: {err}");
                    vec![None; count]
                }
            }
        } else {
            let mut results = Vec::with_capacity(kinds.len());
            for kind in kinds.drain(..) {
                let event = NewDurableEvent {
                    session_seq: None,
                    session_id: session_id.clone(),
                    origin: EventOrigin::Local,
                    source_node: None,
                    source_node_id: None,
                    source_seq: None,
                    kind,
                };
                match ctx.journal.append_durable(&event).await {
                    Ok(persisted) => results.push(Some(persisted)),
                    Err(err) => {
                        eprintln!("event_emit bench: durable write failed: {err}");
                        results.push(None);
                    }
                }
            }
            results
        };

        let mut state = state.lock().unwrap();
        for (seq, persisted) in seqs.drain(..).zip(results) {
            state.complete(seq, persisted.map(EventEnvelope::Durable), &ctx.fanout);
        }
        for reply in drains.drain(..) {
            let _ = reply.send(state.next_publish);
        }
    }
}

struct SplitWriters {
    ctx: Arc<SplitCtx>,
    sessions: Mutex<HashMap<String, Arc<SplitSession>>>,
}

impl SplitWriters {
    fn session(&self, session_id: &str) -> Arc<SplitSession> {
        let mut sessions = self.sessions.lock().unwrap();
        sessions
            .entry(session_id.to_string())
            .or_insert_with(|| {
                let state = Arc::new(Mutex::new(SplitState {
                    next_seq: 1,
                    next_publish: 1,
                    ready: BTreeMap::new(),
                }));
                let (tx, rx) = mpsc::unbounded_channel();
                tokio::spawn(split_writer(
                    self.ctx.clone(),
                    session_id.to_string(),
                    state.clone(),
                    rx,
                ));
                Arc::new(SplitSession {
                    session_id: session_id.to_string(),
                    fanout: self.ctx.fanout.clone(),
                    state,
                    writer: tx,
                })
            })
            .clone()
    }

    async fn drain(&self, session_id: &str) -> i64 {
        let (tx, rx) = oneshot::channel();
        let _ = self.session(session_id).writer.send(WriteCmd::Drain(tx));
        rx.await
            .map_or(0, |tip| i64::try_from(tip).unwrap_or(i64::MAX))
    }
}

enum Emitter {
    Baseline(Arc<AgentConfig>),
    Production(Arc<EventSink>),
    Ordered(Writers),
    Split(SplitWriters),
}

impl Emitter {
    fn emit(&self, session_id: &str, kind: AgentEventKind) {
        match self {
            Emitter::Baseline(config) => config.emit_event(session_id, kind),
            Emitter::Production(sink) => sink.session_stream(session_id).emit(kind),
            Emitter::Ordered(writers) => writers.emit(session_id, kind),
            Emitter::Split(writers) => writers.session(session_id).emit(kind),
        }
    }

    /// `None` for the baseline, which has no drain operation.
    async fn drain(&self, session_id: &str) -> Option<i64> {
        match self {
            Emitter::Baseline(_) | Emitter::Production(_) => None,
            Emitter::Ordered(writers) => Some(writers.drain(session_id).await),
            Emitter::Split(writers) => Some(writers.drain(session_id).await),
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

fn enable_wal(store: &SqliteStorage) {
    let conn = store.conn();
    let conn = conn.lock().unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .expect("enable WAL");
    assert_eq!(mode.to_ascii_lowercase(), "wal", "journal_mode");
    conn.execute_batch("PRAGMA synchronous=NORMAL;")
        .expect("synchronous=NORMAL");
}

async fn build_env(variant: Variant, storage: Storage, sessions: usize) -> Env {
    let tmp = tempfile::tempdir().expect("temp dir");
    let db_path = match storage {
        Storage::File | Storage::FileWal => tmp.path().join("event_emit.sqlite3"),
        Storage::Memory => PathBuf::from(":memory:"),
    };
    let store = Arc::new(SqliteStorage::connect(db_path).await.expect("sqlite store"));
    if matches!(storage, Storage::FileWal) {
        enable_wal(&store);
    }

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

    let backend: Arc<dyn StorageBackend> = store.clone();
    let (emitter, fanout) = match variant {
        Variant::Baseline => {
            let config = make_agent_config(&tmp, store.clone());
            let fanout = config.event_sink.fanout().clone();
            (Emitter::Baseline(config), fanout)
        }
        Variant::Production => {
            let fanout = Arc::new(EventFanout::new());
            let sink = Arc::new(EventSink::new(backend.event_journal(), fanout.clone()));
            (Emitter::Production(sink), fanout)
        }
        Variant::PerSession | Variant::Global => {
            let fanout = Arc::new(EventFanout::new());
            let sink = Arc::new(EventSink::new(backend.event_journal(), fanout.clone()));
            (
                Emitter::Ordered(Writers::new(sink, variant == Variant::Global)),
                fanout,
            )
        }
        Variant::Split | Variant::SplitBatched => {
            let fanout = Arc::new(EventFanout::new());
            let ctx = Arc::new(SplitCtx {
                store: store.clone(),
                journal: backend.event_journal(),
                fanout: fanout.clone(),
                batched: variant == Variant::SplitBatched,
            });
            (
                Emitter::Split(SplitWriters {
                    ctx,
                    sessions: Mutex::new(HashMap::new()),
                }),
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

async fn emit_session(
    emitter: Arc<Emitter>,
    scenario: Scenario,
    session_index: usize,
    session_id: String,
) -> (Vec<Instant>, Duration, Instant) {
    let events_per_session = scenario.events_per_session();
    let mut emitted_at = Vec::with_capacity(events_per_session);
    let mut cost = Duration::ZERO;
    for j in 0..events_per_session {
        let durable = scenario.is_durable(j);
        if scenario.paced && !durable {
            tokio::time::sleep(TOKEN_INTERVAL).await;
        }
        let kind = scenario.make_event(session_index, j);
        let before = Instant::now();
        emitter.emit(&session_id, kind);
        let after = Instant::now();
        emitted_at.push(before);
        cost += after - before;
        if !scenario.paced && (j + 1) % PACE == 0 {
            tokio::task::yield_now().await;
        }
    }
    (emitted_at, cost, Instant::now())
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

    let emitters: Vec<_> = env
        .session_ids
        .iter()
        .enumerate()
        .map(|(session_index, session_id)| {
            tokio::spawn(emit_session(
                env.emitter.clone(),
                scenario,
                session_index,
                session_id.clone(),
            ))
        })
        .collect();

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
                if scenario.is_durable(j) {
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
        "  burst: {BURST_DELTAS} deltas before each durable event, yield every {PACE} emits; \
         paced: {PACED_DELTAS} deltas every {TOKEN_INTERVAL:?}, then {PACED_DURABLE} durable \
         events back to back"
    );
    for storage in Storage::ALL {
        for scenario in SCENARIOS {
            println!();
            println!(
                "== storage={} scenario={} ({} sessions x {} groups of {}+{}) ==",
                storage.name(),
                scenario.name,
                scenario.sessions,
                scenario.groups,
                scenario.deltas,
                scenario.durable
            );
            println!(
                "{:<14} {:>9} {:>11} {:>10} {:>10} {:>10} {:>10} {:>9} {:>9} {:>7} {:>7}",
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
                    "{:<14} {:>9} {:>11.0} {:>10} {:>10} {:>10} {:>10} {:>9} {:>9} {:>7} {:>7}{}{}",
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

# Design

## Context

Current event path (see proposal.md for motivation):

| Stage | Today | Code |
|---|---|---|
| Emit (durable) | `AgentConfig::emit_event` spawns one task per event | `agent/agent_config.rs:201` |
| Emit (ephemeral) | published inline to the fanout | `event_sink.rs:112` |
| Persist | each task takes the single `Mutex<Connection>`, one transaction per event | `session/sqlite_storage/mod.rs:60`, `event_journal.rs:13` |
| Sequence | `stream_seq` allocated from a **global** counter (`event_journal_seq`, id = 1) at insert time | `event_journal.rs:318` |
| Fanout | `tokio::broadcast`, 1024 slots | `event_fanout.rs:9` |
| Remote relay | `EventForwarder` `tell`s each event; `EventRelayActor` awaits each write (already ordered) | `agent/remote/event_forwarder.rs`, `event_relay.rs:166` |
| Consumers | WebSocket forwarder exits on lag (`Err(_) => break`); stdio and remote forwarder log and continue | `acp/websocket.rs` `spawn_event_forwarders`, `acp/stdio.rs:728`, `event_forwarder.rs:121` |
| Prompt response | sent by the dispatcher when the actor replies; no link to events | `acp/shared.rs` `dispatch_rpc_message_with_context` |

Constraints: sessions hold no client bridge (`unify-session-interactions`); all mesh nodes upgrade together; existing journals must keep loading.

Benchmark baseline (`benches/event_emit.rs`, file storage): about 1.4k durable writes/s; baseline reorders 40–630 durable events per burst; a single ordered queue that also carries ephemeral deltas raised delta p50 latency from ~2µs to 36–376ms.

## Goals / Non-Goals

**Goals:**
- One ordering authority per session, assigned at emit time.
- Prompt-response ordering for local and remote sessions with no extra messages.
- Lossless live delivery: lag and gaps are recovered, never silently skipped.

**Non-Goals:**
- Ordering across different sessions (including parent vs. delegated child).
- Applying the watermark to requests other than `session/prompt` (later change).
- Changing which ACP notifications exist (`acp-session-updates`).

## Decisions

### 1. Sequence assigned at emit time, per session
Each session keeps an atomic counter (`session_seq`), shared by everything that emits for the session (actor, execution task, delegation, scheduler) through the session's emit handle. Emitting assigns the next number synchronously, before any spawn or write. Durable and ephemeral events share the counter, so streaming deltas are ordered relative to durable events without waiting on storage.

*Alternatives:* journal-assigned order (today; reflects lock order), one ordered writer queue for everything (benchmarked: correct but delays deltas by tens to hundreds of ms).

### 2. Ordered durable writer per session, ephemeral inline
Durable events go through one ordered writer per session (channel + task); the writer persists, then publishes. Ephemeral events publish inline with their sequence number. To keep fanout order equal to sequence order, the fanout publish of an ephemeral event that follows unpublished durable events is held by the session's publisher until those are published (a small per-session reorder buffer keyed by `session_seq`). An ephemeral event therefore waits only for durable events with a lower `session_seq`, never for later ones; the wait is bounded by the persistence latency of those earlier events. Durable events are published only after they are persisted, so a consumer that recovers from the journal (decision 6) can always find every durable event it was told about. The writer may batch consecutive durable events into one transaction.

*Alternatives:* global writer (benchmarked competitive single-session, worse at 8 sessions); per-event spawn (today).

### 3. Journal stores the producer's sequence
Add `session_seq INTEGER` to `event_journal` with a unique index on `(session_id, session_seq)`. Producer-assigned for local events; for relayed events it is the host's `session_seq`. The global `stream_seq` stays as the insert-order cursor for global-stream consumers. Replay orders by `session_seq` (falling back to `stream_seq` for legacy rows, which get `session_seq` backfilled from `stream_seq` order in the migration). This replaces the "producers never set `stream_seq`" rule with "producers set `session_seq`; the journal still assigns `stream_seq`".

*Alternative:* reuse `source_seq` for local events — rejected, it means "remote origin cursor" and drives dedup.

### 4. Relay keeps the host sequence
`DurableEvent` and `EphemeralEvent` carry `session_seq`. The relay persists with the host's `session_seq` (already passed as `source_seq` for dedup; both are set). The attached node never renumbers a remote session's events, so a watermark from the host is directly comparable on the attached node.

### 5. Prompt watermark
The session actor replies to a prompt with `event_tip` = the last `session_seq` the run produced (taken from the counter after the run ends, no drain needed). It travels in the prompt reply between nodes and in `PromptResponse._meta` internally. Each connection's forwarder records, per session, the highest `session_seq` it has handled (advanced for every event it consumes, including filtered ones). The dispatcher waits until the requesting connection's position reaches `event_tip`, then sends the response. Waiters are released on relay disconnect, consumer lag recovery failure, connection close, or a safety limit (30s), with a warning (spec: Bounded response wait).

*Alternatives:* run-end barrier keyed by `run_id` (prompt-specific, needs `RunCompleted` moved before the reply); forwarder produces the response (request-to-run mapping and buffering); client-side reordering (pushes the bug to every client).

### 6. Lag and gap recovery in consumers
On `RecvError::Lagged` or a `session_seq` gap, a consumer loads the missing durable range from the journal (`load_session_stream` by `session_seq`), delivers it, then resumes live delivery, discarding live events at or below the recovered position. Missed ephemeral events are not recoverable: the journal does not store them, and no replacement buffer is kept. The recovered durable event supersedes them (for example `AssistantMessageStored` carries the full content of the missed deltas). Consequently the "deltas precede the stored message" and production-order guarantees for ephemeral events apply to connections that did not lag; a connection that lagged receives the recovered durable events in order but may miss the deltas in the lagged window. The WebSocket forwarder's `Err(_) => break` on lag is replaced by this recovery — this is the explicit fix deferred from `unify-session-interactions`. On the remote path, the attached node requests the missing range from the host (existing reconnect backfill path, `after_source_seq`).

### 7. SQLite settings decided by measurement
Extend `benches/event_emit.rs` with: split variant (decision 2), WAL + `synchronous=NORMAL`, batched writer, and a paced emitter (realistic token rate with bursts at turn end). Adopt WAL/batching only if they meet the benchmark thresholds; otherwise leave connection settings unchanged.

## Risks / Trade-offs

- [Reorder buffer holds ephemeral deltas behind a slow durable write] → this is accepted behaviour, scoped by the spec: a delta waits only for earlier durable events, never later ones. Durable writes are rare during streaming; measure the added delay with the paced benchmark (task 1.2) and record it. Durable events are not published before they are persisted, because lag recovery reads them from the journal.
- [Mesh `tell` ordering assumed for the relay] → verify kameo per-connection ordering with a test; gap recovery (decision 6) covers violations.
- [Migration on large journals] → backfill `session_seq` in one transaction per session batch; measure with `benches/session_load.rs` scale data.
- [Watermark wait on a dead forwarder] → release on disconnect/lag/close plus the 30s safety limit; log every limit release.
- [Counter shared across emitters needs a session-scoped handle] → emitters that today call `config.emit_event(session_id, ..)` move to the session emit handle; keep `emit_event` as a thin lookup for call sites outside a session.

## Migration Plan

1. Journal migration adds `session_seq` and backfills it; old binaries cannot read journals that rely on `session_seq` ordering, so downgrade requires restoring a backup (documented).
2. All mesh nodes upgrade together (prompt reply carries `event_tip`; relayed events carry `session_seq`). An old node answering without `event_tip` is treated as "no watermark" (response sent immediately) — the only compatibility allowance.
3. Rollback: revert the binary and restore the pre-migration database.

## Open Questions

- Exact safety-limit value (30s proposed) — tunable without changing specs or tasks.
- Whether `stream_seq` can eventually be dropped for session consumers once all readers use `session_seq`.

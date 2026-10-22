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

Probe evidence (task 1.1, `agent_config::tests::back_to_back_tool_events_keep_production_order_in_journal`): emitting 50 `ToolCallStart`/`ToolCallEnd` pairs back to back through `AgentConfig::emit_event` on a 4-worker runtime fails today — the journal stored e.g. `end:tc-22` before `start:tc-22` and `start:tc-3` first. Replay order does not match production order.

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

### Benchmark record (task 1.2)

`cargo bench -p querymt-agent --bench event_emit`, Apple M4 Pro, rustc 1.99.1, 4 tokio workers, 2026-10-21. Raw output: Criterion run of all 75 rows (3 storages × 5 scenarios × 5 variants) plus the custom report. Burst scenarios: 4 deltas before each durable event. Paced scenarios: 50 deltas at 2 ms intervals, then 8 durable events back to back, 4 turns per session.

Criterion `time` (median), selected rows (all rows ran):

| storage / scenario | baseline | per_session | global | split | split_batched |
|---|---|---|---|---|---|
| file / single_100 | 57.5 ms | 101.3 ms | 74.2 ms | 65.8 ms | 2.28 ms |
| file / single_1000 | 1.363 s | 1.595 s | 1.654 s | 1.583 s | 40.5 ms |
| file / multi_8x125 | 1.833 s | 1.303 s | 1.830 s | 1.496 s | 48.5 ms |
| file / paced_4x4 | 699.3 ms | 698.1 ms | 699.6 ms | 712.2 ms | 681.5 ms |
| file_wal / single_1000 | 58.9 ms | 74.9 ms | 73.8 ms | 74.8 ms | 10.2 ms |
| file_wal / multi_8x125 | 62.8 ms | 105.7 ms | 123.7 ms | 117.5 ms | 9.8 ms |
| file_wal / paced_4x4 | 683.7 ms | 689.7 ms | 681.8 ms | 682.5 ms | 676.6 ms |
| memory / single_1000 | 25.4 ms | 40.9 ms | 41.8 ms | 41.7 ms | 6.8 ms |
| memory / multi_8x125 | 22.0 ms | 25.4 ms | 41.8 ms | 25.4 ms | 6.2 ms |

(Paced totals are dominated by the 2 ms token pacing; latency below is the meaningful metric.)

Custom report, latency and ordering (p50/p99 emit→fanout, inversions per run):

| storage / scenario / variant | dur p50 | dur p99 | eph p50 | eph p99 | emit/ev | inv.dur | inv.all |
|---|---|---|---|---|---|---|---|
| file / single_1000 / baseline | 338.6 ms | 717.3 ms | 1 µs | 12 µs | 336 ns | 523 | 999 |
| file / single_1000 / split_batched | 6.3 ms | 14.1 ms | 6.3 ms | 14.1 ms | 228 ns | 0 | 0 |
| file / paced_4x4 / baseline | 29.0 ms | 51.3 ms | 4 µs | 282 µs | 5.4 µs | 97 | 118 |
| file / paced_4x4 / split | 24.0 ms | 71.2 ms | 3 µs | 57.7 ms | 2.0 µs | 0 | 0 |
| file / paced_4x4 / split_batched | 6.0 ms | 62.2 ms | 5 µs | 23.6 ms | 2.7 µs | 0 | 0 |
| file_wal / paced_1x4 / baseline | 1.9 ms | 5.4 ms | 8 µs | 187 µs | 3.3 µs | 24 | 24 |
| file_wal / paced_1x4 / split_batched | 982 µs | 1.4 ms | 13 µs | 61 µs | 5.2 µs | 0 | 0 |
| file_wal / paced_4x4 / baseline | 4.9 ms | 13.8 ms | 5 µs | 171 µs | 6.3 µs | 95 | 107 |
| file_wal / paced_4x4 / split | 4.2 ms | 11.0 ms | 6 µs | 4.3 ms | 3.7 µs | 0 | 0 |
| file_wal / paced_4x4 / split_batched | 1.3 ms | 3.2 ms | 6 µs | 118 µs | 4.5 µs | 0 | 0 |
| memory / paced_4x4 / split_batched | 797 µs | 3.9 ms | 6 µs | 129 µs | 4.5 µs | 0 | 0 |

The baseline reorders events in every row (inv.dur > 0). All ordered variants have 0 inversions.

**Thresholds** (used by tasks 2.3, 3.3 and 6.1; evaluated on the production storage setting):
- T1 ordering: 0 durable and 0 total inversions in every row.
- T2 streaming latency (paced, realistic load): ephemeral p99 ≤ 1 ms.
- T3 durable latency (paced): durable p99 ≤ the baseline's on the same storage.
- T4 burst throughput: Criterion time ≤ the baseline's on the same storage.
- T5 caller-side cost: emit/ev ≤ 10 µs.

**Result:** only `split_batched` meets T1–T5, and only with WAL. On the current `file` setting, paced_4x4 ephemeral p99 is 23.6 ms (fails T2), because deltas wait behind earlier durable writes that each pay a full fsync. With WAL + `synchronous=NORMAL`, it is 118 µs. Unbatched `split` fails T4 on every storage. Decision for 2.3/3.3: per-session ordered writer **with batching** (≤ 64 events per transaction), and the journal connection switches to WAL + `synchronous=NORMAL` for file databases (no effect on `:memory:`). `synchronous=NORMAL` in WAL mode can lose the last committed transactions on power loss but cannot corrupt the database. Lag recovery reads only what was published, and durable events are published only after commit, so a lost tail never contradicts what clients saw before the crash beyond that window.

### Implementation record (tasks 2.1–2.3)

Shipped as `SessionEventStream` (`session_event_stream.rs`), one per session, owned by `EventSink::session_stream`. `AgentConfig::emit_event`, `emit_event_persisted` and the tool context emit through it. The writer batches up to 64 durable events per transaction. Events with no local production order (remote-stream annotations, local annotations on remote-hosted sessions) carry `session_seq = 0` and are stored with a NULL `session_seq`.

Re-run of `cargo bench -p querymt-agent --bench event_emit` with the shipped code (`production` rows; `baseline` now runs the same code, so the pre-change reference is the table above). Against the thresholds:

| Threshold | file | file_wal |
|---|---|---|
| T1 ordering (0 inversions) | pass, all rows | pass, all rows |
| T2 paced ephemeral p99 ≤ 1 ms | paced_1x4 64 µs pass; **paced_4x4 2.99 ms fail** | 58 µs / 88 µs pass |
| T3 paced durable p99 ≤ pre-change baseline | 3.9 ms vs 23.3 ms; 12.2 ms vs 51.3 ms pass | 1.7 ms vs 5.4 ms; 7.6 ms vs 13.8 ms pass |
| T4 burst time ≤ pre-change baseline | single_1000 54 ms vs 1.36 s; multi_8x125 41 ms vs 1.83 s pass | 15.9 ms vs 58.9 ms; 15.6 ms vs 62.8 ms pass |
| T5 emit cost ≤ 10 µs | max 5.1 µs pass | max 4.8 µs pass |

Open point for task 3.3 (resolved, see the decision record below): T2 held with WAL + `synchronous=NORMAL` but not on the pre-WAL `file` setting with four sessions streaming at once (their durable batches contend for the single connection's fsync). The table above is the pre-WAL measurement; WAL is now the shipped `file` setting.

### Decision record (tasks 2.3, 3.1–3.3)

**3.3 — WAL adopted.** `SqliteStorage::connect_with_options` now switches file databases to `journal_mode=WAL` + `synchronous=NORMAL` (`configure_journal_durability`; `:memory:` untouched; a failure to enable WAL is logged and the defaults are kept). Re-run of `event_emit` with the shipped code, `file` storage (now WAL): paced_1x4 / paced_4x4 ephemeral p99 68 µs / 65 µs (T2 pass, was 2.99 ms for paced_4x4 before WAL), durable p99 5.2 / 4.1 ms (T3 pass), single_1000 15.7 ms vs 1.36 s baseline (T4 pass), emit cost max 5.4 µs (T5 pass), 0 inversions in every row (T1 pass). `file_wal` rows are unchanged within noise.

**3.1 — replay read path.** The first implementation ordered replay with `ORDER BY session_seq, stream_seq`. SQLite cannot serve that from `idx_event_journal_session_seq (session_id, stream_seq)` and added a temp B-tree sort, which made `session_load` ~1.7x slower (acp/core_cold at scale 2x: 19.6 ms → 33.6 ms; `audit_view` 12 ms → 27 ms). Adding a covering index did not change the plan. `load_session_stream` now reads in `stream_seq` index order and restores production order in Rust (`restore_production_order`) only when the sequenced rows are out of order; unsequenced rows (`session_seq` NULL/0) keep their insert slot. Result at scale 2x versus the pre-change code: audit_view 10.4 ms vs 12.0 ms, acp/core_cold 16.2 ms vs 18.3 ms, acp/replay_cold 31.5 ms vs 36.4 ms (no regression).

**3.1/3.2 — migration `0024_event_journal_session_seq`.** Adds the column and the partial unique index `(session_id, session_seq) WHERE session_seq IS NOT NULL`; backfills locally produced legacy rows 1..n per session from `stream_seq` order. Relayed legacy rows stay NULL (host numbering unknown; numbering them locally could collide with host values carried by later relayed events). Unsequenced rows (annotations) are stored as NULL, never 0, so they never collide in the unique index.

### Review record (post-implementation fixes)

A review of the ticked tasks against the code found the following gaps; all are fixed and tested.

- **Served pages must keep `stream_seq` order.** Pages served to other nodes (`GetEventStreamSince`, used by backfill and gap recovery) went through `restore_production_order`. If it ever reordered a page, the receiver would reject it, because it requires strictly increasing `seq`. New `EventJournal::load_session_page` reads raw `stream_seq` order and `GetEventStreamSince` uses it; local replay keeps the restore. Tests: `journal_page_is_raw_stream_seq_order_while_replay_restores_production_order`, `journal_replay_keeps_unsequenced_rows_in_their_slot`.
- **Host numbers vs local numbers (guard).** Relayed rows store the host's `session_seq` in the same column as local numbers. A session has one producer (forks run on the host and renumber their copies as local rows), so a clash cannot happen today, but nothing prevented it. Decision: limit the unique index to local rows (`AND source_node_id IS NULL`) and compute the local counter's start from local rows only. Migration 0024 was uncommitted and was edited in place; a dev database that already applied the earlier 0024 keeps the old index and must be reset or have the index recreated. Test: `journal_relayed_session_seq_does_not_collide_with_local_numbering`. Alternative not taken: a separate `source_session_seq` column (cleaner, more code).
- **Stream release.** `EventSink` kept every session's stream and writer task forever. `SessionActor` now implements `Actor::on_stop` and calls `EventSink::retire_session_stream`, which flushes the writer (new `WriterMsg::Flush` barrier) and removes the stream only if it is idle and unshared. A later emit creates a fresh stream that continues after the journal's highest local number, so numbers are never reused. Tests: `retiring_a_stream_drains_it_and_a_new_one_never_reuses_numbers`, `retire_keeps_a_stream_that_is_still_in_use`.
- **Loss at the end of a burst (signal).** The relay detected gaps only when a later event arrived, so a lost tail stayed missing (and a prompt watermark waited up to the 30 s limit). The host `EventForwarder` now sends `RelayLagged` (mesh id `querymt::RelayLagged`) when it lags, and the relay fetches the durable tail from the host. Test: `relay_lag_signal_fetches_missed_tail_without_a_later_event`. Not covered by a test: a real lagging forwarder over the mesh (the test sends the signal directly).
- **Typeshare.** Regenerated with `./scripts/generate-types.sh`; `session_seq` is present for `DurableEvent`, `EphemeralEvent` and `AgentEvent`.
- **Docs.** `mesh.md` no longer says the prompt response may precede its updates; `event-ordering.md` documents the local-only unique index, raw-order served pages and the lag signal.

### Final verification record (task 6.1)

- `cargo test -p querymt-agent --all-features`: 2716 lib tests plus the other targets pass; `cargo clippy --all-targets` and `cargo fmt --check` are clean. `remote_prompt_updates_are_sent_before_prompt_response` and `local_prompt_updates_are_sent_before_prompt_response` passed 50 of 50 consecutive runs.
- `event_emit` (shipped code, `file` = WAL): T1 0 inversions in every row. T2 paced ephemeral p99 49 µs (1x4) / 93 µs (4x4), under 1 ms. T3 paced durable p99 1.43 ms / 5.85 ms, against pre-change baselines of 23.3 ms / 51.3 ms. T4 single_1000 15.3 ms and multi_8x125 16.0 ms, against 1.36 s / 1.83 s. T5 emit cost at most 3.9 µs, under 10 µs. `file_wal` and `memory` rows agree within noise. The `baseline` rows in this run execute the same code as `production`, so the pre-change reference is the table recorded above.
- `session_load` was compared against a clean `HEAD` worktree, run back to back on the same machine. The first comparison showed `acp/core_cold` at scale 2x about 14% slower (21.7 ms vs 19.0 ms), while `audit_view` was equal. Cause: `max_session_seq_blocking` (the startup counter lookup) filtered on `source_node_id IS NULL`, which made SQLite use the `(session_id, stream_seq)` index and scan every row of the session. Adding `AND session_seq IS NOT NULL` lets it answer `MAX` from the partial unique index. After the fix, scale 2x versus `HEAD`: audit_view 12.0 vs 11.8 ms, acp/core_cold 17.2 vs 18.8 ms, acp/replay_cold 33.2 vs 34.2 ms, so there is no regression.
- Merge note: migration `0024_event_journal_session_seq` clashes in number with `0024_delegation_isolation` and later migrations already applied from other branches (up to `0028`). Renumber when merging.

Follow-up (separate change): one ordered writer per database that batches across sessions (option b from the review), to be measured against WAL with a "global batched" `event_emit` variant first.

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

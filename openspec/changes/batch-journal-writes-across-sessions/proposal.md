# Proposal

## Why

`ordered-session-events` ships one ordered writer per session. Each writer batches up to 64 durable events per transaction, but every session still commits its own batches, so with many busy sessions those commits compete for the single journal connection. The shipped journal uses WAL + `synchronous=NORMAL`. With four sessions streaming at once, that setting already meets thresholds T1-T5 (see the benchmark and decision records in the archived `ordered-session-events` design).

A cross-session writer (one ordered writer per database, batching across sessions) is therefore an optimisation, not a correctness fix. Whether it is worth its complexity is unknown until it is measured at higher session counts. This was task 7.7 of `ordered-session-events` and is split out here so that change can be archived.

## What Changes

Measurement comes first; implementation is conditional on the result.

- Add a `global_batched` variant to `crates/agent/benches/event_emit.rs`: one writer per database, batching events across sessions, while keeping per-session `session_seq` order.
- Run it against the per-session batched writer on WAL + `synchronous=NORMAL` at higher session counts than the original benchmark (for example 16 and 64 sessions), on `file` (WAL) storage.
- Evaluate it against the same thresholds the earlier change used:
  - T1: 0 durable and 0 total inversions.
  - T2: paced ephemeral p99 <= 1 ms.
  - T3: paced durable p99 <= the per-session writer's.
  - T4: burst time <= the per-session writer's.
  - T5: emit cost <= 10 us.
- Build the shared writer only if it beats per-session batching on WAL by a margin that the design records. Otherwise record the result and close the change without code changes.

## Constraints (carried over from `ordered-session-events`)

- Per-session `session_seq` order is preserved, and fanout order equals `session_seq` order.
- Durable events are published only after commit, because lag recovery reads them from the journal.
- The flush barrier (`WriterMsg::Flush`) that `retire_session_stream` relies on keeps working, and numbers are never reused after a stream is retired.
- One session's slow or failing batch must not delay another session's ephemeral deltas beyond T2.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None expected. `session-event-stream` guarantees per-session ordering, which this change must not weaken. Confirm in design.

## Impact

- `crates/agent/benches/event_emit.rs` for the measurement.
- If built: `crates/agent/src/session_event_stream.rs`, `EventSink` and the SQLite journal writer.
- No migration, ACP or typeshare changes expected; confirm in design.

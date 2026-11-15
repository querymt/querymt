# Baseline (agent side, task 0.1)

Instrumentation is in place (`agent/load_telemetry.rs`). The numbers below come from the `session load telemetry` log lines in the otelite instance (the most recent 1,000 lines, 2026-11-13 01:31 to 2026-11-14 09:42) and from `~/.qmt/sessions.db`. Desktop-side metrics (IPC deliveries, log emits, wire bytes, end-to-end load time) are recorded in the desktop change's `baseline.md`.

These are dogfooding numbers, not a controlled before/after run. There was no throttled-link run, and no pre-change build was measured on the same sessions. Treat them as a record of real use, not as proof of the original targets.

## Where each metric comes from

| Metric | Source |
| --- | --- |
| Snapshot bytes | agent log `session load telemetry: ... snapshot_bytes=` |
| Snapshot events | agent log `snapshot_events=` |
| Mesh requests per load/attach | agent log `mesh_requests=` / `mesh_events=` (kind `local_load` or `remote_attach`) |

Counters are process-wide and reported as the delta over one load.
`mesh_requests` counts `GetEventStreamSince` and `GetEventStreamChunk` replies that crossed the mesh.
Note: `mesh_requests` for a `session/load` of a bookmarked remote session is logged only for the snapshot build; the background backfill and the attach-subscription replay are separate requests and show in the cumulative counters of the following load.

## Measured (dogfooding, 1,000 log lines)

| Scenario | Loads | Sessions | Snapshot bytes p50 / p95 | Snapshot events p50 / p95 / max | Mesh requests | Elapsed p50 / p95 / max |
| --- | --- | --- | --- | --- | --- | --- |
| Local | 848 | 45 | 618 KB / 1.43 MB | 417 / 1,388 / 1,604 | 0 | 88 / 446 / 4,522 ms |
| Local, 1,000+ snapshot events | 202 | n/a | n/a | n/a | 0 | 223 / 823 ms |
| Remote attach (one session, `01a1cf38`, owner `nas`) | 152 | 1 | 883 KB / 1.05 MB | 1,138 / 1,399 / 1,491 | 0 | 4,172 / 12,978 / 24,000 ms |

Findings:

- Local loads are fast. A 1,000-event session loads in about 0.2 s at the median.
- Every sampled remote attach reported `mesh_requests=0`. The snapshot build did not itself fetch from the owner, because the history was already local. That is consistent with "a reopen transfers only new events", but it does not prove it, because the backfill and subscription requests are logged separately (see the note above).
- **Remote attach latency grew during the week.** For the one remote session sampled, the median attach time grew from about 0.2-1 s (2026-11-13 01:00-10:00) to about 12 s (from 14:00 on) while `mesh_requests` stayed 0 and the snapshot stayed about 1 MB. In the database that session now holds 7,407 local events (first journaled 2026-11-12 10:05); I did not measure its size at the start of the sampled period.
- The growth is not explained by the history transfer (no mesh history requests, constant snapshot size). The most likely candidate is the owner round trip that is not counted in `mesh_requests`: `remote_prompt_records_for` (`agent/handle/model_registry.rs:203`) calls `get_user_prompts` on the owner for every prompt message in the window, after the snapshot is built and before `log_finished`. This is unverified. A local-only cause (the read ordered by `source_seq`, whose query plan uses `USE TEMP B-TREE FOR ORDER BY`) has not been ruled out. Neither has been profiled.
- Follow-up needed: count `get_user_prompts` in the telemetry (or log per-stage timings of the attach) and re-measure a large remote session. Until then, requirement "each remote event crosses the mesh once" is only supported for history events, and the load-time goal is not demonstrated for remote sessions with several thousand events.

## Remote sync state in the database (2026-11-14)

- 9,721 remote-origin events across 8 sessions were journaled in the last 5 days; `remote_session_coverage` has 40 rows, one per tail-first session.
- Per owner, sessions with and without a recorded history epoch: `nas` 37 with, 40 without; `dagon` 3 with, 5 without; all other peers (phones, `info2k1.hu`) none.
- 62 session/source pairs have fewer rows locally than their `source_seq` span. This is expected for the tail-first coverage design (older ranges are fetched on demand), and is not a defect by itself.
- The `invalid history chunk` errors in the log (2026-11-07 and 2026-11-09, builds 166 and 171) predate the shipped build and have not recurred.

## Mixed-version matrix (task 5.1)

| Combination | Load | Attach | History | Notes |
| --- | --- | --- | --- | --- |
| New agent, old owner | ok | ok | ok | Reported by the maintainer from a week of daily use. The database shows 40 of 77 `nas` sessions and 5 of 8 `dagon` sessions with no recorded epoch, consistent with the fallback path for owners without chunk support. The only chunk-related errors found in otelite (`invalid history chunk`, 2026-11-07 and 2026-11-09) predate the shipped build; no search was made for other failure messages. |
| Old agent, new owner | not run | not run | not run | |
| stdio / non-snapshot client | not run | not run | not run | |
| Mobile client | not run | not run | not run | |
| Old agent on journal with gaps | not run | not run | not run | |

## Test evidence (tasks 5.3 and 5.4)

- `accounting::failure_tests::settlement_arrives_in_a_history_chunk_and_over_the_live_relay` and `agent::session_actor::tests::local_copy_replaced_after_the_owners_undo_keeps_retained_charges_once` pass.
- `cargo nextest run -p querymt-agent --features remote`: 4,303 run, 4,303 passed, 12 skipped. This requires `QUERYMT_REMOTE_CONNECT_OPEN_BUDGET_MS` to be unset. With it set to 20000 in the shell, `remote_connect::tests::connect_budget_defaults_are_bounded_and_reason_specific` fails, with or without this change (the test's override guard checks only the open budget but asserts a relation that depends on it).

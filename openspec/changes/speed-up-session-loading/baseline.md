# Baseline (agent side, task 0.1)

Status: instrumentation is in place (`agent/load_telemetry.rs`); the baseline numbers below have NOT been measured yet. Desktop-side metrics (IPC deliveries, log emits, wire bytes, end-to-end load time) are recorded in the desktop change's `baseline.md`.

## Where each metric comes from

| Metric | Source |
| --- | --- |
| Snapshot bytes | agent log `session load telemetry: ... snapshot_bytes=` |
| Snapshot events | agent log `snapshot_events=` |
| Mesh requests per load/attach | agent log `mesh_requests=` / `mesh_events=` (kind `local_load` or `remote_attach`) |

Counters are process-wide and reported as the delta over one load, so measure one session at a time.
`mesh_requests` counts `GetEventStreamSince` and `GetEventStreamChunk` replies that crossed the mesh.
Note: `mesh_requests` for a `session/load` of a bookmarked remote session is logged only for the snapshot build; the background backfill and the attach-subscription replay are separate requests and show in the cumulative counters of the following load.

## Scenarios to record

1. Local session (existing, large history).
2. Remote session with about 5,000 events, over a throttled link (for example 5 Mbit/s, 100 ms RTT).

| Scenario | Snapshot bytes | Snapshot events | Mesh requests | Mesh events |
| --- | --- | --- | --- | --- |
| Local | not measured | not measured | n/a | n/a |
| Remote ~5,000 events, throttled | not measured | not measured | not measured | not measured |

## Mixed-version matrix (task 5.1)

| Combination | Load | Attach | History | Notes |
| --- | --- | --- | --- | --- |
| New agent, old owner | not run | not run | not run | |
| Old agent, new owner | not run | not run | not run | |
| stdio / non-snapshot client | not run | not run | not run | |
| Mobile client | not run | not run | not run | |
| Old agent on journal with gaps | not run | not run | not run | |

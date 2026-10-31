# Tasks

## 1. Interrupted outcome

- [x] 1.1 Introduce a shared run-outcome type/constants (`completed`, `failed`, `cancelled`, `interrupted`) in `crates/agent/src/events.rs` and use it in `PromptFinished` (`agent/session_actor.rs`) instead of string literals; `RunCompleted.outcome` stays `String`. Verify: existing `RunCompleted` serialization tests pass unchanged and a new unit test round-trips `{"type":"run_completed","data":{"run_id":"r","outcome":"interrupted"}}`.
- [x] 1.2 Confirm `classify_durability` treats `RunCompleted` as durable (add an assertion if missing). Verify: unit test in `events.rs`.

## 2. Cross-platform run lease

- [x] 2.1 Add `crates/agent/src/session/run_lease.rs` with `RunLease` (owns a `std::fs::File` locked via `File::try_lock`), `RunLeaseRoot::try_acquire(session_id) -> Result<Option<RunLease>, io::Error>` (`None` = busy), lease path `<root>/<session_id>.lock`, directory created on demand, files never deleted; open files only through std (no raw `libc`/WinAPI) so handles are close-on-exec / non-inheritable. Validate `session_id` as a UUID-like token before building a path. Verify: unit tests for acquire, busy, release-on-drop, rejected invalid ids.
- [x] 2.2 Test same-process exclusivity: a second `try_acquire` for the same session while a lease is held returns busy (D4). Verify: unit test passes on Linux, macOS and Windows.
- [x] 2.3 Cross-process crash test using the existing self-reexec helper pattern (`worktree/lock.rs` `admin_lock_blocks_across_processes`): child acquires the lease and is killed (`Child::kill`, i.e. `TerminateProcess` on Windows); parent polls `try_acquire` with a bounded timeout (≤5 s) and succeeds. Verify: test passes on all three OSes.
- [x] 2.4 Inheritance test: while holding a lease, spawn a long-lived child process (`sleep`/`timeout`-equivalent via the test binary itself), drop the lease, and assert it is immediately acquirable while the child still runs. Verify: test passes on all three OSes.
- [x] 2.5 Leftover-file test: create the lease file, then acquire without any holder. Verify: unit test.
- [x] 2.6 Expose `run_lease_root() -> Option<PathBuf>` on the storage backend: `SqliteStorage` returns `<db_dir>/<db_stem>.run-leases`, mock/in-memory stores return `None`, with a test-only constructor that sets an explicit root. Verify: unit test that `~/.qmt/sessions.db` maps to `~/.qmt/sessions.run-leases` and a Windows-style path (`C:\Users\x\.qmt\sessions.db`) maps correctly via `Path` APIs.

## 3. Lease held for the run's lifetime

- [x] 3.1 Add `AgentError::SessionRunningElsewhere { session_id }` and map it to an ACP error with message "session is running in another process". Verify: unit test for the ACP mapping.
- [x] 3.2 In `launch_prompt` and the scheduled-prompt handler (`agent/session_actor.rs`), acquire the lease before mutating actor state or emitting `RunStarted`; store it on the actor (`SessionActor::run_lease`, not in the `Clone` `ActiveRun`) and release it in `PromptFinished` only after `RunCompleted` is persisted. A busy lease is retried for `RUN_LEASE_GRACE` (500 ms) so a momentary holder (reconciler, delete) does not reject the run; lease file I/O runs on the blocking pool. Busy → reply `SessionRunningElsewhere` (scheduled: emit `ScheduledExecutionFailed`), no `RunStarted`; other I/O errors → `warn!` and run unleased. Verify: actor tests — lease held during a run (second acquire busy), released after `PromptFinished`, busy lease rejects the prompt with no `run_started` journaled, a momentary holder only delays the prompt (`run_lease_momentary_holder_delays_but_does_not_reject_prompt`).
- [x] 3.3 Hand the lease over to a queued prompt launched from `PromptFinished` instead of releasing and re-acquiring it (design D6), so no other owner can start a run between queued turns; release only when the queue is empty. Verify: actor test with one queued prompt completes both runs, each with its own `run_started`/`run_completed`, and the lease stays held between them.

## 4. Reconciliation

- [x] 4.1 (plus partial index migration `0032`, see design D8) Add `EventJournal::last_unfinished_run(session_id) -> SessionResult<Option<String>>` with a default implementation over `load_session_stream`, and a SQLite override using the `(session_id, stream_seq)` index and `json_extract(payload_json,'$.data.run_id')` restricted to `run_started`/`run_completed` events with `origin = local` (mirrored remote events are ignored; see design D8). Verify: tests for no runs, mirrored remote run, matched run, unmatched last run, unmatched earlier run followed by a matched later run (returns the earlier one only if it is the last unfinished), on both implementations.
- [x] 4.2 Implement `reconcile_orphaned_run(config, session_id)` per design D7 (skip without lease root or for remote/bookmarked sessions; query; `try_acquire`; under the lease, loop re-query + `emit_durable(RunCompleted{outcome:"interrupted"})` until no unfinished run remains (bounded); release). Returns the closed run ids. Verify: unit tests — orphan closed once; several orphans closed in one call; held lease → nothing written; second call → no-op; remote bookmark → nothing written; mirrored remote run without a bookmark → nothing written; journal with no runs → nothing written.
- [x] 4.3 Concurrency test: two tasks (and, via the self-reexec helper, two processes) reconcile the same orphaned session simultaneously. Verify: exactly one `interrupted` event in the journal.
- [x] 4.4 Call reconciliation in `SessionMaterializer::materialize_session_actor` before the actor is registered. Verify: materializer test with a journal containing an unmatched `run_started` — after materialization the journal has one `interrupted` completion.
- [x] 4.5 Call reconciliation in `load_session_snapshot` for local sessions before the audit view is read. Verify: test that the returned snapshot's audit events end with `run_completed` / `interrupted` for a seeded orphan, and unchanged for a session whose lease is held.
- [x] 4.6 In the `Cancel` handler, when `active_run` is `None`, spawn reconciliation (non-blocking). Verify: actor test — Cancel on an idle actor with a seeded orphan journals one `interrupted` completion; Cancel during a live run does not.
- [x] 4.7 In `stop_session_in_runtime`, replace the silent return for unregistered sessions with reconciliation (local sessions only). Verify: handle test — Stop on an unloaded session with a seeded orphan journals one `interrupted` completion.
- [x] 4.8 On session delete, best-effort remove the lease file only after acquiring it; ignore failures. Verify: test that deleting a session removes its lease file when free and leaves it when held.

## 5. End-to-end and platform coverage

- [x] 5.1 Integration test reproducing the incident: a child process starts a run (journals `run_started`, holds the lease) and is killed; the parent loads the session and observes `run_completed`/`interrupted` in the snapshot; the session lease is then free for a new run (run start under a free lease is covered by the actor tests in 3.2). Verify: test passes on Linux, macOS and Windows.
- [ ] 5.2 Add a `windows-latest` job to `.github/workflows/test-agent.yml` running `cargo nextest run -p querymt-agent --lib --features "${features}" -E 'test(/run_lease|run_reconcile|reconcile_|orphan|last_unfinished_run/)'` (lease, reconciliation, actor/handle lease tests and incident tests; test names follow these prefixes), using the same embedded-dashboard fixture step adapted for PowerShell. Verify: job is green on the PR.
- [x] 5.3 Run `cargo bench -p querymt-agent --bench session_load` before/after. Verify: no regression beyond noise; record numbers in the PR description.
- [x] 5.4 Manual check against a copy of the real database (`sqlite3 ~/.qmt/sessions.db ".backup /tmp/qmt-copy.db"`, run `qmtcode --db /tmp/qmt-copy.db --dashboard`): open session `01a17c4b-3b1f-72d2-99e1-16024ff942d5`. Verify: dashboard shows it not running; journal of the copy has one `interrupted` `run_completed` for run `3c1abf0d-3bae-4250-b3c5-19b28910ce13`.
- [x] 5.5 Document run leases (location, per-machine scope, "running in another process" error, network-filesystem caveat) in `docs/docs/agent/configuration.md` next to the sessions database path. Verify: docs build (`docs-preview` workflow) passes.

## Workflow follow-up

- In `querymt-desktop`, render outcome `interrupted` as "Turn interrupted." in `src/lib/domain/session-snapshot.ts` (separate repo/PR; not required for correctness).
- Separate changes already identified: guarantee `PromptFinished` on panic/abort of a run task; reconcile `ready` workspaces with missing checkouts; diagnose the "forked child … invalid parameter" error.

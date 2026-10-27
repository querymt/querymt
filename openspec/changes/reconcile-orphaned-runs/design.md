# Design

## Context

See proposal.md (Why) for the incident. Relevant current state:

- `SessionActor::launch_prompt` (interactive) and the scheduled-prompt handler (`agent/session_actor.rs`) set `active_run`, emit `RunStarted`, spawn the run, and on completion send `PromptFinished`, which emits `RunCompleted { outcome }` and clears `active_run`. A process crash skips the second half, so the journal keeps an unmatched `run_started`.
- `Cancel` with no `active_run` only emits `Cancelled`. `stop_session_in_runtime` returns early when the session is not in the registry (`agent/handle/session_control.rs`).
- The desktop dashboard derives run state from `run_started` / `run_completed` in the load snapshot (`querymt-desktop/src/lib/domain/session-snapshot.ts`), and that verdict overrides other heuristics at load. Any `run_completed` whose outcome is not `failed` renders as not running.
- `worktree/lock.rs` already uses `std::fs::File::try_lock`, which is `flock` on Unix and `LockFileEx` on Windows, for cross-process locks. The toolchain is stable, so this API is available.
- `SqliteStorage::connect(path)` knows the database path. Mock and in-memory stores don't have one.

## Goals / Non-Goals

**Goals:** close runs abandoned by dead processes; never close a live run; behave the same on Windows, Linux and macOS; repair already-stuck sessions on first load; add no new dependency.

**Non-Goals:**
- Hung runs inside a live process. A run whose task never finishes still holds its lease. Guaranteeing `PromptFinished` (panic/abort handling) is a separate change.
- Closing dangling `tool_call_start` / `snapshot_start` events. The dashboard already normalizes in-progress tools at load completion.
- Stale workspace records and the "forked child … invalid parameter" error.
- Remote/mesh sessions.
- Leases across machines (network filesystems).

## Decisions

### D1: The run lease is an OS file lock held for the run's lifetime
- `RunLease` wraps an open `std::fs::File` on which `try_lock()` (exclusive) succeeded.
- Dropping it closes the handle, which releases the lock.
- The OS releases the lock when the process exits, however it exits.
- Acquisition is always non-blocking (`try_lock`): busy means "owned by someone alive".
- Lease file I/O runs on the blocking pool (`RunLeaseRoot::acquire`), so it never stalls an async worker. The lease directory is created only when the first open reports `NotFound`.

*Alternatives considered:*
- DB heartbeat: needs thresholds, and sleep/resume looks like a crash.
- PID stamp: PIDs are reused.
- Single-process DB lock: rejected by the user. Option A was chosen.

### D2: Lease location is derived from the database path
- The lease root is `<db_dir>/<db_stem>.run-leases/`, e.g. `~/.qmt/sessions.run-leases/`. A separate `--db` gets separate leases, because a lease guards a journal.
- Each session's file is `<session_public_id>.lock`. Session IDs are UUIDs, so the name is valid on every platform: no reserved characters, no reserved device names, and short paths.
- The storage backend exposes `run_lease_root() -> Option<PathBuf>`. SQLite returns `Some`. Mock and in-memory stores return `None`, which disables both leasing and reconciliation. Tests opt in with a temp directory.

### D3: Lease files are never deleted
- Deleting a lock file lets two acquirers lock different files. On Unix one holds the unlinked inode while another creates a new file. On Windows, std opens with `FILE_SHARE_DELETE`, so deleting an open file leaves it delete-pending and later opens fail.
- Keeping the empty files avoids both. They are tiny and bounded by the session count.
- Session deletion removes the file best-effort, only after acquiring its lock. On Unix it unlinks while holding the lock; on Windows it releases first. The race above is acceptable there only because the deleted session never runs again.

### D4: Lock exclusivity holds within one process as well
- On Unix, `flock` locks belong to the open file description. On Windows, `LockFileEx` locks belong to the handle. So a second `open` + `try_lock` in the *same* process reports busy while the run's handle is held.
- This lets the reconciling process detect its own live run without consulting the actor registry. The behavior gets a test on all three OSes.

### D5: Lease handles must not leak into child processes
- Runs spawn shells and tools. If a child inherited the lock handle, the lease would outlive the run and a crash would never be detected.
- Rust's std opens files `O_CLOEXEC` on Unix and non-inheritable on Windows.
- The lease module asserts this through std only (no raw `libc`/WinAPI opens). A test spawns a long-lived child while holding the lease, drops the lease, and checks it is immediately acquirable.

### D6: Run start acquires the lease before `RunStarted`; `PromptFinished` releases it after `RunCompleted`
- The lease is stored on the session actor (`SessionActor::run_lease`), not in `ActiveRun`. `ActiveRun` is `Clone`, and a clone must never be able to extend the lease.
- `RunCompleted` is persisted (awaited) before the lease can be released.
- **Handover:** if a queued prompt is waiting when a run finishes, the lease passes straight to the next run and is not released in between. Otherwise another process (or a reconciler) could take it between two queued turns, and every queued prompt would be rejected. The lease is released only when the queue is empty.
- **Grace:** reconcilers and session delete hold the lease for a few milliseconds. A run start that finds it busy retries for up to `RUN_LEASE_GRACE` (500 ms) before treating it as owned by a live run. Without this, a prompt racing a snapshot load or a Cancel would be wrongly rejected as "running in another process".
- If the lease is still busy after the grace period, the prompt is rejected with a new `AgentError::SessionRunningElsewhere` (mapped to an ACP error with a clear message). No `RunStarted` is emitted and actor state is left untouched.
- A scheduled prompt rejected this way emits `ScheduledExecutionFailed`. The scheduler handles that event only after `fire_schedule` has registered the cycle and its timeout, and terminal handling is idempotent per (schedule, turn), so a racing timeout is dropped.
- An I/O error other than busy is logged, and the run proceeds unleased (fail-open). Reconciliation then never sees a held lease for it, so a crash leaves it to the next load, same as today. This keeps a read-only or odd filesystem from blocking all prompts. A session id that is not file-name safe (`InvalidInput`) is logged at debug level only.

### D7: Reconciliation algorithm
`reconcile_orphaned_run(session_id)`:
1. Skip if there is no lease root, or if the session is remote/bookmarked.
2. Query the journal for the last `run_started` whose `run_id` has no later `run_completed` (D8). If there is none, return.
3. `try_lock` the session lease. If it is busy, return: the run is live somewhere, or Windows is still releasing a dead process's lock (see Risks).
4. While holding the lease, repeat until the query returns none (bounded to 64 iterations): re-run the query (another reconciler may have closed runs in between), then `emit_durable(RunCompleted { run_id, outcome: "interrupted" })`. Each write is awaited, so the events are in the journal before the function returns. While the lease is held no run can start, so every unmatched `run_started` is an orphan, including older ones left by repeated crashes.
5. Release the lease.

Holding the lease across steps 4–5 serializes concurrent reconcilers and a racing run start, which gives idempotency without DB constraints.

### D8: Journal query
- Add `EventJournal::last_unfinished_run(session_id) -> Option<String>`.
- The default implementation scans `load_session_stream`.
- The SQLite override uses `json_extract(payload_json, '$.data.run_id')` over `run_started` / `run_completed` rows of that session.
- Only events with `origin = 'local'` are considered (the default implementation filters `EventOrigin::Local`). Events mirrored from a remote host describe runs that host owns and closes. The remote bookmark check alone is not enough: dismissing a remote session removes the bookmark but keeps the mirrored events, and the lease (a file on this machine) is always free for a run on another machine. `origin` is used rather than `source_node_id IS NULL` because legacy remote rows can have a NULL `source_node_id`.
- A partial index `idx_event_journal_run_lifecycle` on `event_journal(session_id, stream_seq) WHERE kind IN ('run_started','run_completed')` (migration `0032`) bounds the read to run lifecycle rows. Without it the query read every row of the session and added ~18% to `session_load` snapshot time on the benchmark; with it the change is within noise. A test asserts the query plan uses the index, so the query predicate and index predicate must stay identical.

### D9: Call sites
- Actor materialization (`SessionMaterializer::materialize_session_actor`), before the actor is registered. No run of this process can exist yet.
- `load_session_snapshot`, for local sessions before reading the audit view, so the served snapshot already contains the closing event. This covers loads where the actor is already materialized.
- `Cancel` handler when `active_run` is `None`. It runs as a spawned task so the actor mailbox isn't blocked on I/O.
- `stop_session_in_runtime` when the session is not in the registry, instead of the silent return.

### D10: Outcome value
- `RunCompleted.outcome` stays a `String`. Add the `"interrupted"` value through a shared constant/enum used by both `PromptFinished` and reconciliation.
- typeshare output is unchanged, because the field type doesn't change.

## Risks / Trade-offs

- [Windows releases locks of a terminated process asynchronously ("depends on available system resources")] → reconciliation treats busy as "not now" and never blocks. The next load or cancel retries. Tests poll with a bounded timeout instead of asserting instant release.
- [Network filesystems (NFS/SMB) give weak or no lock semantics] → documented: run ownership is per machine with a local database. `try_lock` errors other than busy fail open (D6), so behavior is no worse than today.
- [A run hung inside a live process keeps its lease, so the session stays "running"] → that is correct (it is running). The separate `PromptFinished`-guarantee change covers it.
- [Two processes on one machine with the same database previously could interleave turns; now the second is rejected] → intentional behavior change, surfaced as a clear error.
- [Processes that take no lease: older binaries still running during an upgrade, and runs that fell back to unleased (D6)] → a new process that loads such a session sees a free lease and closes the live run as `interrupted`. The owner's own `run_completed` follows later, so the dashboard shows the run as not running until it ends. This is accepted for the upgrade window and documented; restart old instances when upgrading.
- [Lease files accumulate] → one empty file per session that ever ran. They are removed best-effort on session delete.
- [Reconciliation adds a query to every load] → indexed and bounded to one session (D8). It is measured with the existing `session_load` bench.

## Migration Plan

- One additive migration (`0032_event_journal_run_lifecycle_index`) creates the partial index; it holds no data of its own and is built once on upgrade. On first load after upgrade, each stuck session gets one `interrupted` event.
- Rollback: older binaries ignore the extra `run_completed` events, the lease directory and the unused index. The `"interrupted"` outcome displays as completed in the current dashboard.

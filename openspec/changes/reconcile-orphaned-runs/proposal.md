# Proposal

## Why

When a QueryMT process dies during a turn, the session journal keeps a `run_started` event that never gets a matching `run_completed`. The embedded dashboard rebuilds a session's run state from those two events when it loads the session. So the session shows "Agent is working…" every time it is opened, permanently. Cancel does not help. It only acts on a run held in memory, and the new process has none, so it writes a `cancelled` event, which the dashboard ignores for run state. Five sessions in a real `~/.qmt/sessions.db` are in this state today (for example `01a17c4b-…`, run `3c1abf0d-…`, started 2026-10-27 07:37:49, process died at 09:09:50).

The journal can't be closed blindly when a session loads. Several processes can share one `sessions.db` (successive `qmtcode` instances, the desktop app, multi-process workspace use), so an unmatched `run_started` may belong to a run that is still live in another process.

## What Changes

- Every local run holds an exclusive, OS-level, per-session **run lease** for its whole lifetime: a lock on a per-session lock file. The operating system releases the lease when the owning process exits for any reason, including a crash or kill. This works on Linux, macOS and Windows.
- A run whose `run_started` has no matching `run_completed`, and whose lease is free, is **orphaned**. QueryMT closes it by journaling `run_completed` with the new outcome `interrupted`.
- QueryMT reconciles orphaned runs:
  - before a local session actor accepts input;
  - before a session load snapshot is served;
  - when Cancel or Stop is requested for a session with no in-memory run.

  So opening or cancelling a crashed session clears its "running" state.
- A new run can't start while another process holds the session's run lease. The prompt is rejected with a clear "session is running in another process" error, instead of two processes interleaving turns in one journal.
- `RunCompleted.outcome` gains the value `interrupted`. This is additive: existing values are unchanged.
- Remote-hosted (bookmarked) sessions are out of scope. Their runs are owned by the host node's own journal.

## Capabilities

### New Capabilities
- `session-run-lifecycle`: how a session run's start and end are recorded durably, how run ownership is established across processes on one machine, and how runs abandoned by a dead process are detected and closed.

### Modified Capabilities
- None. `acp-session-updates` and `session-interactions` don't define run start/completion semantics. `session-event-stream` (in the in-flight `ordered-session-events` change) governs ordering, which this change relies on but does not alter.

## Impact

- **Code (`crates/agent`):**
  - new run-lease module (a lock file per session beside the sessions database);
  - `agent/session_actor.rs`: the interactive (`launch_prompt`) and scheduled run start paths acquire the lease, `PromptFinished` releases it, and Cancel with no active run triggers reconciliation;
  - `agent/session_materializer.rs` (actor materialization);
  - `session/load_snapshot.rs`;
  - `agent/handle/session_control.rs` (stop for an unloaded session);
  - `events.rs` (`interrupted` outcome);
  - session store / event journal (a query for the last unfinished run of a session).
- **Dependencies:** none new. The lease uses the standard library's cross-platform file locking, already used by `worktree/lock.rs`. That is `flock` on Unix and `LockFileEx` on Windows.
- **Filesystem:** new directory `<db-stem>.run-leases/` beside the sessions database (e.g. `~/.qmt/sessions.run-leases/`), holding one small empty lock file per session that has run. The files are kept, not deleted.
- **CI:** a Windows job runs the run-lease tests, because the agent test workflow currently runs only on Linux.
- **Desktop UI (`querymt-desktop`, separate repo):** works without changes. A closed run renders as "completed" with the generic label. A follow-up adds a "Turn interrupted." label for the new outcome.
- **Existing data:** sessions already stuck are repaired the first time they are loaded after upgrade. No migration and no manual database edits.

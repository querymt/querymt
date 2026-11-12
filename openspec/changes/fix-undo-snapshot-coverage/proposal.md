# Proposal

## Why

Undo snapshots were very slow in a git repository that had a normal `.gitignore`. Commit f4b968a5 (`fix/snapshot`, not released yet) fixed this by adding a stat cache and by leaving out files above `max_file_bytes`. The stat cache is the part that fixes the speed. The size cap creates holes in undo coverage:

- A file can be in one snapshot and missing from the next.
- Each pair of snapshots then needs special one-sided rules.
- Undo can overwrite a file whose current content it never saved, and redo then reports success (CodeRabbit finding on `crates/agent/src/snapshot/git.rs`).

Reviewing the code also found more bugs that make undo report success while it loses or misplaces data, or that make snapshots costly for no gain:

- Undoing a move deletes the new file and never restores the original.
- Restore errors are ignored.
- The executable bit is lost.
- Whole blobs are loaded into memory on restore.
- Large blobs are deflated even though they don't compress.
- Every snapshot writes a new commit, even when nothing changed.
- `max_snapshots` and `max_age_days` are parsed but never reach the snapshot store, and GC never deletes anything, so their documented defaults (100 snapshots, 30 days) suggest pruning that never happens.

Undo also restored nothing for every turn recorded since steering support (`64f6727b`, in nightly builds only, no release):

- Each turn writes a start record (pre-turn snapshot) and a change record (changed files). Since that commit they carry different turn ids: the start a fresh UUID, the change the run id.
- Undo pairs them by turn id, found no pairs, restored nothing, reported success with no files, and stored a revert frame. The next prompt then deleted the undone turn's history.
- On a real session database, all 1,004 change records written since then are unpaired, and all 1,917 earlier ones are paired.
- The tests missed it because the undo fixture writes both records itself with the same id; no test ran a real prompt.

The ids involved are all plain `String`s. `SnapshotId` is only an alias for `String`, change records will hold three string ids side by side (`turn_id`, `snapshot_id`, `base_snapshot_id`), and the runtime pairs the pre-turn ids by position in a `(String, String)` tuple. Swapping a turn id and a snapshot id, or passing a message id where a snapshot id is expected, compiles. Distinct types would not by themselves have caught the UUID-vs-run-id mismatch (both are valid turn ids), but they make the pairing this fix relies on checked by the compiler instead of by convention.

This change makes snapshots complete and fast, instead of fast because they are incomplete. It also makes garbage collection real but opt-in: by default nothing is deleted, so undo never fails because of GC unless the user asks for GC.

## What Changes

- **Remove the size cap.** Every file inside the snapshot scope is captured, whatever its size. Removed: the `[agent.execution.snapshot] max_file_bytes` key, the `qmt-skipped` commit trailer, and every rule that depended on it. This key was never released, so removing it breaks no released config.
- **Never leave out a file that changed while it was being read.** Such a file is captured from a point-in-time copy instead of being skipped.
- **Make large files cheap to capture and restore:**
  - Blobs of 16 MiB or more are stored without zlib compression.
  - Restore streams large blobs to disk instead of loading them into memory.
  - Diffs no longer run rename detection, which also read blobs.
  - The stat cache from f4b968a5 stays.
- **Reuse the previous snapshot id when nothing changed**, instead of writing a new commit every time.
- **Snapshot scope is decided by path:**
  - `.gitignore`, `.ignore` and `.git/info/exclude` are honoured, and dot-directories are never captured.
  - **BREAKING:** `.gitignore` now also applies outside git repositories. Before, it was silently ignored there, so such files were snapshotted and could be reverted by undo.
  - `.ignore` already worked (the `ignore` crate default). It is now specified and documented as the way to opt files out. No new ignore-file name is added.
- **Report large-file cost.** When a file of 64 MiB or more enters undo snapshots for the first time, a single warning names it and points to `.ignore`.
- **Report store growth.** When a workspace's snapshot store is 5 GiB or larger, one warning per workspace per process names the store and its size and points to `.ignore` and the retention keys.
- **Garbage collection is supported and off by default:**
  - `max_snapshots` and `max_age_days` default to unset, which means no GC. The documented 100/30 defaults never had an effect, so the default behaviour doesn't change: undo to any recorded turn and redo of any revert frame keep working.
  - **BREAKING:** setting either key now enables GC. It reaches the snapshot store and, after each turn, prunes snapshots beyond the count or older than the age. Before, the values were ignored.
  - Content referenced only by pruned snapshots is deleted from disk (once 10 or more are pruned), never while a snapshot or restore of the same workspace runs, and without changing kept snapshot ids.
  - Undo or redo that needs a pruned snapshot fails before any file changes, and redo keeps its frame.
  - **BREAKING:** `0` for either key is now a config load error.
- **Fix undo of moves.** Undoing a turn that moved a file now restores the original path and removes the new one. Before, rename detection reported only the new path.
- **Restore writes files atomically and reports failures:**
  - Each file is written to a temporary file and renamed into place.
  - Permissions are kept, and the executable bit follows the snapshot.
  - Write and delete errors fail the undo or redo instead of being dropped.
  - Partial restores are therefore reported as errors, and a failed undo still leaves a redo frame.
- **Make each turn's change record self-contained.**
  - `TurnSnapshotPatch` gains an optional `base_snapshot_id`: the pre-turn snapshot. Undo uses it directly, so it no longer depends on finding the matching start record.
  - The start and change records of a turn share the run id again, so builds that pair by id keep working on new records.
  - Records without `base_snapshot_id` (released builds and nightly builds) are resolved through the start record with the same turn id, then through the nearest earlier start record. This keeps turns recorded by nightly builds undoable without rewriting the session database.
  - Undo fails before any file changes, and records no revert frame, when a turn after the target changed files and its pre-turn snapshot can't be found.
  - **BREAKING:** undo to a message inside a turn that had already started changing files (for example a steering message) is refused. Snapshots are per turn, so only whole turns can be reverted. Before, this silently restored nothing.
- **Rust API changes:**
  - **BREAKING (unreleased):** `GitSnapshotBackend::with_max_file_bytes` and `snapshot::git::DEFAULT_MAX_FILE_BYTES` are removed.
  - `GitSnapshotBackend::with_snapshot_base` becomes public, so embedders and benches can choose where snapshots are stored.
  - **BREAKING:** `SnapshotError` gains an `Io` variant that keeps the `std::io::Error` source and the path.
  - **BREAKING:** `GcResult` gains a `deleted_objects` field, so struct literals outside the crate stop compiling.
  - **BREAKING:** `SnapshotBackendConfig.max_snapshots` and `max_age_days` become `Option<NonZeroUsize>` and `Option<NonZeroU64>`, defaulting to `None`.
  - **BREAKING:** `GcConfig::default()` becomes no limits (GC off). `GcConfig` gains `From<&SnapshotBackendConfig>` and `is_enabled()`.
  - **BREAKING:** `MessagePart::TurnSnapshotPatch` gains `base_snapshot_id: Option<SnapshotId>`, so struct literals outside the crate stop compiling.
  - **BREAKING:** `snapshot::SnapshotId` becomes an opaque newtype instead of an alias for `String`. Every `SnapshotBackend` method, `SnapshotError::InvalidSnapshotId` and `RevertState.snapshot_id` use it.
  - New `model::TurnId` newtype. **BREAKING:** `TurnSnapshotStart`/`TurnSnapshotPatch` take `turn_id: TurnId` and `snapshot_id: SnapshotId`, and `ExecutionContext::turn_id`/`with_turn_id` use `TurnId`.
  - `SnapshotId` wraps an `Arc<str>` and `TurnId` wraps a `Uuid` (`Copy`). Both serialize as plain strings, so stored messages, revert frames and mesh payloads keep their exact bytes for every UUID turn id, which is every turn id the code mints.
- **Benchmark.** A criterion bench `snapshot_track` measures capture cost before and after the change.

## Non-goals

- GC on by default.
- Protecting snapshots that live sessions still reference from enabled GC. A follow-up can pin every snapshot that a session's history or redo frame references (for example with refs in the snapshot store), let GC delete only unpinned content, and drop a session's pins when the session is deleted. Until then, enabled GC can prune a live session's early turns, which is documented next to the keys.
- A content-addressed side store that uses `clonefile`/reflink for large files (nono's approach). Hashing still costs about as much as an uncompressed write, so this would add a second object kind for a small gain. Revisit if the bench shows the write still dominates.
- Parallel hashing with rayon or `WalkBuilder::build_parallel`. Measure with the new bench first.
- A separate snapshot ignore file (for example `.qmtignore`). `.ignore` already covers this.
- A hard snapshot budget that fails the snapshot (nono's `WalkBudget`).
- Capturing symlinks, and capturing files that can't be read because of permissions. Today such a file still fails the snapshot.
- Serializing concurrent `track` calls on one workspace.
- Undo to a point inside a turn. Snapshots are taken per turn, so this would need per-tool snapshots.
- Undo of a turn whose process died before its change record was written. Its start record exists, but undo doesn't diff it against the current state.
- Newtypes for message, session and run ids, and for the `turn_id` fields of hook payloads, agent events and scheduler messages. Hook payloads are an external contract and events reach the desktop; they stay `String` and receive `TurnId` converted at their boundary.
- Validating the snapshot id format while deserializing. Snapshot ids stay opaque until a backend uses them, and a stored turn id that is not a UUID loads through a derived UUID instead of failing (design D18).

## Capabilities

### New Capabilities
- `workspace-snapshots`: what undo snapshots capture (scope and completeness), how undo and redo restore it, what snapshots cost, and how long they are kept (no GC by default, opt-in GC).

### Modified Capabilities
None. `tool-snapshots` keeps deciding *when* a snapshot is taken. This change only covers *what* is captured and restored.

## Impact

- **Packages:**
  - `querymt-agent`: `snapshot`, `agent/undo` and its tests, `agent/agent_config_builder`, `agent/session_actor` (GC error logging, turn snapshot records), `model` (`TurnSnapshotPatch`, `TurnId`), `agent/core` (runtime turn snapshot), `agent/execution_context` and the execution modules that read the turn id, `session/domain` and `session/sqlite_storage` (revert frames), `test_utils` (undo fixture), `api/quorum`, `config/execution`, and a new bench.
  - No feature flags gate this code; it builds with default features.
  - No other package changes.
- **Config (TOML/JSON):**
  - `[agent.execution.snapshot] max_file_bytes` is removed. It was unreleased, so setting it is now an unknown-field error.
  - `max_snapshots` and `max_age_days` default to unset (no GC). **BREAKING:** setting them now deletes old snapshots, and `0` is rejected. The JSON schema gains `minimum: 1` for both keys.
- **Behaviour:**
  - **BREAKING:** `.gitignore` applies to snapshots outside git repositories.
  - Undo and redo now fail when a restore write fails, instead of reporting success.
  - With default config, undo never fails because a snapshot was deleted; nothing is deleted.
  - **BREAKING:** with `max_snapshots`/`max_age_days` set, undo and redo past the retention window fail without changing files.
  - Undo restores files again for turns recorded since steering support.
  - **BREAKING:** undo to a message inside a turn that already changed files fails without changing files; undo that can't find a turn's pre-turn snapshot fails instead of reporting success.
- **Public Rust API:** see What Changes. Unreleased removals are listed for completeness.
- **Dependencies:** `tempfile` moves from dev-dependency to normal dependency of `querymt-agent`, and `flate2` (`default-features = false`, `rust_backend`) becomes a normal dependency. Both are already in the lockfile.
- **Persisted state:**
  - The snapshot store under the user cache dir gets uncompressed large objects. By default it only grows; with retention set, old content is actually deleted.
  - The store format stays plain git, readable by `git`.
- **Persisted messages:** turn change records gain an optional `base_snapshot_id` inside `message_parts.content_json`. There is no schema change or migration, and existing records are not rewritten. Older builds ignore the field and pair by turn id, which new records satisfy. Mesh peers on older versions ignore it too: kameo encodes with named msgpack fields, and `MessagePart` does not deny unknown fields.
- **Typed ids:** `SnapshotId` and `TurnId` are `#[serde(transparent)]`, so `message_parts.content_json`, the `revert_states.snapshot_id` column and kameo's named msgpack keep their bytes; no migration. Neither `MessagePart` nor `RevertState` is a `#[typeshare]` type, and the undo-stack ext payload carries only `message_id` and `restore_failed`, so no desktop type changes. No other workspace package references these types.
- **Unchanged:** session DB schema, ACP, hooks and middleware (their payloads keep `turn_id: String`, converted at the boundary), Extism contract, Python bindings and typeshare types.
- **querymt-desktop follow-up:** optional. `UndoError` keeps its variants, but undo on a steering message now returns an error; the UI may hide undo there.
- **Docs and changelog:** `docs/docs/agent/configuration.md`, plus a `CHANGELOG.md` Unreleased entry.

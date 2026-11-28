# Proposal

## Why

Starting an isolated delegate costs seconds of non-LLM time that no trace can explain. In session `01a2201c-d800-76a2-b613-5ba012f56452` (otelite trace `5cd932100c3d1a5e139d602d417fc227`, querymt checkout, 1,934 tracked files, 38 MB), the `coder` delegate:

- spent **3.4 s with no spans** between the `delegate` tool returning and `delegation.create_session`. The workspace id (UUIDv7) was minted 2.7 s into that gap and the child session 0.7 s later;
- then had its first mutating tool wait **0.73 s** for a 3.3 s `snapshot.git.track` that hashed and wrote every file of the fresh worktree (`reused=false hashed_files=1934 bytes_hashed=38396018`). The parent's snapshot of the same content takes 54 ms.

The code explains most of it. `capture_working_state_validated` runs three full working-tree captures per attempt (a before manifest, the capture, an after manifest). Each capture reads every tracked file and writes it through `write_blob_stream`, which compresses and writes a temporary loose object before it checks whether the object exists. The fresh worktree's snapshot store starts empty, so its first snapshot compresses and writes every baseline blob again, although the source repository already holds them. Nothing in `worktree/` emits a span, so none of this shows up in traces.

## What Changes

- **Isolated delegation startup becomes traceable.** Spans cover the phases between the `delegate` tool and the delegate's first prompt:
  - queueing for a parallel slot (a sibling of `delegation.execute` under the delegation's handler span);
  - claim and admission;
  - parent workspace resolution;
  - baseline capture (attempts, files read, bytes, objects written);
  - batch-lock, admin-lock and mutation-gate waits (the gate wait names its checkout);
  - provisioning (branch, checkout);
  - the `delegation_start` hook.

  Each span records the counts that explain its duration, as `snapshot.git.track` already does.
- **Baseline capture reads only what changed.** The capture compares each tracked file's stat data with its real-index entry itself, instead of running a status pass. A file whose stat data matches, and that is not racily clean, reuses the entry's object id and is not read. These are always read:
  - modified tracked files;
  - entries flagged assume-unchanged, skip-worktree or fsmonitor-valid;
  - files whose attributes or config convert content (line endings, `ident`, working-tree encoding);
  - non-ignored untracked files.

  A blob is hashed before it is written, and an object that already exists is not written again. The baseline still holds raw working bytes and on-disk modes, as today.
- **Capture validation stops re-capturing.** The before/after source manifest becomes HEAD, the index entries and the stat fingerprint of every captured and untracked path. Each attempt makes one capture instead of three. Files modified within the racy window are re-hashed once to catch same-size rewrites in the same timestamp tick. Validation still detects concurrent changes to HEAD, the index, or any captured or untracked path, and still retries or fails as today.
- **The first snapshot of a managed workspace does not rewrite its baseline.** The snapshot store of a managed checkout can read objects from its source repository. A snapshot skips writing an object only when it is reachable from the workspace's retained baseline. Everything else, including content the delegate stages or commits in its linked worktree, is written to the store itself. Files of 16 MiB or more are still copied into the store so they restore by streaming.
- **Worktree locks and gates stop blocking the async runtime.** Lock-file directory creation and opening, gate canonicalisation and repository resolution move off the Tokio worker threads.

No configuration keys, tool arguments, ACP shapes or stored formats change. Baselines, results and undo behave as before.

## Non-goals

- The delegation planning summary. The embedded `coder-team` profile already turns it off (a separate change). Its 2,000-token cutoff and running it concurrently with allocation are left for a follow-up.
- Non-isolated (shared) delegation: it allocates no workspace and does not show the gap.
- A faster checkout (`git worktree add`), measured at about 0.7 s here. It gets a span, not an optimisation. The design names copy-on-write materialisation as the follow-up if it matters; sparse checkout is not an option.
- Deduplicating snapshot files of 16 MiB or more against the source repository.
- Changing the 2-second racy window of the snapshot stat cache.
- Exporting long-lived parent spans before they end. Traces of a running session will still show children whose parent has not been exported yet.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `isolated-delegation`: adds requirements that baseline capture reads only changed content (but always reads racy, flagged and converted entries), keeps raw working contents, writes no object the repository holds, and validates with stat fingerprints and a racy re-hash instead of re-capturing; and that each startup phase of an isolated delegation is traceable and records its counts.
- `workspace-snapshots`: adds requirements that snapshots of a managed workspace reuse the baseline content its source repository retains, store all other content (including committed workspace content and large files) themselves, and that other snapshot stores stay self-contained.

## Impact

- **Packages:**
  - `querymt-agent-services`:
    - `worktree/capture.rs`, `worktree/repo_ops.rs`, `worktree/service.rs`, `worktree/allocate.rs`, `worktree/lock.rs`, `worktree/gate.rs`;
    - `snapshot/snapshot_backend.rs` and `snapshot/git` (linked object store, `ObjectSink`).
  - `querymt-agent-runtime`: `delegation/core.rs` spans, and the `link_object_source` call where managed session runtimes are created (`agent/session_materializer.rs` and the isolated creation paths).
  - `querymt-agent`: facade tests only.
- **Features:** no feature-gated code changes. The `remote` admission path in `execute_delegation_unboxed` only gets a span.
- **Public Rust API (additive, non-breaking):**
  - new `pub struct CaptureStats`;
  - new provided method `RepositoryOps::capture_working_state_with_stats`, which defaults to calling `capture_working_state` with zero stats, so existing implementors compile unchanged;
  - new provided method `SnapshotBackend::link_object_source(worktree, objects_dir, baseline_commit)`, a no-op by default.

  `CapturedWorkingState` (re-exported through `querymt_agent::worktree`) gains no fields, and no existing signature changes.
- **Config / hooks / middleware / ACP / Extism / typeshare / Python:** unchanged.
- **Session DB:** unchanged.
- **On-disk:** the snapshot store of a managed workspace gains an `objects/info/alternates` entry pointing at its source repository's object directory, and a `qmt-baseline` file naming the baseline commit. It stays a plain Git object store that `git` can read.
- **querymt-desktop:** no follow-up.

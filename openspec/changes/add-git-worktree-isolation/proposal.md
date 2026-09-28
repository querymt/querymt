# Proposal

## Why

Concurrent coding sessions and parallel delegates currently share writable checkouts, so edits, verification, and undo can interfere with one another. Git worktree isolation is needed independently of the unmerged WorkPacket feature, including delegates that must see their parent's uncommitted work.

## What Changes

- Add durable, session-bound Git workspaces for top-level sessions and parallel delegates, with explicit local versus isolated execution policy and no dependency on structured plans or WorkPackets.
- Create each isolated writer in its own linked worktree and managed branch; persist repository identity separately from execution cwd and restore that binding on load/resume.
- Capture tracked working-tree edits and non-ignored untracked files into a stable private baseline for delegates, without committing, stashing, or modifying the parent's checkout/index. The baseline reflects working-file contents, not the parent's staging partition.
- Reuse the existing gix-backed `SnapshotBackend` and undo/redo flows inside each workspace. Keep undo snapshots separate from real repository branches and baseline/result commits.
- Route delegate tools, verification, hooks, and supported workspace integrations to the effective workspace. Report retained results with baseline-relative changes rather than silently merging into the parent.
- Provide explicit creation, inspection, result review, and safe removal operations through agent APIs and a small worktree command surface. Preserve dirty, unmerged, and interrupted work.
- Preserve existing local execution by default; isolated sessions default their delegates to isolation. Callers may explicitly request isolated delegation from local sessions as well. Requested isolation fails closed rather than falling back to shared execution.

## Capabilities

### New Capabilities

- `git-worktree-workspaces`: Managed checkout lifecycle, durable session binding, workspace-aware execution and project identity.
- `isolated-delegation`: Private baselines including parent edits, parallel child isolation, workspace-correct verification and retained results.
- `worktree-undo-compatibility`: Reuse of existing snapshot/undo behavior with workspace-scoped provenance and no cross-checkout restoration.

### Modified Capabilities

None. Existing published specs do not cover workspace isolation or undo. Agent skill discovery remains unchanged and runs against the effective session workspace.

## Impact

- `crates/agent`: new worktree orchestration and durable registry, session storage/schema and materialization, session/fork configuration, delegation lifecycle and results, tool context, hooks/verification, session listing, agent APIs and command dispatch.
- Reuse the existing `gix` dependency and snapshot backend. Use an injectable Git CLI runner for linked-worktree and private-index operations where that avoids reimplementing Git behavior.
- SQLite migrations add workspace associations and baseline/result provenance. Existing sessions remain local and existing undo histories remain valid in their original checkout.
- ACP/editor/MCP integrations must expose or validate effective workspace context; unsupported remote filesystem isolation returns a clear error in this first version.
- No WorkPacket dependency, live cwd switching, automatic result integration, generalized filesystem sandbox, remote repository transfer, ignored-file copying, or automatic cleanup sweep in this change.

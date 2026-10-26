# Proposal

## Why

Concurrent coding sessions and parallel delegates currently share writable checkouts, so edits, verification, and undo can interfere with one another. Git worktree isolation is needed independently of the unmerged WorkPacket feature, including delegates that must see their parent's uncommitted work.

## What Changes

- Add durable, session-bound Git workspaces for top-level sessions and parallel delegates, with explicit local versus isolated execution policy and no dependency on structured plans or WorkPackets.
- Create each isolated writer in its own linked worktree and managed branch; persist repository identity separately from execution cwd and restore that binding on load/resume.
- Capture tracked working-tree edits and non-ignored untracked files into a stable private baseline for delegates, without committing, stashing, or modifying the parent's checkout/index. The baseline reflects working-file contents, not the parent's staging partition.
- Reuse the existing gix-backed `SnapshotBackend` and undo/redo flows inside each workspace. Keep undo snapshots separate from real repository branches and baseline/result commits.
- Route delegate tools, verification, hooks, and supported workspace integrations to the effective workspace. Report retained results with baseline-relative changes rather than silently merging into the parent.
- Add explicit, parent-requested acceptance (`accept_delegation_result`) that integrates exactly a reviewed delegate's baseline-relative delta into the parent checkout's current branch as one commit, running the repository's commit hooks natively. Completion never integrates.
- Provide explicit creation, inspection, result review, and safe removal operations through agent APIs and a small worktree command surface. Preserve dirty, unmerged, and interrupted work.
- Preserve existing local execution by default; isolated sessions default their delegates to isolation. Callers may explicitly request isolated delegation from local sessions as well. Requested isolation fails closed rather than falling back to shared execution.
- Keep top-level isolation an explicit per-session request on every surface (ACP `session/new` metadata, native `/worktree create`, simple API, Python). Configuration enables the capability and tunes how requested worktrees are built; it never turns an option-less session into an isolated one. This matches comparable tools (Claude Code `--worktree`, Codex's per-chat Worktree choice), neither of which has a global "always isolate" default.
- Add opt-in configuration surfaces: `[agent.workspace]` / `[quorum.workspace]` TOML sections (`enabled`, `storage_root`, optional default `baseline`), matching `AgentBuilder`/`QuorumBuilder` options, and Python keyword arguments. These construct one shared workspace service per agent or quorum without custom Rust wiring.
- Add a per-delegate `isolation` setting in quorum configuration (`inherit` | `isolated` | `shared`), with precedence explicit tool-call value, then delegate configuration, then inheritance from the parent.
- Expose create/list/status/result/publish/archive/remove/recover through the simple API and Python, backed by one typed operations core shared with the ACP extensions and native `/worktree` so no surface duplicates semantics. Errors carry stable machine-readable codes on every surface, and agents advertise workspace support so clients can offer an explicit Local/Worktree choice.

- List isolated sessions under the cwd they were started from and nest them under their source session (fork origin `worktree`), disclosing their workspace on list and load, instead of grouping by repository identity.
- Add an explicit ownership handoff that starts a fresh conversation in an existing managed checkout (clean by default, optionally seeded with a summary); the previous owner stays loadable read-only.

## Capabilities

### New Capabilities

- `git-worktree-workspaces`: Managed checkout lifecycle, durable session binding, workspace-aware execution and project identity.
- `isolated-delegation`: Private baselines including parent edits, parallel child isolation, workspace-correct verification, retained results, and explicit result acceptance.
- `worktree-undo-compatibility`: Reuse of existing snapshot/undo behavior with workspace-scoped provenance and no cross-checkout restoration.

### Modified Capabilities

None. Existing published specs do not cover workspace isolation or undo. Agent skill discovery remains unchanged and runs against the effective session workspace.

## Impact

- `crates/agent`: new worktree orchestration and durable registry, session storage/schema and materialization, session/fork configuration, delegation lifecycle and results, tool context, hooks/verification, session listing, agent APIs and command dispatch.
- `crates/agent/src/config` (workspace settings, delegate isolation, JSON schema), `crates/agent/src/api` (builders, session options, worktree operations facade), `crates/py/querymt-agent-py` (keyword arguments, `agent.worktrees`, typed errors, stubs, tests, CI), example configurations, and agent/Python documentation.
- Upgrade `gix` from 0.80 to 0.90 and use typed, injectable native repository operations for worktree administration, capture, refs, and result review. Production isolation operations, including result acceptance, must not require an installed Git executable; repository hooks and configured merge/filter drivers run as configured external processes. Preserve the existing native snapshot backend; Git CLI usage in test fixtures is not a production dependency.
- Correct review gaps in authoritative fork cwd routing, fail-closed fork creation, conditional branch deletion, newly staged ignored-file capture, effective repository-layout validation, workspace-scoped slash execution, replayable provisioning, alternate Git-directory layouts, and baseline-relative result inspection for both top-level workspaces and delegates.
- SQLite migrations add workspace associations and baseline/result provenance. Existing sessions remain local and existing undo histories remain valid in their original checkout.
- ACP/editor/MCP integrations must expose or validate effective workspace context; unsupported remote filesystem isolation returns a clear error in this first version.
- No WorkPacket dependency, live cwd switching, unrequested result integration, raw merges of private delegate branches, generalized filesystem sandbox, remote repository transfer, ignored-file copying, automatic cleanup sweep, or configured default execution mode in this change.

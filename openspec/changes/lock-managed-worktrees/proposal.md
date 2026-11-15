# Proposal

## Why

`add-git-worktree-isolation` shipped managed worktrees but deferred three things that were found while upgrading to gix 0.90 (see its archived design, "As-built: gix 0.90 upgrade"):

- Managed worktrees are not locked, so `git worktree prune` or `git worktree remove` run outside QueryMT can delete them.
- Worktree creation is a single step, so a failure part-way needs rollback rather than a validated prepare phase followed by a commit phase.
- Creation is refused for sparse-checkout sources, because gix 0.90 copies the source's sparse state into every child. Sparse monorepos therefore cannot use isolation.

## What Changes

- Lock each managed worktree when it is created (`Proxy::set_lock`), with a reason that identifies QueryMT and the owning workspace.
- Removal becomes unlock, then `Force::Never` removal, then relock if removal is refused, so dirty or unmerged work is never discarded by an override-lock removal.
- Prune and recovery understand QueryMT locks: they never prune a locked managed worktree, and recovery can relock or unlock one deliberately.
- Split worktree creation into a two-phase `prepare_add_worktree` (validate and reserve) and commit, so refusals happen before anything is written on disk.
- Support sparse-checkout sources: replace the up-front refusal with an explicit, tested behaviour for children of sparse repositories (sparse child or full child, to be decided in design), with fail-closed errors where unsupported.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `git-worktree-workspaces`: lifecycle and recovery requirements (`Conservative lifecycle and recovery`, `Supported repository administration layouts`) gain locking, two-phase creation and sparse-source behaviour.

## Impact

- `crates/agent/src/worktree` (`repo_ops.rs`, manager, recovery), `docs/docs/agent/worktrees.md`, and a `CHANGELOG.md` Unreleased entry.
- Existing managed worktrees created before this change are unlocked; recovery or the first operation on them should lock them (design to confirm).
- No new configuration keys, no SQLite migration expected, and no ACP or typeshare changes expected; confirm in design.

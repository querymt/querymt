# Proposal

## Why

A top-level worktree can publish its work as an immutable result, but nothing
brings that result back to the checkout it was created from. Users merge by
hand, and the obvious `git merge qmt/workspace/<id>` is wrong whenever the
worktree was created from a dirty checkout: the private baseline commit
carries the source's uncommitted files (in one real session, 54 untracked
files and 13k lines alongside 12 changed files), so a branch merge commits
them too. The correct manual procedure (`git cherry-pick --no-commit <result>`
then `git commit`) is not discoverable. Delegate acceptance already
implements the safe operation: a three-way merge of exactly the
baseline-relative delta, refusing to overwrite local edits, running the
repository's commit hooks natively, crash-safe and replay-safe. Worktrees
should get the same operation.

## What Changes

- New `/worktree merge [id] [--commit <rev>] [--archive|--remove]` native
  command, `querymt/worktree/merge` ACP extension method,
  `AgentWorktrees::merge` (Rust) and `agent.worktrees.merge(...)` (Python).
- By default merge first publishes the worktree's current working state as a
  retained result (the same capture as `/worktree publish`: commits, staged,
  unstaged and non-ignored untracked files) and merges exactly that result.
  `--commit <rev>` merges a chosen version instead and takes no snapshot: a
  retained result, or a commit on the worktree branch after its baseline,
  which is then retained like a published result.
- The target is the source checkout: the effective checkout of the session
  the worktree was created from, else the worktree's recorded source cwd. The
  result lands as one commit on that checkout's current branch, containing
  only the baseline-relative delta, through the delegate acceptance pipeline
  (local-state refusal, conflicts reported without changes, native
  pre-commit/prepare-commit-msg/commit-msg hooks with no bypass, compare-and-
  swap branch update, create-only integration record, crash recovery). The
  merge is a squash: the worktree branch's own commits are not kept as
  history.
- Snapshotting refuses while the worktree's owner is busy (`owner_active`);
  merging refuses while the source session is busy (`source_active`).
- `--archive` / `--remove` run the existing archive or archive-and-remove
  operation once the source contains the result (merged, already merged, or
  no changes). A follow-up failure is reported next to the merge outcome and
  never undoes or hides the merge.
- Merge conflicts and refusals carry stable codes and, where relevant, the
  offending paths on every surface (`merge_conflict`, `touches_local_changes`,
  `baseline_local_edits`, `commit_rejected`, `source_missing`, ...).
- The `/worktree merge` reply is durable and replayed on load, like the
  other mutating native commands.
- Workspace capability metadata adds `merge: true`.
- The delegate acceptance pipeline is split into a shared planning step and a
  shared transaction; `accept_delegation_result` behaviour is unchanged.

## Non-goals

- No two-parent merge commit and no preservation of the worktree branch's
  individual commits.
- No merge into an arbitrary checkout or branch other than the source
  checkout's current branch; no merge of removed workspaces.
- No commit signing (unchanged from acceptance: `commit.gpgSign` is refused).
- No automatic merge on session completion, and no model-facing tool for
  top-level worktrees (delegates keep `accept_delegation_result`).
- No UI work; the client follow-up is named below.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `git-worktree-workspaces`: adds merging a worktree into its source checkout
  (snapshot or chosen version, delta-only commit with hooks, refusals,
  follow-up archive/remove, durable reply); merge joins the shared operation
  core on all surfaces; capability metadata advertises merge.

## Impact

- Packages:
  - `querymt-agent-services`: `worktree::integrate` (shared
    `IntegrationPlan`/`integrate`, `accept_delegation_result` on top),
    `worktree::service` (`merge_workspace`, retaining a chosen commit as a
    result), `worktree::ops` (`WorktreeOps::merge`, `MergeOptions`,
    `WorktreeMergeResponse`, refusal error variant with paths,
    `OwnerHost::ensure_idle`), `worktree::service::capability_meta_for`.
  - `querymt-agent-runtime`: native command parsing, durable reply and ACP
    dispatch (`agent::handle::ext_worktree_ops`, ACP extension routing),
    `OwnerHost` implementation on the agent handle. Features: none specific.
  - `querymt-agent`: `api::worktrees::AgentWorktrees::merge`, facade tests.
  - `querymt-agent-py`: `Worktrees.merge` binding, `.pyi` stub, README.
- Public Rust API (additive): `WorktreeOps::merge`, `AgentWorktrees::merge`,
  `MergeOptions`, `WorktreeMergeResponse`, `WorkspaceService::merge_workspace`.
  `WorktreeOpError` gains a variant and `OwnerHost` gains a required method
  (**BREAKING** for out-of-tree `OwnerHost` implementors; in-tree only today).
- ACP wire shapes (**BREAKING**, additive): new extension method
  `querymt/worktree/merge`; native `/worktree merge` subcommand and usage
  text; `worktree_command_completed` gains `command = "merge"`; capability
  metadata gains `merge`. Error data gains `paths` for merge refusals.
- Typeshare / journal (**BREAKING**, additive): the `worktree_archived` event
  payload gains an optional `merged` field (commit and branch) so a
  self-archiving merge is replayed with its outcome
  (`querymt-agent-protocol-types`, re-exported by `querymt-agent-core`).
- Python bindings: new `Worktrees.merge(...)` method (additive).
- Config, hooks, middleware, Extism contract, session DB schema: unchanged. Git refs: merge reuses `refs/qmt/results/...` and
  `refs/qmt/integrations/<workspace>/<result>`.
- Depends on `allow-removal-of-published-worktrees` (durable replies to
  mutating native commands, result commit message), which must land first.
- querymt-desktop / querymt-gpui follow-up: offer "Merge into source" for
  worktree sessions when `merge` is advertised (snapshot or pick a published
  result, optional archive/remove), and render merge refusals with their
  paths.

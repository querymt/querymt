# Proposal

## Why

`/worktree publish` captures a checkout's uncommitted work into a retained
result ref, but `/worktree remove` still refuses any checkout with uncommitted
files, so a published workspace can never be removed without first committing
or discarding the very edits that were just preserved. The failure is also hard
to see: the refusal reply is transient and unidentified, the only durable
message after `remove --archive` tells the user to run `/worktree remove`
again, and cancelling the now read-only session returns an internal error.
A real session hit all of this at once (archived, 12 files "unpreserved" that
were byte-identical to the published result, no visible reason).

## What Changes

- Removal accepts a dirty checkout when its captured working state is exactly
  the tree of one of the workspace's retained published results. The checkout
  is then removed (discarding the preserved files), and the response names the
  result ref that preserves them (`preserved_by`). Ignored files and working
  state that differs from every published result still block removal.
- The `unpreserved_content` refusal names publication as a way forward, and
  when a published result exists but no longer matches the files, says so and
  names that result.
- **BREAKING** (ACP session updates): replies to mutating native `/worktree`
  commands (`publish`, `archive`, `remove`, `recover`, `handoff`) become durable,
  identified session updates that are replayed on load: a new
  `worktree_command_completed` event carrying the command, its outcome code and
  text, rendered with a stable `messageId` and
  `_meta.querymt.worktreeCommand`. `list`, `status` and `result` replies stay
  transient. This replaces the "output is not replayed" rule.
- **BREAKING** (ACP session updates): `remove --archive` records the owner's
  archived update after the removal attempt, and the update states the removal
  outcome (removed, or refused with code and reason). Its text no longer tells
  the user to run `/worktree remove` when removal was just attempted.
- `session/cancel` on a read-only session (archived workspace or transferred
  owner) succeeds without effect instead of returning an internal error.
- Published result commits get their own commit message instead of reusing
  "QueryMT private baseline".

## Non-goals

- No force-removal option; removal still never discards content that no
  retained ref preserves.
- No backfill of an archived update lost to a crash between archive and
  announcement (the existing "logged, archive stays committed" rule applies).
- No compatibility with older journal readers for the new event kind.
- No UI work; the required client follow-up is named below.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `git-worktree-workspaces`: conservative removal accepts working state
  preserved by a published result; refusal messages name publication and stale
  results; native command output of mutating commands is durable and replayed;
  the archive-and-remove update reports the removal outcome; read-only sessions
  accept `session/cancel` as a no-op.

## Impact

- Packages:
  - `querymt-agent-services`: `worktree::lifecycle` (removal checks),
    `worktree::repo_ops` (`RepositoryOps::commit_tree`, discard option on
    `remove_worktree`, result commit message), `worktree::capture`,
    `worktree::error` (`UnpreservedContent` gains `stale_result`),
    `worktree::ops` (`WorktreeRemoveResponse.preserved_by`, deferred archive
    announcement, `OwnerHost::announce_archived` gains the removal outcome).
  - `querymt-agent-protocol-types` (re-exported by `querymt-agent-core`): new
    `AgentEventKind::WorktreeCommandCompleted`, `WorktreeArchived` gains an
    optional removal outcome, replay projection of both.
  - `querymt-agent-runtime`: native `/worktree` reply emission
    (`agent::handle::ext_worktree_ops`), `session/cancel` for read-only
    sessions (`agent::handle::send_agent_impl`). Features: none specific; the
    cancel change touches the `remote` and non-`remote` paths.
  - `querymt-agent` (facade tests), `querymt-agent-py` (remove response gains
    an additive field; no binding signature change).
- Public Rust API: additive fields on `WorktreeRemoveResponse` and
  `RemovalReport`; `WorktreeError::UnpreservedContent` gains a field;
  `RepositoryOps` (crate trait) gains a method and a `remove_worktree`
  parameter; `OwnerHost::announce_archived` signature changes.
- Wire shapes: new durable event kind `worktree_command_completed` in the
  journal and typeshare `AgentEventKind`; `worktree_archived` payload gains an
  optional field; `session/update` replay now includes these messages.
  Stable error codes are unchanged. No session DB schema change (events are
  stored as JSON).
- Config, hooks, middleware, Extism contract: unchanged.
- querymt-desktop / querymt-gpui follow-up: render
  `_meta.querymt.worktreeCommand` replies (error styling by `code`), and switch
  the session to read-only live when `worktreeArchived` or `worktreeHandoff`
  metadata arrives, instead of only on load.

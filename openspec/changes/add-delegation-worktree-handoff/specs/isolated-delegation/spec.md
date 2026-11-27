# Spec Delta

## ADDED Requirements

### Requirement: Delegation by worktree handoff
A delegating session SHALL be able to run a new delegate in the isolated worktree of one of its earlier delegations by naming that delegation as `handoff_from` on the `delegate` call. Ownership SHALL move to the new delegate session before it runs; no worktree SHALL be allocated, and files, branch, index, baseline and retained results SHALL be unchanged. The previous owner SHALL become read-only.

#### Scenario: Reviewer takes over a coder's worktree
- **WHEN** session P delegated to `coder` as delegation D1 in isolated worktree W, D1 completed with edits, and P delegates to `reviewer` with `handoff_from = D1`
- **THEN** the reviewer's session owns W and runs in W's execution cwd with the coder's edits present
- **AND** no new worktree is created and W's branch, baseline and earlier results are unchanged
- **AND** loading the coder's session discloses read-only access with reason `owner_transferred` and the reviewer's session as current owner

#### Scenario: Handoff from a failed or cancelled delegation
- **WHEN** the delegation named by `handoff_from` failed or was cancelled after editing files in its retained worktree
- **THEN** the new delegate takes ownership of that worktree and sees those edits

#### Scenario: Second round after a review
- **WHEN** a delegate that ran by handoff in W has completed and P delegates again with `handoff_from = D1`
- **THEN** the new delegate takes ownership of W from the previous delegate

#### Scenario: Handoff is disclosed to the caller and hooks
- **WHEN** P calls `delegate` with `handoff_from`
- **THEN** the `delegation_requested` event and the `pre_delegation` hook input carry `handoff_from`
- **AND** the tool result discloses that the delegate runs in the existing worktree of that delegation rather than a new or shared checkout

### Requirement: Refused delegation handoff changes nothing
A delegation with `handoff_from` SHALL fail before its delegate runs, with an error naming the reason, when the named delegation was not made by the delegating session, has no isolated worktree, the worktree is not ready, its owner is busy or not hosted locally, or the call also requests `isolation = "shared"`. Ownership, files and the previous owner's access SHALL be unchanged and no worktree SHALL be allocated.

#### Scenario: Handoff from another session's delegation
- **WHEN** session Q calls `delegate` with `handoff_from` naming a delegation made by session P
- **THEN** the delegation fails with `not_parent_session` and P's worktree keeps its owner

#### Scenario: Handoff from a shared delegation
- **WHEN** `handoff_from` names a delegation that ran in the shared checkout
- **THEN** the delegation fails with `no_isolated_workspace`

#### Scenario: Owner still running
- **WHEN** `handoff_from` names a delegation whose worktree owner has a running turn or queued input
- **THEN** the delegation fails with `owner_active` and the running owner is not interrupted

#### Scenario: Shared isolation with handoff
- **WHEN** a `delegate` call sets both `handoff_from` and `isolation = "shared"`
- **THEN** the call is rejected as an invalid request and nothing is queued

#### Scenario: Unknown delegation
- **WHEN** `handoff_from` names a delegation id that does not exist
- **THEN** the delegation fails with an error naming the id and nothing is allocated

### Requirement: Handoff results are accepted under the worktree's delegation
A delegate that ran by handoff SHALL publish its result on the worktree when it completes, fails or is cancelled, as any isolated delegate does. Its completion output SHALL name the delegation that created the worktree as the id to pass to `accept_delegation_result`. Accepting that id with the new result commit SHALL integrate the whole baseline-relative delta, including earlier owners' edits.

#### Scenario: Accept coder work plus reviewer fixes
- **WHEN** the coder of D1 changed file A, the reviewer that took over by handoff fixed file B and completed with result R2, and P calls `accept_delegation_result` with D1 and R2
- **THEN** one commit changing A and B is created on P's branch

#### Scenario: Completion names the worktree's delegation
- **WHEN** a delegate that ran by handoff from D1 completes with changes
- **THEN** its completion output tells the delegating session to call `accept_delegation_result` with delegation id D1 and the new result commit

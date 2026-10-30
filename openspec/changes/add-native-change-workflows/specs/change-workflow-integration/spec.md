# Spec Delta

## Purpose

Defines how each change gets its own workspace, how progress is checkpointed, and how finished changes are archived and integrated into the base checkout one at a time through a local merge queue.

## ADDED Requirements

### Requirement: One managed workspace per change
Each change SHALL get its own managed worktree, created from the base branch with the `current_working_tree` baseline so untracked project files such as agent skills are present. All of the change's sessions SHALL use this workspace, with ownership passed between chunk sessions by handoff.

#### Scenario: Skills available in the workspace
- **WHEN** a change workspace is created from a checkout whose `.agents/skills` is untracked but not ignored
- **THEN** the skills are present in the change workspace

#### Scenario: Next chunk takes ownership
- **WHEN** a chunk ends and the next chunk starts
- **THEN** the workspace is handed off to the new session without a conversation summary

### Requirement: Clean base preflight
A run SHALL refuse to create change workspaces while the base checkout has uncommitted edits to tracked files, so these edits cannot enter any change's baseline.

#### Scenario: Dirty tracked file
- **WHEN** the base checkout has a modified tracked file at preflight
- **THEN** the run is refused and the modified paths are listed

### Requirement: Checkpoint after each passing chunk
After each chunk whose gate passes, the run SHALL publish the workspace state as an immutable result checkpoint and record it. The checkout's index and files SHALL be left unchanged.

#### Scenario: Checkpoint recorded
- **WHEN** a chunk's gate passes
- **THEN** a new result checkpoint exists for the workspace and the journal records its commit

### Requirement: Merge forecast
After each integration into the base, the run SHALL test every active change's latest checkpoint against the new base with an in-memory three-way merge that writes nothing to the repository or any checkout.

#### Scenario: Clean forecast
- **WHEN** a forecast merges without conflicts
- **THEN** the change is marked as able to take the new base at its next chunk boundary

#### Scenario: Conflicted forecast
- **WHEN** a forecast reports conflicts
- **THEN** the conflicting paths are recorded and one bounded conflict-resolution chunk is scheduled for that change

### Requirement: Archive before integration
After fixes, the run SHALL archive the change with `openspec archive <change> --yes` in the change workspace and validate the resulting specs strictly. If archive fails, one bounded spec-sync agent step MAY run before a retry.

#### Scenario: Archive succeeds
- **WHEN** archive and strict spec validation both succeed
- **THEN** the change is queued for integration with a final checkpoint that includes the archive

#### Scenario: Archive keeps failing
- **WHEN** archive still fails after the spec-sync step
- **THEN** the change is blocked with the archive error

### Requirement: Serialized local merge queue
Changes SHALL integrate into the base checkout one at a time. For each change, the run SHALL merge its final checkpoint with the current base, run the full gate on the merged result in a queue workspace, and integrate only if base HEAD has not moved since the merge was computed.

#### Scenario: Base moved during gating
- **WHEN** base HEAD changes while the queue's gate is running
- **THEN** the integration is not applied, and the change is merged and gated again

#### Scenario: Full gate fails on the merged result
- **WHEN** the full gate fails on the merged result
- **THEN** the change is returned to a bounded fix chunk, and the base is unchanged

### Requirement: Baseline-relative integration
Integration SHALL apply only the change's baseline-to-checkpoint delta as one commit on the base branch. It SHALL refuse when that delta touches paths with local work in the base checkout, reusing the refusal codes of delegated-result integration.

#### Scenario: Local work collides
- **WHEN** the delta touches a path with uncommitted edits in the base checkout
- **THEN** integration is refused with `touches_local_changes` and the colliding paths, and nothing is written

#### Scenario: Successful integration
- **WHEN** the merged result passed the gate and base HEAD is unchanged
- **THEN** one commit is added to the base branch, dependent changes become ready, and merge forecasts run

### Requirement: Workspaces are retained
The run SHALL NOT remove a change workspace automatically. After integration the workspace SHALL be archived, and removal is left to the user.

#### Scenario: After integration
- **WHEN** a change has integrated
- **THEN** its workspace is archived and remains inspectable

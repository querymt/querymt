# Spec Delta

## Purpose

Defines how the flow driver gives each change its own workspace, checkpoints progress, forecasts merges, archives the change and integrates finished changes one at a time through a local merge queue.

## ADDED Requirements

### Requirement: One workspace per change
Each change SHALL get its own workspace. With the QueryMT backend it SHALL be created with `querymt/worktree/create` from the base using the `current_working_tree` baseline, and chunk sessions SHALL pass ownership with `querymt/worktree/handoff` without a summary. With the plain backend the driver SHALL create a git worktree on branch `flow/<change>` and copy configured untracked paths into it.

#### Scenario: QueryMT workspace includes untracked skills
- **WHEN** the base checkout has untracked, non-ignored `.agents/skills`
- **THEN** the change workspace contains them

#### Scenario: Plain backend copies configured paths
- **WHEN** the workflow lists `.agents/` under `copy_untracked` and the plain backend is used
- **THEN** the driver-managed worktree contains `.agents/`

### Requirement: Clean base preflight
The driver SHALL refuse to create workspaces while the base checkout has uncommitted edits to tracked files.

#### Scenario: Dirty tracked file
- **WHEN** a tracked file is modified in the base checkout
- **THEN** the run is refused listing the paths

### Requirement: Checkpoint after each passing chunk
After each chunk whose gate passes, the driver SHALL record an immutable checkpoint commit of the workspace: with QueryMT via `querymt/worktree/publish`, otherwise as a commit on `flow/<change>` created by the driver.

#### Scenario: Checkpoint recorded
- **WHEN** a chunk's gate passes
- **THEN** the journal records the checkpoint commit and baseline commit

### Requirement: Merge forecast
After each integration, the driver SHALL test every active change's latest checkpoint delta against the new base with an in-memory three-way merge that writes nothing to the repository or any checkout.

#### Scenario: Conflicted forecast
- **WHEN** a forecast reports conflicts
- **THEN** the paths are recorded and one bounded conflict-resolution chunk is scheduled

### Requirement: Archive before integration
The driver SHALL run `openspec archive <change> --yes` in the change workspace and `openspec validate --specs --strict`. On failure it MAY run one bounded spec-sync agent step before retrying, then block.

#### Scenario: Archive fails twice
- **WHEN** archive fails after the spec-sync step
- **THEN** the change is blocked with the archive error

### Requirement: Serialized local merge queue
Changes SHALL integrate one at a time. For each, the driver SHALL apply the final checkpoint's baseline-relative delta onto the current base in a driver-owned queue worktree, run the full gate there, and advance the base branch only if it has not moved since; otherwise it SHALL redo the merge and gate.

#### Scenario: Base moved
- **WHEN** the base branch moves while the queue gate runs
- **THEN** the base is not updated and the change is merged and gated again

#### Scenario: Gate fails on merged result
- **WHEN** the full gate fails in the queue worktree
- **THEN** the base is unchanged and a bounded fix chunk is scheduled

### Requirement: Baseline-relative integration commit
Integration SHALL add exactly one commit to the base branch containing only the change's baseline-to-checkpoint delta, so inherited untracked baseline files are never committed unless the change modified them. If the base branch is checked out in the base checkout, the driver SHALL update only the paths the commit touches and SHALL refuse when any of those paths has local state.

#### Scenario: Local work collides
- **WHEN** the commit touches a path with uncommitted state in the base checkout
- **THEN** integration is refused with `touches_local_changes`, listing the paths, and nothing is written

#### Scenario: Inherited skills not committed
- **WHEN** the baseline contained untracked `.agents/skills` that the change did not modify
- **THEN** the integration commit does not add them

### Requirement: Workspaces are retained
The driver SHALL NOT delete change workspaces automatically. After integration it SHALL archive QueryMT workspaces with `querymt/worktree/archive` and leave driver-managed worktrees in place for `flow clean`.

#### Scenario: After integration
- **WHEN** a change has integrated
- **THEN** its workspace remains inspectable until the user removes it

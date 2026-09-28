# Spec Delta

## Purpose

Preserve existing session undo and redo semantics inside isolated checkouts while preventing snapshots from one workspace from modifying another workspace.

## ADDED Requirements

### Requirement: Existing undo and redo operate inside the bound workspace
Isolated sessions SHALL retain existing configured snapshot, undo, and redo behavior for the paths supported by the snapshot backend. These operations SHALL use the bound session cwd and SHALL NOT move project branch references or restore files in sibling or original checkouts. Isolation SHALL NOT broaden a subdirectory session's undo scope implicitly.

#### Scenario: Undo in one isolated session
- **WHEN** two isolated sessions edit the same supported relative path and one undoes its turn
- **THEN** only that session's checkout is restored
- **AND** sibling and original checkout contents and project branch references are unchanged

#### Scenario: Redo inside an isolated delegate
- **WHEN** an isolated delegate undoes and then redoes a supported file edit
- **THEN** the file is restored within the delegate checkout using the existing redo semantics
- **AND** the parent checkout and immutable delegate baseline remain unchanged

#### Scenario: Subdirectory-scoped undo
- **WHEN** an isolated session operates from a repository subdirectory and undoes a turn
- **THEN** snapshot path resolution retains the configured cwd scope rather than silently restoring unrelated paths outside it

### Requirement: Snapshot history has workspace provenance
New snapshot and revert state SHALL be associated with the workspace in which it was captured. Undo/redo SHALL reject a provenance mismatch. A conversation fork into another workspace SHALL NOT make inherited parent snapshot entries executable against the new checkout. Resuming the same workspace SHALL retain access to its existing compatible snapshot history.

#### Scenario: Resume then undo
- **WHEN** an isolated session is resumed in its persisted workspace after restart
- **THEN** its retained compatible undo and redo history remains usable

#### Scenario: Fork with inherited snapshots
- **WHEN** a forked isolated conversation contains snapshot entries from its source workspace
- **THEN** those entries remain historical context but cannot restore files in the fork's new workspace
- **AND** new turns in the fork acquire their own usable snapshot history

#### Scenario: Parent undo with isolated children
- **WHEN** a parent with isolated delegates undoes its own turn
- **THEN** child snapshots from other workspaces are excluded from parent undo aggregation
- **AND** parent undo can succeed without restoring or changing isolated child files

#### Scenario: Workspace binding mismatch
- **WHEN** an undo or redo request directly selects a snapshot associated with another workspace
- **THEN** it fails before filesystem restoration with an explicit provenance error

### Requirement: Snapshot history and repository results remain separate
Undo history SHALL retain its existing independent lifecycle and SHALL NOT be treated as a complete export of Git working state or as an integration branch. Git baseline/result references SHALL remain available independently of snapshot garbage collection. Repository administrative files SHALL NOT be captured or restored as ordinary session files.

#### Scenario: Snapshot garbage collection
- **WHEN** old undo snapshots are collected
- **THEN** managed worktree baseline and retained result references remain available for review and recovery

#### Scenario: Linked-worktree metadata
- **WHEN** a snapshot is captured or restored at the root of a linked worktree
- **THEN** its Git administrative marker and shared repository metadata remain intact

### Requirement: Legacy behavior remains compatible
Existing local sessions and their compatible snapshot histories SHALL continue to operate without workspace migration. Disabling snapshots SHALL NOT disable worktree creation or isolated delegation, and enabling isolation SHALL NOT silently enable snapshots when they were disabled.

#### Scenario: Legacy local undo
- **WHEN** an existing local session with retained snapshots is resumed without isolation
- **THEN** its undo and redo behavior remains compatible in the original checkout

#### Scenario: Isolation without snapshots
- **WHEN** an isolated session is created with snapshots disabled
- **THEN** worktree isolation and delegate baseline capture remain available
- **AND** undo reports the existing unavailable-backend behavior rather than relying on private Git baselines as substitute undo history

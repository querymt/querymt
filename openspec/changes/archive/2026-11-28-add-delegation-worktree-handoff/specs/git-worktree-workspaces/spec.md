# Spec Delta

## MODIFIED Requirements

### Requirement: Worktree session lineage
A session created from a source session through the worktree surface, or by handoff, SHALL record that session as its parent with fork origin `worktree`, persisted atomically with the session row. Listing SHALL count and nest `worktree` children like user forks and return them for the forks and children scopes. Delegated children SHALL stay hidden. A delegate session that receives a workspace by delegation handoff SHALL record the delegating session as its parent with fork origin `delegation`.

#### Scenario: Native create records lineage
- **WHEN** `/worktree create` is issued from session S
- **THEN** the new session is listed nested under S with fork origin `worktree`

#### Scenario: Explicit source on other surfaces
- **WHEN** creation through the ACP extension, simple API, or Python names a source session
- **THEN** the new session records the same lineage as the native command
- **AND** an empty cwd defaults to the source session's effective cwd, and a cwd outside the source's repository is rejected with `invalid_options`

#### Scenario: Isolated root without a source session
- **WHEN** an isolated session is created through `session/new` metadata without a source session
- **THEN** it is a root session listed in its source cwd's group and marked with its workspace

#### Scenario: Isolated fork stays with its parent
- **WHEN** an isolated conversation fork is listed
- **THEN** it appears in its parent's group, nested under the parent

#### Scenario: Retired workspace does not move
- **WHEN** a session's workspace is archived or removed
- **THEN** the session stays in the same group and its listed workspace lifecycle reflects the retirement

#### Scenario: Delegates remain hidden
- **WHEN** an isolated or shared delegate exists for a listed session
- **THEN** it is not listed as a fork or worktree child

#### Scenario: Delegate handoff successor keeps delegation lineage
- **WHEN** a delegate session receives a workspace by delegation handoff from session P
- **THEN** its parent is P with fork origin `delegation`, and it is not listed as a fork or worktree child of P or of the previous owner

## ADDED Requirements

### Requirement: Delegate session as handoff successor
A delegating session SHALL be able to move ownership of a ready workspace created by one of its delegations to a new delegate session, with the same owner checks, atomic transfer and announcement as an ownership handoff. The delegate session SHALL start without copied history and SHALL run only after it owns the workspace. Files, branch, index and baseline SHALL be unchanged, and a failed start SHALL never leave the workspace failed or without an owner.

#### Scenario: Delegation handoff to a delegate session
- **WHEN** a delegating session runs a delegate with `handoff_from` naming the delegation that created a ready workspace
- **THEN** the delegate's session owns the same checkout and branch, starts without copied history, and runs only after it owns the workspace
- **AND** a durable ownership-transferred event naming the workspace and the delegate session, with no summary seeded, is recorded on the previous owner's stream

#### Scenario: Delegate successor fails to start
- **WHEN** ownership has moved to a delegate session by delegation handoff and the delegate session then fails to start, before or after its actor started
- **THEN** the delegation fails, the workspace stays ready and is not marked failed, and the workspace is owned by either the previous owner or the delegate session, never by no session
- **AND** when it is back with the previous owner, that owner is writable again (not marked as handed off) and the workspace can be handed off again

# Spec Delta

## Purpose

Provide durable isolated Git execution workspaces so concurrent coding sessions can operate on one project without sharing writable checkouts.

## ADDED Requirements

### Requirement: Explicit isolated session creation
The system SHALL support local and isolated-worktree session creation without requiring a structured plan or WorkPacket. Omitted isolation options SHALL preserve existing local execution. Each isolated session SHALL receive a distinct writable checkout and branch before execution begins. Top-level isolated creation SHALL default to the selected commit or current HEAD, and SHALL provide an explicit option to include current working changes.

#### Scenario: Concurrent isolated sessions
- **WHEN** two isolated sessions are created for the same project
- **THEN** their execution directories and branches are distinct
- **AND** editing a file in one does not change the other checkout or the original checkout

#### Scenario: Dirty source with commit baseline
- **WHEN** a top-level isolated session is created from a commit while the source has local changes
- **THEN** the session starts from that commit and reports that local changes are excluded
- **AND** the source files, staging state, and branch remain unchanged

#### Scenario: Explicit current-working-tree baseline
- **WHEN** isolated creation requests inclusion of current working changes
- **THEN** the checkout contains tracked working contents and non-ignored untracked files from a validated baseline
- **AND** ignored untracked files are excluded and that exclusion is disclosed

#### Scenario: Existing local behavior
- **WHEN** a caller creates a session without enabling isolation
- **THEN** the existing local cwd behavior remains available without creating a managed worktree

### Requirement: Durable workspace identity and binding
The system SHALL persist workspace ownership, repository/project identity, execution path, baseline, branch, and relative cwd independently of conversation contents. Load and resume SHALL use the persisted isolated workspace binding and SHALL reject conflicting cwd requests or unavailable workspaces rather than silently falling back. A managed workspace SHALL NOT be attached to a second writable session.

#### Scenario: Resume after restart
- **WHEN** an isolated session is resumed after process restart
- **THEN** it uses the same checkout, relative cwd, and baseline association

#### Scenario: Missing checkout or conflicting request
- **WHEN** the persisted checkout is missing or the caller supplies a different cwd for an isolated session
- **THEN** execution is rejected with a workspace-specific diagnostic
- **AND** no tools execute in the original checkout as a fallback

#### Scenario: Subdirectory session
- **WHEN** a session is isolated from a repository subdirectory
- **THEN** its execution cwd is the corresponding subdirectory in the new checkout
- **AND** creation fails explicitly if that directory is absent from the selected baseline

#### Scenario: Second writable owner
- **WHEN** another session attempts to attach to an owned managed checkout
- **THEN** the system rejects the attachment or creates a distinct workspace through an explicit creation request

### Requirement: Workspace-consistent execution
The system SHALL initialize filesystem tools, indexes, workspace-dependent configuration, and execution-related hooks using the effective session workspace. It SHALL disclose the execution path and SHALL NOT silently use an editor or external tool integration bound to another checkout. Isolation SHALL NOT be represented as filesystem sandboxing.

#### Scenario: Tools and indexes use the isolated checkout
- **WHEN** an isolated session reads, edits, searches, or executes a command with default cwd
- **THEN** the operation and workspace index use its isolated workspace rather than the original checkout

#### Scenario: Fixed-root integration mismatch
- **WHEN** an editor bridge or workspace-dependent external tool cannot operate against the isolated root
- **THEN** that integration is disabled or rejected with an explicit diagnostic rather than silently serving the original checkout

### Requirement: Independent workspace for conversation forks
Forking an isolated session SHALL create a new workspace from the source's current working contents before the fork executes. The system SHALL identify that file baseline separately from the historical conversation fork point and SHALL NOT switch an existing active session's workspace in place.

#### Scenario: Fork an older message
- **WHEN** a user forks an isolated conversation at an earlier message
- **THEN** the new session has a distinct checkout based on current source working contents
- **AND** the response explains that selecting a historical message did not rewind filesystem state

### Requirement: Workspace inspection and project grouping
The system SHALL provide creation, listing, status, result inspection, and removal through agent operations and a worktree command surface. Creating through that surface SHALL return a new session identity without moving the current session. Workspace views SHALL distinguish project identity from execution path and SHALL expose ownership, branch, baseline mode, live change status, and lifecycle errors.

#### Scenario: Project has several managed checkouts
- **WHEN** the user lists sessions and workspaces for a project
- **THEN** related isolated sessions can be grouped under the same repository identity while retaining their distinct execution paths

#### Scenario: Create from an existing conversation
- **WHEN** a user requests worktree creation through the current conversation's command surface
- **THEN** the result identifies the new isolated session and path
- **AND** the current conversation keeps its existing workspace

### Requirement: Conservative lifecycle and recovery
Managed workspaces SHALL use durable storage, survive session completion/cancellation, and support reconciliation after interrupted provisioning or removal. Removal SHALL reject active ownership, uncommitted files (including untracked and ignored files), or committed results lacking a retained recovery reference or integration proof. The system SHALL NOT delete externally owned worktrees or unknown pre-existing directories. Retained references SHALL be disclosed.

#### Scenario: Dirty or active workspace removal
- **WHEN** removal targets an active workspace or one containing uncommitted, untracked, or ignored files
- **THEN** removal is refused and the blocking state is reported without deleting files

#### Scenario: Clean checkout with unique commits
- **WHEN** a clean inactive checkout has commits not integrated into the target history
- **THEN** removal is refused unless those results have an explicitly retained recovery reference
- **AND** any successful removal reports the preserved reference

#### Scenario: Crash during creation
- **WHEN** the process restarts after worktree allocation but before session startup completed
- **THEN** reconciliation identifies the allocated resources and reports or completes their binding without overwriting user directories or launching duplicate owners

### Requirement: Requested isolation fails closed
When Git, a usable repository/base, supported repository layout, durable storage, or local filesystem execution is unavailable, requested isolation SHALL fail with an actionable error before agent execution. The system SHALL NOT downgrade to shared execution. Concurrent administration SHALL prevent duplicate allocation and ownership across processes.

#### Scenario: Unsupported remote filesystem execution
- **WHEN** isolated execution is requested on a remote filesystem host unsupported by this version
- **THEN** dispatch fails explicitly rather than treating a local worktree path as remote
- **AND** remote model inference with local filesystem execution is not rejected solely because the model is remote

#### Scenario: Concurrent allocation
- **WHEN** two processes request workspace allocation for one repository concurrently
- **THEN** each successful request receives unique resources and durable ownership without corrupting Git administration data

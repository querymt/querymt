# git-worktree-workspaces Specification

## Purpose

Provide durable isolated Git execution workspaces so concurrent coding sessions can operate on one project without sharing writable checkouts.

## Requirements

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

#### Scenario: Enabled service without a request stays local
- **WHEN** the workspace service is enabled by configuration and a session is created through ACP, the simple API, or Python without isolation options
- **THEN** the session executes locally in the requested cwd and no managed worktree is created

### Requirement: Durable workspace identity and binding
The system SHALL persist workspace ownership, repository/project identity, execution path, baseline, branch, relative cwd, and source cwd independently of conversation contents. Load and resume SHALL use the persisted isolated workspace binding and SHALL reject conflicting cwd requests or unavailable workspaces rather than silently falling back. A managed workspace SHALL NOT be attached to a second writable session; its single owner MAY change only through an explicit ownership handoff.

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
- **THEN** the system rejects the attachment, creates a distinct workspace through an explicit creation request, or transfers ownership through an explicit handoff

### Requirement: Workspace source cwd
Each managed workspace SHALL record the cwd of the session or request it was isolated from, normalized like session cwds. When that source is inside a managed checkout, the source cwd SHALL be inherited from that checkout's workspace. The source cwd SHALL be retained after archival and removal.

#### Scenario: Nested isolation keeps the original source
- **WHEN** a workspace is created from a session that already runs in a managed checkout whose source cwd is `/p/app`
- **THEN** the new workspace's source cwd is `/p/app`, not the parent checkout path

#### Scenario: Path aliases match session rows
- **WHEN** a workspace is isolated from a session whose cwd is a symlinked or aliased path
- **THEN** the recorded source cwd equals that session's stored cwd

### Requirement: Workspace-consistent execution
The system SHALL initialize filesystem tools, indexes, workspace-dependent configuration, and execution-related hooks using the effective session workspace. It SHALL disclose the execution path and SHALL NOT silently use an editor or external tool integration bound to another checkout. Isolation SHALL NOT be represented as filesystem sandboxing.

#### Scenario: Tools and indexes use the isolated checkout
- **WHEN** an isolated session reads, edits, searches, or executes a command with default cwd
- **THEN** the operation and workspace index use its isolated workspace rather than the original checkout

#### Scenario: Workspace-scoped slash-command execution
- **WHEN** a slash command has different prompt, include, or script contents in the original and isolated checkouts
- **THEN** the advertised command and its actual expansion/execution use the isolated workspace sources
- **AND** a command present only in the isolated checkout can be invoked without consulting the original checkout's registry

#### Scenario: Fixed-root integration mismatch
- **WHEN** an editor bridge or workspace-dependent external tool cannot operate against the isolated root
- **THEN** that integration is disabled or rejected with an explicit diagnostic rather than silently serving the original checkout

### Requirement: Independent workspace for conversation forks
Forking an isolated session SHALL create a new workspace from the source's current working contents before the fork executes. The system SHALL identify that file baseline separately from the historical conversation fork point and SHALL NOT switch an existing active session's workspace in place.

#### Scenario: Fork an older message
- **WHEN** a user forks an isolated conversation at an earlier message
- **THEN** the new session has a distinct checkout based on current source working contents
- **AND** the response explains that selecting a historical message did not rewind filesystem state

#### Scenario: Fork binding is authoritative for downstream operations
- **WHEN** an isolated fork changes files independently of its source and creates a delegate or another workspace
- **THEN** those operations use the fork's bound execution cwd and current contents rather than the copied source cwd
- **AND** persisted session execution cwd and the workspace association remain consistent after restart

### Requirement: Crash-safe isolated fork creation
An isolated fork SHALL remain non-runnable until its independent workspace is durably bound. Invalid options, allocation/binding failure, or process interruption SHALL NOT leave a conversation that can load, resume, or dispatch as shared local execution. Pending or failed isolation SHALL be distinguishable from a legacy local session.

#### Scenario: Invalid fork request
- **WHEN** isolation options are invalid
- **THEN** the request fails before creating a runnable fork or allocating workspace resources

#### Scenario: Failure after conversation copy
- **WHEN** workspace allocation or owner binding fails after fork history has been persisted
- **THEN** the fork is compensated or retained with explicit non-runnable isolation intent
- **AND** listing and load/resume cannot expose it as a runnable local fork

#### Scenario: Crash before owner binding
- **WHEN** the process restarts after copying fork history but before binding its independent workspace
- **THEN** recovery retains or completes the isolated intent without executing in the source checkout
- **AND** only a fully bound fork becomes runnable

### Requirement: Workspace inspection and project grouping
The system SHALL provide creation, listing, status, result inspection, and removal through agent operations and a worktree command surface. Creating through that surface SHALL return a new session identity without moving the current session. Workspace views SHALL distinguish project identity from execution path and SHALL expose ownership, branch, baseline mode, live change status, and lifecycle errors.

#### Scenario: Project has several managed checkouts
- **WHEN** the user lists sessions for a project with local and isolated sessions started from `/p/app`
- **THEN** all of them appear in the `/p/app` group, isolated sessions are marked with their workspace, and each keeps its distinct execution path

#### Scenario: Create from an existing conversation
- **WHEN** a user requests worktree creation through the current conversation's command surface
- **THEN** the result identifies the new isolated session and path
- **AND** the current conversation keeps its existing workspace

### Requirement: Source-cwd session grouping
Session listing SHALL group a managed session under its workspace's source cwd and an unassociated local session under its cwd. Filtering by `cwd` SHALL use the same key, so browse, search, and per-group paging agree. A session's listed cwd SHALL remain its execution path. Listing SHALL disclose on each managed session its workspace identity, branch, checkout path, source cwd, baseline mode, lifecycle, owner, and handoff target if any.

#### Scenario: Mixed local and isolated sessions
- **WHEN** the user lists sessions with local and isolated sessions started from `/p/app`
- **THEN** all of them appear in the `/p/app` group, isolated sessions carry workspace metadata, and each keeps its execution path

#### Scenario: Search agrees with browse
- **WHEN** a search matches an isolated session
- **THEN** it is returned in the same group as browsing would place it

### Requirement: Worktree session lineage
A session created from a source session through the worktree surface, or by handoff, SHALL record that session as its parent with fork origin `worktree`, persisted atomically with the session row. Listing SHALL count and nest `worktree` children like user forks and return them for the forks and children scopes. Delegated children SHALL stay hidden.

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

### Requirement: Ownership handoff
The owner of a ready managed workspace SHALL be able to hand ownership to a new session in the same checkout on every management surface. The owner SHALL be local and idle, with no shared delegates. Ownership SHALL move atomically, leaving one owner or a recoverable intent. The new session SHALL start without history unless a summary is requested. Files, branch, index, and baseline SHALL be unchanged.

#### Scenario: Clean handoff from an idle owner
- **WHEN** the owner of a ready workspace requests a handoff without a summary
- **THEN** a new session owns the same checkout and branch, has no copied history, and is listed nested under the previous owner
- **AND** the response identifies the new session, previous session, workspace, and execution cwd

#### Scenario: Handoff with a summary
- **WHEN** a handoff explicitly requests a summary
- **THEN** the new session starts with a summary of the previous conversation and the response reports that it was seeded

#### Scenario: Owner still active
- **WHEN** the owner has a running turn, shared delegates, or execution in another process
- **THEN** the handoff is refused with `owner_active` and ownership is unchanged

#### Scenario: Not the owner or not ready
- **WHEN** a handoff targets a session that does not own a ready workspace, or a remote-hosted session
- **THEN** it is refused with a stable code and nothing is created

#### Scenario: Crash during handoff
- **WHEN** the process stops after the new session row is written but before ownership is transferred
- **THEN** recovery leaves the previous owner as the single owner or completes the transfer, and the new session never runs without ownership

#### Scenario: Native command reply
- **WHEN** `/worktree handoff` is issued from the owner session
- **THEN** the requesting client receives the handoff result in the prompt response `_meta["querymt.worktree"]`
- **AND** every client attached to the previous owner receives the ownership-transferred session update before that response, although the previous owner's actor was unloaded to complete the handoff
- **AND** no client bridge route is used to deliver either

#### Scenario: Handoff announced on every surface
- **WHEN** a handoff completes through the ACP extension, the simple API, or Python
- **THEN** a durable ownership-transferred event is recorded on the previous owner's session stream, naming the workspace, the successor session, and whether a summary was seeded
- **AND** a failure to record that event is logged and does not fail or undo the committed handoff

### Requirement: Previous owners are read-only
After a handoff, the previous owner SHALL remain listable and loadable. Loading it SHALL replay its history and disclose read-only access with reason `owner_transferred` and the current owner session. Prompts, queued or steered input, undo/redo, forks, and worktree operations that would run or mutate it SHALL be refused with `owner_transferred`. It SHALL never reload as a local or shared session in the checkout, and its snapshots SHALL be context-only.

#### Scenario: Load a previous owner
- **WHEN** a client loads a session whose workspace was handed off
- **THEN** the history is replayed and the load response discloses `sessionAccess.mode = "read_only"` with the current owner

#### Scenario: Standard ACP client loads a previous owner
- **WHEN** a client that does not use the load snapshot loads a previous owner over stdio or WebSocket
- **THEN** the load succeeds without materializing an actor, and replayed `session/update` notifications contain its history followed by the ownership-transferred update
- **AND** the replay contains no configuration-option update for the read-only session

#### Scenario: Prompt a previous owner
- **WHEN** a client prompts a previous owner
- **THEN** the prompt is refused with `owner_transferred` and no tool executes

### Requirement: Native command output is delivered through the session event stream
Native `/worktree` output SHALL reach clients only through the prompt response and the issuing session's ordered event stream, never a client bridge route. The update SHALL reach every attached connection before the prompt response. A failed delivery SHALL NOT fail a completed operation.

#### Scenario: Output reaches attached clients before the response
- **WHEN** any native `/worktree` command succeeds on a session attached to one or more connections
- **THEN** each connection receives the output as an agent message update before the prompt response, on stdio and WebSocket alike

#### Scenario: Output is not replayed
- **WHEN** a session that ran a native `list`, `status`, or `result` command, or a command rejected as a usage error, is loaded later
- **THEN** that command's output update is not replayed
- **AND** replies to mutating commands are replayed as durable replies, and the durable ownership-transferred and archived updates are replayed

#### Scenario: No bridge route registered
- **WHEN** a native command runs and no client bridge route exists for the session
- **THEN** output delivery and the prompt response are unchanged

### Requirement: Baseline-relative workspace result inspection
Result publication and inspection SHALL support top-level workspaces as well as delegates. Review SHALL identify the immutable baseline and result, baseline-relative changed paths and change kinds, verification state, and captured HEAD. Live HEAD/dirty state SHALL be labeled separately. Dirty-parent baselines SHALL carry a warning against directly merging inherited baseline edits.

#### Scenario: Top-level uncommitted result
- **WHEN** a top-level workspace explicitly publishes a result containing uncommitted edits
- **THEN** the system retains an immutable result reference and exposes its baseline-relative delta through the API and native command surface
- **AND** publication does not modify its real index, working files, or source checkout

#### Scenario: Live files change after publication
- **WHEN** files or HEAD change after a result has been published
- **THEN** inspection of that retained result still presents its original immutable delta
- **AND** live state is reported separately and a subsequent publication does not overwrite the retained result

#### Scenario: Result capture fails
- **WHEN** result publication cannot faithfully capture the checkout
- **THEN** the error and recoverable workspace are reported without claiming a complete result or successful integration

### Requirement: Conservative lifecycle and recovery
Managed workspaces SHALL use durable storage, survive session completion/cancellation, and support reconciliation after interrupted provisioning or removal. Removal SHALL reject active ownership, uncommitted, untracked, or ignored files not preserved by a published result, or committed results lacking a retained recovery reference or integration proof. The system SHALL NOT delete externally owned worktrees or unknown pre-existing directories. Retained references SHALL be disclosed.

#### Scenario: Dirty or active workspace removal
- **WHEN** removal targets an active workspace, or one containing ignored files or uncommitted or untracked files whose working state no retained published result matches
- **THEN** removal is refused and the blocking state is reported without deleting files
- **AND** the `unpreserved_content` message names committing, publishing (`/worktree publish`), or deleting the listed files as the way forward

#### Scenario: History-preserving explicit archival
- **WHEN** a user explicitly archives a workspace whose owner is idle
- **THEN** its session history and durable binding remain available for inspection
- **AND** the owner session loads read-only and resume, prompts, and mutations are refused rather than running in another checkout
- **AND** conservative removal can proceed without deleting the owning conversation
- **AND** archive and removal are refused with `owner_active` while a turn runs, input is queued, or any other cooperating actor holds a checkout activity lease, including in another process

#### Scenario: Interrupted removal
- **WHEN** deletion is interrupted after Removing intent was persisted
- **THEN** explicit recovery rechecks content/reference safety and idempotently completes removal
- **AND** no session can attach as an owner during removal

#### Scenario: Clean checkout with unique commits
- **WHEN** a clean inactive checkout has commits not integrated into the target history
- **THEN** removal is refused unless those results have an explicitly retained recovery reference
- **AND** any successful removal reports the preserved reference

#### Scenario: Branch changes after preservation check
- **WHEN** an external process moves a managed branch after removal inspects its target but before branch deletion
- **THEN** the moved branch is retained or removal is refused and its preservation is revalidated
- **AND** neither normal removal nor interrupted-removal recovery unconditionally deletes the newly observed history

#### Scenario: Crash during creation
- **WHEN** the process restarts after worktree allocation but before session startup completed
- **THEN** reconciliation identifies the allocated resources and reports or completes their binding without overwriting user directories or launching duplicate owners

#### Scenario: Recovery from initial provisioning intent
- **WHEN** allocation stops after persisting intent but before capturing or binding a baseline
- **THEN** recovery has sufficient durable source, policy, and placement information to retry safely or retain an actionable non-runnable state
- **AND** a newly captured recovery-time baseline is disclosed rather than represented as an immutable capture from before the interruption

#### Scenario: Recovery after baseline binding
- **WHEN** allocation stops after binding a baseline but before publishing its retained reference or checkout
- **THEN** recovery preserves that exact baseline and restores the missing reference before marking the workspace ready
- **AND** a conflicting existing reference is diagnosed rather than overwritten

### Requirement: Archive from any session
Archival SHALL succeed from the owning session and from any other local session whenever the owner is idle, unloading an idle locally hosted owner actor before taking the exclusive activity lease. It SHALL refuse with `owner_active` only for active execution and with `owner_not_local` for a remotely hosted owner. The native command SHALL default to the issuing session's workspace.

#### Scenario: Archive from the owning session
- **WHEN** `/worktree archive` is issued without an id from the session that owns a ready workspace and no turn is running
- **THEN** the workspace is archived, the session's actor is unloaded, and every attached client receives the archived update before the prompt response
- **AND** a later prompt in that session is refused with `workspace_archived`

#### Scenario: Archive from another session while the owner is loaded and idle
- **WHEN** another session archives a workspace whose owner actor is loaded but idle
- **THEN** the owner is unloaded and the archive succeeds without any client closing the owner session

#### Scenario: Owner busy
- **WHEN** the owner has a running turn, queued or steering input, shared delegates, or execution in another process
- **THEN** archive is refused with `owner_active` and a message naming the reason, the actor stays loaded, and nothing changes

#### Scenario: Repeated archive
- **WHEN** an archived workspace is archived again
- **THEN** the operation succeeds and reports the archived workspace without emitting a second update

#### Scenario: Remotely hosted owner
- **WHEN** the owner session is hosted on another node
- **THEN** archive is refused with `owner_not_local` and nothing changes

#### Scenario: Archived update cannot be recorded
- **WHEN** the durable archived update on the owner's session stream fails to record
- **THEN** the failure is logged and the archive remains committed

### Requirement: Archived owners are read-only
An archived workspace's owner session SHALL remain listable and loadable. Load SHALL replay history without materializing an actor and disclose `sessionAccess = { mode: "read_only", reason: "workspace_archived" }` with the workspace identity. Prompts, queued or steered input, undo/redo, forks, configuration changes, and mutating worktree commands SHALL be refused with `workspace_archived`. Status, result, list, remove, and recover SHALL remain available from that session.

#### Scenario: Load an archived owner
- **WHEN** a client loads the owner session of an archived workspace
- **THEN** history and the archived update are replayed, the response discloses read-only access with reason `workspace_archived`, and no configuration-option update is replayed

#### Scenario: Clean up from the archived conversation
- **WHEN** `/worktree remove` is issued from the archived owner session
- **THEN** removal runs with the same safety checks as from any other session

### Requirement: Archive-and-remove convenience
Removal SHALL accept an explicit archive option (native `--archive`) that archives the workspace under the archival rules and then removes it. If removal is refused after a successful archive, the workspace SHALL stay archived and the response SHALL report both outcomes. Without the option, removing a non-archived owned workspace SHALL be refused with `workspace_not_archived` and a message pointing to archive.

#### Scenario: One-step cleanup
- **WHEN** `/worktree remove --archive` targets a clean workspace with an idle owner
- **THEN** the workspace is archived and removed, and retained refs are reported
- **AND** the owner's archived update states that the workspace was archived and removed

#### Scenario: Archive succeeds, removal refused
- **WHEN** `/worktree remove --archive` targets a workspace with untracked or ignored files that no published result preserves
- **THEN** the workspace is archived, removal is refused with `unpreserved_content` listing the blocking paths, and no files are deleted
- **AND** the owner's archived update states that removal was refused, with the code and reason, and does not instruct the user to run `/worktree remove` as if it had not been attempted

#### Scenario: Archived update replayed with the removal outcome
- **WHEN** the owner session of a workspace archived by `/worktree remove --archive` is loaded later
- **THEN** the replayed archived update is recorded after the removal attempt and carries the same removal outcome it had live

### Requirement: Visible native command failures
A refused or failed native `/worktree` command, including a usage error, SHALL complete the prompt successfully with `_meta["querymt.worktree"].error = { code, message }` and SHALL emit its message as a session update before the response. Refusal messages SHALL name the next action. ACP extension methods SHALL keep returning errors with `data.code` and `data.message`.

#### Scenario: Refusal is shown in the conversation
- **WHEN** a native `/worktree archive` is refused because the owner is busy
- **THEN** the client shows a message with `owner_active` and the reason instead of a generic "Invalid request"
- **AND** the prompt response carries the same code and message in its metadata

#### Scenario: Specific removal refusal codes
- **WHEN** removal is blocked by an active owner, a non-archived owner, uncommitted/untracked/ignored files, or unpreserved commits
- **THEN** it is refused with `owner_active`, `workspace_not_archived`, `unpreserved_content` (listing paths), or `unpreserved_commits` respectively, each message naming the next action

### Requirement: Session archival releases the owner actor
Setting a session's lifecycle to archived or resolved SHALL unload that session's actor when it owns a managed workspace and is idle, releasing its activity lease. It SHALL NOT archive the workspace, and a busy actor SHALL NOT prevent the lifecycle change.

#### Scenario: Archive the session, then the worktree elsewhere
- **WHEN** a user archives an idle owner session from the session list and then runs `/worktree archive <id>` from another session
- **THEN** the worktree archive succeeds

### Requirement: Supported repository administration layouts
For a usable non-bare source checkout, workspace creation, inspection, capture, review, recovery, and removal SHALL use its actual Git common-directory and checkout identities. Separate Git-directory layouts and linked checkouts backed by bare common repositories SHALL NOT be misinterpreted as ordinary directories. A bare source without a working checkout SHALL be rejected explicitly.

#### Scenario: Separate Git directory
- **WHEN** a supported source checkout stores its Git directory outside the checkout root
- **THEN** its full workspace lifecycle operates on the correct repository without assuming a .git directory directly below a checkout

#### Scenario: Linked checkout backed by a bare common repository
- **WHEN** a usable linked source checkout belongs to a bare common repository
- **THEN** managed workspace creation, status, result review, recovery, and removal use the correct shared repository and per-checkout state

### Requirement: Repository operations do not require a Git executable
Built-in workspace lifecycle, baseline/result capture, inspection, and recovery SHALL operate without an installed Git executable. Inherited Git repository/index environment overrides SHALL NOT redirect these operations. This requirement SHALL NOT imply that arbitrary user shell commands or configured external tools are available.

#### Scenario: Git executable unavailable
- **WHEN** the Git executable is unavailable and a supported source repository already exists
- **THEN** built-in creation, capture, inspection, result publication, recovery, and conservative removal remain available

#### Scenario: Inherited Git environment overrides
- **WHEN** the process inherits Git directory, worktree, or index overrides pointing at another repository
- **THEN** built-in operations use only the resolved workspace repository and leave the other repository unchanged

### Requirement: Opt-in workspace configuration surfaces
The workspace service SHALL be enableable through single-agent and quorum TOML, the simple API, and Python, and SHALL be disabled by default. Configuration SHALL NOT make option-less sessions isolated. A quorum SHALL share one service across planner and delegates. Invalid settings SHALL fail when the agent is built.

#### Scenario: Disabled by default
- **WHEN** an agent is built from a configuration without a workspace section
- **THEN** no workspace service exists, `/worktree` is not advertised, and embedded profiles behave as before

#### Scenario: Quorum enabled from TOML
- **WHEN** a quorum configuration enables the workspace section
- **THEN** the planner and every delegate use the same workspace service instance
- **AND** isolated delegation succeeds without custom Rust wiring

#### Scenario: Invalid workspace settings
- **WHEN** settings contain unknown keys, an unusable storage root, or a storage backend without a workspace repository
- **THEN** building the agent fails with an actionable error before any session starts

#### Scenario: Python isolated session
- **WHEN** a Python caller enables worktrees and requests a worktree session
- **THEN** the session executes in a managed checkout under the storage root and exposes its binding
- **AND** the source checkout is unchanged

### Requirement: Configured default baseline
Configuration MAY set a default baseline mode for top-level isolated creation. It SHALL apply only when isolation is explicitly requested without a baseline choice, SHALL NOT decide whether to isolate, and SHALL be overridden by explicit request values. The resolved baseline mode SHALL be disclosed.

#### Scenario: Default current-working-tree baseline
- **WHEN** the configured baseline is current-working-tree and a worktree session is requested without a baseline choice
- **THEN** the checkout includes the source's validated working contents and the response discloses that mode

#### Scenario: Explicit request overrides the default
- **WHEN** a request specifies a commit baseline or an explicit ref while the configured default is current-working-tree
- **THEN** the session starts from that commit and reports that local changes are excluded

### Requirement: One operation core for all management surfaces
Creation, handoff, listing, status, result, publication, merge, archival, removal, and recovery SHALL share one typed implementation across ACP extensions, native `/worktree`, the simple API, and Python. Failures SHALL carry stable machine-readable error codes on every surface alongside human-readable diagnostics.

#### Scenario: Equivalent results across surfaces
- **WHEN** the same create or result operation is performed through native `/worktree` and through Python
- **THEN** both report equivalent session, workspace, path, branch, baseline, and review fields

#### Scenario: Service unavailable
- **WHEN** worktree operations are invoked on an agent without a workspace service
- **THEN** ACP and native surfaces return the `workspace_service_unavailable` code and Python reports `agent.worktrees` as unavailable

#### Scenario: Typed failure code
- **WHEN** an isolated request fails because of an unsupported repository layout or a missing workspace
- **THEN** ACP error data and the Python exception expose the corresponding stable code

#### Scenario: Equivalent merge across surfaces
- **WHEN** a merge is performed through native `/worktree merge`, `querymt/worktree/merge`, the simple API, and Python
- **THEN** each reports the same outcome (`merged`, `already_merged` or `no_changes`), commit, branch, changed paths and follow-up result
- **AND** a refused merge exposes the same code and paths on every surface

### Requirement: Workspace capability discoverability
Agents with a workspace service SHALL advertise workspace support, the default baseline, and the `sourceSession`, `handoff`, `handoffSummary`, and `merge` features through additive capability metadata. Isolated session creation and managed session load SHALL disclose the resolved execution cwd, workspace identity, branch, baseline mode, and excluded content.

#### Scenario: Client offers an explicit choice
- **WHEN** a client initializes against an agent with the service enabled
- **THEN** the capability metadata indicates that worktree sessions can be requested

#### Scenario: Binding disclosed at creation
- **WHEN** a worktree session is created through ACP
- **THEN** the creation response identifies the execution cwd, workspace ID, branch, and baseline mode

#### Scenario: Handoff capability advertised
- **WHEN** a client initializes against an agent whose workspace service supports handoff with summaries
- **THEN** the workspace capability metadata includes `sourceSession`, `handoff`, and `handoffSummary` set to true

#### Scenario: Merge capability advertised
- **WHEN** a client initializes against an agent with the workspace service enabled
- **THEN** the workspace capability metadata includes `merge` set to true

### Requirement: Requested isolation fails closed
When a usable repository/base, supported repository layout, durable storage, or local filesystem execution is unavailable, requested isolation SHALL fail with an actionable error before agent execution. The system SHALL NOT downgrade to shared execution. Concurrent administration SHALL prevent duplicate allocation and ownership across processes.

#### Scenario: Unsupported remote filesystem execution
- **WHEN** isolated execution is requested on a remote filesystem host unsupported by this version
- **THEN** dispatch fails explicitly rather than treating a local worktree path as remote
- **AND** remote model inference with local filesystem execution is not rejected solely because the model is remote

#### Scenario: Concurrent allocation
- **WHEN** two processes request workspace allocation for one repository concurrently
- **THEN** each successful request receives unique resources and durable ownership without corrupting Git administration data

### Requirement: Published working state satisfies removal content safety
Removal SHALL treat uncommitted and untracked files as preserved when the checkout's full working state (tracked changes, deletions and non-ignored untracked files) is exactly the content of one of the workspace's retained published results, re-checked under the removal's exclusive lease immediately before deletion. It SHALL then remove the checkout and report that result reference as preserving the working state. Ignored files SHALL still block removal.

#### Scenario: Remove after publishing uncommitted work
- **WHEN** a workspace with uncommitted and untracked edits is published and then removed without further edits
- **THEN** removal succeeds, the checkout is deleted, and the response reports the published result reference as `preserved_by` and lists it among the retained references
- **AND** the published result still presents the removed edits through result inspection

#### Scenario: Edited after publishing
- **WHEN** files change after the last publication and removal is requested
- **THEN** removal is refused with `unpreserved_content` listing the blocking paths, the message states that the working state differs from the most recent published result and names that result, and nothing is deleted

#### Scenario: Ignored files with a matching publication
- **WHEN** the checkout's non-ignored working state matches a published result but ignored files are present
- **THEN** removal is refused with `unpreserved_content` listing the ignored paths, and nothing is deleted

#### Scenario: Recovery of an interrupted removal of published work
- **WHEN** removal of a published dirty checkout is interrupted after Removing intent was persisted and recovery is run
- **THEN** recovery re-checks the working state against the published results and completes removal only if it still matches

### Requirement: Read-only sessions accept cancel
`session/cancel` on a session that is read-only because its workspace was archived or its ownership was transferred SHALL succeed without effect, on local and mesh-enabled agents alike.

#### Scenario: Cancel an archived owner
- **WHEN** a client sends `session/cancel` for the owner session of an archived workspace
- **THEN** the notification succeeds without an error and nothing changes

#### Scenario: Cancel a previous owner
- **WHEN** a client sends `session/cancel` for a session whose workspace was handed off
- **THEN** the notification succeeds without an error and nothing changes

### Requirement: Durable replies to mutating native commands
The reply to a native `/worktree` `publish`, `archive`, `remove`, `recover`, or `handoff` command, successful or refused, SHALL be recorded durably on the issuing session's stream with a stable message identity, the command, and its outcome code, and SHALL be replayed on load with the same identity. When the outcome is already recorded as a durable announcement on the issuing session's stream, that announcement SHALL be the reply.

#### Scenario: Mutating command reply is replayed
- **WHEN** a session that ran a native `publish`, `archive`, `remove`, `recover`, or `handoff` command, successful or refused, is loaded later
- **THEN** the command's reply is replayed as an agent message with the same message identity it had live, and `_meta.querymt.worktreeCommand` carries the command name, the outcome `code` (absent on success), and `archived: true` when a refused removal followed a successful archive

#### Scenario: Refused removal reply survives reload
- **WHEN** `/worktree remove` is refused with `unpreserved_content` and the session is reloaded
- **THEN** the refusal message, including the blocking paths and the way forward, is part of the replayed conversation

#### Scenario: Announcement replaces the reply
- **WHEN** a mutating command's outcome is already recorded as a durable announcement on the issuing session's stream (a handoff or archive of the issuing session's own workspace)
- **THEN** that announcement is the reply and no second reply update is recorded

### Requirement: Merge a worktree into its source checkout
A worktree SHALL be mergeable into its source checkout, as one commit on that checkout's current branch containing exactly the baseline-relative delta of the merged version. The source checkout is the effective checkout of the session the worktree was created from, else the worktree's recorded source cwd. Merge SHALL be available as native `/worktree merge [id] [--commit <rev>] [--archive|--remove]` and on every management surface.

#### Scenario: Merge commits only the worktree's own changes
- **WHEN** a worktree created from a checkout with uncommitted files A changes only file B, and `/worktree merge` runs in the worktree session
- **THEN** one commit changing only B is created on the source checkout's current branch with the configured identity
- **AND** A stays uncommitted and untouched in the source checkout
- **AND** the reply names the commit, the branch and the changed paths

#### Scenario: Merge layers onto a moved source branch
- **WHEN** the source branch gained unrelated commits after the worktree was created
- **THEN** the merge commit is created on top of the moved branch and keeps those commits' changes

#### Scenario: Merge from the source session by id
- **WHEN** the source session runs `/worktree merge <workspace-id>`
- **THEN** that workspace is merged into the source checkout exactly as when run from the worktree session

#### Scenario: Merge is a single squashed commit
- **WHEN** the worktree branch holds several commits and uncommitted edits
- **THEN** the source branch gains exactly one new commit whose only parent is the previous source HEAD
- **AND** its message names the worktree and carries `QueryMT-Workspace`, `QueryMT-Result` and `QueryMT-Baseline` trailers

#### Scenario: Merge source checkout no longer exists
- **WHEN** neither the source session's checkout nor the recorded source cwd resolves to a checkout of the worktree's repository
- **THEN** merge is refused with `source_missing` and nothing changes

### Requirement: Merge version selection
By default merge SHALL first publish the worktree's current working state as a retained result and merge exactly that result. With `--commit <rev>` it SHALL take no snapshot and merge the named version: a retained result of the workspace (full id or at least 7 hex digits), or a commit on the worktree branch after its baseline, which is then retained like a published result.

#### Scenario: Default merge snapshots the working state
- **WHEN** a worktree has a new commit, staged and unstaged edits, a non-ignored untracked file and an ignored file, and `/worktree merge` runs
- **THEN** a new retained result is published and its baseline-relative delta, including the untracked file and excluding the ignored file, is merged
- **AND** the reply reports the published result and the excluded ignored files
- **AND** the worktree's branch, index and files are unchanged

#### Scenario: Merge a previously published result
- **WHEN** `/worktree merge --commit <result>` names an earlier retained result and the worktree has changed since
- **THEN** exactly that result's delta is merged and no new result is published

#### Scenario: Merge a commit on the worktree branch
- **WHEN** `--commit <rev>` names a commit on the worktree branch after its baseline
- **THEN** that commit is retained as a result and its baseline-relative delta is merged

#### Scenario: Merge refuses an unrelated commit
- **WHEN** `--commit <rev>` names no retained result and no commit between the baseline and the worktree branch head, or matches several retained results
- **THEN** merge is refused with `commit_not_on_workspace_branch` or `ambiguous_result_commit` and nothing changes

#### Scenario: Snapshot while the owner is busy
- **WHEN** a default merge targets a worktree whose owner has a running turn, queued input or steering
- **THEN** merge is refused with `owner_active` and no result is published

### Requirement: Merge protects local work and runs commit hooks
Merge SHALL apply the same safety rules as explicit delegate result acceptance: it SHALL refuse paths with local state, report overlapping edits as a conflict, run the repository's pre-commit, prepare-commit-msg and commit-msg hooks natively without bypass, and advance the branch only by compare-and-swap. Every refusal or failure SHALL leave the source checkout, its index and its branch unchanged.

#### Scenario: Merge refuses touched paths with local edits
- **WHEN** a path the merge would write has staged, unstaged, untracked or ignored local state in the source checkout
- **THEN** merge is refused with `touches_local_changes` naming the paths and nothing changes

#### Scenario: Merge refuses inherited uncommitted edits
- **WHEN** a delta path also differed between the source HEAD and the worktree's baseline at creation (it was uncommitted in the source then)
- **THEN** merge is refused with `baseline_local_edits` naming the paths and nothing changes

#### Scenario: Merge reports overlapping edits as a conflict
- **WHEN** the source branch and the merged version edited the same lines since the baseline
- **THEN** merge is refused with `merge_conflict` naming the paths and nothing changes

#### Scenario: Merge rejected by a commit hook
- **WHEN** a pre-commit or commit-msg hook exits non-zero
- **THEN** merge is refused with `commit_rejected` and the hook output, the touched paths are restored and the branch is unchanged

#### Scenario: Merge refused while the source session is busy
- **WHEN** the source session has a running turn, queued input or steering
- **THEN** merge is refused with `source_active` and nothing changes

#### Scenario: Merge refused for unsupported source state
- **WHEN** the source checkout has a detached or unborn HEAD, an operation in progress, sparse checkout, `commit.gpgSign` enabled or no user identity
- **THEN** merge is refused with the same stable code as result acceptance and no commit is created

#### Scenario: Merge refused for an unmergeable workspace
- **WHEN** the workspace is provisioning, failed, missing, removing or removed, or is archived and no `--commit` is given
- **THEN** merge is refused with the workspace's lifecycle code and nothing changes

### Requirement: Repeated and interrupted merges
A merged version SHALL be recorded so that merging it again reports the original commit instead of creating a second one. An interrupted merge SHALL be recovered before the next merge or acceptance on the same checkout.

#### Scenario: Merging the same version again
- **WHEN** the same retained result is merged a second time
- **THEN** the reply reports `already_merged` with the original commit and no commit is created
- **AND** when the recorded commit is no longer on the source branch, merge is refused with `integration_record_unreachable`

#### Scenario: Source already contains the changes
- **WHEN** the source branch already contains the whole delta
- **THEN** the reply reports `no_changes` and no commit is created

#### Scenario: Merge interrupted by a crash
- **WHEN** the process stops after writing files or after moving the branch but before completing
- **THEN** the next merge or acceptance on that checkout first restores the touched paths or completes the index update, according to where the branch points

### Requirement: Merge follow-up archive or removal
With `--archive` or `--remove`, merge SHALL archive, or archive and remove, the workspace under the existing rules once the source contains the merged version (merged, already merged, or no changes). A follow-up failure SHALL be reported alongside the successful merge outcome and SHALL NOT undo it. The two options SHALL NOT be combined.

#### Scenario: Merge then remove
- **WHEN** `/worktree merge --remove` merges a worktree whose working state is fully captured by the merged result and whose owner is idle
- **THEN** the merge commit is created, the workspace is archived and removed, and the reply reports both outcomes and the retained refs

#### Scenario: Merge succeeds but removal is refused
- **WHEN** `/worktree merge --commit <older-result> --remove` merges while the worktree holds later unpublished edits
- **THEN** the merge commit is created, the workspace is archived, removal is refused with `unpreserved_content`, and the reply reports the merge and the refusal

#### Scenario: Follow-up skipped after a refused merge
- **WHEN** `--archive` or `--remove` is given and the merge is refused or conflicts
- **THEN** the workspace is neither archived nor removed

#### Scenario: Archive and remove together are rejected
- **WHEN** `/worktree merge --archive --remove` is issued
- **THEN** it is rejected with `invalid_options` before anything runs

### Requirement: Durable merge reply
The reply to a native `/worktree merge`, successful or refused, SHALL be recorded in the issuing session's history and replayed on load. When the merge archives the issuing session's own workspace, the archived update SHALL be that reply's durable record and SHALL state the merge outcome.

#### Scenario: Merge reply survives reload
- **WHEN** a session that ran `/worktree merge` is loaded later
- **THEN** the merge reply, including a refusal code and paths, is replayed in order

#### Scenario: Self-archiving merge reply
- **WHEN** `/worktree merge --archive` runs in the worktree session and succeeds
- **THEN** the session becomes read-only and its replayed archived update names the merge commit

# isolated-delegation Specification

## Purpose

Allow parallel delegates to work from their parent's current code state in separate checkouts, with correct verification and reviewable results that do not overwrite parent work.

## Requirements

### Requirement: Resolved delegation isolation policy
The system SHALL support explicit isolated delegation from local or isolated parents. Delegates of isolated sessions SHALL default to isolated execution; existing local sessions SHALL retain shared execution unless isolation is configured or requested. The resolved policy SHALL be persisted and disclosed. Requested isolation SHALL NOT fall back to sharing on failure.

#### Scenario: Parallel delegates of an isolated parent
- **WHEN** an isolated parent dispatches two delegates without overriding the isolation policy
- **THEN** each delegate receives its own checkout distinct from the parent and sibling
- **AND** concurrent writes to the same relative path do not affect either other checkout

#### Scenario: Isolated delegate of a local parent
- **WHEN** a local parent explicitly requests an isolated delegate
- **THEN** the delegate runs in its own checkout and the parent remains local

#### Scenario: Explicit shared override
- **WHEN** permitted policy explicitly selects shared execution
- **THEN** the operation is labeled shared rather than isolated

### Requirement: Per-delegate configured isolation
Quorum configuration SHALL allow each delegate to declare `inherit`, `isolated`, or `shared` isolation, defaulting to `inherit`. Resolution SHALL apply an explicit non-inherit tool-call value first, then the delegate's configured value, then inheritance from the parent. The resolved policy and its source SHALL be persisted and disclosed.

#### Scenario: Configured isolated delegate of a local parent
- **WHEN** a delegate is configured `isolated` and a local parent delegates to it without an isolation argument
- **THEN** the delegate runs in its own checkout and the disclosed source is the delegate configuration

#### Scenario: Tool-call value overrides configuration
- **WHEN** a delegate is configured `isolated` and the tool call explicitly requests `shared`
- **THEN** the delegate shares the parent checkout, labeled as shared from the request

#### Scenario: Inherit falls through to the parent
- **WHEN** a delegate is configured `inherit` and the tool call omits isolation
- **THEN** isolated parents isolate it and local parents share their checkout

#### Scenario: Configured isolation without a service
- **WHEN** a delegate is configured `isolated` but the quorum has no workspace service
- **THEN** building the quorum fails with an actionable error rather than failing later at dispatch

### Requirement: Private baseline includes parent edits
An isolated delegate SHALL start from a validated immutable baseline containing the parent's tracked working-file contents, tracked deletions, and non-ignored untracked files. It SHALL exclude ignored untracked files and disclose that exclusion. The baseline SHALL reflect working contents rather than reproduce the staging partition.

#### Scenario: Parent has partially staged edits
- **WHEN** the parent has different staged and unstaged contents for a file and starts an isolated delegate
- **THEN** the delegate sees the working-file contents
- **AND** the parent's staged contents, working contents, HEAD, and stash remain unchanged

#### Scenario: Baseline equals HEAD
- **WHEN** the parent's captured working contents equal HEAD
- **THEN** the delegate baseline identifies that committed state without requiring a parent commit

#### Scenario: Newly staged ignored file
- **WHEN** a file absent from HEAD was explicitly staged despite an ignore rule and its working contents subsequently changed
- **THEN** the baseline includes its working contents as a tracked file
- **AND** genuinely untracked ignored files remain excluded
- **AND** capture leaves the real index byte-for-byte unchanged

### Requirement: Baseline capture preserves file semantics without touching the source
Capture SHALL preserve supported Git file semantics, including executable bits, symlinks, binary contents, and tracked hidden files. It SHALL NOT modify the parent's HEAD, branch, index, stash, or working files.

#### Scenario: Mixed local changes
- **WHEN** parent changes include a tracked deletion, a tracked dotfile, a binary file, an executable, a symlink, a non-ignored new file, and an ignored untracked environment file
- **THEN** the supported tracked and non-ignored changes appear in the delegate baseline with their Git file semantics preserved
- **AND** the ignored environment file is not copied and the exclusion is disclosed

### Requirement: Consistent baseline capture and provenance
Delegates dispatched as one parallel batch SHALL use the same immutable baseline. Independently requested delegates SHALL capture the then-current parent state. Capture SHALL coordinate with cooperating QueryMT mutations, validate for observed concurrent external changes, and retry or fail instead of launching from a known inconsistent capture. Unsupported or conflicted repository states SHALL fail explicitly before child execution rather than omit content silently.

#### Scenario: Siblings share starting contents
- **WHEN** two delegates are dispatched in one batch
- **THEN** both report the same baseline identity while using different writable checkouts

#### Scenario: Parent changes after launch
- **WHEN** the parent edits a file after a delegate baseline has been captured
- **THEN** the running delegate's baseline and checkout are unaffected

#### Scenario: Change detected during capture
- **WHEN** source validation detects that files, HEAD, or index changed during capture
- **THEN** the system retries with a new validated capture or returns an actionable failure without launching the child

#### Scenario: Unsupported source state
- **WHEN** the source has unresolved index conflicts or an unsupported repository feature that prevents faithful materialization
- **THEN** delegation fails before execution and identifies the unsupported state

#### Scenario: Unsupported features from a subdirectory session
- **WHEN** capture is requested from a repository subdirectory and the repository contains unsupported submodules or sparse checkout
- **THEN** validation examines the actual checkout root and Git index/tree state and rejects the unsupported source before child execution

#### Scenario: Effective LFS attributes
- **WHEN** captured paths use unsupported LFS filters declared by nested attributes, Git-directory attributes, or attribute macros
- **THEN** capture fails with an actionable feature/path diagnostic before launching a child
- **AND** the source is not accepted merely because the root attribute file lacks a literal filter declaration

### Requirement: Child-workspace verification and execution context
The system SHALL run delegate tools and verification against the child's effective workspace. Execution-related hook context SHALL distinguish the parent and child workspace; pre-allocation policy hooks SHALL identify parent context explicitly. Workspace-dependent integrations SHALL use the child root or fail explicitly.

#### Scenario: Delegation from an isolated conversation fork
- **WHEN** an isolated fork diverges from its source and dispatches an isolated delegate
- **THEN** the delegate baseline includes the fork's current contents rather than the source session's contents
- **AND** default shared execution where explicitly permitted uses the fork's effective cwd, not the source checkout
- **AND** this routing remains correct after restart and for nested delegation

#### Scenario: Verification distinguishes child from parent
- **WHEN** a delegate changes code so that a verification check has a different result in its checkout than in the parent checkout
- **THEN** the reported verification outcome is the child's outcome

#### Scenario: Hooks inspect a delegate
- **WHEN** a hook runs for an allocated delegate's execution or completion
- **THEN** its context identifies the child's execution path and the parent association without substituting the orchestrator default cwd

### Requirement: Durable baseline-relative results
A completed isolated delegate SHALL report its workspace, branch, baseline identity, current HEAD, verification status, and changes relative to the baseline. Uncommitted results SHALL be preserved as an immutable reviewable result when capture succeeds, without modifying the child's real staging state. Results SHALL reach the parent only through explicit acceptance, never by completion. Failure or cancellation SHALL retain recoverable work and report result-capture failures explicitly.

#### Scenario: Result excludes inherited edits
- **WHEN** the parent baseline includes changes to file A and the delegate changes only file B
- **THEN** the delegate result presents the baseline-to-result change to B rather than claiming inherited changes to A as delegate work
- **AND** the parent checkout remains unchanged by completion

#### Scenario: Uncommitted delegate output
- **WHEN** a delegate finishes with uncommitted edits
- **THEN** the result is reviewable from a retained immutable result reference if capture succeeds
- **AND** the child's index and working files remain unchanged by result publication

#### Scenario: Interrupted delegate
- **WHEN** a delegate fails or is cancelled after editing files
- **THEN** its checkout is retained and reported as recoverable rather than automatically removed

#### Scenario: Immutable review after subsequent edits
- **WHEN** the delegate publishes a result and then changes files or HEAD
- **THEN** review of the retained result reports the original baseline-to-result paths and change kinds
- **AND** captured HEAD and immutable result identity are distinct from live HEAD/dirty state
- **AND** subsequent publication retains the previous result reference

#### Scenario: Completion exposes review information
- **WHEN** an isolated delegate completes and its output is summarized for the parent
- **THEN** completion and summarizer output provide baseline-relative review information or a durable reference to that review
- **AND** verification not performed and capture failures are explicit rather than represented as verified or complete results

#### Scenario: Dirty-baseline integration guidance
- **WHEN** a user inspects a result based on uncommitted parent edits
- **THEN** the system identifies the baseline-relative delta and warns that directly merging the private branch can also introduce baseline changes
- **AND** the system does not imply the result has already been applied or accepted

### Requirement: Explicit result acceptance
The parent session of a completed isolated delegation SHALL be able to accept a reviewed result, named only by delegation and result commit, as one native commit of exactly the baseline-relative delta on its current branch. Files SHALL be written before commit hooks run. The branch SHALL advance only by compare-and-swap, atomically with a create-only integration record; earlier refusals or failures SHALL change nothing. Acceptance SHALL be edit-class for permissions.

#### Scenario: Delta-only commit
- **WHEN** the parent accepts a result whose baseline included uncommitted parent edits to file A while the delegate changed only file B, and the parent has unrelated local edits to file C
- **THEN** one commit changing only B is created on the current branch with the configured identity
- **AND** C's local edits and staging remain unchanged

#### Scenario: Touched path has local state
- **WHEN** a path in the delta has staged, unstaged, untracked, or ignored local state, or already differed between the source HEAD and the baseline
- **THEN** acceptance is refused naming the paths and nothing changes

#### Scenario: Overlapping sibling result
- **WHEN** a sibling result was accepted first and both edited the same lines
- **THEN** acceptance reports a conflict with the paths and nothing changes
- **AND** a non-overlapping sibling result is layered onto the moved branch

#### Scenario: Hooks observe and may reject the result
- **WHEN** a pre-commit hook reads the changed files from the working tree
- **THEN** it observes the result contents
- **AND** when a hook exits non-zero, the touched paths are restored, the branch is unchanged, and no integration record exists

#### Scenario: Hook re-stages paths
- **WHEN** a hook rewrites and re-stages a path the acceptance writes
- **THEN** the committed tree contains the re-staged content
- **AND** when a hook stages any other path (including a delta path already integrated by a sibling), acceptance is refused and the branch is unchanged
- **AND** when a hook edits a written file without re-staging it, the commit holds the applied content and the edit remains visible as a working-tree change

#### Scenario: Branch moved during acceptance
- **WHEN** the current branch is moved by an external process after preflight and before the branch update
- **THEN** acceptance is refused, the branch keeps the external commit, and the touched paths are restored

#### Scenario: Repeated acceptance
- **WHEN** the same result is accepted again
- **THEN** the original integration commit is reported and no second commit is created
- **AND** when the recorded commit is no longer reachable from the current branch, acceptance is refused instead of reporting it as integrated

#### Scenario: Unsupported commit configuration or state
- **WHEN** `commit.gpgSign` is enabled, no user identity is configured, HEAD is detached or unborn, sparse checkout is enabled, or a merge, rebase, cherry-pick, or revert is in progress
- **THEN** acceptance is refused with a stable code and no unsigned or partial commit is created

#### Scenario: Unauthorized or inapplicable acceptance
- **WHEN** a session other than the delegation's parent requests acceptance, the delegation is not complete, it ran without an isolated workspace, or the result commit does not match a retained result
- **THEN** acceptance is refused and nothing changes

#### Scenario: No Git executable
- **WHEN** no Git executable is available
- **THEN** acceptance, including hook execution, succeeds

#### Scenario: Interrupted acceptance
- **WHEN** the process stops after applying files or after the branch update but before completing
- **THEN** the next acceptance on that checkout first restores the touched paths or completes the index update according to the branch target, and removes only a lock QueryMT provably owns
- **AND** a touched path edited since the interruption is never overwritten; recovery is refused and the intent kept
- **AND** a reference transaction that applied only one of its two updates is resolved by the same recovery rather than rolled back immediately

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
A delegation with `handoff_from` SHALL fail before its delegate runs, with an error naming the reason, when the named delegation was not made by the delegating session, has no isolated worktree, the worktree is not ready, its owner is busy, its owner's delegation has not reported its terminal state, its owner is not hosted locally, or the call also requests `isolation = "shared"`. Ownership, files and the previous owner's access SHALL be unchanged and no worktree SHALL be allocated.

#### Scenario: Handoff from another session's delegation
- **WHEN** session Q calls `delegate` with `handoff_from` naming a delegation made by session P
- **THEN** the delegation fails with `not_parent_session` and P's worktree keeps its owner

#### Scenario: Handoff from a shared delegation
- **WHEN** `handoff_from` names a delegation that ran in the shared checkout
- **THEN** the delegation fails with `no_isolated_workspace`

#### Scenario: Owner still running
- **WHEN** `handoff_from` names a delegation whose worktree owner has a running turn or queued input
- **THEN** the delegation fails with `owner_active` and the running owner is not interrupted

#### Scenario: Owner's delegation still finishing
- **WHEN** `handoff_from` names a delegation whose delegate's turn ended but which is still verifying, summarizing or capturing its result
- **THEN** the delegation fails with `owner_active`, the owner's session is not unloaded, and the finishing delegation publishes its result from the unchanged worktree

#### Scenario: Shared isolation with handoff
- **WHEN** a `delegate` call sets both `handoff_from` and `isolation = "shared"`
- **THEN** the call is rejected as an invalid request and nothing is queued

#### Scenario: Unknown delegation
- **WHEN** `handoff_from` names a delegation id that does not exist
- **THEN** the delegation fails with an error naming the id and nothing is allocated

### Requirement: Handoff results are accepted under the worktree's delegation
A delegate that ran by handoff SHALL publish its result on the worktree when it completes, as any isolated delegate does. Its completion output SHALL name the delegation that created the worktree as the id to pass to `accept_delegation_result`. Accepting that id with the new result commit SHALL integrate the whole baseline-relative delta, including earlier owners' edits.

#### Scenario: Accept coder work plus reviewer fixes
- **WHEN** the coder of D1 changed file A, the reviewer that took over by handoff fixed file B and completed with result R2, and P calls `accept_delegation_result` with D1 and R2
- **THEN** one commit changing A and B is created on P's branch

#### Scenario: Completion names the worktree's delegation
- **WHEN** a delegate that ran by handoff from D1 completes with changes
- **THEN** its completion output tells the delegating session to call `accept_delegation_result` with delegation id D1 and the new result commit

### Requirement: Acceptance judges the run that published the result
Every result a delegation publishes on completion SHALL record that delegation as its publisher. `accept_delegation_result` SHALL require the result's publishing delegation to be complete, and the worktree's own delegation when the result has no publisher.

#### Scenario: Accept after a failed coder
- **WHEN** the coder of D1 failed after editing file A, a reviewer that took over by handoff completed with result R2, and P calls `accept_delegation_result` with D1 and R2
- **THEN** one commit changing A is created on P's branch

#### Scenario: Result without a publisher
- **WHEN** D1 failed and P accepts a result of its worktree that was published without a delegation (an explicit publish)
- **THEN** the acceptance is refused with `delegation_not_complete` and nothing changes

### Requirement: Baseline capture reads only changed content
Baseline capture SHALL NOT read the content of a tracked file whose stat data matches its entry in the source's real index, as Git compares stat data. It SHALL take that entry's content identity instead. It SHALL read modified tracked files and non-ignored untracked files. It SHALL find non-ignored untracked files regardless of the repository's `status.showUntrackedFiles` setting.

#### Scenario: Clean checkout reads no file content
- **WHEN** an isolated delegate is requested from a checkout whose working files all match the index and HEAD, and the index was written in a later timestamp tick than any of them was modified
- **THEN** the capture reads no file content
- **AND** the baseline identifies HEAD's commit

#### Scenario: One edited file is the only file read
- **WHEN** an isolated delegate is requested after one tracked file was edited in a checkout of many files
- **THEN** the capture reads that file and no unchanged tracked file
- **AND** the delegate's checkout has the edited contents

#### Scenario: Staged then edited file keeps working contents
- **WHEN** a file was staged and then edited again before an isolated delegate is requested
- **THEN** the baseline holds the edited working contents, not the staged contents
- **AND** the real index is byte-for-byte unchanged

#### Scenario: Untracked files are captured when status hides them
- **WHEN** the repository sets `status.showUntrackedFiles=no` and a new non-ignored file exists
- **THEN** the capture reads that file
- **AND** the baseline includes it

### Requirement: Index entries that can hide working content are read
Baseline capture SHALL read a tracked file whose real-index entry is racily clean (modified no earlier than the index was written) or carries an assume-unchanged, skip-worktree, fsmonitor-valid or intent-to-add flag, even when its stat data matches the entry. It SHALL also read a tracked file whose recorded kind (regular file or symbolic link) differs from the file on disk.

#### Scenario: Racily clean entry is read
- **WHEN** a tracked file's index entry is racily clean and the file was rewritten with the same size and modification time
- **THEN** the capture reads that file
- **AND** the baseline holds its current contents

#### Scenario: Assume-unchanged file edited is captured
- **WHEN** a tracked file marked assume-unchanged in the real index was edited before an isolated delegate is requested
- **THEN** the capture reads that file
- **AND** the baseline holds its edited contents

#### Scenario: Intent-to-add file is read
- **WHEN** a file was added with intent-to-add and has content before an isolated delegate is requested
- **THEN** the capture reads that file
- **AND** the baseline holds its contents

#### Scenario: File replaced by a symlink is read
- **WHEN** a tracked regular file was replaced by a symbolic link
- **THEN** the capture reads the link
- **AND** the baseline records a symbolic link with its target

### Requirement: Baselines hold raw working contents
The baseline SHALL hold the raw working bytes and on-disk modes of the checkout, and SHALL equal one built by reading every candidate file. Baseline capture SHALL read a tracked file whose effective attributes or configuration convert its content between the index and the working tree. Baseline capture SHALL read a tracked file whose index blob size differs from the file size.

#### Scenario: Line-ending converted file keeps working bytes
- **WHEN** a tracked file's attributes convert line endings between the index and the working tree
- **THEN** the capture reads that file
- **AND** the baseline holds its working bytes, as a capture that reads every file would

#### Scenario: Index blob normalised under earlier settings is read
- **WHEN** a tracked file's index blob was converted under earlier settings that no longer apply, and its stat data matches the index entry
- **THEN** the capture reads that file
- **AND** the baseline holds its working bytes

#### Scenario: Selective capture matches a full read
- **WHEN** a checkout holds a tracked deletion, an edited file, a staged-only addition, an executable, a symlink and a non-ignored untracked file
- **THEN** the baseline tree is identical to the tree obtained by reading every candidate file

### Requirement: Baseline capture writes no content the repository holds
Baseline capture SHALL NOT write an object to the source repository that the repository already holds. It SHALL write only the blobs, trees and commit of contents the repository does not yet contain.

#### Scenario: Repeated delegation from the same dirty state
- **WHEN** two isolated delegates are requested one after the other from a checkout whose working state did not change in between
- **THEN** the second capture writes no new blob or tree

#### Scenario: Repeated delegation with a large dirty file writes nothing
- **WHEN** two isolated delegates are requested one after the other from a checkout with a dirty file of at least the large-file threshold, and nothing changed in between
- **THEN** the second capture writes no new blob or tree

#### Scenario: New untracked file is written once
- **WHEN** an isolated delegate is requested after a new non-ignored file was created
- **THEN** the capture writes that file's blob
- **AND** the baseline includes the file

### Requirement: Capture validation reads file contents once per attempt
Each capture attempt SHALL read the content of each file at most once, except that a captured file modified within the racy window of the attempt's start SHALL be read once more to verify the captured content, and a file of at least the large-file threshold that the repository does not yet hold SHALL be read once more to write it. A verified file whose content no longer matches SHALL be treated as a change detected during capture, which is retried or fails as already required.

#### Scenario: Same-size rewrite after the read is detected
- **WHEN** a captured file modified within the racy window is rewritten with the same size after the capture read it, and its stat data after the rewrite equals its stat data before the capture
- **THEN** the attempt is discarded and a new validated capture is made, or the delegation fails before the child launches

#### Scenario: Quiet checkout needs one attempt
- **WHEN** nothing changes the checkout while the capture runs
- **THEN** the baseline is produced by one attempt that read each changed file once, apart from files verified because they were modified within the racy window and large files written for the first time

### Requirement: Capture validation compares stat fingerprints
Validation SHALL compare HEAD, the index entries, and the size, modification time and mode of every captured path and every non-ignored untracked path, plus change time and inode where the platform provides them and the repository's stat settings use them, taken before and after the capture. Any difference SHALL be treated as a change detected during capture, which is retried or fails as already required.

#### Scenario: External edit to an already dirty file during capture
- **WHEN** a file that already differs from HEAD is rewritten by another process while the capture runs
- **THEN** the attempt is discarded and a new validated capture is made, or the delegation fails before the child launches

#### Scenario: Untracked file created during capture
- **WHEN** a new non-ignored file appears in the checkout while the capture runs
- **THEN** the attempt is discarded and a new validated capture is made, or the delegation fails before the child launches

### Requirement: Isolated delegation startup is traceable
When tracing is enabled, each phase between claiming a delegation and sending the delegate's first prompt SHALL be recorded as its own span under the delegation's handler span. The parallel-slot wait and `delegation.execute` SHALL be siblings. Admission, parent workspace resolution, baseline capture, lock and gate waits, provisioning, the `delegation_start` hook and session creation SHALL be under `delegation.execute`.

#### Scenario: No unattributed startup time
- **WHEN** an isolated delegation starts and its delegate receives its first prompt
- **THEN** the spans under the handler span cover the interval from the end of the claim to the first prompt, except for gaps of at most 50 ms in total

#### Scenario: Queued delegation shows its wait
- **WHEN** a delegation waits because the parallel-delegation limit is reached
- **THEN** a queue-wait span records the time from the claim to the start of execution

#### Scenario: Shared delegation has no allocation spans
- **WHEN** a shared (non-isolated) delegation starts
- **THEN** no baseline capture or provisioning span is recorded for it

### Requirement: Startup spans record what they did
A baseline capture span SHALL record its attempt count, files read, bytes read and objects written. A lock or gate wait span SHALL cover only the time spent waiting and SHALL record the lock it waited for, and a gate wait span SHALL record the checkout the gate serialises. A provisioning span SHALL record the number of files checked out. The workspace id SHALL be recorded once it exists.

#### Scenario: Capture span explains a slow capture
- **WHEN** a capture reads 100 files totalling 10 MB in one attempt
- **THEN** its span records 1 attempt, 100 files read and 10 MB read

#### Scenario: Contended gate is visible
- **WHEN** baseline capture waits for the source checkout's mutation gate held by another operation
- **THEN** a gate-wait span records the wait time and the checkout it serialises

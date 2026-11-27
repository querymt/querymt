# Spec Delta

## ADDED Requirements

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

## MODIFIED Requirements

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

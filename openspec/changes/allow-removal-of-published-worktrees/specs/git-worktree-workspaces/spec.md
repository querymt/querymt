# Spec Delta

## ADDED Requirements

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

## MODIFIED Requirements

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

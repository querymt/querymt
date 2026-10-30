# Spec Delta

## Purpose

Defines how QueryMT executes a workflow run: agent sessions and their context budgets, gate commands, read-only roles, the human inbox, the durable run journal, restart recovery and the control surfaces.

## ADDED Requirements

### Requirement: Fresh session per agent step
Each agent step SHALL run in a new QueryMT session created with the role's profile, inside the change's workspace. The prompt SHALL be rendered from a fixed template filled only with computed values: change name, progress, next unchecked task, previous handoff, gate failures and chosen findings.

#### Scenario: Chunk prompt contents
- **WHEN** an implementation chunk starts after an earlier chunk
- **THEN** its prompt contains the change name, current task progress, the next unchecked task and the previous chunk's handoff section verbatim

#### Scenario: Rendered prompt is recorded
- **WHEN** any agent step starts
- **THEN** the exact rendered prompt is recorded in the run journal

### Requirement: Context budget enforcement
For roles with a budget, the run SHALL track context tokens from each model response. At `wrap_up` it SHALL steer the session once to finish the current task, write the handoff and not start another task. At `stop` it SHALL cancel the session.

#### Scenario: Wrap-up steer
- **WHEN** a session's context tokens first reach the `wrap_up` threshold
- **THEN** exactly one wrap-up steer is delivered to the active run, and the event is recorded

#### Scenario: Hard stop
- **WHEN** a session's context tokens reach the `stop` threshold
- **THEN** the session is cancelled, and the chunk is evaluated from repository and task state like any other ended chunk

#### Scenario: Steer refused
- **WHEN** the wrap-up steer is refused because the run is no longer steerable
- **THEN** the refusal is recorded, and the `stop` threshold still applies to any later run

### Requirement: Handoff section
Implementation prompts SHALL require a `## Handoff` section at the end of the session. The run SHALL extract it verbatim from the full final assistant message and pass it only to the next chunk's prompt. It SHALL NOT be used to decide transitions.

#### Scenario: Missing handoff
- **WHEN** a chunk ends without a handoff section
- **THEN** the next chunk's prompt states that no handoff was provided, and progress is still evaluated from task state

### Requirement: Gate commands
Gate commands SHALL run in the change's workspace with the declared resources. The exit code SHALL be the result, and a bounded tail of the output SHALL be recorded.

#### Scenario: Gate passes
- **WHEN** the chunk gate exits with status 0
- **THEN** the gate is recorded as passed for that chunk

#### Scenario: Gate times out
- **WHEN** a gate exceeds its configured timeout
- **THEN** it is terminated and recorded as failed with reason `timeout`

### Requirement: Read-only roles cannot mutate
Sessions for roles marked read-only SHALL be denied every mutating tool. After every read-only step, the run SHALL verify that the workspace, the base checkout and idle change workspaces are unchanged.

#### Scenario: Reviewer attempts an edit
- **WHEN** a reviewer session calls a file-editing tool
- **THEN** the call is denied and the file is unchanged

#### Scenario: Shell write detected
- **WHEN** a read-only step's workspace differs from its starting state after the step
- **THEN** the change is blocked with reason `read_only_violation`, listing the changed paths

### Requirement: Writes stay inside the step's workspace
Sessions in a workflow run SHALL be granted edit permissions only for paths inside their own change workspace.

#### Scenario: Edit outside the workspace
- **WHEN** an implementation session tries to edit a file in the base checkout
- **THEN** the permission is denied, and the attempt is recorded

### Requirement: Human inbox
Approvals, finding triage, cap overrides and questions raised by unattended sessions SHALL become inbox items on the run. A question from an unattended session SHALL wait for an inbox answer or a configured policy answer instead of being cancelled.

#### Scenario: Triage request
- **WHEN** review produces findings
- **THEN** an inbox item lists them, and the fix step starts only after the user selects findings or dismisses all of them

#### Scenario: Agent question while unattended
- **WHEN** an implementation session asks a question during a run
- **THEN** the question appears in the run's inbox, and the session receives the answer the user gives there

### Requirement: Durable run journal
Every workflow run SHALL have an append-only journal containing definitions hash, events, requested effects, rendered prompts and decisions, stored in the QueryMT session store.

#### Scenario: Journal inspection
- **WHEN** the user requests status for a run
- **THEN** each change's current step, progress, blocked reason and pending inbox items are reported from the journal

### Requirement: Restart recovery
After a QueryMT restart, an interrupted run SHALL resume without repeating completed effects. A step interrupted mid-session SHALL be re-evaluated from repository and task state before anything new starts.

#### Scenario: Restart during a chunk
- **WHEN** QueryMT restarts while a chunk session is running
- **THEN** after resume, the interrupted chunk is evaluated as ended and the engine decides the next step from task progress

#### Scenario: Restart after integration
- **WHEN** QueryMT restarts after a change was integrated but before the journal recorded the next step
- **THEN** resume detects the integration and does not integrate the change again

### Requirement: Control surfaces
Workflow runs SHALL be controllable through the ACP extension methods `querymt/workflow/start`, `status`, `plan`, `approve`, `answer`, `resume` and `abort`, all listed in `querymt/capabilities`, and through a `qmt flow` CLI with the same operations. Progress SHALL be emitted as workflow events.

#### Scenario: Capability discovery
- **WHEN** a client calls `querymt/capabilities` on an agent built with workflow support
- **THEN** the workflow methods are listed

#### Scenario: Abort
- **WHEN** the user aborts a run
- **THEN** active sessions are cancelled, running gates are terminated, no further effects are requested, and the workspaces are kept

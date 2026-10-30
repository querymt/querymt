# Spec Delta

## Purpose

Defines the flow driver's deterministic decision engine: scheduling across changes, the per-change step sequence, loop caps, progress rules, the durable journal and exact replay.

## ADDED Requirements

### Requirement: Decisions depend only on recorded inputs
Every decision SHALL be computed only from the loaded definitions and the run's recorded events. Time, identifiers, command results, agent output and human answers SHALL enter the engine only as recorded events.

#### Scenario: Replay reproduces decisions
- **WHEN** a recorded journal is replayed against the same definitions
- **THEN** the engine requests the same sequence of actions and reaches the same state

#### Scenario: Definitions changed
- **WHEN** a run is resumed and the definitions' content hash differs from the recorded one
- **THEN** the driver refuses to continue until the user explicitly accepts the new definitions

### Requirement: Transitions use observable facts only
The engine SHALL decide step completion from task progress reported by `openspec ... --json`, command exit codes, schema-valid structured outputs, repository state and human answers, and SHALL NOT use free-form agent text.

#### Scenario: Claimed completion with unchecked tasks
- **WHEN** a session ends claiming all tasks are done but task progress shows unchecked tasks
- **THEN** the change stays in the implement step and another chunk is scheduled

### Requirement: Per-change step sequence
Each change SHALL pass through the workflow steps in graph order (default: preflight, workspace, implement, verify, review, triage, fix, archive, integrate). A step SHALL start only when its prerequisites have succeeded.

#### Scenario: Preflight failure
- **WHEN** a change fails `openspec validate --strict` or has incomplete planning artifacts
- **THEN** it is marked blocked with the failing check and no workspace is created

#### Scenario: No findings
- **WHEN** review produces no findings
- **THEN** triage and fix are skipped

### Requirement: Implementation chunk loop
The implement step SHALL repeat chunks, each in a new session followed by the chunk gate, until all tasks are done. A chunk counts as progress only if completed tasks increased and the chunk gate passed.

#### Scenario: Gate failure carried forward
- **WHEN** a chunk's gate fails
- **THEN** the next chunk's prompt contains the failing gate output and asks to fix it before new tasks

### Requirement: Hard loop caps
Every loop SHALL have a hard cap: chunks per change, consecutive chunks without progress, review and fix rounds, and malformed-output retries. Reaching a cap SHALL block the change, and only a human approval MAY extend it by one round.

#### Scenario: No progress twice
- **WHEN** two consecutive chunks make no progress
- **THEN** the change is blocked with `no_progress` and an inbox item is created

### Requirement: Structured step outputs
Outputs the engine reads (review findings, handoff, blocked report) SHALL validate against declared schemas. Invalid output SHALL be retried up to the cap and then block the change.

#### Scenario: Malformed findings
- **WHEN** reviewer findings fail schema validation twice
- **THEN** the change is blocked with `malformed_output`

### Requirement: Cross-change scheduling
A step SHALL start only when its prerequisites are complete, its change's blockers are integrated, and the change, session and named-resource limits allow it. Ready work SHALL be ordered by longest remaining dependency chain, then selection order. Running sessions SHALL NOT be interrupted for reprioritization.

#### Scenario: Session limit
- **WHEN** the session limit is reached
- **THEN** no new session starts until one ends

#### Scenario: Resource capacity
- **WHEN** a resource is at capacity
- **THEN** further commands using it wait

### Requirement: One writer per change workspace
At most one writing step SHALL be active in a change's workspace at a time; read-only steps of that change MAY run in parallel on a fixed commit.

#### Scenario: Parallel reviewers
- **WHEN** review fans out to two read-only reviewers
- **THEN** both run concurrently against the same checkpoint commit

### Requirement: Blocked changes are isolated
A blocked change SHALL hold up only its dependents.

#### Scenario: Independent change continues
- **WHEN** A is blocked and C does not depend on A
- **THEN** C continues

### Requirement: Durable journal
Each run SHALL have an append-only journal under `<git-common-dir>/flow/<run-id>/` containing the definitions hash, detected agent capabilities, events, requested actions, rendered prompts and decisions. A lock SHALL prevent two driver processes from driving the same run.

#### Scenario: Second driver refused
- **WHEN** a second `flow resume` targets a run already being driven
- **THEN** it exits with an error naming the holding process

#### Scenario: Status
- **WHEN** the user runs `flow status`
- **THEN** each change's step, progress, blocked reason and pending inbox items are reported from the journal

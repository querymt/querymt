# Spec Delta

## Purpose

Defines the deterministic decision core of a change workflow: how changes are scheduled, how each change moves through its steps, which loops are capped, and how a run can be replayed exactly from its recorded events.

## ADDED Requirements

### Requirement: Decisions depend only on recorded inputs
Every workflow decision SHALL be computed only from the loaded definitions and the run's recorded events. Time, identifiers, command results, model output and human answers SHALL enter the engine only as recorded events.

#### Scenario: Replay reproduces decisions
- **WHEN** a recorded run's event log is replayed against the same definitions
- **THEN** the engine requests exactly the same sequence of effects and reaches the same state

#### Scenario: Definitions changed between runs
- **WHEN** a run is resumed and the workflow definitions' content hash differs from the recorded one
- **THEN** the engine refuses to continue until the user explicitly accepts the new definitions

### Requirement: Transitions use observable facts only
The engine SHALL decide step completion from task progress reported by `openspec ... --json`, command exit codes, schema-valid structured outputs, repository state and human answers. It SHALL NOT use free-form model text to decide a transition.

#### Scenario: Agent claims completion without ticked tasks
- **WHEN** an implementation session ends saying all tasks are done but task progress shows unchecked tasks
- **THEN** the change stays in the implementation step and the next chunk is scheduled

### Requirement: Per-change step sequence
Each selected change SHALL pass through the workflow's steps in graph order. The default order is: preflight, workspace, implement, verify, review, triage, fix, archive, integrate. A step SHALL start only when all of its prerequisite steps have succeeded.

#### Scenario: Preflight failure stops the change
- **WHEN** a change fails strict validation or has incomplete planning artifacts
- **THEN** the change is marked blocked with the failing check, and no workspace is created for it

#### Scenario: Triage skipped without findings
- **WHEN** the review step produces no findings
- **THEN** triage and fix are skipped and the change proceeds to archive

### Requirement: Implementation chunk loop
The implement step SHALL repeat chunks until task progress reports all tasks done. Each chunk SHALL run in a new session and be followed by the chunk gate. A chunk counts as progress only if the number of completed tasks increased and the chunk gate passed.

#### Scenario: Chunk makes progress
- **WHEN** a chunk ticks two more tasks and the chunk gate passes
- **THEN** the chunk is checkpointed and the next chunk is scheduled if tasks remain

#### Scenario: Gate failure is carried forward
- **WHEN** a chunk's gate fails
- **THEN** the next chunk's prompt includes the failing gate output, and that chunk must fix it before starting new tasks

### Requirement: Hard loop caps
Every loop SHALL have a hard cap from the definitions: maximum chunks per change, maximum consecutive chunks without progress, maximum review and fix rounds, and maximum retries for malformed structured output. Reaching a cap SHALL block the change, not continue it.

#### Scenario: No progress twice
- **WHEN** two consecutive chunks make no progress
- **THEN** the change is blocked with reason `no_progress`, and a human decision is requested

#### Scenario: Review cap reached
- **WHEN** the reviewer reports blocking findings after the final allowed fix round
- **THEN** the change is blocked; only a human approval can add one more round

### Requirement: Structured step outputs
Steps whose output the engine reads (review findings, handoff notes, blocked reports) SHALL produce output that validates against a declared schema. Output that fails validation SHALL be retried up to the cap and then block the change.

#### Scenario: Malformed findings
- **WHEN** the reviewer returns findings that do not match the findings schema
- **THEN** the engine requests one reformat attempt, and blocks the change if that attempt also fails

### Requirement: Cross-change scheduling
The engine SHALL start a step only when its prerequisites are complete, its change's blockers are integrated, and the change, session and named-resource limits allow it. Ready work SHALL be ordered by longest remaining dependency chain, then by the user's selection order.

#### Scenario: Session limit respected
- **WHEN** four sessions are active and the session limit is four
- **THEN** no further session is started until one ends

#### Scenario: Resource capacity respected
- **WHEN** two gate commands that use the `cargo` resource are running and its capacity is two
- **THEN** a third such command waits until one finishes

### Requirement: One writer per change workspace
At most one step that may write SHALL be active in a change's workspace at a time. Read-only steps of the same change MAY run in parallel on a fixed commit.

#### Scenario: Parallel reviewers
- **WHEN** the review step fans out to two read-only reviewers
- **THEN** both run at the same time against the same checkpoint commit

### Requirement: Scheduling switches only at step boundaries
The engine SHALL NOT interrupt a running session to reprioritize work. It MAY reorder, start or defer work only when a step finishes.

#### Scenario: Higher-priority change becomes ready
- **WHEN** a blocker integrates while another change is mid-chunk
- **THEN** the running chunk continues, and the newly ready change starts when capacity frees

### Requirement: Blocked changes are isolated
A blocked change SHALL hold up only the changes that depend on it. Other changes SHALL continue.

#### Scenario: One change blocked
- **WHEN** change A is blocked and change C does not depend on A
- **THEN** C continues, and changes blocked by A remain waiting

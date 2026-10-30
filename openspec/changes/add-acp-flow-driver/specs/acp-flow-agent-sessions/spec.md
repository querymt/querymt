# Spec Delta

## Purpose

Defines how the flow driver drives agents as an ACP client: capability detection, one session per step, context budgets, wrap-up delivery, permission and question handling, and recovery after a driver restart.

## ADDED Requirements

### Requirement: Capability detection
On connecting, the driver SHALL call `querymt/capabilities` and record which QueryMT extensions are available. A failure or `method_not_found` SHALL mean a plain ACP agent. Behavior that depends on an extension SHALL use the recorded result.

#### Scenario: QueryMT agent
- **WHEN** the agent lists `querymt/worktree/create`, `querymt/session/steer` and `querymt/session/runtimeState`
- **THEN** the run records the QueryMT backend and uses those methods

#### Scenario: Plain ACP agent
- **WHEN** `querymt/capabilities` returns `method_not_found`
- **THEN** the run records the plain backend and uses only standard ACP methods

### Requirement: Fresh session per agent step
Each agent step SHALL use a new ACP session whose working directory is the change's workspace, configured with the role's profile or model when the agent supports it, named `flow/<run>/<change>/<step>/<n>` when naming is supported. The prompt SHALL be rendered from a fixed template with a closed set of computed variables and recorded in the journal.

#### Scenario: Chunk prompt contents
- **WHEN** a later chunk starts
- **THEN** its prompt contains the change name, task progress, next unchecked task and the previous chunk's handoff section verbatim

### Requirement: Context budget from usage updates
The driver SHALL track context size from ACP `usage_update` notifications (`used`) for each step session. At `wrap_up` it SHALL deliver one wrap-up instruction; at `stop` it SHALL send `session/cancel`.

#### Scenario: Hard stop
- **WHEN** `used` reaches `stop`
- **THEN** the session is cancelled and the chunk is evaluated from repository and task state

#### Scenario: No usage updates
- **WHEN** an agent never sends `usage_update`
- **THEN** the run records that budgets are unenforced for that role and applies the step's wall-clock timeout instead

### Requirement: Wrap-up delivery
With the QueryMT backend, wrap-up SHALL be sent with `querymt/session/steer` using the active run id from `querymt/session/runtimeState`. With the plain backend, or when the steer is refused, the driver SHALL cancel the turn and send a follow-up prompt in the same session asking to finish only the current task and write the handoff.

#### Scenario: Steer accepted
- **WHEN** the QueryMT agent accepts the steer
- **THEN** no cancel is sent and the event is recorded

#### Scenario: Steer refused
- **WHEN** the steer is refused
- **THEN** the driver cancels and sends the follow-up prompt, and records both

### Requirement: Handoff section
Implementation prompts SHALL require a `## Handoff` section at the end. The driver SHALL extract it from the full streamed final assistant message and use it only in the next chunk's prompt.

#### Scenario: Missing handoff
- **WHEN** a chunk ends without a handoff
- **THEN** the next prompt states that none was provided

### Requirement: Permission decisions
The driver SHALL answer every permission request for workflow sessions: grant edits only when every location is inside the step's workspace, and deny all mutating operations for read-only roles. Denials SHALL be recorded.

#### Scenario: Edit outside workspace
- **WHEN** an implementer requests to edit a file in the base checkout
- **THEN** the driver denies it

#### Scenario: Reviewer edit
- **WHEN** a read-only reviewer requests an edit inside its workspace
- **THEN** the driver denies it

### Requirement: Post-step mutation checks
Because tools may write without asking permission, the driver SHALL compare the step's workspace, the base checkout and idle change workspaces with pre-step snapshots after every step, and SHALL block the change with `read_only_violation` or `foreign_write` on unexpected differences.

#### Scenario: Reviewer shell write
- **WHEN** a read-only step's workspace changed
- **THEN** the change is blocked with `read_only_violation`, listing the paths

### Requirement: Questions go to the inbox
Elicitations and question requests from workflow sessions SHALL become inbox items. The driver SHALL answer them with the user's inbox reply or a configured policy answer, never by silently cancelling.

#### Scenario: Question while unattended
- **WHEN** a session asks a question
- **THEN** it appears in `flow status`, and `flow answer` delivers the reply to the session

### Requirement: Reattach after driver restart
On `flow resume`, the driver SHALL reconnect, reload recorded sessions with `session/load` when supported and reattach to running turns. When a session cannot be reattached, its step SHALL be evaluated as ended from repository and task state.

#### Scenario: Driver crash during a chunk on QueryMT
- **WHEN** the driver restarts while a QueryMT chunk session is still running
- **THEN** it reattaches, keeps enforcing the budget, and the chunk continues

#### Scenario: Agent restarted too
- **WHEN** the recorded session no longer has a running turn
- **THEN** the chunk is evaluated as ended and the engine decides the next step

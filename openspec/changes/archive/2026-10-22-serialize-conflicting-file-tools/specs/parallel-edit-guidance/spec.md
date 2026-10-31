# Spec Delta

## Purpose

Detects when a model emits several mutations of the same file in one response and steers it toward a single atomic `multiedit` call, either by advising it (warn) or by rejecting the redundant calls (deny).

## ADDED Requirements

### Requirement: Detect multiple writes to one file per response
The system SHALL detect when a single model response contains two or more tool calls that write the same file, using the same path identity rules as tool batch scheduling. Calls that only read a file, or whose file access is undetermined, MUST NOT count toward detection.

#### Scenario: Two edits to one file
- **WHEN** a response contains two `edit` calls on `design.md`
- **THEN** the batch is flagged for `design.md`

#### Scenario: Edits to different files
- **WHEN** a response contains one `edit` on `a.md` and one on `b.md`
- **THEN** nothing is flagged

#### Scenario: Earlier gated calls do not disable detection
- **WHEN** a response contains a call with invalid arguments and two `edit` calls on `design.md`
- **THEN** the batch is still flagged for `design.md`

#### Scenario: Shell does not trigger detection
- **WHEN** a response contains one `edit` on `a.md` and a `shell` call
- **THEN** nothing is flagged

### Requirement: Warn mode runs all calls and advises multiedit
In warn mode (the default), the system SHALL execute every call in a flagged batch and, before the next model request, add a guidance message naming each affected file and recommending one `multiedit` call per file. The message MUST be placed after the batch's tool results, never between a tool call and its result.

#### Scenario: Guidance after a flagged batch
- **WHEN** a response with two `edit` calls on `design.md` completes in warn mode
- **THEN** both edits are applied and the next model request includes a message naming `design.md` and recommending `multiedit`

#### Scenario: Valid message ordering
- **WHEN** the guidance message is added
- **THEN** every tool call in the conversation is still directly followed by its tool result

#### Scenario: Guidance is sent once
- **WHEN** a flagged batch is followed by two further model requests
- **THEN** the guidance message is added only before the first of them

#### Scenario: Undelivered guidance does not leak into a later turn
- **WHEN** a flagged batch is cancelled before the next model request
- **THEN** the guidance message is not added to the following turn

### Requirement: Flagged batches notify the UI without delaying tools
The system SHALL publish a durable notice naming each flagged file and its number of writes, in both warn and deny modes. Publishing MUST NOT delay the execution of the batch's tool calls.

#### Scenario: Notice for a flagged batch
- **WHEN** a response with two `edit` calls on `design.md` is flagged
- **THEN** a `parallel_edit_guard` notice naming `design.md` is published for the session

### Requirement: Deny mode rejects redundant same-file writes
In deny mode, the system SHALL execute only the first writing call per flagged file and return an error result for each later writing call to that file, without executing it. The error MUST name the file and tell the model to combine the edits into one `multiedit` call.

#### Scenario: Second edit rejected
- **WHEN** a response contains edits A then B on `design.md` in deny mode
- **THEN** A is applied, B is not executed, and B's result is an error recommending `multiedit` for `design.md`

#### Scenario: One result per call
- **WHEN** calls are rejected in deny mode
- **THEN** every tool call in the response still receives exactly one result

#### Scenario: Unaffected calls still run
- **WHEN** a deny-mode batch also contains an `edit` on another file
- **THEN** that edit is executed normally

### Requirement: Guard is on by default and configurable
The guard SHALL be active in warn mode for every agent unless configured otherwise. Users MUST be able to choose warn mode, deny mode, or disable it through middleware configuration.

#### Scenario: Default agent
- **WHEN** an agent is built without any guard configuration
- **THEN** flagged batches produce warn-mode guidance

#### Scenario: Explicit deny
- **WHEN** the configuration contains a `parallel_edit_guard` middleware entry with `mode = "deny"`
- **THEN** flagged batches are handled in deny mode and no second guard instance is added

#### Scenario: Disabled
- **WHEN** the configuration disables the `parallel_edit_guard` middleware
- **THEN** no guidance or rejection happens, while same-file writes are still serialized by tool batch scheduling

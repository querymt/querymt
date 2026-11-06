# Spec Delta

## Purpose

Defines how QueryMT invokes tools hosted on MCP servers and reports their outcome, progress, cancellation, and server-side long-running tasks to the model and to ACP clients.

## ADDED Requirements

### Requirement: In-band MCP tool errors are reported as failed tool calls
When an MCP server returns a tool result marked as an error, the system SHALL record the tool call as failed. The model SHALL receive the server's error content, ACP clients SHALL see the tool call with status `failed`, and the tool-call outcome SHALL be recorded as `error`.

#### Scenario: Server reports isError
- **WHEN** an MCP tool call returns a result with `isError: true` and text content "quota exceeded"
- **THEN** the tool result given to the model contains "quota exceeded" and is marked as an error
- **AND** the ACP `tool_call_update` for that call has status `failed`

#### Scenario: Error flag absent or false
- **WHEN** an MCP tool call returns a result whose `isError` is absent or `false`
- **THEN** the tool call is recorded as successful

#### Scenario: Error message visible to non-agent callers
- **WHEN** a direct user of the MCP tool adapter calls a tool that returns `isError: true` with text "quota exceeded"
- **THEN** the call returns an error whose message contains "quota exceeded"

### Requirement: Structured-only MCP results are visible to the model
When an MCP tool result has no content blocks but carries non-null structured content, the system SHALL give the model the structured content serialized as JSON text. When content blocks are present, the system SHALL return them and SHALL NOT add the structured content again. A `null` structured content SHALL be treated as absent.

#### Scenario: Only structuredContent present
- **WHEN** an MCP tool returns an empty `content` array and `structuredContent` `{"temp": 21}`
- **THEN** the tool result given to the model is the text `{"temp":21}`

#### Scenario: Both content and structuredContent present
- **WHEN** an MCP tool returns text content "21 degrees" and `structuredContent` `{"temp": 21}`
- **THEN** the tool result given to the model contains "21 degrees" only

#### Scenario: Null structuredContent
- **WHEN** an MCP tool returns an empty `content` array and `structuredContent` `null`
- **THEN** the tool result given to the model is empty

### Requirement: Unknown MCP content is preserved
When an MCP tool result contains a content block or embedded resource kind the system does not recognise, the system SHALL pass it to the model as its JSON text rather than dropping it.

#### Scenario: Unrecognised content block
- **WHEN** an MCP tool result contains a content block of an unrecognised type
- **THEN** the tool result given to the model includes that block's JSON text

### Requirement: MCP input requests are rejected explicitly
When an MCP server answers a tool call by requesting additional client input (an MRTR `input_required` result, or a task in status `input_required`), the system SHALL fail the tool call with an error stating that additional input is not supported. For a task, the system SHALL also request cancellation of that task.

#### Scenario: Tool call requires input
- **WHEN** an MCP server answers a tool call with an `input_required` result
- **THEN** the tool call is recorded as failed with an error stating that additional input is not supported

### Requirement: MCP tool progress is forwarded to clients
For every MCP tool call made over a connection whose client handler the system owns, progress notifications received for the in-flight call SHALL be published to ACP clients as in-progress updates of that tool call, carrying the progress message and, when the total is known and positive, the percentage completed rounded down. Calls over externally owned connections SHALL NOT forward progress.

#### Scenario: Server sends progress
- **WHEN** an in-flight MCP tool call receives a progress notification with progress 3, total 10, and message "indexing"
- **THEN** ACP clients receive a `tool_call_update` for that tool call with status `in_progress` showing "indexing (30%)"

#### Scenario: Progress without total
- **WHEN** an in-flight MCP tool call receives a progress notification with message "indexing" and no total
- **THEN** ACP clients receive an in-progress update showing "indexing" with no percentage

#### Scenario: Externally owned connection
- **WHEN** an MCP tool is called over an already-connected peer supplied by the embedding runtime
- **THEN** no progress is forwarded for the call and it behaves as before

### Requirement: MCP progress updates are throttled, ordered, and transient
Progress updates SHALL be rate-limited to at most one per tool call per 250 ms. They SHALL NOT be published after the tool call's final update, and SHALL NOT be persisted in session history.

#### Scenario: Progress flood is throttled
- **WHEN** a server sends 20 progress notifications for one tool call within 100 ms and the call continues running for another 500 ms
- **THEN** ACP clients receive at most 2 progress updates for it within that 100 ms window
- **AND** the last update received reflects the 20th notification

#### Scenario: Progress after completion is ignored
- **WHEN** a progress notification arrives, or a throttled update is still pending, for a tool call that has already ended
- **THEN** no progress update is published for it
- **AND** the tool call's final update is the last update clients receive for it

#### Scenario: Progress is not replayed
- **WHEN** a session that received MCP progress updates is reloaded
- **THEN** the replayed history contains the tool call start and end but no progress updates

### Requirement: Cancelling a run cancels in-flight MCP tool calls on the server
When a run is cancelled while an MCP tool call is in flight, the system SHALL notify the MCP server that the request is cancelled and SHALL produce exactly one error result for that tool call without waiting for the server to respond.

#### Scenario: User cancels during a slow MCP tool
- **WHEN** the user cancels a run while an MCP tool call has not yet returned
- **THEN** the MCP server receives `notifications/cancelled` for that request
- **AND** the tool call ends with a single error result whose text contains "Cancelled by user"
- **AND** no progress update for that tool call is published afterwards

### Requirement: Task support is declared only when configured
The system SHALL declare task support during initialization only to MCP servers whose configuration enables tasks.

#### Scenario: Tasks enabled
- **WHEN** task support is enabled in a server's configuration
- **THEN** the system declares task support to that server during initialization

#### Scenario: Tasks not enabled
- **WHEN** task support is not enabled in a server's configuration
- **THEN** the system does not declare task support to that server

### Requirement: Server-side long-running tasks are followed to completion
Whenever a server answers a tool call by creating a task, the system SHALL follow it until it reaches a terminal state and SHALL use the final result as the tool result. Non-terminal task status SHALL be published as progress of that tool call.

#### Scenario: Task completes
- **WHEN** a tasks-enabled MCP server answers a tool call by creating a task that later completes with text "done"
- **THEN** the tool result given to the model is "done"
- **AND** ACP clients received progress updates while the task was working

#### Scenario: Task completes with an in-band error
- **WHEN** a task completes with a result whose `isError` is `true`
- **THEN** the tool call is recorded as failed with the result's content

#### Scenario: Task fails
- **WHEN** a task created for a tool call ends in state `failed`
- **THEN** the tool call is recorded as failed with the server's error message

### Requirement: Task polling follows server hints
While following a task, the system SHALL poll at the server's suggested interval, capped at 10 seconds, or every 1 second when none is given. It SHALL poll earlier when the server pushes a task status notification.

#### Scenario: Interval capped
- **WHEN** a server suggests a poll interval of 60 seconds
- **THEN** the system polls the task at most 10 seconds apart

#### Scenario: Status notification shortens the wait
- **WHEN** the server pushes a task status notification before the poll interval elapses
- **THEN** the system fetches the task state without waiting for the full interval

### Requirement: Cancelling a run cancels server-side tasks
When a run is cancelled while a tool call is being followed as a server-side task, the system SHALL request cancellation of that task on the server. When the run is cancelled before the server has acknowledged the task, the system SHALL send only the request cancellation notification.

#### Scenario: Cancel during task
- **WHEN** the user cancels a run while a tool call's task is in state `working`
- **THEN** the system sends a task cancellation request for that task to the server
- **AND** the tool call ends with a single error result whose text contains "Cancelled by user"

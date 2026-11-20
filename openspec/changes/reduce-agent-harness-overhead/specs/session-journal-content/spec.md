# Spec Delta

## Purpose

Defines which session activity is stored in the durable event journal, which is delivered to live connections only, and which is not recorded at all, so the journal holds each fact once.

## ADDED Requirements

### Requirement: Tool calls and model responses produce no progress records
The agent SHALL NOT store a progress entry and SHALL NOT emit a `progress_recorded` event for the start of a tool call or for the receipt of a model response. The tool-call start event and the model request-end event SHALL remain the only records of those facts. Progress entries of other kinds (artifacts, checkpoints, notes written by tools) SHALL be recorded as before.

#### Scenario: Tool call leaves no progress record
- **WHEN** the model calls `shell` and the call completes
- **THEN** the journal contains the tool-call start and end events for that call and no `progress_recorded` event
- **AND** no progress entry is stored for the call

#### Scenario: Model response leaves no progress record
- **WHEN** a model request completes with two tool calls
- **THEN** the journal contains the request-end event and no `progress_recorded` event for the response

#### Scenario: Artifact progress is still recorded
- **WHEN** a tool records an artifact as progress
- **THEN** the journal contains a `progress_recorded` event for that artifact

### Requirement: Per-call snapshot events are live-only
`snapshot_start` and `snapshot_end` events of a tool call SHALL be delivered to attached connections in production order and SHALL NOT be stored in the journal. The snapshot record in message history, which undo and redo use, SHALL be stored as before.

#### Scenario: Snapshot events reach a live connection
- **WHEN** snapshots are enabled, a connection is attached and the model calls `edit`
- **THEN** the connection receives `snapshot_start` before the tool-call end event and `snapshot_end` after it, in production order

#### Scenario: Snapshot events are not journaled
- **WHEN** snapshots are enabled and the model calls `edit` on one file
- **THEN** the journal contains no `snapshot_start` or `snapshot_end` event for that call

#### Scenario: Undo still works without journaled snapshot events
- **WHEN** a turn edits a file and the user undoes that turn after reloading the session
- **THEN** the file is restored to its pre-turn content

### Requirement: The model selection is recorded only when it changes
The agent SHALL store a `provider_changed` event only when the session's provider, model, configuration or provider node differs from the session's most recent `provider_changed` event, or when the session has none. Loading, resuming or re-attaching a session whose selection is unchanged SHALL NOT store one.

#### Scenario: Loading a session does not repeat the selection
- **WHEN** a session whose last `provider_changed` event names `anthropic` / `claude-sonnet-5-5` is loaded with that selection unchanged
- **THEN** no `provider_changed` event is stored

#### Scenario: Switching the model is recorded
- **WHEN** the user changes the session's model from `claude-sonnet-5-5` to `claude-opus-5-5`
- **THEN** one `provider_changed` event naming `claude-opus-5-5` is stored

#### Scenario: New session records its selection
- **WHEN** a session is created and configured with a model
- **THEN** exactly one `provider_changed` event is stored for it

#### Scenario: Re-selecting the current model
- **WHEN** the user selects the model the session already uses
- **THEN** no `provider_changed` event is stored

### Requirement: Journals written before this change stay readable
A session whose journal contains `progress_recorded` events for tool calls or model responses, journaled `snapshot_start` or `snapshot_end` events, or repeated `provider_changed` events SHALL load, replay and export without error, and those events SHALL be handled as they were when they were written.

#### Scenario: Older session loads
- **WHEN** a client loads a session recorded before this change
- **THEN** the load succeeds and the conversation, tool calls and usage match what the session showed before

#### Scenario: Older session exports
- **WHEN** a session recorded before this change is exported
- **THEN** the export succeeds and contains every turn of the session

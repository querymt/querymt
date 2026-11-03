# Spec Delta

## Purpose

Defines how much of a session's history a load returns, how older history and full tool content are served on demand, and that local and remote sessions behave identically.

## ADDED Requirements

### Requirement: Turn boundaries
A turn SHALL be one agent run. It starts at the run's first event, including the user prompt blocks and run-start events that precede the prompt, and it includes every prompt received while the run is active, such as steering. Windowed loads, older-history requests and remote transfer boundaries SHALL all use this definition.

#### Scenario: Window starts with a complete run
- **WHEN** a windowed load returns its first (oldest) turn
- **THEN** the response includes that turn's prompt blocks, attachments and run-start event

#### Scenario: Steering does not start a new turn
- **WHEN** the user steers a running turn with another prompt
- **THEN** the run counts as one turn toward the window limit

### Requirement: Recent-window session load
When a client requests a load window, the agent SHALL return only the most recent turns of the session: at most 20 turns and at most 1 MiB of event data, measured as JSON-encoded size after truncation. The window SHALL start at a turn boundary and SHALL contain at least one complete turn when the session has any history.

#### Scenario: Long session returns its recent turns
- **WHEN** a client that requests a load window loads a session with 200 turns
- **THEN** the load returns the last 20 turns and reports that older history exists

#### Scenario: Byte cap shortens the window
- **WHEN** the last 20 turns of a session exceed 1 MiB
- **THEN** the load returns the most recent whole turns that fit within 1 MiB, starting at a turn boundary

#### Scenario: Single oversized turn
- **WHEN** the most recent turn alone exceeds 1 MiB
- **THEN** the load still returns that complete turn

#### Scenario: Short session is complete
- **WHEN** a session has fewer than 20 turns within 1 MiB
- **THEN** the load returns the whole history and reports that no older history exists

### Requirement: Whole-session state in a windowed load
A windowed load SHALL report, computed over the entire session: token usage totals, cost accounting, delegation state, the current model and provider, queued and steered inputs that are still pending, and links to child sessions. After a windowed load, live usage updates SHALL continue to report whole-session totals.

#### Scenario: State matches a full load
- **WHEN** the same session is loaded once with a window and once without
- **THEN** both loads report identical token totals, accounting, delegation state, current model, pending inputs and child-session links

#### Scenario: Model chosen before the window
- **WHEN** the session's last model change is older than the returned window
- **THEN** the load still reports that model as current

#### Scenario: Remote session whose older history is not yet local
- **WHEN** a windowed load of a remote session is answered before the attaching agent holds the session's whole history
- **THEN** the whole-session state is computed from the events held so far and is marked partial, and a later load, once the history is complete locally, reports the exact state

#### Scenario: Usage after a windowed load
- **WHEN** a request completes after a windowed load
- **THEN** the reported session totals equal the whole-session totals before the request plus that request's usage

### Requirement: Older history on demand
The agent SHALL provide a request that returns the turns immediately preceding a position reported by a load or a previous older-history response, using the same turn and byte limits as the load window. Positions SHALL remain valid while the session's history is only appended to. Repeated requests SHALL return every event exactly once, in conversation order, until the beginning of the session.

#### Scenario: Scrolling to the start
- **WHEN** the client repeatedly requests older history, starting from the position reported by the load
- **THEN** the combined results contain every event of the session exactly once, in conversation order, and the final response reports that no older history exists

#### Scenario: History rewritten since the position was reported
- **WHEN** older history is requested after an undo has removed or changed events since the position was reported
- **THEN** the request fails with a history-changed error, so the client can reload the session

#### Scenario: Unknown session
- **WHEN** older history is requested for a session the agent does not know
- **THEN** the request fails with a session-not-found error

### Requirement: Truncated tool content in loads
A load or older-history response SHALL replace any tool result, and any string tool-call argument, larger than 32 KiB with a valid UTF-8 leading portion and a marker giving the full size. Stored history SHALL never be truncated.

#### Scenario: Large tool result
- **WHEN** a session contains a tool result of 500 KiB
- **THEN** the load contains at most 32 KiB of that result together with a truncation marker reporting 500 KiB

#### Scenario: Large tool argument
- **WHEN** a session contains a file-write tool call whose content argument is 5 MB
- **THEN** the load contains at most 32 KiB of that argument together with a truncation marker reporting its full size

#### Scenario: Small tool content
- **WHEN** a tool result or argument is 32 KiB or smaller
- **THEN** the load contains it unchanged with no truncation marker

### Requirement: Full tool content on demand
The agent SHALL provide a request that returns the complete result or the complete arguments of a tool call in a session, for local and remote sessions.

#### Scenario: Complete result of a truncated tool call
- **WHEN** full content is requested for the result of a tool call whose result was truncated in a load
- **THEN** the response contains the complete, untruncated result

#### Scenario: Complete arguments of a truncated tool call
- **WHEN** full content is requested for the arguments of a tool call whose arguments were truncated in a load
- **THEN** the response contains the complete, untruncated arguments

#### Scenario: Unknown tool call
- **WHEN** full content is requested for a tool call that does not exist in the session
- **THEN** the request fails with a tool-call-not-found error

### Requirement: Identical behaviour for local and remote sessions
Windowed loads, older-history requests, truncation and full-content requests SHALL behave the same for local sessions and for remote sessions, through both session load and remote attach.

#### Scenario: Remote session load
- **WHEN** a client loads or attaches a remote session with a load window
- **THEN** the response has the same window, whole-session state, history position and truncation as a local session with the same events

### Requirement: Compatibility with clients and agents without windowing
The agent SHALL advertise windowed loading and full-content requests as capabilities. A load without a window request SHALL return the full, untruncated history, in the same form as before.

#### Scenario: Client without a window request
- **WHEN** a client that does not request a load window loads a session
- **THEN** the agent returns the full history with no truncated tool content

#### Scenario: Capabilities advertised
- **WHEN** a client reads the agent's capabilities
- **THEN** windowed loading and full tool content are reported as separate capability flags

#### Scenario: Client that receives history as session updates
- **WHEN** a client that does not use load snapshots loads a session
- **THEN** it receives the same history updates as before

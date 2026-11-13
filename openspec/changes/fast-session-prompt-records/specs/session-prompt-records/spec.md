# Spec Delta

## Purpose

Defines the structured user prompt records that accompany session loads, older-history pages and peer prompt requests: what each record contains, which prompts are included, how they are ordered, and that producing them costs only the prompt content read.

## ADDED Requirements

### Requirement: Prompt record content
A prompt record SHALL describe one user prompt or steering message: its message id, its zero-based position among all of the session's messages in message history order, its creation timestamp, and the content blocks of the message's first prompt or steering part. Messages without a prompt or steering part SHALL produce no record.

#### Scenario: Prompt and steering messages produce records
- **WHEN** a session holds a prompt message with a text block and an image block, an assistant reply with tool calls, and a steering message
- **THEN** its prompt records are exactly one record for the prompt with both blocks and one for the steering message, each carrying its message id and timestamp

#### Scenario: Record position counts every message
- **WHEN** a session's messages are a prompt, three assistant or tool messages, and a second prompt
- **THEN** the first prompt's record has position 0 and the second prompt's record has position 4

### Requirement: Message history order
A session's message history SHALL be ordered by creation time, and messages created within the same second SHALL be ordered by when they were stored. Prompt record positions SHALL use the same order as the full message history.

#### Scenario: Same-second messages keep insertion order
- **WHEN** three messages of one session are stored in the same second
- **THEN** the full message history lists them in the order they were stored
- **AND** the prompt records' positions match their indexes in that history

### Requirement: Prompt record selection
A request for prompt records of specific message ids SHALL return records for exactly those ids that are prompt or steering messages of the session, in position order, and SHALL ignore unknown ids and ids of other messages. A request without ids SHALL return every prompt record of the session in position order.

#### Scenario: Requested ids select their records
- **WHEN** a peer asks the session owner for prompt records of two prompt message ids, one assistant message id and one unknown id
- **THEN** the reply contains exactly the two prompt records, in position order

#### Scenario: Request without ids returns every prompt
- **WHEN** prompt records are requested without message ids for a session with five prompts and one steering message
- **THEN** six records are returned in position order

### Requirement: Load snapshots include the window's prompt records
A full session load SHALL include every prompt record of the session. A windowed load that reports older history SHALL include the prompt records of the prompts whose prompt events are in the returned window, and no others; a windowed load that covers the whole session SHALL include every prompt record of the session. A prompt is identified by the message id carried on its prompt event.

#### Scenario: Full load includes every prompt
- **WHEN** a client loads a local session with 30 prompts without a load window
- **THEN** the snapshot's prompt records contain all 30 prompts

#### Scenario: Windowed load includes only window prompts
- **WHEN** a client loads a local session with 30 runs using a window of 20 turns
- **THEN** the snapshot's prompt records are exactly those of the 20 runs in the window

#### Scenario: Windowed load of a short session includes every prompt
- **WHEN** a client loads a local session with 5 runs using a window of 20 turns
- **THEN** the snapshot reports no older history and its prompt records contain all 5 prompts

#### Scenario: Window first prompt shares a second with an older prompt
- **WHEN** the first run of a returned window starts in the same second as an older prompt outside the window
- **THEN** the snapshot includes the window's first prompt record and excludes the older prompt's record

### Requirement: Prompt records read only prompt content
Producing prompt records or prompt blocks for history replay SHALL read only the prompt and steering content of the session's messages, so its cost does not grow with other message content. Other message content that cannot be decoded SHALL NOT cause a prompt record request, a session load, a history replay or a peer's prompt request to fail.

#### Scenario: Large tool output does not slow prompt records
- **WHEN** prompt records of 20 message ids are requested for a session with 1,000 messages that hold 50 MB of tool results
- **THEN** the reply takes less than 250 ms on the test machine

#### Scenario: Corrupt tool result does not fail a load
- **WHEN** a session holds a stored tool result whose content cannot be decoded and a client loads that session
- **THEN** the load succeeds and its snapshot includes the session's prompt records

#### Scenario: Corrupt tool result does not fail a peer prompt request
- **WHEN** a peer asks the owner for the prompt records of a session that holds a tool result whose content cannot be decoded
- **THEN** the owner replies with the requested prompt records

#### Scenario: Corrupt tool result does not fail session update replay
- **WHEN** a client that receives history as session updates loads a session that holds a tool result whose content cannot be decoded
- **THEN** the replay succeeds and each replayed user message carries its structured prompt blocks

### Requirement: Prompt record reads are traced
Reading prompt records and reading a session's full message history SHALL each be recorded as a tracing span named `session_store.get_user_prompts` and `session_store.get_history` respectively, carrying the session id and the number of rows and content bytes read.

#### Scenario: Prompt record read emits a span
- **WHEN** prompt records are read for a session with three prompts
- **THEN** a `session_store.get_user_prompts` span is recorded with that session id, 3 rows and the bytes of those three records' content

#### Scenario: Full history read emits a span
- **WHEN** the full message history of a session with 10 message parts is read
- **THEN** a `session_store.get_history` span is recorded with that session id, 10 rows and the bytes of those parts' content

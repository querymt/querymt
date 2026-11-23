## Purpose

Lets ACP clients address stable positions in a session's timeline through announced cursors and fork a session at one of those positions.

## ADDED Requirements

### Requirement: Cursor announcement
WHEN a client advertises the `session.cursors` capability, QueryMT SHALL send a `cursor_update` anchored `after` each user message, agent message, agent thought, tool call and compaction. The first `cursor_update` for a cursor id SHALL include its anchor. Each cursor's `cursor_update` SHALL follow the update that introduces its anchored entity. Clients that did not advertise `session.cursors` SHALL receive no `cursor_update`.

#### Scenario: Live turn announces cursors after its entities
- **WHEN** a client that advertised `session.cursors` prompts a session and the turn produces an agent message and one tool call
- **THEN** the client receives a `cursor_update` anchored `after` the user message, one anchored `after` the agent message, and one anchored `after` the tool call
- **AND** each `cursor_update` arrives after the update that introduced its anchored entity

#### Scenario: Compaction announces a cursor
- **WHEN** a compaction completes in a session whose client advertised `session.cursors`
- **THEN** the client receives a `cursor_update` anchored `after` that compaction's id

#### Scenario: Client without the cursors capability
- **WHEN** a client omits `session.cursors` or sends it as `null`
- **THEN** it receives no `cursor_update` for any session, live or replayed

### Requirement: Stable cursor identity
A cursor id SHALL identify the same anchor for the session's lifetime, across live delivery, history replay, reconnects and agent restarts. When history is replayed as session updates to a client that advertised `session.cursors`, QueryMT SHALL announce a cursor for every replayed anchor.

#### Scenario: Replay reproduces the same cursor ids
- **WHEN** a client that advertised `session.cursors` loads a session whose history is replayed as session updates
- **THEN** it receives a `cursor_update` for every anchor in the replayed history
- **AND** each cursor id equals the id announced for the same anchor while the session was live

#### Scenario: Cursor ids survive an agent restart
- **WHEN** the agent restarts and a client reloads a session it saw before the restart
- **THEN** the cursor ids announced for existing anchors are unchanged

### Requirement: Forkable cursors
QueryMT SHALL advertise `capabilities.fork` on cursors anchored after a user message or an agent message, and SHALL NOT advertise it on cursors anchored after thoughts, tool calls or compactions. A cursor's fork capability SHALL stay the same for the session's lifetime.

#### Scenario: Message cursors are forkable
- **WHEN** QueryMT announces a cursor anchored after a user message or an agent message
- **THEN** that `cursor_update` carries `capabilities.fork`

#### Scenario: Tool-call cursors are not forkable
- **WHEN** QueryMT announces a cursor anchored after a tool call, thought or compaction
- **THEN** that `cursor_update` carries no fork capability

### Requirement: Fork at a cursor
`session/fork` SHALL accept `cursorId`. For a forkable cursor that QueryMT announced on the source session, the fork's history SHALL be the source history through the anchored message, the same result as forking at that message with `_meta.querymt.message_id`. WHEN both are given, `cursorId` SHALL take precedence. WHEN neither is given, the fork SHALL keep its existing whole-session behaviour.

#### Scenario: Fork at an agent message cursor
- **WHEN** a client forks session `s` with the `cursorId` announced after the second agent message
- **THEN** the new session contains the source history through that agent message and nothing after it
- **AND** its workspace handling matches a fork at the same message via `_meta.querymt.message_id`

#### Scenario: Message id hint still works
- **WHEN** a client forks without `cursorId` but with `_meta.querymt.message_id`
- **THEN** the fork is created at that message as before

#### Scenario: Cursor takes precedence over the message id hint
- **WHEN** a client forks with a forkable `cursorId` and a different `_meta.querymt.message_id`
- **THEN** the fork is created at the cursor's message

#### Scenario: Remote-hosted source session
- **WHEN** a client forks a remote-hosted session at a forkable cursor
- **THEN** the fork is created at the same message boundary as for a local session with the same history

### Requirement: Invalid fork cursors are rejected
A `session/fork` whose `cursorId` is unknown, belongs to another session, or is not forkable SHALL fail with error code `-32602`, SHALL create no session, and SHALL NOT fall back to forking the whole session.

#### Scenario: Unknown cursor is rejected
- **WHEN** a client forks with a `cursorId` that QueryMT never announced on the source session
- **THEN** the request fails with code `-32602`
- **AND** no fork session exists afterwards

#### Scenario: Non-forkable cursor is rejected
- **WHEN** a client forks with the `cursorId` anchored after a tool call
- **THEN** the request fails with code `-32602` and no fork session is created

#### Scenario: Cursor from another session is rejected
- **WHEN** a client forks session `a` with a forkable `cursorId` that was announced on session `b`
- **THEN** the request fails with code `-32602` and no fork session is created

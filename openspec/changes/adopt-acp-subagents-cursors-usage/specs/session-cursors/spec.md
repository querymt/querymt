## Purpose

Lets ACP clients address stable positions in a session's timeline through announced cursors and fork a session at one of those positions.

## ADDED Requirements

### Requirement: Cursor announcement
WHEN a client advertises the `session.cursors` capability, QueryMT SHALL send a `cursor_update` anchored `after` each user message, agent message, agent thought and tool call, and after each compaction when the client also advertises `compaction` (see "Compaction cursors follow the compaction capability"). The first `cursor_update` for a cursor id SHALL include its anchor. Each cursor's `cursor_update` SHALL follow the update that introduces its anchored entity. Clients that did not advertise `session.cursors` SHALL receive no `cursor_update`.

#### Scenario: Live turn announces cursors after its entities
- **WHEN** a client that advertised `session.cursors` prompts a session and the turn produces an agent message and one tool call
- **THEN** the client receives a `cursor_update` anchored `after` the user message, one anchored `after` the agent message, and one anchored `after` the tool call
- **AND** each `cursor_update` arrives after the update that introduced its anchored entity

#### Scenario: Compaction announces a cursor
- **WHEN** a compaction completes in a session whose client advertised `session.cursors` and `compaction`
- **THEN** the client receives a `cursor_update` anchored `after` that compaction's id

#### Scenario: Client without the cursors capability
- **WHEN** a client omits `session.cursors` or sends it as `null`
- **THEN** it receives no `cursor_update` for any session, live or replayed

### Requirement: Compaction cursors follow the compaction capability
A cursor anchored after a compaction SHALL be sent only to a client that advertised both `session.cursors` and `compaction`, because it follows the compaction update that client receives.

#### Scenario: Compaction cursor needs the compaction capability
- **WHEN** a client advertised `session.cursors` but not `compaction`, and a compaction completes
- **THEN** it receives no compaction update and no `cursor_update` anchored to that compaction, live or replayed
- **AND** it still receives the cursors of messages, thoughts and tool calls

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
- **THEN** the node that owns the session creates the fork at the same message boundary as for a local session with the same history
- **AND** the fork is hosted on the owner and the forking node records it as a remote session, so loading it works

### Requirement: Invalid fork cursors are rejected
A `session/fork` whose `cursorId` is unknown, belongs to another session, or is not forkable SHALL fail with error code `-32602`, SHALL create no session, and SHALL NOT fall back to forking the whole session. A `_meta.querymt.message_id` that is not a message of the source session SHALL be rejected the same way. The same rules apply when the source is remote-hosted, and the rejection SHALL create no session on either node.

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

#### Scenario: Message id hint from another session is rejected
- **WHEN** a client forks session `a` with `_meta.querymt.message_id` naming a message of session `b`
- **THEN** the request fails with code `-32602` and no fork session is created

#### Scenario: Invalid target on a remote-hosted source
- **WHEN** a client forks a remote-hosted session with an unknown, non-forkable or foreign `cursorId`
- **THEN** the request fails with code `-32602`
- **AND** neither the owning node nor the forking node has a new session

### Requirement: Remote forks keep the local fork contract
A `session/fork` of a remote-hosted session SHALL NOT silently drop a request that the owner cannot receive, and SHALL report the owner's refusals as a local fork would.

#### Scenario: Workspace options are rejected, not dropped
- **WHEN** a client forks a remote-hosted session with workspace options in `_meta.querymt.sessionWorkspace`
- **THEN** the request fails with code `-32602` and `data.code` `remote_fork_workspace_options`
- **AND** neither node has a new session

#### Scenario: Read-only source keeps its error
- **WHEN** a client forks a remote-hosted session whose workspace was handed to a successor or archived
- **THEN** the request fails with the code and data a local fork reports (`-32600`, `data.code` `owner_transferred` or `workspace_archived`)

#### Scenario: Fork is recorded with the owner's metadata
- **WHEN** a remote-hosted fork succeeds
- **THEN** the forking node's bookmark for it has fork origin `user` and the owner's creation time, working directory and title
- **AND** if the forking node could not attach it, the request still succeeds and `session/load` connects it from the bookmark

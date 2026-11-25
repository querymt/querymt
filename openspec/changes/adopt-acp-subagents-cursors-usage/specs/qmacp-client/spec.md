## Purpose

Defines what the `qmacp` ACP client advertises to agents and how it reports subagents, turn usage and session cursors to its callers.

## ADDED Requirements

### Requirement: Advertised client capabilities
`qmacp` SHALL advertise the `subagents` and `session.cursors` client capabilities in `initialize`, in addition to the capabilities it advertised before.

#### Scenario: Initialize advertises subagents and cursors
- **WHEN** `qmacp` connects to an agent
- **THEN** its `initialize` request contains `clientCapabilities.subagents` and `clientCapabilities.session.cursors`

### Requirement: Turn usage in prompt results
WHEN a `session/prompt` response carries `usage`, `qmacp` SHALL include those token counts in the result it reports for that prompt. WHEN the response has no `usage`, the reported result SHALL say that usage is unavailable and SHALL NOT show zero counts.

#### Scenario: Prompt result shows turn usage
- **WHEN** a `qmacp prompt` completes and the agent's response carries `usage` with 1200 input and 300 output tokens
- **THEN** the JSON result contains those token counts for the turn

#### Scenario: Prompt result without usage
- **WHEN** the agent's prompt response has no `usage`
- **THEN** the JSON result reports usage as absent rather than as zero

### Requirement: Subagents in events and load snapshots
`qmacp` SHALL report each received `subagent_update` as a delegation event that carries the child session id, title, description, state and `_meta.querymt` stage. It SHALL report a tool call's `session` reference with that tool call. `qmacp` SHALL build the delegations shown for a loaded session from the snapshot's `subagentUpdates`.

#### Scenario: Live subagent update is reported
- **WHEN** an agent sends a `subagent_update` for child `c1` with state `running` and `_meta.querymt.stage` `verifying`
- **THEN** `qmacp follow` output contains a delegation event for `c1` with state `running` and stage `verifying`

#### Scenario: Load snapshot delegations come from subagent updates
- **WHEN** `qmacp inspect` loads a session whose snapshot contains `subagentUpdates` for two children
- **THEN** its output lists both delegations with their latest state

#### Scenario: Delegate tool call references the child session
- **WHEN** a delegate tool call update carries a `session` content reference to `c1`
- **THEN** the tool call reported by `qmacp` names `c1` as its child session

### Requirement: Fork at a cursor from qmacp
`qmacp` SHALL record the cursors announced for a session and SHALL offer a fork command that sends `session/fork` with a given `cursorId`. An agent error for an invalid cursor SHALL be reported as an RPC error with the agent's code.

#### Scenario: Fork at a cursor
- **WHEN** a user runs the fork command with session `s` and a cursor id announced on `s`
- **THEN** `qmacp` sends `session/fork` with that `cursorId` and reports the new session id

#### Scenario: Invalid cursor error is surfaced
- **WHEN** the agent rejects the fork with code `-32602`
- **THEN** `qmacp` exits with its RPC error classification and reports code `-32602`

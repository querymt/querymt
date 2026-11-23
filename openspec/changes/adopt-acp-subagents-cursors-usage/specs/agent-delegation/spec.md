## ADDED Requirements

### Requirement: Delegated sessions are announced as subagents
WHEN a delegation creates a child session and the parent's client advertised `subagents`, QueryMT SHALL send a `subagent_update` on the parent session with the child session id, title set to the target agent id, and description set to the objective. This announcement SHALL arrive before any live event of the child session or any update that references the child.

#### Scenario: Child is announced before its events
- **WHEN** the root agent delegates to member `coder` with objective `fix the build` and a child session `c1` is created
- **THEN** the parent's client receives a `subagent_update` on the parent session with `sessionId` `c1`, `title` `coder` and `description` `fix the build`
- **AND** it receives that update before any session update for `c1`

#### Scenario: Remote member child is announced
- **WHEN** the root agent delegates to a member hosted on another mesh node
- **THEN** the parent's client receives a `subagent_update` for the remote child session before that child's events

#### Scenario: Client without the subagents capability
- **WHEN** the parent's client omits `subagents`
- **THEN** it receives no `subagent_update`
- **AND** it still receives the delegate tool call and the child session's events

### Requirement: Subagent state follows the delegation lifecycle
QueryMT SHALL report the child's state in `subagent_update`: `running` while the delegation executes, verifies or finalizes, and `idle` once it completes, fails or is cancelled. Verification, finalization and cancel-requested stages SHALL be reported as `running` with `_meta.querymt.stage`. The confirmed child model, provider node and lifecycle timestamps SHALL be carried in `_meta.querymt`.

#### Scenario: Verification is reported as running with a stage
- **WHEN** a delegation enters verification
- **THEN** the parent's client receives a `subagent_update` for the child with state `running` and `_meta.querymt.stage` `verifying`

#### Scenario: Completion reports idle
- **WHEN** a delegation completes
- **THEN** the parent's client receives a `subagent_update` for the child with state `idle`
- **AND** the delegate tool call is reported `completed`

#### Scenario: Failure reports idle and a failed tool call
- **WHEN** a delegation fails after its child session was created
- **THEN** the child's state is reported `idle` and the delegate tool call is reported `failed`

#### Scenario: Selected model is reported in metadata
- **WHEN** the child session confirms model `gpt-5` on node `gpu-node`
- **THEN** a `subagent_update` carries `_meta.querymt` with that model id and provider node id

### Requirement: Delegated children can be cancelled by the client
QueryMT SHALL advertise the cancel capability on a child while its delegation is running, and SHALL clear it once the delegation ends. A `session/cancel` for a running child SHALL cancel that delegation exactly as a delegation cancel requested by the agent would.

#### Scenario: Client cancels a running child
- **WHEN** a client sends `session/cancel` for a running child session whose `subagent_update` advertised cancel
- **THEN** the delegation is cancelled, the delegate tool call is reported `failed`, and the child's state becomes `idle`

#### Scenario: Cancel capability is withdrawn after completion
- **WHEN** a delegation completes
- **THEN** the final `subagent_update` for the child no longer advertises cancel

### Requirement: Delegate tool calls reference their child session
WHEN the parent's client advertised `subagents`, the delegate tool call SHALL carry `session` content referencing the child session once the child has been announced. The reference SHALL NOT be sent before the child's announcing `subagent_update`.

#### Scenario: Tool call gains a session reference
- **WHEN** a delegate tool call creates child `c1`
- **THEN** a tool call update for that call carries `session` content with `sessionId` `c1`, sent after the `subagent_update` announcing `c1`

### Requirement: Subagent state on load and replay
WHEN a client that advertised `subagents` loads a session, it SHALL receive the latest subagent state of every child the session created. Replayed history SHALL contain one materialized `subagent_update` per child, sent before that child is referenced. Load snapshots SHALL carry the same latest updates in `subagentUpdates`, including the live state of delegations still running.

#### Scenario: Replay restores completed delegations
- **WHEN** a client loads a session that delegated twice, both completed
- **THEN** replay contains one `subagent_update` per child with state `idle`, each before that child's session reference

#### Scenario: Snapshot shows a running delegation
- **WHEN** a client loads a session through a load snapshot while a delegation is verifying
- **THEN** `subagentUpdates` contains that child with state `running` and `_meta.querymt.stage` `verifying`

### Requirement: No QueryMT delegation notification
QueryMT SHALL NOT send the `querymt/session/delegationUpdate` extension notification, and load snapshots SHALL NOT contain `delegationUpdates`. Subagent updates are the only reported form of delegation state.

#### Scenario: Delegation produces no extension notification
- **WHEN** a delegation runs from request through completion
- **THEN** no client receives a `querymt/session/delegationUpdate` notification

#### Scenario: Load snapshot has no delegationUpdates
- **WHEN** a client loads a session that delegated
- **THEN** the load snapshot contains `subagentUpdates` and no `delegationUpdates` key

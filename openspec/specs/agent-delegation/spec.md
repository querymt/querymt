# agent-delegation Specification

## Purpose

Defines what happens after an agent delegates a task: the request always runs or fails, and the delegated session's activity and questions reach the user, for local and remote members alike.

## Requirements

### Requirement: A delegation request always runs or fails
In every runtime, whatever configuration created it, an accepted delegation request SHALL be executed on the target member. A request that cannot be executed SHALL fail with an error returned to the delegating agent. A delegation SHALL NOT remain in the requested state without anything executing it.

#### Scenario: Delegation from a plain agent runs
- **WHEN** a runtime built from an `[agent]` configuration has one member and its root agent delegates a task to that member
- **THEN** a session is created for the member and runs the task
- **AND** the delegation is recorded as complete and the root agent receives the result

#### Scenario: Protocol target that cannot be built fails the delegation
- **WHEN** the root agent delegates to a protocol delegation target whose runtime fails to build on first use
- **THEN** the delegation fails with an error naming the target and the cause
- **AND** a later delegation to the same target attempts the build again

### Requirement: Delegation is enabled when the root agent may call the delegate tool
Delegation SHALL be enabled for a runtime exactly when its root agent may call the `delegate` tool: its `tools` list is empty or selects `delegate`, and delegation was not turned off with the deprecated `[quorum] delegation = false`. WHEN delegation is enabled, the root agent SHALL be offered the `delegate` tool. WHEN it is disabled, the root agent SHALL NOT be offered it. Member agents SHALL never be offered the `delegate` tool.

#### Scenario: Unrestricted agent can delegate
- **WHEN** an `[agent]` configuration sets no `tools` restriction and the runtime has a member
- **THEN** the root agent's tool list contains `delegate`
- **AND** a delegation to that member runs

#### Scenario: Agent whose tools exclude delegate cannot delegate
- **WHEN** an `[agent]` configuration sets `tools = ["read_tool"]` and defines one delegate
- **THEN** the root agent's tool list does not contain `delegate`
- **AND** `querymt/agents/list` still lists the configured delegate

#### Scenario: Member agents are not offered the delegate tool
- **WHEN** a configured delegate with no `tools` restriction runs a delegated session
- **THEN** that session's tool list does not contain `delegate`

### Requirement: Delegated activity is visible to the delegator's client
Live events of a delegated session SHALL be delivered to the client that owns the parent session, for every member: configured, protocol, added at runtime, or remote. Delivery SHALL NOT depend on when the member was added or first built relative to when the client connected.

#### Scenario: Protocol target activity reaches the parent's client
- **WHEN** a client owns a session whose root agent delegates to a protocol delegation target
- **THEN** the client receives the delegated session's tool and message events

#### Scenario: Activity of a member added after connect reaches the client
- **WHEN** a client connects, a member is then added, and the root agent delegates to it
- **THEN** the client receives the delegated session's events without reconnecting

#### Scenario: Remote member activity reaches the parent's client
- **WHEN** the root agent delegates to a member hosted on another mesh node
- **THEN** the client that owns the parent session receives the delegated session's events

### Requirement: Interactions raised in delegated sessions reach the user
An approval, permission request or question raised in a delegated session SHALL be delivered to the client that owns the parent session, and the user's answer SHALL resume the delegated turn. No kind of member SHALL fail or skip an interaction only because its session is delegated.

#### Scenario: Protocol target skill approval reaches the user
- **WHEN** a protocol delegation target attached to MCP server `docs` loads skill `docs.guide` and no skill permission pattern matches it
- **THEN** the approval request, offering `Allow`, `Allow for this session` and `Deny`, is delivered to the client that owns the parent session
- **AND** after the user answers `Allow`, the skill loads and the delegated turn continues

#### Scenario: Protocol target tool permission request reaches the user
- **WHEN** a protocol delegation target calls a tool whose permission is `ask`
- **THEN** the permission request is delivered to the client that owns the parent session
- **AND** the user's decision is applied to that tool call

#### Scenario: Remote member interaction reaches the user
- **WHEN** a session delegated to a member on another mesh node raises a permission request
- **THEN** the request is delivered to the client that owns the parent session
- **AND** the user's answer resumes the remote turn

### Requirement: A delegation runs in the runtime that requested it
WHEN several runtimes share one event stream, as profile runtimes do, a delegation request SHALL be executed only by the runtime that owns the requesting session, using that runtime's roster.

#### Scenario: Two profiles delegate at the same time
- **WHEN** profile `a` with member `coder` and profile `b` with member `writer` each have a session that delegates at the same time
- **THEN** the first delegation runs on `coder` and the second on `writer`
- **AND** neither fails with an unknown-agent error

### Requirement: Remote members do not depend on their peer being reachable at startup
A configured remote member SHALL be added to the roster whether or not its peer is reachable when the runtime starts. Delegating to a member whose peer is unreachable SHALL fail with an error naming the peer. Delegating SHALL succeed once the peer is reachable, without a restart.

#### Scenario: Remote member is listed while its peer is unreachable
- **WHEN** a runtime starts with remote agent `gpu` on peer `gpu-node` and that peer is not reachable
- **THEN** `querymt/agents/list` contains `gpu`
- **AND** delegating to `gpu` fails with an error naming `gpu-node`

#### Scenario: Delegation succeeds once the peer appears
- **WHEN** the peer of a listed remote member becomes reachable after the runtime started
- **THEN** the next delegation to that member creates a session on the peer and completes

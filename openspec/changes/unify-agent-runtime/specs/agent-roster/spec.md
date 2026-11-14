# Spec Delta

## Purpose

Defines the set of member agents that a runtime's root agent can delegate to (its roster), how clients read that set, and how it is changed while the runtime is running.

## ADDED Requirements

### Requirement: Every runtime has one roster
Every agent runtime SHALL have exactly one roster: the list of member agents its root agent can delegate to. The roster SHALL exist for every configuration shape and SHALL be empty when no members are configured. Member IDs SHALL be unique within a roster. Each member SHALL record its origin (`config`, `protocol` or `runtime`) and its location (`local` or `remote`).

#### Scenario: Plain agent has an empty roster
- **WHEN** a runtime is built from an `[agent]` configuration with no delegates, no remote agents and no protocol delegation targets
- **THEN** `querymt/agents/list` returns `"agents": []` and `"generation": 0`

#### Scenario: Members from every source are listed together
- **WHEN** a runtime has a configured delegate `coder`, a configured remote agent `gpu` and a protocol delegation target `writer`
- **THEN** `querymt/agents/list` returns all three
- **AND** `coder` has origin `config` and location `local`, `gpu` has origin `config` and location `remote`, and `writer` has origin `protocol` and location `local`

### Requirement: Clients can list the roster
`querymt/agents/list` SHALL return `generation` (an integer) and `agents`, an array ordered by ID. Each entry SHALL contain `id`, `name`, `description`, `capabilities`, `origin` and `location`. Listing SHALL NOT build or start any member. Every runtime SHALL advertise `querymt/agents/list`, `querymt/agents/add`, `querymt/agents/remove` and the notification `querymt/agents/changed` in its capability listing.

#### Scenario: Listing does not build members
- **WHEN** a client lists a roster that contains a protocol delegation target that has never been delegated to
- **THEN** the target is returned
- **AND** the target's runtime has still not been built

#### Scenario: Roster methods are advertised
- **WHEN** a client reads the capabilities of a runtime built from a plain `[agent]` configuration
- **THEN** the three roster methods and the `querymt/agents/changed` notification are listed

### Requirement: Members can be added while the runtime is running
`querymt/agents/add` SHALL add one member described by `agent`, an object tagged by `kind`. `kind: "local"` SHALL accept the keys of a `[[delegates]]` entry except `peer`. `kind: "remote"` SHALL accept the keys of a `[[remote_agents]]` entry and SHALL require a running mesh. A valid request SHALL add the member with origin `runtime` and return the new `generation`. A request that is invalid, or made while delegation is disabled, SHALL fail naming the cause and leave the roster unchanged.

#### Scenario: Member added to a plain agent can be delegated to
- **WHEN** a runtime was started with an empty roster, a client adds a local member `reviewer`, and the root agent then delegates to `reviewer`
- **THEN** a session is created for `reviewer`, the delegation completes, and the root agent receives its result

#### Scenario: Added remote member is listed
- **WHEN** a runtime with a running mesh receives `querymt/agents/add` with `kind: "remote"`, `id: "gpu"` and `peer: "gpu-node"`
- **THEN** `querymt/agents/list` contains `gpu` with origin `runtime` and location `remote`

#### Scenario: Remote member without a mesh is rejected
- **WHEN** a runtime without a mesh receives `querymt/agents/add` with `kind: "remote"`
- **THEN** the request fails with an error stating that a mesh is required
- **AND** the roster and its generation are unchanged

#### Scenario: Duplicate member ID is rejected
- **WHEN** a client adds a member whose `id` equals that of an existing member
- **THEN** the request fails with an error naming the ID and the existing member's origin
- **AND** the roster and its generation are unchanged

#### Scenario: Invalid member is rejected
- **WHEN** a client adds a local member with an unknown key, or with a middleware entry of an unknown type
- **THEN** the request fails with an error naming the key or the middleware
- **AND** the roster and its generation are unchanged

#### Scenario: Peer routing is not accepted for a member added at runtime
- **WHEN** a client adds a local member whose description sets `peer`
- **THEN** the request fails with an error stating that `peer` is only supported for configured delegates
- **AND** the roster and its generation are unchanged

#### Scenario: Adding a member while delegation is disabled is rejected
- **WHEN** a client adds a member to a runtime whose root agent's `tools` list does not select `delegate`
- **THEN** the request fails with an error stating that delegation is disabled and naming the `delegate` tool
- **AND** the roster and its generation are unchanged

### Requirement: Members can be removed while the runtime is running
`querymt/agents/remove` SHALL remove the member with the given `id`, whatever its origin, and return the new `generation`. A delegation already running on that member SHALL be allowed to finish. New delegations to that ID SHALL fail as an unknown agent. Removing an unknown ID SHALL fail and leave the roster unchanged.

#### Scenario: Removed member is no longer a delegation target
- **WHEN** a client removes member `coder` and the root agent then delegates to `coder`
- **THEN** the delegate tool call fails with an unknown-agent error
- **AND** no delegation is recorded

#### Scenario: Running delegation survives removal of its member
- **WHEN** a delegation to `coder` is running and a client removes `coder`
- **THEN** the removal succeeds
- **AND** the running delegation completes and the root agent receives its result

#### Scenario: Removing an unknown member fails
- **WHEN** a client removes an ID that is not in the roster
- **THEN** the request fails with an unknown-agent error
- **AND** the generation is unchanged

### Requirement: Roster changes are announced to clients
Each successful add or remove SHALL increase the roster generation by exactly one and SHALL send `querymt/agents/changed` with the new `generation` to every connected client. A failed request SHALL send nothing.

#### Scenario: Clients are notified of a roster change
- **WHEN** a member is added while two clients are connected
- **THEN** both clients receive `querymt/agents/changed` carrying the generation returned by the add request

### Requirement: The delegate tool lists the current roster
WHEN delegation is enabled, the root agent's `delegate` tool definition SHALL list every current member with its ID, name, description, capabilities and default isolation, ordered by ID. A roster change SHALL appear in the tool definition offered on the next turn of every session of that runtime. The definition SHALL NOT change while the roster is unchanged.

#### Scenario: Delegate tool lists members ordered by ID
- **WHEN** a runtime has members `writer` and `coder` and its root agent starts a turn
- **THEN** the `delegate` tool definition in the model input lists `coder` before `writer`, each with its description and default isolation

#### Scenario: Root agent is told about a member added between turns
- **WHEN** a session has run one turn and a member `reviewer` is then added
- **THEN** the `delegate` tool definition of the session's next turn lists `reviewer`
- **AND** a `tools_available` event is recorded for that turn

#### Scenario: Root agent is told when a member was removed
- **WHEN** a session was offered a `delegate` tool listing `coder` and `coder` is then removed
- **THEN** the `delegate` tool definition of the session's next turn does not list `coder`

#### Scenario: Unchanged roster keeps the tool definition unchanged
- **WHEN** a session runs two turns without any roster change in between
- **THEN** the `delegate` tool definition is identical in both turns
- **AND** no `tools_available` event is recorded for the second turn

### Requirement: Runtime roster changes are not persisted
Roster changes made while running SHALL last only for the life of the runtime. After a restart, or after a profile's runtime is rebuilt, the roster SHALL contain exactly the members that its configuration and protocol directory define.

#### Scenario: Runtime changes are gone after restart
- **WHEN** a member `reviewer` is added and a configured member `coder` is removed, and the runtime is then rebuilt from the same configuration and session database
- **THEN** `querymt/agents/list` contains `coder` and does not contain `reviewer`
- **AND** `generation` is 0

### Requirement: Roster requests address one runtime
The roster methods SHALL accept an optional `profileId`. With `profileId`, a request SHALL apply to that profile's runtime only, and the `querymt/agents/changed` notification SHALL carry the same `profileId`. Without it, a request SHALL apply to the root agent's runtime. An unknown `profileId` SHALL fail.

#### Scenario: Adding a member to one profile leaves another unchanged
- **WHEN** profiles `a` and `b` are loaded and a client adds member `reviewer` with `profileId: "a"`
- **THEN** listing with `profileId: "a"` contains `reviewer`
- **AND** listing with `profileId: "b"` does not, and its generation is unchanged

#### Scenario: Unknown profile is rejected
- **WHEN** a roster method is called with a `profileId` that does not exist
- **THEN** the request fails with an error naming the profile ID

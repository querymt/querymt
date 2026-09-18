## Purpose

Define how the agent discovers, lists, and loads skills from global and project skill directories, and guarantee that skill availability tracks the filesystem at turn boundaries without requiring an agent restart.

## ADDED Requirements

### Requirement: Skill discovery at session start

The agent SHALL discover skills from all configured global and project skill sources when a session starts and SHALL advertise every discovered skill in the skill tool listing (callable ID + description). A protocol skill's callable ID SHALL be its explicit stable `id`, or the existing protocol fallback when `id` is absent; its human-readable `name` SHALL remain display metadata. Project `.agents/skills` sources SHALL be discovered recursively and accept the protocol lowercase `skill.md` spelling; skills whose protocol metadata disables them (`enabled: false`) SHALL be excluded.

#### Scenario: Skills from multiple sources are listed

- **WHEN** the agent session starts with skills present in both a project source (e.g. `.agents/skills`) and a global source
- **THEN** the skill tool listing includes every discovered skill from both sources

#### Scenario: Protocol skill uses its stable ID

- **WHEN** a protocol skill declares an `id` that differs from its human-readable `name`
- **THEN** the skill tool advertises and loads the skill by that stable ID
- **AND** the human-readable name remains available as display metadata

#### Scenario: Disabled protocol skill is excluded

- **WHEN** a skill definition in a project `.agents/skills` source declares `enabled: false`
- **THEN** that skill does not appear in the skill tool listing and cannot be loaded

### Requirement: Added skills appear in the next model-facing schema

The agent SHALL refresh skill discovery before generating each model-facing skill tool schema. WHEN a skill is added to any configured source after the session has started, THEN the skill tool schema generated for the next model request SHALL include it, and loading it by callable ID SHALL succeed without restarting the agent. Refreshes MAY also occur when a schema is requested for a non-turn context.

#### Scenario: Skill added mid-session becomes available

- **WHEN** a valid skill directory is added to a project skill source while a session is in progress
- **THEN** the skill tool schema generated for the next model request includes the new skill, and loading it by callable ID returns its content

### Requirement: Removed skills stop being offered in the next model-facing schema

A successful refresh SHALL use replace semantics: the advertised skill set after the refresh MUST equal the valid, enabled set currently present in the configured sources. WHEN a skill's source directory (or its definition file) is deleted after startup, THEN the skill SHALL no longer appear in the schema generated for the next model request, and invoking it SHALL fail with an error that identifies the callable ID as unavailable and lists the currently available callable IDs, or explicitly states that none are available.

#### Scenario: Deleted skill is no longer listed

- **WHEN** a skill directory is removed from a skill source while a session is in progress
- **THEN** the skill tool schema generated for the next model request does not include the deleted skill

#### Scenario: Stale invocation fails with available names

- **WHEN** the model attempts to load a skill whose source was removed
- **THEN** the tool call fails with an error naming the missing callable ID and enumerating the currently available callable IDs, or stating that none are available

### Requirement: Mid-turn invocation of a newly added skill succeeds

Loading a skill SHALL NOT rely solely on the advertised listing. WHEN the model invokes a skill by callable ID that is present on disk but not yet in the advertised listing, THEN the agent SHALL re-check the skill sources once during that invocation and load the skill if found.

#### Scenario: Invocation of unadvertised but present skill

- **WHEN** a skill is added to a source between two turns and the model invokes it before the listing refresh
- **THEN** the invocation succeeds and returns the skill content

### Requirement: Changed skill content is served after a schema refresh

WHEN a skill's definition file content changes after it has been discovered, THEN the skill tool schema generated for the next model request and subsequent loads of that skill SHALL use the updated content and metadata. Loading an already-registered skill before that schema refresh is not required to re-read its definition file.

#### Scenario: Edited skill description propagates

- **WHEN** a discovered skill's definition file is edited to change its description
- **AND** the skill tool schema is generated for the next model request
- **THEN** the tool listing and subsequent loaded content reflect the edited description

### Requirement: Failed refresh preserves the known skill set

A refresh SHALL publish its replacement skill set atomically. WHEN any configured source that is selected for discovery cannot be traversed completely, THEN the refresh SHALL fail, the agent SHALL retain the previously published skill set unchanged, and the skill tool SHALL remain usable. A missing source directory SHALL count as an empty source rather than a refresh failure. An invalid individual skill definition SHALL be diagnosed and excluded without invalidating otherwise successful source discovery.

#### Scenario: Unreadable source does not publish a partial snapshot

- **WHEN** a refresh encounters an unreadable skill source while other sources are intact
- **THEN** no partial replacement is published
- **AND** previously discovered skills remain listed and loadable

#### Scenario: Missing source is treated as empty

- **WHEN** a previously present configured source directory is deleted
- **THEN** refresh succeeds and removes skills that came only from that source

#### Scenario: Invalid skill does not block valid skills

- **WHEN** a traversable source contains both a malformed skill definition and valid skill definitions
- **THEN** refresh succeeds with the malformed skill excluded and the valid skills available

### Requirement: Permission configuration applies across refreshes

Permission rules configured for a skill SHALL continue to apply to that skill after any refresh. WHEN a skill is configured to be denied or to require approval and it is (re)discovered by a refresh, THEN the same permission behavior SHALL be enforced on invocation.

#### Scenario: Denied skill stays denied after refresh

- **WHEN** a skill is configured as denied and is re-discovered during a refresh
- **THEN** an attempt to load it is still rejected with a permission error

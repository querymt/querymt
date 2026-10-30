# Spec Delta

## Purpose

Defines the portable files the external flow driver reads: the repository workflow file, per-machine agent and role settings and cross-change dependency files, plus their validation and the dry-run plan.

## ADDED Requirements

### Requirement: Repository workflow file
A repository SHALL describe its change workflow in `openspec/flow.yaml`: a schema version, the base branch, gate commands, named resources with capacities, an integration mode and the step graph. The file SHALL contain no machine-specific values such as endpoints, model names or absolute paths.

#### Scenario: Valid workflow file loads
- **WHEN** `openspec/flow.yaml` declares a supported version, a base branch, gates and a step graph
- **THEN** the driver loads it and reports its steps, gates and resources

#### Scenario: Unknown key rejected
- **WHEN** the file contains a key the schema version does not define
- **THEN** loading fails with an error naming the key and its location, and no run starts

### Requirement: Default workflow
When a repository has no `openspec/flow.yaml`, the driver SHALL use a built-in default workflow (implement chunks, verify, review, triage, fix, archive, integrate) that requires gate commands supplied by machine settings or flags.

#### Scenario: No workflow file and no gates
- **WHEN** neither the repository nor the machine settings define a full gate
- **THEN** the run is refused with an error explaining that a full gate is required

### Requirement: Machine agent and role settings
Per-machine settings outside the repository SHALL define named ACP agents, each reached by a WebSocket URL or a stdio command, and SHALL map each workflow role to an agent, an optional profile or model, a context budget (`wrap_up` lower than `stop`) and a read-only flag. They SHALL also set limits for active changes and sessions.

#### Scenario: Role mapped to a WebSocket agent
- **WHEN** the implementer role names an agent with a `ws://` endpoint and a profile
- **THEN** implementation sessions are created on that endpoint with that profile

#### Scenario: Missing role mapping
- **WHEN** the workflow uses a role the machine settings do not define
- **THEN** the run is refused before any workspace is created, naming the role

#### Scenario: Invalid budget
- **WHEN** a role's `wrap_up` is not lower than its `stop`
- **THEN** loading fails with an error naming the role

### Requirement: Cross-change dependency files
The driver SHALL read `openspec/changes/<change>/dependencies.yaml` in the existing pi format (`blocked_by` as names or `{change, reason}`, `not_blocked_by`, `related`), SHALL reject unknown keys and invalid names, and SHALL treat a blocker found only under `openspec/changes/archive/` as satisfied.

#### Scenario: Blocked change waits
- **WHEN** change B lists A under `blocked_by` and both are selected
- **THEN** B starts only after A has been integrated

#### Scenario: Archived blocker
- **WHEN** B's blocker exists only in the archive
- **THEN** the dependency is satisfied

### Requirement: Dependency cycles are refused
The driver SHALL detect cycles among the selected changes before any work starts.

#### Scenario: Cycle
- **WHEN** A is blocked by B and B by A
- **THEN** planning fails listing the cycle, and nothing is created

### Requirement: Dry-run plan
`flow plan` SHALL validate all definitions, connect to each configured agent to report its detected capabilities, resolve the dependency graph and print the ordered plan and the first prompts, without creating sessions, workspaces or files.

#### Scenario: Dry run has no side effects
- **WHEN** the user runs `flow plan` for a set of changes
- **THEN** the plan and agent capabilities are printed, and no session, workspace, branch or file is created

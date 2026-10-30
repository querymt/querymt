# Spec Delta

## Purpose

Defines the portable files that describe a change workflow: the repository workflow file, per-machine role settings and cross-change dependency files, plus how they are validated and previewed.

## ADDED Requirements

### Requirement: Repository workflow file
A repository SHALL describe its change workflow in `openspec/flow.yaml`: a schema version, the base branch, gate commands, named resources with capacities, an integration mode and the step graph. The file SHALL contain no machine-specific values such as model names, endpoints or absolute paths.

#### Scenario: Valid workflow file loads
- **WHEN** `openspec/flow.yaml` declares a supported version, a base branch, gates and a step graph
- **THEN** the workflow loads and its steps, gates and resources are available to a run

#### Scenario: Unknown keys are rejected
- **WHEN** the workflow file contains a key that the schema version does not define
- **THEN** loading fails with an error naming the key and its location, and no run starts

#### Scenario: Unsupported version is rejected
- **WHEN** the workflow file declares a schema version newer than the engine supports
- **THEN** loading fails with an error naming the supported versions

### Requirement: Default workflow
When a repository has no `openspec/flow.yaml`, the system SHALL use a built-in default workflow (implement chunks, verify, review, triage, fix, archive, integrate) that requires only gate commands to be supplied through machine settings or flags.

#### Scenario: Repository without a workflow file
- **WHEN** a run is started in a repository without `openspec/flow.yaml` and gate commands are supplied
- **THEN** the run uses the built-in default workflow and reports that it did so

#### Scenario: No gate commands anywhere
- **WHEN** neither the repository nor the machine settings define gate commands
- **THEN** the run is refused with an error explaining that at least a full gate is required

### Requirement: Machine role settings
Per-machine settings, kept outside the repository, SHALL map each workflow role (for example implementer and reviewer) to a QueryMT profile or provider and model, a context budget with `wrap_up` and `stop` token thresholds, and a read-only flag, and SHALL set concurrency limits for changes and sessions.

#### Scenario: Role resolves to a profile
- **WHEN** the implementer role names an existing profile
- **THEN** implementation sessions in the run are created with that profile

#### Scenario: Missing role mapping
- **WHEN** the workflow uses a role that the machine settings do not define
- **THEN** the run is refused before any workspace is created, naming the missing role

#### Scenario: Invalid budget
- **WHEN** a role's `wrap_up` threshold is not lower than its `stop` threshold
- **THEN** loading fails with an error naming the role

### Requirement: Cross-change dependency files
The system SHALL read `openspec/changes/<change>/dependencies.yaml` in the existing pi format, with the keys `blocked_by` (names or `{change, reason}` entries), `not_blocked_by` and `related`, and SHALL reject unknown keys and invalid change names.

#### Scenario: Blocked change waits for its blocker
- **WHEN** change B lists change A under `blocked_by` and both are selected
- **THEN** B is scheduled only after A has been integrated

#### Scenario: Blocker already archived
- **WHEN** a change lists a blocker that exists only under `openspec/changes/archive/`
- **THEN** the dependency counts as satisfied

#### Scenario: Malformed dependency file
- **WHEN** a dependency file has an unknown top-level key
- **THEN** planning fails with an error naming the file and the key

### Requirement: Dependency cycles are refused
The system SHALL detect cycles in the dependency graph of the selected changes before any work starts.

#### Scenario: Cycle detected
- **WHEN** change A is blocked by B and B is blocked by A
- **THEN** planning fails, the error lists the cycle, and no workspace or session is created

### Requirement: Dry-run plan
The system SHALL provide a dry run that validates all definitions, resolves the dependency graph and prints the ordered plan, the first prompts each step would send and every limit that applies, without creating workspaces, sessions or files.

#### Scenario: Dry run has no side effects
- **WHEN** the user requests a dry run for a set of changes
- **THEN** the plan is printed, and the repository, the session store and the worktree storage are unchanged

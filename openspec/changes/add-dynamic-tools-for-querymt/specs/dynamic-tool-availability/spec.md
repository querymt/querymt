# Spec Delta

## Purpose

Allow a running QueryMT session to change which registered tools are visible to the model at safe boundaries while preserving policy, consistency, and in-flight calls.

## ADDED Requirements

### Requirement: Registration and activation are distinct
The system SHALL distinguish tools known to a session from tools currently exposed to the model.

#### Scenario: Registered inactive tool
- **WHEN** a runtime integration registers a tool as inactive
- **THEN** the tool can be activated later but is not included in model tool definitions

#### Scenario: Active tool is deactivated
- **WHEN** an authorized integration deactivates a registered tool
- **THEN** subsequent model requests omit that tool without deleting its registration

### Requirement: Changes become visible at safe boundaries
Dynamic registration and activation changes SHALL take effect atomically for a subsequent model request and SHALL NOT alter the tool definitions of an in-flight model request.

#### Scenario: Activation during a turn
- **WHEN** a tool is activated while another model request is in flight
- **THEN** the current request retains its original tool set and the next eligible request sees the activation

#### Scenario: Batch activation
- **WHEN** several activation changes are committed together
- **THEN** a model request observes either the previous complete set or the new complete set

### Requirement: In-flight calls retain their generation
A tool call selected from a published tool generation SHALL remain executable against that generation even if the tool is subsequently deactivated or unregistered.

#### Scenario: Deactivation after selection
- **WHEN** the model has selected a tool and the tool is deactivated before execution completes
- **THEN** the selected call can complete using its captured tool implementation

### Requirement: Policy applies to dynamic tools
Dynamic tools SHALL be subject to the session's allowlist, denylist, capability, permission, and validation policies before becoming model-visible or executable.

#### Scenario: Denied tool is activated
- **WHEN** an integration activates a registered tool denied by session policy
- **THEN** the tool remains absent from model definitions and cannot be invoked through dynamic lookup

### Requirement: Name collisions are deterministic
The system SHALL reject ambiguous dynamic registrations or apply an explicitly documented source-precedence rule without silently replacing an unrelated tool.

#### Scenario: Duplicate tool name
- **WHEN** a dynamic registration uses a name already owned by another source and no replacement authority is supplied
- **THEN** registration fails with a diagnostic identifying the collision

### Requirement: Tool-set changes are observable
The system SHALL emit a tool-availability update when the effective model-visible tool set changes and SHALL avoid emitting an update when a requested mutation leaves the effective set unchanged.

#### Scenario: Effective activation change
- **WHEN** activation adds a policy-allowed tool to the model-visible set
- **THEN** observers receive one update describing the new effective tool set

#### Scenario: Idempotent activation
- **WHEN** an already active tool is activated again
- **THEN** no effective-change event is emitted

### Requirement: MCP refresh remains supported
MCP `tools/list_changed` updates SHALL continue to atomically refresh MCP registrations and SHALL compose with session activation policy.

#### Scenario: MCP removes an active tool
- **WHEN** an MCP server refresh no longer includes an active tool
- **THEN** the next eligible model request omits it and observers receive an effective tool-set update

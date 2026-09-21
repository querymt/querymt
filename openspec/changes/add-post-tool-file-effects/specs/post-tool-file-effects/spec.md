# Spec Delta

## Purpose

Expose authoritative filesystem effects from completed tool calls to post-tool integrations without requiring them to infer changes from tool names or arguments.

## ADDED Requirements

### Requirement: PostToolUse follows filesystem reconciliation
When post-tool file-effect tracking is active, the system SHALL complete filesystem reconciliation before invoking `PostToolUse` handlers and SHALL invoke those handlers before the next model request.

#### Scenario: Shell command modifies a file
- **WHEN** a shell tool modifies a workspace file and returns
- **THEN** the matching `PostToolUse` handlers receive the completed file effects before the next model request

#### Scenario: Handler observes canonical tool result
- **WHEN** filesystem reconciliation completes after a tool call
- **THEN** `PostToolUse` receives both the canonical tool result and the reconciled effects from that same call

### Requirement: Tool-agnostic effect detection
The system SHALL detect workspace effects independently of the invoked tool's name, source, and declared arguments.

#### Scenario: Unknown provider tool writes a file
- **WHEN** a provider or MCP tool not known to QueryMT writes a workspace file
- **THEN** the changed file is reported when tracking is active for that call

#### Scenario: Tool makes no filesystem change
- **WHEN** tracking completes for a tool that does not alter the workspace
- **THEN** the hook payload contains a completed empty effect set

### Requirement: Normalized file-effect payload
The `PostToolUse` input SHALL represent added, modified, and removed workspace paths in a normalized, deterministic form and SHALL identify the workspace root used for reconciliation.

#### Scenario: Multiple effect kinds
- **WHEN** one tool adds, modifies, and removes files
- **THEN** each path appears once under its corresponding effect kind in deterministic order

#### Scenario: Path is inside the workspace
- **WHEN** a changed path is reported to a hook
- **THEN** it is represented relative to the identified workspace root

### Requirement: Tracking availability is explicit
The hook contract SHALL distinguish unavailable or disabled tracking from successful tracking that found no changes.

#### Scenario: Tracking unavailable
- **WHEN** QueryMT cannot establish a workspace root or tracking is disabled
- **THEN** the file-effects field is absent or null

#### Scenario: Successful empty reconciliation
- **WHEN** reconciliation succeeds and finds no changes
- **THEN** the file-effects field is present with empty effect collections

### Requirement: Snapshot policy independence
The system SHALL be able to report post-tool file effects without requiring user-visible snapshot persistence or undo support to be enabled.

#### Scenario: Snapshot persistence disabled
- **WHEN** a session disables snapshot persistence but enables a post-tool integration that requires file effects
- **THEN** the integration still receives reconciled file effects

### Requirement: Backward-compatible hooks
Existing `PostToolUse` handlers that do not consume file effects SHALL continue to receive their existing input fields and outputs SHALL retain their existing semantics.

#### Scenario: Legacy handler ignores new field
- **WHEN** an existing handler runs after the file-effects capability is enabled
- **THEN** it can complete without reading the new field and its result is processed as before

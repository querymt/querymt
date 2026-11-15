# Spec Delta

## Purpose

Enable QueryMT users to run the separately installed pi-lens package for automatic changed-file feedback and warm model-callable analysis without emulating Pi's native extension host.

## ADDED Requirements

### Requirement: External installation
The integration SHALL use a separately installed pi-lens package and SHALL NOT require QueryMT Rust crates to link pi-lens, Node, LSP, or language-server libraries.

#### Scenario: pi-lens is installed
- **WHEN** the configured pi-lens executables are available
- **THEN** QueryMT can start the integration without recompilation

#### Scenario: pi-lens is absent
- **WHEN** the configured executable cannot be started
- **THEN** QueryMT reports an actionable notice and otherwise continues without pi-lens

### Requirement: Persistent warm service
QueryMT SHALL support running the pi-lens MCP service for the session so its supported model-facing tools and warm analysis state are available.

#### Scenario: MCP initialization succeeds
- **WHEN** a session starts with pi-lens enabled
- **THEN** supported pi-lens MCP tools are available subject to QueryMT tool policy

#### Scenario: MCP service exits
- **WHEN** the pi-lens service terminates unexpectedly
- **THEN** its tools become unavailable and the agent remains usable

### Requirement: Automatic changed-file analysis
The integration SHALL invoke pi-lens automatic analysis from `PostToolUse` using QueryMT's reconciled changed-file set rather than relying only on tool names or argument fields.

#### Scenario: sed changes a source file
- **WHEN** a shell command modifies a supported source file
- **THEN** pi-lens receives that file for post-tool analysis

#### Scenario: Several files change
- **WHEN** one tool modifies multiple supported files
- **THEN** each changed file is analyzed at most once for that tool call and contributions are deterministically ordered

#### Scenario: No supported files change
- **WHEN** reconciliation finds no changed files supported by pi-lens
- **THEN** automatic analysis is skipped without adding model context

### Requirement: Diagnostic context delivery
Non-empty automatic pi-lens findings SHALL be added to the current tool result context before the next model request and SHALL be subject to configured size limits.

#### Scenario: Analysis finds an error
- **WHEN** pi-lens reports a diagnostic for a changed file within the deadline
- **THEN** the model receives a bounded diagnostic contribution with that tool result

#### Scenario: Analysis is clean
- **WHEN** pi-lens reports no findings
- **THEN** no diagnostic context is added

### Requirement: Observational failure policy
Automatic pi-lens analysis SHALL be fail-open: timeout, process failure, unavailable warm service, or malformed output SHALL NOT turn a successful tool call into an error.

#### Scenario: Hook times out
- **WHEN** automatic analysis exceeds its configured deadline
- **THEN** QueryMT preserves the original tool result and emits a non-fatal notice

### Requirement: Hook wire compatibility
QueryMT SHALL accept the documented pi-lens PostToolUse output envelope, including its camelCase field names, while continuing to accept QueryMT's existing snake_case output.

#### Scenario: pi-lens emits additionalContext
- **WHEN** the hook returns `hookSpecificOutput.additionalContext`
- **THEN** QueryMT parses it as a post-tool context contribution

### Requirement: Lifecycle mapping
The integration SHALL initialize and terminate pi-lens session state with the QueryMT session and SHALL invoke supported turn-end processing at the corresponding QueryMT stop boundary with bounded execution.

#### Scenario: Session closes
- **WHEN** the QueryMT session ends
- **THEN** QueryMT requests pi-lens session shutdown and releases the MCP process

### Requirement: Supported-surface disclosure
Documentation SHALL distinguish supported MCP/hook behavior from unsupported native Pi UI, renderer, command, and event-bus surfaces.

#### Scenario: User reviews compatibility
- **WHEN** a user opens the pi-lens integration documentation
- **THEN** the supported and unsupported surfaces and prerequisite changes are explicitly listed

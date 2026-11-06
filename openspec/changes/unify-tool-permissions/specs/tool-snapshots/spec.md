# Spec Delta

## Purpose

Decides which tool calls take a filesystem snapshot for undo, based on what each tool declares about itself instead of user-maintained tool lists.

## ADDED Requirements

### Requirement: Snapshots follow the tool's declared effect
When a snapshot policy other than none is configured, a tool call SHALL take a snapshot unless the tool declares itself read-only. There SHALL be no configuration key that lists mutating tools or assumes all tools mutate.

#### Scenario: Read-only built-in tool
- **WHEN** snapshots are enabled and the model calls `read_tool`
- **THEN** no snapshot is taken for that call

#### Scenario: Mutating built-in tool
- **WHEN** snapshots are enabled and the model calls `edit`
- **THEN** a snapshot is taken for that call

#### Scenario: Removed keys
- **WHEN** a config sets `mutating_tools` or `assume_mutating`
- **THEN** loading fails with an unknown-field error

### Requirement: Built-in tools declare read-only behavior
Every built-in tool SHALL declare whether it only reads state. Tools that write files, run commands, change tasks, ingest knowledge or delegate work MUST NOT declare themselves read-only.

#### Scenario: Shell is not read-only
- **WHEN** snapshots are enabled and the model calls `shell`
- **THEN** a snapshot is taken

#### Scenario: Search is read-only
- **WHEN** snapshots are enabled and the model calls `search_text`
- **THEN** no snapshot is taken

### Requirement: MCP tools use the server's read-only hint
An MCP tool SHALL be treated as read-only only when its server declares the `readOnlyHint` annotation as true. A missing or false hint MUST be treated as mutating.

#### Scenario: Annotated read-only MCP tool
- **WHEN** snapshots are enabled and the model calls an MCP tool annotated `readOnlyHint: true`
- **THEN** no snapshot is taken

#### Scenario: Unannotated MCP tool
- **WHEN** snapshots are enabled and the model calls an MCP tool without annotations
- **THEN** a snapshot is taken

### Requirement: Provider tools are treated as mutating
A tool executed by the LLM provider SHALL be treated as mutating for snapshot purposes.

#### Scenario: Provider tool call
- **WHEN** snapshots are enabled and the model calls a provider-executed tool
- **THEN** a snapshot is taken

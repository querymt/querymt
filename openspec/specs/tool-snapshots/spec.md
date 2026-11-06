# tool-snapshots Specification

## Purpose

Decides which tool calls take a filesystem snapshot for undo, based on what each tool declares about itself instead of user-maintained tool lists.

## Requirements

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
Every built-in tool SHALL declare whether it leaves workspace files untouched. Tools that write files or run commands, and tools that integrate a delegate's result into the checkout, MUST NOT declare themselves read-only. Tools that only change session or knowledge state (tasks, todos, knowledge, skills, delegation requests) SHALL declare themselves read-only because they never change workspace files.

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

### Requirement: Provider fallback calls take no snapshot
A call that reaches no registered built-in or MCP tool and is forwarded to the LLM client SHALL NOT take a snapshot.

#### Scenario: Provider fallback call
- **WHEN** snapshots are enabled and the model calls a tool that only the LLM client provides
- **THEN** no snapshot is taken

### Requirement: Diff snapshots reuse the previous tree
With the diff policy, the scan taken before a call SHALL be seeded with the workspace tree captured after the session's previous snapshot for the same root, so only files whose size or modification time changed are re-hashed.

#### Scenario: Second mutating call
- **WHEN** a session runs two mutating calls in the same workspace and a file is added between them
- **THEN** the second snapshot reports the added file

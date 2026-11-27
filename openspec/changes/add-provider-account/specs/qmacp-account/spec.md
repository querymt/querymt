# qmacp-account Specification

## Purpose

Defines how the qmacp CLI and the qmcp MCP server expose provider account snapshots to scripts, operators and MCP clients.

## ADDED Requirements

### Requirement: qmacp has an account command

qmacp SHALL provide `qmacp account [--provider <name>] [--refresh]`, which calls `querymt/auth/account` with the matching params and prints the response payload (`{ "accounts": [ … ] }`) as JSON on stdout. Before calling, it SHALL verify that `querymt/capabilities` advertises `features.auth_account` and the method, and otherwise SHALL fail with a non-zero exit and an error naming the missing support, as the lifecycle and mesh commands do.

#### Scenario: Account command prints the server payload
- **WHEN** `qmacp account --provider codex` runs against a server that advertises `auth_account` and answers with one `ok` entry
- **THEN** stdout is the JSON payload with that single entry and the exit code is 0

#### Scenario: Account command refuses an older server
- **WHEN** `qmacp account` runs against a server whose capabilities lack `auth_account`
- **THEN** no `querymt/auth/account` call is made, the exit code is non-zero and the error names the missing support

### Requirement: qmcp has an account tool

qmcp SHALL expose a read-only, idempotent MCP tool `account` with optional arguments `provider` (string) and `refresh` (boolean), which performs the same capability gate and `querymt/auth/account` call as `qmacp account` and returns the payload under `result`, as `models` and `profiles` do. The tool SHALL be listed with the other discovery tools in the qmcp README.

#### Scenario: Account tool returns the payload under result
- **WHEN** an MCP client calls `account` with `provider: "codex"` against a server that advertises `auth_account`
- **THEN** the tool result is `{ "result": { "accounts": [ … ] } }` with the single entry and the tool is annotated read-only

#### Scenario: Account tool refuses an older server
- **WHEN** an MCP client calls `account` against a server whose capabilities lack `auth_account`
- **THEN** the tool returns an error naming the missing support and no `querymt/auth/account` call is made

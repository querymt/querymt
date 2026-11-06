# Spec Delta

## Purpose

Defines how tools are named in configuration and at runtime, how those names are validated, and the single matching rule shared by allowlists, deny lists and approval lists.

## ADDED Requirements

### Requirement: Tool selector forms
A tool selector SHALL be one of: a bare name (`shell`) selecting a built-in or provider tool, `server.tool` selecting one MCP tool, or `server.*` selecting every tool of an MCP server. Any other form, including empty names or empty segments, MUST be rejected.

#### Scenario: Valid selectors
- **WHEN** a config lists `tools = ["shell", "github.create_issue", "ibkr.*"]`
- **THEN** the config loads with one built-in selector, one MCP tool selector and one MCP server selector

#### Scenario: Malformed selector
- **WHEN** a config lists `"github."`, `".create_issue"`, `""` or `"a.*.b"` in `tools`, `permissions.ask` or `permissions.always_ask`
- **THEN** loading fails with an error that names the field and the malformed entry

### Requirement: One matching rule for every tool list
Allowlists, deny lists and approval lists SHALL match tools with the same rule: a bare selector matches only non-MCP tools of that name; `server.tool` matches only that tool from that server; `server.*` matches every tool from that server. A bare selector MUST NOT match an MCP tool.

#### Scenario: Wildcard in an approval list
- **WHEN** `permissions.ask = ["ibkr.*"]` and the model calls `place_order` served by `ibkr`
- **THEN** the call prompts

#### Scenario: Bare name does not match an MCP tool
- **WHEN** `permissions.ask = ["lookup"]` and the model calls `lookup` served by MCP server `alpha`
- **THEN** the call runs without a prompt from the approval list

#### Scenario: Server-scoped allow
- **WHEN** `tools = ["alpha.lookup"]` and servers `alpha` and `beta` both expose `lookup`
- **THEN** only `alpha`'s `lookup` is offered to the model and a call routed to `beta.lookup` is refused as not allowed

### Requirement: Runtime allow and deny lists use selectors
Runtime changes to a session's allowed or denied tools SHALL accept the same selector forms and apply the same matching rule. A malformed selector MUST be rejected without changing the session's lists.

#### Scenario: Deny an MCP server at runtime
- **WHEN** a client sets the session's denied tools to `["github.*"]`
- **THEN** no `github` tool is offered to the model and any call to one is refused as not allowed

#### Scenario: Bare deny does not hide an MCP tool
- **WHEN** a client denies `["lookup"]` and MCP server `alpha` exposes `lookup`
- **THEN** `alpha.lookup` remains available

### Requirement: Approval entries are validated against the configuration
Loading SHALL fail when a `permissions.ask` or `permissions.always_ask` entry names an MCP server that the agent does not configure, or, when the agent sets a non-empty `tools` list, selects a tool that list does not cover.

#### Scenario: Misspelled server
- **WHEN** `[[mcp]] name = "ibkr_trade"` and `permissions.always_ask = ["ibkr-trade.place_order"]`
- **THEN** loading fails with an error naming `ibkr-trade.place_order` and the unknown server

#### Scenario: Approval entry outside the tool list
- **WHEN** `tools = ["read_tool"]` and `permissions.ask = ["shell"]`
- **THEN** loading fails with an error stating `shell` is not in `tools`

### Requirement: Unmatched approval entries are reported after discovery
After an MCP server's tools are discovered, the system SHALL emit a warning for each `server.tool` approval entry of that server that matches no discovered tool.

#### Scenario: Misspelled MCP tool
- **WHEN** `permissions.always_ask = ["ibkr.placeOrder"]` and server `ibkr` exposes only `place_order`
- **THEN** a warning names `ibkr.placeOrder` as matching no tool of `ibkr`

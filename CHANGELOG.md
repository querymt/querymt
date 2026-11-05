# Changelog

## Unreleased

### Breaking

- **`querymt_agent::config::McpServerConfig`** gained fields on both variants
  (`tasks`, `cache`; `auth` and `socket_path` on `Http`) and the variants are
  now `#[non_exhaustive]`. Build values with `McpServerConfig::stdio(..)` /
  `::http(..)` and the `with_*` setters instead of struct literals, and add `..`
  to exhaustive patterns. `McpServerConfig::from_acp` now returns `Result`
  instead of panicking on unknown transports. TOML/JSON configs without the new
  keys keep working: every new key has a default.
- **`McpToolAdapter::call`** now returns `Err(McpToolError::Reported { parts })`
  when an MCP server answers with `isError: true`, instead of `Ok`. The agent
  turns this into a failed tool call (ACP status `failed`, outcome `error`) that
  carries the server's content verbatim. Other callers of the adapter must
  handle the `Err`; its `Display` is the server's text. This can surface
  previously hidden failures to hooks and middleware.
- rmcp requirement raised to `3.8` in `querymt`, `querymt-agent` and
  `querymt-mobile-ffi`.

### Added

- MCP results that carry only `structuredContent` now reach the model as JSON
  text.
- Server progress notifications for MCP tool calls become the transient
  `AgentEventKind::ToolCallProgress` event (throttled to one per 250 ms per
  call, never persisted or replayed, never published after the call's
  `ToolCallEnd`), forwarded to ACP clients as an in-progress `tool_call_update`.
- Cancelling a run sends `notifications/cancelled` for the in-flight MCP call
  (and `tasks/cancel` for a followed task).
- Opt-in MCP tasks extension (`tasks = true`): long-running tasks are followed
  until they finish. `input_required` (MRTR and task status) fails the call
  with a clear error.
- `cache = false` per MCP server disables response caching of tool listings.
- HTTP MCP servers can connect over a Unix domain socket (`socket_path`, Unix
  only).
- OAuth for HTTP MCP servers declared in the agent configuration
  (`auth = { type = "oauth", .. }`). Login uses the `_querymt/auth/*` methods
  with provider id `mcp:<server>`; credentials are kept in the secret store,
  bound to the server URL. A server that needs authorization is skipped and
  reported instead of failing the session, and a completed login applies to
  sessions that connect afterwards. See `docs/docs/agent/configuration.md`.
- New transient events `AgentEventKind::ToolCallProgress` and
  `AgentEventKind::McpServerAuthRequired` (ACP notification
  `querymt/session/mcpAuthRequired`).
- MCP server settings are validated for file configs and for servers supplied by
  ACP clients: an empty or `@` socket path, `socket_path` with `auth`, and
  `auth` with an `Authorization` header are rejected.

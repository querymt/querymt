# Changelog

## Unreleased

### Behavior changes

- **Selecting the profile's model keeps the profile's `[agent.parameters]`**
  (`max_tokens`, `n_ctx`, `enable_thinking`, sampling, ...). Previously any
  model selection, including mesh clients re-selecting the profile's own
  model, rebound the session to a configuration without them. Other models
  still start without the profile's parameters. Re-selecting the model, provider
  and node a session is already bound to is now a no-op, so a configuration set
  with `set_llm_config` survives automatic re-selection. Existing sessions are
  not repaired.
- **`agent_mode` / `plan_mode` `default`** is now applied: sessions start in the
  configured mode (it was ignored and every session started in `build`). The
  planners in `coder_agent`, `coder_delegate` and `learning_pair` now really
  start in plan mode. An unknown mode name fails the config load.
- **Hooks are enabled by default** when any are configured; set
  `[agent.hooks] enabled = false` to switch them off. An invalid hook matcher
  regex now fails the config load instead of never matching.
- **A failing `post_tool_use` hook** no longer turns a successful tool result
  into an error; the result is kept and a `hook_notice` is emitted.
- Hook tool matchers also match the server-qualified MCP name
  (`server.tool`); hook input gained `mcp_server`; hook commands get
  `QMT_PROFILE_DIR`.

### Added

- **`accept_delegation_result` asks by default** before it integrates a
  delegate's result into the checkout, offering Allow once, Allow for this
  session and Reject. List it in `permissions.always_ask` to ask on every call.
  A `pre_tool_use` hook that allows the call skips the prompt; unattended
  quorum profiles need `permissions.timeout_sec` or an allowing hook.
- **Snapshots** skip built-ins that never touch workspace files (`delegate`,
  `skill`, task, todo and knowledge tools) and the provider fallback, and the
  diff policy reuses the previous workspace tree instead of re-hashing every
  file before each call.
- **Tool approval**: `[agent.permissions]` (also `[planner]` and
  `[[delegates]]`) with `ask`, `always_ask` and `timeout_sec`, and
  `permission_decision: "ask"` from `pre_tool_use` hooks, show one standard
  permission prompt before any built-in, MCP or provider tool runs. `ask`
  offers "Allow for this session"; `always_ask` and hook asks never do.
  Python: `ask=`, `always_ask=`, `approval_timeout_sec=`, `mode=`.
- Example profiles `trading_analyst.toml` and `trading_desk.toml` with guard
  hooks under `examples/hooks/trading/`.

### Breaking

- **Tool permission keys removed** (unknown-field errors that name the key):

  | Removed | Replacement |
  | --- | --- |
  | `mutating_tools`, `assume_mutating` | none; snapshots follow each tool's read-only declaration. Approval via `permissions.ask` |
  | `confirm_tools` | `permissions.always_ask` |
  | `confirm_timeout_sec` | `permissions.timeout_sec` |
  | `agent_mode.default` | `mode` on `[agent]`, `[planner]`, `[[delegates]]` |
  | Python `assume_mutating=`, `mutating_tools=` | `ask=`, `always_ask=`, `approval_timeout_sec=` |

  Provider tools no longer prompt by kind; list them in `permissions.ask`.
  A bare name in `tools` or in a deny list no longer matches MCP tools; use
  `server.tool` or `server.*`. Permission option IDs are now `allow_once`,
  `allow_session`, `reject`, and `SetAllowedTools` / `SetDeniedTools` carry
  typed selectors: upgrade all mesh nodes and the desktop UI together.
  Unattended profiles must set `permissions.timeout_sec`.

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

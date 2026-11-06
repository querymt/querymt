# Proposal

## Why

The rmcp 3.x upgrade only changed type names, so QueryMT still uses MCP the way it did with rmcp 1.x. As a result:

- Tool failures that a server reports in-band (`isError`) reach the model as successful results.
- Results that carry only `structuredContent` reach the model empty.
- Progress from long-running tools is dropped.
- Hosted MCP servers that require OAuth can't be used without pasting a token by hand.

rmcp 3.x already implements most of what is needed. The SDK now cancels abandoned calls, dispatches progress, caches responses, supports the Tasks extension, handles OAuth discovery and refresh, and has a Unix-socket transport. Using these is now mostly integration work.

## What Changes

- **Result fidelity:** an MCP result with `isError: true` becomes a failed tool call (ACP status `failed`, tool-call outcome `error`). When `content` is empty, `structuredContent` is passed to the model as JSON text.
- **Progress:** server progress notifications for an MCP tool call made over a connection that QueryMT manages become a new transient agent event, which is forwarded to ACP clients as an in-progress `tool_call_update`. No progress update is ever published after the tool call's final update.
- **Cancellation:** cancelling a run that has an MCP tool call in flight sends `notifications/cancelled` to that server. rmcp already does this when the future is dropped. This change pins it down with tests and a spec.
- **Long-running tasks (SEP-2663):** this is opt-in per server. QueryMT declares the tasks extension and follows a task the server creates until it reaches a terminal state. While it runs, task status is reported as progress, and agent cancellation cancels the task on the server.
- **Response cache (SEP-2549):** tool listings honour the server's TTLs, which rmcp already does by default. Each server gets a config option to turn caching off, and a `list_changed` notification always triggers a fresh fetch.
- **OAuth for HTTP MCP servers:** a new `auth = { type = "oauth", … }` setting on HTTP servers in the agent configuration uses rmcp's authorization (discovery, dynamic client registration, PKCE, refresh). Credentials are kept in QueryMT's secret store, bound to the server's URL. The login flow reuses the existing `_querymt/auth/*` ACP extension and the loopback callback listener. A server that needs authorization is skipped and reported, and does not fail the session. A completed login applies to sessions that connect afterwards.
- **Unix-socket HTTP transport:** HTTP servers can set `socket_path` to reach a Streamable HTTP endpoint over a Unix domain socket. This works on Unix only.
- **Out of scope:** MRTR `input_required` rounds (SEP-2322) on tool calls, and the task `input_required` status. Both fail the tool call with a clear error. Today `input_required` already surfaces as an unexpected-response error.

**BREAKING:** `querymt_agent::config::McpServerConfig` gains new fields on both variants (`auth`, `socket_path`, `tasks`, `cache`). Downstream struct literals and exhaustive patterns must be updated. Backward compatibility of the Rust API is explicitly not a goal of this change. TOML/JSON configs without the new keys keep working, because every new key has a default.

## Capabilities

### New Capabilities
- `mcp-tool-invocation`: how MCP tool calls are run and reported: mapping results and errors, progress, cancellation, and server-side long-running tasks.
- `mcp-server-connection`: how MCP servers are connected and kept usable: OAuth authorization and credential lifecycle, response caching, and transport selection (including Unix sockets).

### Modified Capabilities

None. The existing `tool-batch-scheduling` guarantee of exactly one result per tool call on cancellation is unchanged and still holds.

## Impact

- **Code:**
  - `crates/querymt/src/mcp/adapter.rs` (result mapping, `call_tool_once` loop, task polling, `McpToolError`)
  - `crates/querymt/src/mcp/progress.rs` (new: `ProgressSink`, `McpNotificationRouter`)
  - `crates/agent/src/elicitation.rs` (`McpClientHandler`: `on_progress`, `on_task_status`, client capabilities)
  - `crates/agent/src/agent/execution/tool_calls.rs` (MCP branch)
  - `crates/agent/src/agent/protocol.rs` (transport, auth, cache wiring; per-server auth-required skip)
  - `crates/agent/src/agent/mcp.rs` (`mcp_server_requires_network`, shared server validation)
  - `crates/agent/src/config/mcp.rs` and `config/loader.rs` (new fields, validation)
  - `crates/agent/src/events.rs` and `acp/shared.rs` (new events)
  - `crates/agent/src/auth/service.rs` and `agent/handle/ext_auth.rs` (MCP OAuth flows)
  - `crates/agent/src/mcp_auth.rs` (new: credential store)
  - In-tree `McpServerConfig` constructors and patterns (`events.rs`, `session_registry.rs` tests, `agent_config_builder.rs` tests, `config/tests.rs`, `tests/dotagents_runtime_e2e_test.rs`)
- **Dependencies:** rmcp minimum raised to `3.8` in `querymt`, `querymt-agent` and `querymt-mobile-ffi`. Extra rmcp features: `auth` (behind the agent's `oauth` cargo feature) and `transport-streamable-http-client-unix-socket` (Unix only). `server` is added as a dev-dependency feature for the test harness.
- **APIs:**
  - New transient `AgentEventKind::ToolCallProgress` and `AgentEventKind::McpServerAuthRequired`, exported through typeshare.
  - `_querymt/auth/*` accepts `mcp:<server>` provider ids.
  - New MCP config keys: `auth`, `socket_path` (HTTP only); `tasks`, `cache` (HTTP and stdio).
  - `McpToolAdapter::call` now returns `Err(McpToolError::…)` for in-band errors. This is a behaviour change for every caller of the adapter.
- **`querymt-mobile-ffi`:** only the rmcp version bump. Its `ConnectedMcpPeer` servers get no progress token, because QueryMT does not own their client handler.

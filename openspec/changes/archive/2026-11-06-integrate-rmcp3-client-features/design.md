# Design

## Context

- MCP tools run through `McpToolAdapter::call` (`crates/querymt/src/mcp/adapter.rs`). It implements `CallFunctionTool::call(args) -> Result<Vec<ToolResultPart>>`, calls `Peer::call_tool` (the plain request, with no MRTR handling), keeps only `CallToolResult.content`, and has no access to the session or the tool call id. It holds a `ServerSink` (`Peer<RoleClient>`), not the `RunningService`.
- `execute_prepared_inner` (`crates/agent/src/agent/execution/tool_calls.rs:406`, MCP branch at ~630) maps `Ok` to success and `Err` to error. Everything else (ToolCallStart/End events, ACP mapping, `genai::tool_call_outcome`) is derived from that `is_error` flag.
- `run_prepared_tool_call` (`transitions.rs:2358`) already races execution against `cancellation_token` with `select!`. A cancel drops the MCP future and produces the synthetic result `"Error: Cancelled by user"`. In rmcp ≥3.6, every `Peer` request (including `call_tool_once`, `get_task`, `cancel_task`) goes through `send_request_cancel_on_drop`, whose `AbandonedRequestGuard` sends `notifications/cancelled`. The cancellation requirement is met in code but has no tests.
- `McpClientHandler` (`crates/agent/src/elicitation.rs:1269`) is the `ClientHandler` for agent sessions. It overrides only elicitation and tool-list-changed. Adapters are built in three places: `build_mcp_state` (`protocol.rs:40`), `merge_connected_mcp_peers` (`protocol.rs:94`, for `ConnectedMcpPeer`s whose handler QueryMT does **not** own, e.g. mobile), and `McpClientHandler::on_tool_list_changed`.
- `build_mcp_state` fails the whole session if any server fails to start or list tools (`?` on `start_mcp_server` and `list_all_tools`).
- Session MCP servers are the agent-config servers converted with `to_acp()`, merged by name with ACP-client-supplied servers (`SessionRegistry::merged_mcp_servers`). A client-supplied server with the same name replaces the config one.
- HTTP servers are built in `start_mcp_server` (`protocol.rs:198`) from ACP `McpServerHttp` (name, url, headers, `_meta`). `McpServerStdio` also has `_meta`.
- There is no mechanism to reconnect MCP servers of a live session. `session_mcp.rs` only validates attachments.
- The agent already runs provider OAuth: `AuthService::start_flow(owner, provider)/complete_flow/logout/auth_status`, flows keyed by `FlowIdentity { flow_id, owner, provider }`, a loopback callback listener serving `/callback` and `/auth/callback`, `SecretStore`, and the `_querymt/auth/*` ACP extension (`agent/handle/ext_auth.rs`). `callback_listener_port` only returns a provider's declared port; it does not choose one.
- rmcp 3.8 provides `Peer::call_tool_once -> CallToolResponse::{Complete, InputRequired, Task}`, `get_task`/`cancel_task`, `TASKS_EXTENSION_ID`, `ProgressDispatcher`, `ClientCacheConfig` (enabled by default with `default_ttl = 0`; tool cache invalidated by the service loop *before* `on_tool_list_changed` runs), `Peer::set_response_cache_config`, `AuthClient`/`AuthorizationManager`/`AuthorizationSession`, the `CredentialStore` trait (`load/save/clear/on_refresh_token_rejected`), and `UnixSocketHttpClient::new(&str, &str)` (panics on an empty path or a bare `"@"`).
- `CallToolResult.structured_content` is `Option<Value>` with `#[serde(default)]`; an explicit JSON `null` may arrive as `Some(Null)`, so the adapter treats both as absent.

## Goals / Non-Goals

**Goals:**
- Keep the `CallFunctionTool` trait unchanged.
- Keep result mapping, the call loop, and task polling in `querymt`. Keep event wiring, auth, and config in `agent`.
- Reuse the existing provider OAuth UX and storage rather than adding a second flow.
- Freely change the agent's Rust config API. Backward compatibility of `McpServerConfig` is not a goal.

**Non-Goals:**
- MCP resources and prompts, sampling, roots, and logging. Sampling, roots, and logging are deprecated by SEP-2577.
- MRTR `input_required` rounds (SEP-2322) and the task `input_required` status. They fail the call with a clear error (D6).
- Reconnecting MCP servers of a live session after login. A login applies to sessions that connect afterwards (D8).
- OAuth for MCP servers supplied by ACP clients, or in `querymt-mobile-ffi`, or in the plain `querymt` `McpServerTransportConfig`. Its existing `token` field stays.
- Progress for `ConnectedMcpPeer` servers, whose client handler QueryMT does not own.
- Building a UI for progress. ACP clients already render `tool_call_update` content.
- Enterprise-managed auth, JWT client credentials, and subscriptions (SEP-2575 listen streams).

## Decisions

### D0. Config shape (breaking)
```rust
pub enum McpServerConfig {
    Stdio { name, command, args, env, tasks: bool, cache: bool },
    Http  { name, url, headers, auth: Option<McpAuth>, socket_path: Option<PathBuf>, tasks: bool, cache: bool },
}
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpAuth { OAuth { client_id: Option<String>, scopes: Vec<String>, redirect_port: Option<u16> } }
```
- `tasks` defaults to `false` and `cache` to `true` through serde defaults, so config files without the keys parse.
- The Rust enum gets `#[non_exhaustive]` on both variants, and `McpServerConfig::stdio(name, command)` / `::http(name, url)` constructors with builder-style setters. All in-tree literals and patterns move to these (`events.rs:195`, `session_registry.rs` tests, `agent_config_builder.rs` tests, `config/tests.rs`, `tests/dotagents_runtime_e2e_test.rs`). `events.rs` exhaustive destructuring switches to `..`.
- **ACP round-trip:** the new fields are carried in `_meta.querymt` on `McpServerHttp`/`McpServerStdio` (`{"auth":…, "socketPath":…, "tasks":…, "cache":…}`). Missing `_meta`, or missing keys, means the defaults. `from_acp` no longer panics on unknown variants; it returns `Result`.

### D0a. Shared validation
`validate_mcp_server(&McpServerConfig) -> Result<()>` in `agent/mcp.rs` is called from `config::loader::validate_mcp_servers` (file configs) **and** from `start_mcp_server` after `from_acp` (ACP-supplied servers bypass the loader). It rejects:
- `socket_path` that is empty or exactly `"@"` (so `UnixSocketHttpClient::new` cannot panic)
- `socket_path` together with `auth`
- `auth` together with an `Authorization` header (case-insensitive). The precedence would be ambiguous.

Errors name the server.

### D1. Error mapping via a typed error, not a trait change
New `querymt::mcp::adapter::McpToolError` (`thiserror`):
- `Reported { parts: Vec<ToolResultPart> }`: the server returned `isError: true`. `Display` is the concatenation of the text parts, or `"MCP tool reported an error"` if there are none. Non-agent callers therefore still see the server's message.
- `InputRequiredUnsupported`: `Display` is `"MCP server requested additional input, which is not supported"`.
- `TaskFailed { status, message }`: a task ended `failed` or `cancelled` on the server, or reported `input_required`.
- `Service(ServiceError)`: a transport or protocol error.

In the MCP branch of `execute_prepared_inner`, the error is downcast. `Reported` uses its parts verbatim with `is_error = true`. Any other error keeps the existing `"Error: {e}"` text. `genai::tool_call_outcome` already records `"error"` when `is_error` is set, so the span or metric outcome needs no new code, only a test.

This is a behaviour change for every `McpToolAdapter` caller: in-band errors are now `Err`.

### D2. Structured-content fallback lives in the adapter
In `call()`: if `content` is empty and `structured_content` is `Some(v)`, return `[Text(serde_json::to_string(v))]`. JSON `null` (whether `None` or `Some(Null)`) is treated as absent and yields an empty result. When `content` is non-empty, `structured_content` is never added. Unknown `ContentBlock` and `ResourceContents` variants already map to JSON text from the migration. The same mapping applies to `Reported` parts.

### D3. Progress: per-call sink through a task-local, routed by a QueryMT router
New module `querymt::mcp::progress`:
- `pub struct ProgressUpdate { progress: Option<f64>, total: Option<f64>, message: Option<String> }`.
- `pub trait ProgressSink: Send + Sync { fn report(&self, update: ProgressUpdate); }`
- `tokio::task_local! { pub static CURRENT_PROGRESS: Arc<dyn ProgressSink>; }`
- `pub struct McpNotificationRouter` (cheap `Clone`) holds the token → subscriber map with an early-notification buffer and a `Mutex<HashMap<String /*task id*/, Arc<Notify>>>` for task-status wake-ups (D6). Methods:
  - `handle_progress(ProgressNotificationParam)` (synchronous)
  - `subscribe_progress(token) -> ProgressSubscription` (unsubscribes on drop)
  - `handle_task_status(TaskStatusNotificationParams)`
  - `register_task(id) -> TaskWaker`, which removes the entry on drop.

Wiring:
- `McpToolAdapter::try_new(tool, peer, server_name)` stays. `McpToolAdapter::with_router(self, McpNotificationRouter)` attaches a router.
- `start_mcp_server` creates one router per connection and gives it to the `McpClientHandler`. The handler exposes it, so `build_mcp_state` and `on_tool_list_changed` attach it to every adapter they build.
- `merge_connected_mcp_peers` builds adapters **without** a router.
- `McpClientHandler::on_progress` → `router.handle_progress`. `on_task_status` → `router.handle_task_status`.
- rmcp adds its own `progressToken` to every request and overwrites any token set by the caller, so the request is identical with or without progress forwarding. The adapter therefore sends via `Peer::send_request_with_option`, reads the token from the returned handle, and subscribes only when **both** a router is attached and `CURRENT_PROGRESS` is set. The token is only known after the request is on the wire, so the router keeps a bounded buffer (64) of notifications for unsubscribed tokens and hands them over on subscribe. The adapter drives subscriber → sink in a `select!` alongside the response, drains already-queued updates when the response arrives, and drops the subscription when the call future finishes or is dropped. Later notifications find no subscriber and are buffered at most until evicted.
- Because the adapter sends the request itself, it also sends `notifications/cancelled` when the call future is dropped before a response (`AbandonedRequestGuard`), as rmcp does for its own helpers.

*Alternatives:*
- Add a context argument to `CallFunctionTool::call`. That is a breaking trait change for every tool. Rejected.
- Route by token inside `McpClientHandler` straight to the event sink. That couples the handler to agent events and needs a token→tool_call_id map across crates. Rejected.

### D4. Progress sink, throttling, ordering, and ACP mapping
- New transient `AgentEventKind::ToolCallProgress { tool_call_id, tool_name, progress: Option<f64>, total: Option<f64>, message: Option<String> }`, with `is_durable() == false`. It is exported through typeshare.
- **Agent sink** (`AgentProgressSink`, in the MCP branch). It holds `Mutex<State { closed, last_emit: Option<Instant>, pending: Option<ProgressUpdate>, timer_armed }>` and emits through `config.emit_ephemeral_event`, which is synchronous.
  - **Throttle:** at most one event per tool call per 250 ms. An update inside the window replaces `pending` and arms a single trailing timer task. When the timer fires, it emits `pending` under the lock, but only if not `closed`. The most recent update before the call ends is therefore delivered unless the call finishes within the same window.
  - **Ordering guarantee:** the MCP branch wraps the call as `CURRENT_PROGRESS.scope(sink.clone(), tool.call(args))` and holds a `SinkCloseGuard` that sets `closed = true` and discards `pending` when the call finishes **or** is dropped (cancellation). Emission and closing happen under the same lock, and `ToolCallEnd` is emitted only after `execute_prepared_inner` returns. So no `ToolCallProgress` can follow the tool call's `ToolCallEnd`. A pending update that was never emitted is superseded by `ToolCallEnd`.
- **ACP mapping** (`acp/shared.rs`): `ToolCallUpdate` with status `InProgress` and a single text content block:
  - `pct = floor(progress / total * 100)`, clamped to 0–100. It is computed only when `progress` is `Some` and `total` is `Some(t)` with `t > 0`.
  - Text: `"{message} ({pct}%)"` / `"{message}"` / `"{pct}%"`. If there is neither a message nor a percentage, the event maps to no update.
  - The existing `ToolCallEnd` mapping always sets `content`, so it replaces the progress text in the final update.
- `export/*` and replay ignore `ToolCallProgress`, because it is never persisted.

### D5. Cancellation: test and document, no new code path
`run_prepared_tool_call` already drops the future. The test uses the in-process harness (tasks §2) with a slow tool. It cancels the token and asserts that the server observes `notifications/cancelled` for that request id and that exactly one result exists, whose text contains `"Cancelled by user"`.

### D6. Call loop and tasks
- The adapter always uses `Peer::call_tool_once`:
  - `Complete` → map through D1/D2.
  - `InputRequired` → `Err(McpToolError::InputRequiredUnsupported)`. MRTR is out of scope. This replaces today's opaque `UnexpectedResponse`.
  - `Task(created)` → follow the task (below). This happens even if the server returned a task without the extension being declared; following it is harmless.
- **Declaration:** config `tasks = true` (default `false`). The handler's `get_info` adds `TASKS_EXTENSION_ID` to `ClientCapabilities` extensions **only when `tasks = true`**. The declaration depends only on QueryMT's config, because it is sent in `initialize` before server capabilities are known.
- **Following a task:**
  1. Register a `TaskWaker` with the router.
  2. Loop: wait for `min(poll_interval_ms or 1000, 10000)` ms or the waker's `Notify`, whichever comes first. Then call `get_task`.
  3. Report each non-terminal status to the sink as `ProgressUpdate { progress: None, total: None, message: Some(status_message or "working") }`.
  4. `completed` → decode the payload as `CallToolResult` and map it through D1/D2. A payload with `isError: true` becomes `Reported`.
  5. `failed` / `cancelled` → `TaskFailed` with the server's status message.
  6. `input_required` → issue `cancel_task` and return `TaskFailed { status: input_required, message: "task requires input, which is not supported" }`.
- **Cancellation:** after the task id is known, a `TaskCancelGuard` holding a cloned `Peer` spawns `cancel_task` when it is dropped before a terminal state. If the run is cancelled during the initial `call_tool_once`, before the task id is known, only `notifications/cancelled` is sent. A task the server created concurrently may be orphaned until its TTL. This is accepted and documented.
- There is no client-side timeout. The call is bounded by the server's task TTL and by agent cancellation.

*Why opt-in:* the extension is new and server support is uneven. Declaring it changes how servers respond. If it is enabled by default, a buggy server could break existing tool calls.

### D7. Response cache: keep rmcp defaults, add an opt-out
- rmcp enables the cache by default with `default_ttl = 0`, so servers that send no hints are never cached. The service loop invalidates the tool cache when it receives `tools/list_changed`, before our `on_tool_list_changed` runs `list_all_tools`.
- `cache = false` makes `start_mcp_server` call `peer.set_response_cache_config(ClientCacheConfig::disabled())` immediately after `serve_client` returns, **before** the initial `list_all_tools` in `build_mcp_state`.
- For `ConnectedMcpPeer`s, the cache configuration is the owner's responsibility.
- No other knobs are exposed.

### D8. OAuth for HTTP MCP servers
- **Scope:** OAuth is supported only for HTTP servers declared in the **agent configuration** (`AgentConfig.mcp_servers`).
  - At connect time, `start_mcp_server` applies OAuth to a session server only if an agent-config server has the same name, the same URL (compared after `url::Url` normalisation), and `auth = OAuth`. The auth settings are taken from the agent config, not from the session's `_meta`.
  - A client-supplied server that carries `_meta.querymt.auth` and does not match fails to connect with `"OAuth MCP servers must be declared in the agent configuration: <name>"`.
  - A client-supplied server that overrides a config OAuth server by name with a different URL connects without OAuth, even if it still carries `auth`.
  - `start_mcp_server` therefore receives the config's OAuth server list through a new `McpConnectContext` argument, alongside the existing ones.
- **Storage:** `SecretStoreCredentialStore { server_name, server_url }` implements rmcp `CredentialStore`.
  - Key: `mcp-oauth:<server>`.
  - Value: JSON `{ "server_url", "redirect_uri", "credentials": StoredCredentials }`.
  - `load` returns `None` if `server_url` doesn't match the configured URL. A server renamed onto another URL therefore never receives another server's token.
  - `clear` and `on_refresh_token_rejected` delete the entry.
  - `Debug` is implemented manually and never prints tokens.
- **Redirect URI:** `http://127.0.0.1:<port>/callback`. `port = redirect_port` or the fixed constant `DEFAULT_MCP_OAUTH_REDIRECT_PORT = 47821` in `mcp_auth.rs`. Treat the constant as stable, because it is part of registered redirect URIs. If the stored `redirect_uri` differs from the current one, `start_flow` ignores the stored dynamically registered `client_id` and registers again.
- **Connect** (`auth = OAuth`, `oauth` feature on):
  1. Build an `AuthorizationManager` for the URL with the store and call `initialize_from_store()`.
  2. If credentials load, the transport is `StreamableHttpClientTransport::with_client(AuthClient::new(reqwest_client_with_headers, manager), config)`.
  3. Otherwise, return `AgentError::McpAuthRequired { server }`.
- **Session setup:** `build_mcp_state` catches `McpAuthRequired` and `McpAuthUnavailable` per server, and continues with the other servers. `McpAuthUnavailable` means authorization could not be *checked* (discovery or the authorization server failed, or the credential store was unreadable); it must not take the session down, but it must be visible. Both cases log (warn for missing login, error for unavailable), emit the transient `AgentEventKind::McpServerAuthRequired { server_name, provider_id: "mcp:<server>", error }` (`error` is set only for the unavailable case), and are recorded on `McpToolState` as `SkippedMcpServer`s. Because the event is emitted while the session is still being created and may precede the client's subscription, `session/new` and `session/load` responses also carry `_meta.querymt.skippedMcpServers` (`SkippedMcpServer[]`: `[{server_name, provider_id, error?}]`, snake_case like the notification and event, so one generated type serves all three). Every other connection or listing error still fails the session as today.
- **Private addresses:** discovery uses `AuthorizationManager::new_allowing_private_addresses`, because internal MCP servers commonly use an internal identity provider. Loopback and link-local rules of rmcp still apply.
- **Redirect URI stability:** the connection's credential store keeps the stored `redirect_uri` on saves (token refreshes); only a login writes a new one, together with re-registering the client.
- **Where a login is requested:** only servers connected with OAuth applied (recorded on `McpToolState` at connect time) report "log in with provider mcp:<name>" on a 401/refresh failure. A same-name override without OAuth (including one that still carries `auth`) is an ordinary server.
- **Refresh rejected while the session is live:** rmcp calls `on_refresh_token_rejected` (credentials cleared), and the request fails with `TokenRefreshRejected`. The tool call fails with `"Error: authorization required for MCP server <name>; log in with provider mcp:<name>"`, and `McpServerAuthRequired` is emitted. The connection stays, and its tools keep failing the same way until a new session connects after login.
- **Login:** `AuthService` recognises provider ids prefixed `mcp:`. An id that doesn't name an agent-config OAuth MCP server fails with `"unknown OAuth MCP server: <name>"`.
  - `start_flow(owner, "mcp:<name>")` creates an `AuthorizationSession` (discovery, then the configured `client_id` or DCR with the redirect URI, then PKCE with the configured scopes). It stores the session in a new `mcp_flows: HashMap<FlowIdentity, McpFlow>` keyed by the existing `FlowIdentity { flow_id, owner, provider }`, and spawns the existing loopback listener on the redirect port. It returns the authorization URL with `flow_kind = "redirect_code"`.
  - `complete_flow(owner, flow_id, …)` looks up the flow by identity (so another owner's flow is not found) and calls `handle_callback`. The store persists the tokens together with the URL and redirect URI.
  - `auth_status` appends one entry per agent-config OAuth MCP server: `authenticated` if `load` returns credentials, otherwise `login_required`.
  - `logout` clears the entry.
- **After login:** the server is available to every session that connects its MCP servers afterwards (new, loaded, resumed, or forked), without restarting the agent. Sessions that are already running keep their current connections.
- **Feature gating:** the rmcp `auth` feature is enabled only through the agent's `oauth` cargo feature. Without it, `auth = OAuth` fails to connect with `"OAuth support is not enabled in this build"`, and `mcp:` ids fail with the same message.

*Alternatives:*
- A separate `_querymt/mcp/auth/*` extension. That duplicates client UI work for the same flow. Rejected.
- Live reconnect after login. There is no reconnect mechanism, and adding one (with tool-state swap and in-flight call handling) is a separate change. Deferred.

### D9. Unix socket transport
- `McpServerConfig::Http` gains `socket_path: Option<PathBuf>`, validated by D0a.
- With `cfg(unix)` and the rmcp `transport-streamable-http-client-unix-socket` feature (in the agent's `[target.'cfg(unix)'.dependencies]`), the transport is `StreamableHttpClientTransport::with_client(UnixSocketHttpClient::new(path, url), config)`. Headers are applied through `config.custom_headers`. The URL is used for request addressing and the `Host` header.
- On non-Unix targets, connect fails with `"Unix sockets are unsupported for MCP server <name> on this platform"`.
- A path that isn't valid UTF-8 is rejected at validation.
- Over a socket, headers become rmcp `custom_headers` (one value per name, last wins) and rmcp's reserved names (`accept`, `mcp-session-id`, `mcp-protocol-version`, `last-event-id`) are rejected with the server named; over TCP they are plain reqwest defaults.
- `mcp_server_requires_network` keeps returning `true` for every HTTP server, including socket ones. Sockets are commonly egress proxies, so this is the conservative choice.

## Risks / Trade-offs

- [Task-local progress context is invisible plumbing] → Document it on `McpToolAdapter::call` and `with_router`. With no sink or no router, calls behave exactly as before. Unit tests cover both paths.
- [Progress floods] → The throttle in D4.
- [rmcp auth API surface is large and still moving (3.2–3.7 changed refresh semantics)] → Isolate it in `agent/src/mcp_auth.rs` and require `rmcp = "3.8"`.
- [The MCP OAuth port may be in use by a provider login] → The existing single-listener slot replaces the pending listener, which is the same behaviour as provider flows today. The conflict is logged.
- [Orphaned server tasks when cancelled before task creation is acknowledged] → Accepted (D6). They are bounded by the server's TTL.
- [Tools of a session whose refresh token is rejected keep failing until a new session] → Accepted (D8). The error tells the user which provider id to log in with.
- [Changing `is_error` semantics could surface previously hidden failures to hooks and middleware] → This is intended. Note it in the changelog.
- [Breaking `McpServerConfig` API] → Intended (D0). All in-tree uses are updated in the same change.

## Migration Plan

No data migration is needed. Config files without the new keys get the defaults. Rust users of `McpServerConfig` must switch to the constructors. Rollback is a revert. Stored `mcp-oauth:*` secrets are ignored by older versions.

## Open Questions

- Should progress also be shown in the built-in dashboard UI, or only through ACP? This is deferrable and does not affect the event shape.

# Tasks

## 0. Config shape and validation (breaking)

- [x] 0.1 Reshape `McpServerConfig` per D0: add `tasks: bool` (default false) and `cache: bool` (default true) to both variants; add `auth: Option<McpAuth>` and `socket_path: Option<PathBuf>` to `Http`; add `McpAuth::OAuth { client_id, scopes, redirect_port }`; mark variants `#[non_exhaustive]`; add `McpServerConfig::stdio(..)` / `::http(..)` constructors with setters; update JSON schema examples and the doc comment in `config/mcp.rs`. Verify: config parse tests for every new key and for a config with none of them.
- [x] 0.2 Carry the new fields through `_meta.querymt` in `to_acp`; make `from_acp` return `Result` (no panic on unknown variants) and read `_meta.querymt` with defaults when absent. Verify: ACP round-trip tests for stdio and HTTP with and without the new fields.
- [x] 0.3 Update all in-tree constructors and patterns to the new shape (`events.rs:195`, `session_registry.rs` tests, `agent_config_builder.rs` tests, `config/tests.rs`, `tests/dotagents_runtime_e2e_test.rs`, and any `from_acp` callers). Verify: `cargo check -p querymt-agent --all-targets --all-features`.
- [x] 0.4 Add `validate_mcp_server` in `agent/mcp.rs` (D0a: empty / `"@"` / non-UTF-8 `socket_path`; `socket_path` + `auth`; `auth` + `Authorization` header, case-insensitive), call it from `config::loader::validate_mcp_servers` and from `start_mcp_server` after `from_acp`. Verify: unit tests per rejection, each error names the server; a session test with a client-supplied empty `socket_path` fails that server without panicking.

## 1. Result fidelity (isError, structuredContent)

- [x] 1.1 Add `McpToolError { Reported { parts }, InputRequiredUnsupported, TaskFailed { status, message }, Service(ServiceError) }` in `crates/querymt/src/mcp/adapter.rs` with the `Display` rules of D1; return `Reported` from `McpToolAdapter::call` when `is_error == Some(true)`. Verify: unit test asserts `Err(Reported)` carrying the server text and that `to_string()` contains "quota exceeded".
- [x] 1.2 In `call`, when `content` is empty and `structured_content` is `Some`, return its JSON text; never add it when content exists; apply the same mapping to `Reported` parts. Verify: unit tests for structured-only, both-present, and `"structuredContent": null` (deserialises to `None`, empty result).
- [x] 1.3 Add unit tests for `ToolResultPart::from_mcp_content` covering text, image, audio, embedded text/blob resource, resource link, and the unknown-variant JSON fallback. Verify: `cargo test -p querymt --features mcp mcp::adapter`.
- [x] 1.4 In the `execute_prepared_inner` MCP branch (`crates/agent/src/agent/execution/tool_calls.rs`), downcast `McpToolError::Reported` to use its parts verbatim with `is_error = true`; keep `"Error: {e}"` for other errors. Verify: harness test (§2) with an `isError: true` tool asserts `ToolCallEnd { is_error: true }`, ACP status `failed`, and `tool_call_outcome` recorded as `"error"`.

## 2. In-process MCP test harness

- [x] 2.1 Add a test-only helper in `crates/agent` that spins up an rmcp server over `tokio::io::duplex` with configurable tools (echo, error, structured-only, slow, progress, task, input-required, list-changed) and records what it receives (requests, `_meta`, cancellations, `tasks/cancel`, client capabilities). It connects through the real `start_mcp_server`-equivalent path and returns the `RunningService<RoleClient, McpClientHandler>` plus the adapters built with the handler's router. Enable the rmcp `server` feature under `[dev-dependencies]` only. Verify: a smoke test lists and calls the echo tool.

## 3. Cancellation and rmcp version

- [x] 3.1 Add a test: start a run whose MCP tool sleeps, cancel the token, assert the server received `notifications/cancelled` with the request id and exactly one result whose text contains "Cancelled by user". Verify: `cargo nextest run -p querymt-agent mcp_cancel`.
- [x] 3.2 Document on `McpToolAdapter::call` that dropping the future sends `notifications/cancelled` (rmcp ≥3.6), and raise the rmcp requirement to `"3.8"` in `querymt`, `querymt-agent` and `querymt-mobile-ffi`. Verify: `cargo tree -i rmcp` shows a single 3.8.x.

## 4. Call loop and progress

- [x] 4.1 Switch `McpToolAdapter::call` to `Peer::call_tool_once`: `Complete` → map (§1); `InputRequired` → `Err(InputRequiredUnsupported)`; `Task` → §5 (stub returning an error until 5.2). Verify: harness tests for complete and input-required.
- [x] 4.2 Add `querymt::mcp::progress` with `ProgressUpdate`, `ProgressSink`, `CURRENT_PROGRESS` task-local and `McpNotificationRouter` (wrapping rmcp `ProgressDispatcher`, plus task-id → `Notify` registry with drop-to-unregister `TaskWaker`); add `McpToolAdapter::with_router`. Verify: unit tests for the router (subscribe, early buffer, unsubscribe) and a harness test that nothing is forwarded without a router or without a sink.
- [x] 4.3 In `call`, when both router and sink are present: send via `send_request_with_option`, subscribe to the handle's (rmcp-chosen) token, drive subscriber → sink concurrently with the call, drop the subscription when the call ends or is dropped, and send `notifications/cancelled` on drop. Verify: harness test with a tool emitting 3 progress notifications delivers 3 sink reports; a notification sent after completion is not delivered.
- [x] 4.4 In `start_mcp_server` create one router per connection, give it to `McpClientHandler` (`on_progress` → `handle_progress`, `on_task_status` → `handle_task_status`), expose it from the handler, and attach it to adapters built in `build_mcp_state` and `on_tool_list_changed`. `merge_connected_mcp_peers` builds adapters without a router. Verify: 4.3 passes through the real handler; a routerless-adapter test asserts no progress is forwarded.
- [x] 4.5 Add transient `AgentEventKind::ToolCallProgress { tool_call_id, tool_name, progress: Option<f64>, total: Option<f64>, message: Option<String> }` with `is_durable() == false`; regenerate typeshare types. Verify: `events.rs` classification test asserts non-durable; typeshare output updated.
- [x] 4.6 Map `ToolCallProgress` in `acp/shared.rs` per D4 (floor percentage, clamp 0–100, only when `total > 0`; `"{message} ({pct}%)"` / `"{message}"` / `"{pct}%"`; no update when neither); ignore it in `export/*` and replay. Verify: mapping unit tests (with total, without total, `total = 0`, no message, neither) and a replay test asserting no progress entries.
- [x] 4.7 Implement `AgentProgressSink` per D4 (250 ms throttle with trailing timer; `SinkCloseGuard` sets `closed` and discards pending on finish or drop; emission and closing under one lock) and wrap the MCP call in `CURRENT_PROGRESS.scope(...)`. Verify: harness test with 20 notifications in 100 ms while the call runs 500 ms more: ≤2 events in the burst window and the last one reflects the 20th notification; a test where the call ends right after a burst asserts no `ToolCallProgress` follows `ToolCallEnd` in the event stream; the cancellation test (3.1) asserts the same.

## 5. Server-side tasks (SEP-2663)

- [x] 5.1 Declare `TASKS_EXTENSION_ID` in `McpClientHandler::get_info` only when the server config has `tasks = true` (pass the flag into the handler). Verify: harness test asserts the server sees `supports_tasks()` only when enabled.
- [x] 5.2 Implement task following in the adapter per D6: register a `TaskWaker`; wait `min(poll_interval_ms or 1000, 10000)` ms or the waker; `get_task`; report non-terminal status as `ProgressUpdate { message }`; `completed` → decode `CallToolResult` through §1 (including `isError` → `Reported`); `failed` / `cancelled` → `TaskFailed`; `input_required` → `cancel_task` then `TaskFailed`. Verify: harness tests for completed, completed-with-isError, failed, input-required (asserts `tasks/cancel` received), and working → completed with progress observed.
- [x] 5.3 Wake the matching poller early from `on_task_status` through the router. Verify: harness test where the server pushes status completes well before the poll interval.
- [x] 5.4 Add `TaskCancelGuard` that spawns `cancel_task` on a cloned peer when dropped before a terminal state; document on the adapter that cancellation before the task is acknowledged sends only `notifications/cancelled`. Verify: harness test cancels the run mid-task and asserts the server received `tasks/cancel` and exactly one result containing "Cancelled by user".

## 6. Response cache

- [x] 6.1 When `cache = false`, call `peer.set_response_cache_config(ClientCacheConfig::disabled())` in `start_mcp_server` right after `serve_client`, before the first `list_all_tools`. Verify: harness test with a TTL-hinted server counts `tools/list` requests: cached when enabled; every listing, including the first, hits the server when disabled.
- [x] 6.2 Test that a `tools/list_changed` notification within the TTL causes `on_tool_list_changed` to fetch fresh tools. Verify: harness test changes the server's tool set, sends list_changed, asserts the new tool appears in `McpToolState`.

## 7. Unix socket transport

- [x] 7.1 Add the rmcp `transport-streamable-http-client-unix-socket` feature under `[target.'cfg(unix)'.dependencies]` in `crates/agent/Cargo.toml`. Verify: `cargo check -p querymt-agent` on macOS.
- [x] 7.2 In `start_mcp_server`, on Unix build `UnixSocketHttpClient::new(path, url)` with headers in `config.custom_headers`; on other targets fail with "Unix sockets are unsupported for MCP server <name> on this platform". Keep `mcp_server_requires_network` true for socket servers. Verify: unix-only test against an rmcp streamable-HTTP server bound to a temp socket (or `#[ignore]` if the server feature is too heavy); a `cfg(not(unix))` unit test for the error text.

## 8. OAuth for HTTP MCP servers

- [x] 8.1 Enable rmcp `auth` via the agent's `oauth` feature; add `crates/agent/src/mcp_auth.rs` with `DEFAULT_MCP_OAUTH_REDIRECT_PORT = 47821` and `SecretStoreCredentialStore { server_name, server_url }` implementing rmcp `CredentialStore` (key `mcp-oauth:<server>`, value `{ server_url, redirect_uri, credentials }`, `load` returns `None` on URL mismatch, `clear` and `on_refresh_token_rejected` delete, redacting `Debug`). Verify: unit tests for load/save/clear/rejection/URL-mismatch using the test secret store.
- [x] 8.2 Add `McpConnectContext` carrying the agent-config OAuth servers to `start_mcp_server`/`build_mcp_state`; implement the D8 scope rules (name + normalised URL match, settings from agent config; client-supplied OAuth without a match fails with the named error; a different-URL override connects without OAuth). Verify: unit tests for match, no-match-with-auth, and different-URL override.
- [x] 8.3 For matched OAuth servers: build `AuthorizationManager` with the store, `initialize_from_store()`; on success use `AuthClient` over the header-configured reqwest client; otherwise return `AgentError::McpAuthRequired { server }`. Without the `oauth` feature return "OAuth support is not enabled in this build". Verify: test with stored valid creds connects to a mock server; test without creds yields `McpAuthRequired`; test with expired access token and valid refresh token refreshes and persists.
- [x] 8.4 In `build_mcp_state`, skip servers failing with `McpAuthRequired` (warn, emit transient `AgentEventKind::McpServerAuthRequired { server_name, provider_id }`, continue); keep failing the session for all other errors. Add the event (non-durable, typeshare, ACP mapping as a session notification through the existing ext-notification path). Verify: session test with one auth-required server and one normal server starts with only the normal server's tools and emits the event; a test with a broken non-OAuth server still fails the session.
- [x] 8.5 Map `TokenRefreshRejected` (and other auth-required errors) during an MCP tool call to `"Error: authorization required for MCP server <name>; log in with provider mcp:<name>"` and emit `McpServerAuthRequired`. Verify: mock-server test rejecting the refresh token asserts the error text, cleared credentials, and `login_required` status.
- [x] 8.6 Extend `AuthService` for `mcp:<server>` ids (agent-config OAuth servers only; unknown → "unknown OAuth MCP server: <name>"): `start_flow` (AuthorizationSession with configured client_id or DCR, re-registering when the stored `redirect_uri` differs; PKCE with configured scopes; `mcp_flows` keyed by `FlowIdentity`; loopback listener on `redirect_port` or the default; `flow_kind = "redirect_code"`), `complete_flow` (owner-scoped lookup, handle callback, persist), `logout` (clear), `auth_status` (one entry per OAuth MCP server, authenticated / login_required). Verify: unit tests per method against a mock authorization server (wiremock or axum test server), including an other-owner completion that fails as not found and a stable redirect URI across two logins.
- [x] 8.7 Verify post-login availability: integration test where an auth-required server is skipped in session A, login completes, and a new session B (without agent restart) has the server's tools; session A is unchanged.
- [x] 8.8 Ensure tokens never appear in logs/events: audit `Debug` impls and log lines touching credentials; add a test asserting `ToolCallStart`/`ToolCallEnd`/`McpServerAuthRequired`/session history contain no access or refresh token string. Verify: test passes; manual grep of debug logs during 8.6 tests.
- [x] 8.9 Update `crates/agent/examples/confs` with an OAuth HTTP MCP example and document `_querymt/auth/*` `mcp:` provider ids, the default redirect port, and the next-session availability rule in the agent docs. Verify: example config parses in the existing config-examples test.

## 9. Integration verification

- [x] 9.1 Run the CI clippy command for `querymt-agent` with all features, plus `cargo clippy -p querymt --features mcp -- -D warnings` and `cargo check -p querymt-mobile-ffi`. Verify: all clean.
- [x] 9.2 Add a changelog entry covering the breaking `McpServerConfig` API, the `McpToolAdapter` `isError` → `Err` behaviour change, and the new events. Verify: entry present.
- [ ] 9.3 Manual smoke test against one real stdio server (e.g. `@modelcontextprotocol/server-everything`, which emits progress) and one real HTTP server; record observed progress updates and an `isError` result in the PR description. Verify: notes attached to PR.

## 10. Review follow-ups

- [x] 10.1 OAuth discovery allows private addresses (`new_allowing_private_addresses`).
- [x] 10.2 Authorization that cannot be checked skips the server (not the session), logs an error, emits `McpServerAuthRequired { error }` and is recorded on `McpToolState`. Test: `unreachable_authorization_skips_the_server_and_surfaces_the_error`.
- [x] 10.3 Connection credential store keeps the stored `redirect_uri` on refresh saves (`for_connection`). Test: `connection_saves_keep_the_registered_redirect_uri`.
- [x] 10.4 Auth-required reporting uses "OAuth applied at connect time" (`McpToolState::is_oauth_server`) instead of a name match; a same-name override carrying `auth` connects without OAuth.
- [x] 10.5 `session/new` / `session/load` responses carry `_meta.querymt.skippedMcpServers`.
- [x] 10.6 Unix-socket headers: reserved names rejected naming the server.
- [x] 10.7 Typeshare: ran `scripts/generate-types.sh`; `target/generated/types.ts` (gitignored build output) contains `tool_call_progress` and `mcp_server_auth_required` (with optional `error`). That covered the event kind only; 10.8 adds the remaining payload types.
- [x] 10.8 Typeshare the MCP auth payloads: `SkippedMcpServer` (moved to `control/notifications.rs`, re-exported from `agent::core`) and `McpAuthRequiredNotification` (params of `querymt/session/mcpAuthRequired`, now built from the struct). Wire format unchanged (snake_case, `error` only when set); tests pin it. `_meta.querymt.skippedMcpServers` stays an untyped meta key whose value is `SkippedMcpServer[]`. Verify: `scripts/generate-types.sh` output contains both types.

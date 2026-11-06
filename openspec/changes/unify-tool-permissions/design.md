# Design

## Context

See proposal.md for motivation; requirements are in `specs/`. Current state that shapes the approach:

- `execute_prepared_inner` (`agent/execution/tool_calls.rs`) dispatches with an `if / else if` chain: allowlist check, the uncommitted `rejected_by_user`, built-in, MCP, and finally `ensure_tool_permission` + provider call. Only provider tools reach the ACP permission prompt.
- `ensure_tool_permission` already owns the interaction lifecycle: task-active check, `permission_request` hooks, `register_pending_permission`, durable `PermissionRequested`, `PendingPermissionGuard`, cancellation via `select!`, `PermissionGranted`. It is the right core to keep.
- `SessionRuntime::permission_cache: HashMap<String, bool>` stores "always allow/reject" by bare tool name.
- `ToolConfig { policy, allowlist: Option<HashSet<String>>, denylist: HashSet<String> }` is matched by `is_mcp_tool_allowed_with`, which allocates two `format!` strings per MCP check. `config/tools.rs` already has an unvalidated `ToolSpec` enum and `parse_tool_spec`.
- Snapshot choice is `mutating_tools.contains(name) || assume_mutating` (`AgentConfig::should_snapshot_tool`).
- Quorum agents are built inside infallible `FnOnce` factories (`quorum.rs` `DelegateFactory`/`PlannerFactory`), which is why `apply_middleware_from_config` swallows errors. Both build paths detect "disabled" middleware by `msg.contains("disabled")`.
- `AgentMode` derives `Deserialize` (snake_case) but not `JsonSchema`.

## Goals / Non-Goals

**Goals:**
- One typed tool selector and one matching rule for allow, deny and approval lists, with no allocation per check.
- One approval gate run before dispatch for all tool sources, reusing the existing interaction lifecycle.
- Parse every user-supplied value (selectors, mode, timeout) at the config boundary; no runtime string checks.
- Quorum and single-agent builds share middleware assembly and fail identically.

**Non-Goals:**
- Persisting session grants across agent restarts or sessions.
- Argument-level approval rules (e.g. "ask for `shell` only when the command matches X"); hooks cover that.
- Changing hook wire formats beyond carrying the `ask` reason.
- Mode-based tool enforcement (modes remain reminders; hooks enforce).

## Decisions

### D1. Typed selectors: upgrade `ToolSpec`, add `ToolRef` and `ToolSet`
`config/tools.rs`:
- `ToolSpec { Builtin(String), McpAll(String), McpSpecific(String, String) }` gains `FromStr` (error type `ToolSpecError`, thiserror, lowercase messages), `Display` (round-trips), and `#[serde(try_from = "String", into = "String")]` + `JsonSchema` as a string. `parse_tool_spec` is deleted.
- `ToolRef<'a> { name: &'a str, server: Option<&'a str> }` is the borrowed identity of a concrete call; `Copy`.
- `ToolSet { builtin: HashSet<String>, mcp: HashMap<String, ServerTools> }` with `enum ServerTools { All, Only(HashSet<String>) }`; implements `FromIterator<ToolSpec>`, `Extend<ToolSpec>`, `contains(ToolRef) -> bool` and `covers(&ToolSpec) -> bool` (for load-time validation). Lookups borrow `&str`, so no allocation.

Alternative: a new `ToolSelector` type next to `ToolSpec` — rejected as duplication. Alternative: keep strings and validate in one function — rejected; validation is forgotten at new call sites (`api-parse-dont-validate`).

### D2. Allow/deny lists become `ToolSet`
`ToolConfig { policy, allow: Option<ToolSet>, deny: ToolSet }` with `ToolConfig::allows(ToolRef) -> bool`, replacing `is_tool_allowed_with` / `is_mcp_tool_allowed_with`. `SetAllowedTools` / `SetDeniedTools` carry `Vec<ToolSpec>`; because `ToolSpec` deserializes through `try_from`, a malformed selector fails at the message boundary (local callers build typed values; remote messages fail to decode) and the session lists stay untouched. Builders take `impl IntoIterator<Item = ToolSpec>`. `collect_tools` builds a `ToolRef` per definition (server looked up once from the MCP snapshot).

Behavior change accepted: a bare deny entry no longer hides an MCP tool, matching the allowlist rule.

### D3. Configuration shape
```toml
[agent]                       # also [planner] and [[delegates]]
mode  = "plan"                # AgentMode, default build
tools = ["ibkr.*", "read_tool"]

[agent.permissions]
ask         = ["shell"]               # prompt; session allow offered
always_ask  = ["ibkr.place_order"]    # prompt every call
timeout_sec = 120                     # NonZeroU64; unset = wait
```
`PermissionsConfig { ask: Vec<ToolSpec>, always_ask: Vec<ToolSpec>, timeout_sec: Option<NonZeroU64> }` with `#[serde(default, deny_unknown_fields)]`. It is converted once into the runtime `Permissions { ask: ToolSet, always_ask: ToolSet, timeout: Option<Duration> }` stored on `AgentConfig`.

Why two lists rather than `ask` + `remember = bool`: the session option is a per-tool property (order placement must never be remembered while `shell` may be), and two lists read as plain intent. Why not `allow/ask/deny`: `tools` and runtime deny already decide availability; a second availability list would conflict. `timeout_sec` sits beside the lists, not inside them, so it applies to hook asks with empty lists (the original bug).

Rust API: `AgentBuilder::permissions(Permissions)` plus `Permissions::new().ask(..).always_ask(..).timeout(Duration)`; `AgentBuilder::mode(AgentMode)`. Python: `ask=`, `always_ask=`, `approval_timeout_sec=`, `mode=` replace `assume_mutating=` / `mutating_tools=`.

### D4. Approval decision is computed once, before dispatch
```rust
enum ApprovalNeed { None, Session, EveryCall { reason: Option<String> } }
impl Permissions { fn need(&self, t: ToolRef) -> ApprovalNeed } // always_ask > ask > none
```
Combined with the pre-tool hook decision during preparation:
- hook `Allow` → `None`
- hook `Ask { reason }` → `EveryCall { reason }`
- otherwise → `Permissions::need`.

`PreparedCall` stores `approval: ApprovalNeed` and the resolved `target: ToolTarget` (D6), replacing `hook_allows_interactive`, `hook_ask` and the duplicate MCP lookup. `PreToolPermissionDecision::Ask` becomes `Ask { reason: Option<String> }`; `PreToolUseResult::ask_reason` is removed.

### D5. One approval function built from `ensure_tool_permission`
`request_approval(config, exec_ctx, call_id, tool: ToolRef, args, need) -> Result<Approval, _>` where
```rust
enum Approval { Granted, Denied(Denial) }
#[derive(thiserror::Error)] enum Denial { Rejected, TimedOut(Duration), Cancelled, TaskInactive, Hook(String) }
```
Order: `None` → granted; `Session` and session grant present → granted; task inactive → `TaskInactive`; `permission_request` hooks (allow → granted, deny → `Hook(msg)`); then the existing interaction with options built from `need`:
- `Session`: `allow_once` (AllowOnce), `allow_session` (ACP kind AllowAlways, label "Allow for this session"), `reject` (RejectOnce)
- `EveryCall`: `allow_once`, `reject`

Title/reason come from the hook reason when present. The wait is `select!` over the response, run cancellation, and `sleep(timeout)` when configured. Every exit emits `PermissionGranted` and drops `PendingPermissionGuard`, the same retirement path cancellation already uses, so a timed-out request becomes not-pending on all clients. The `permission_request` hook keeps running for `EveryCall` needs so policy hooks can still deny.

Session grants: `SessionRuntime::permission_cache` is replaced by `session_grants: ParkingMutex<ToolSet>`. `ToolSet::insert(ToolRef)` records the exact server-qualified tool on `allow_session` for `Session` needs only; lookup is the same allocation-free `contains(ToolRef)`. Reusing `ToolSet` keeps one matching rule and avoids a separate key type. No rejection is stored. The lock is never held across `.await`.

Alternative: keep `rejected_by_user` on the question/elicitation path — rejected (no ACP permission semantics, no events, stdin fallback, stale prompt after timeout).

### D6. Dispatch resolves a `ToolTarget` once
```rust
enum ToolTarget { Builtin(Arc<dyn Tool>), Mcp(Arc<McpToolAdapter>), Provider }
```
resolved during preparation from the registry and MCP snapshot. It yields the `ToolRef` (MCP server name), the snapshot decision (D7) and the invoke branch. `execute_prepared_inner` becomes: allow check → `request_approval` → snapshot → `run_tool(target) -> ToolOutcome`. The `(blocks, bool, &str, &str)` tuple becomes `ToolOutcome { parts, is_error, source: ToolSource, execution: Execution }` with `as_str()` used for span attributes (`type-no-stringly`).

### D7. Snapshots from declared effect
- `Tool::is_read_only(&self) -> bool { false }`; overridden to `true` in read-only built-ins: `read_tool`, `index`, `glob`, `search_text`, `ls`, `mdq`, `get_function`, `get_symbol`, `find_symbol_references`, `language_query`, `web_fetch`, `browse`, `read_task`, `todoread`, `knowledge_query`, `knowledge_stats`, `knowledge_list_unconsolidated`, `session_history`, `question`.
- `McpToolAdapter::is_read_only()` in `crates/querymt` returns `annotations.read_only_hint == Some(true)`.
- `ToolTarget::is_read_only()`; `Provider` → false. `should_snapshot_tool`, `mutating_tools`, `assume_mutating` and `requires_permission_for_tool` are deleted.

Trusting an MCP hint is acceptable here because a snapshot only enables undo; approval never depends on hints.

### D8. Mode is an agent setting; middleware assembly is shared and fallible
- `AgentMode` derives `JsonSchema`; `mode: AgentMode` (`#[serde(default)]`, `Default = Build`) on `AgentSettings`, `PlannerConfig`, `DelegateConfig` and the api `AgentConfig`. Builders call `with_agent_mode(mode)` outside the closures. `configured_default_mode`, `AgentConfigBuilder::set_agent_mode(&self)` and the quorum `if let Ok(..)` are deleted. `AgentModeFactoryConfig` drops `default` and adds `deny_unknown_fields`.
- `MiddlewareFactory::create -> Result<Option<Arc<dyn MiddlewareDriver>>>`; `Ok(None)` = turned off by its own config. The five factories that return `"Middleware disabled"` errors return `Ok(None)`.
- `middleware::build_from_entries(entries, &AgentConfig) -> anyhow::Result<Vec<Arc<dyn MiddlewareDriver>>>` plus the existing auto-added defaults (context, output-limit recovery, parallel edit guard) live in one function used by `AgentBuilder::build` and quorum, with errors wrapped `.with_context(|| format!("middleware '{type}'"))`.
- `DelegateFactory` / `PlannerFactory` return `anyhow::Result<Arc<dyn AgentHandle>>`; `AgentQuorumError::Agent { agent_id, #[source] source }` carries failures out of `AgentQuorumBuilder::build`.

### D9. Load-time validation and discovery warnings
- `config/loader.rs` gets `validate_permissions(tools: &[ToolSpec], perms: &PermissionsConfig, servers: &[McpServerConfig])`, run for the single agent, the planner and every delegate (delegate servers = quorum-level + delegate-level `mcp`). It fails on an MCP selector whose server is not configured, and, when `tools` is non-empty, on an entry the `ToolSet` of `tools` does not `covers`.
- After discovery, wherever a new `McpToolSnapshot` is stored (`build_mcp_state` and the `tools/list_changed` refresh in `elicitation.rs`), `Permissions::unmatched(&snapshot)` yields the `server.tool` approval entries of connected servers that match no discovered tool, and each is reported with `log::warn!` (structured fields `server`, `selector`). Requires `Permissions` to be reachable from the MCP connect context.

## Risks / Trade-offs

- [Headless sessions now block on listed tools] → document that scheduled/unattended profiles must set `timeout_sec`; the stdin fallback no longer applies to approval.
- [Provider tools stop prompting by tool kind] → intentional; users list them in `ask`. Called out in CHANGELOG.
- [Server existence is validated against configured `[[mcp]]` only] → servers attached later (dotagents, session attachments) cannot be named in approval lists at load time; they are still governed by hooks. Revisit if needed.
- [Bare deny entries no longer hide MCP tools] → documented breaking change; use `server.tool`.
- [Wire change of permission option IDs and `SetAllowedTools` payload] → all mesh nodes and the desktop UI must update together; covered by the existing "matching interaction contract" requirement.
- [Built-in read-only list can drift] → defaulting to "not read-only" means a missing override only costs an extra snapshot, never lost undo.

## Migration Plan

No compatibility shims. Removed keys fail with unknown-field errors that name the key; the CHANGELOG maps each: `mutating_tools`/`assume_mutating` → removed (snapshots automatic, approval via `permissions.ask`); `confirm_tools` → `permissions.always_ask`; `confirm_timeout_sec` → `permissions.timeout_sec`; `agent_mode.default` → `mode`. Rollback is reverting the change.

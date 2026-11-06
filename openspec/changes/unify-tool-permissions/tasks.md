# Tasks

## 1. Shared, fallible middleware assembly and typed initial mode

- [x] 1.1 Change `MiddlewareFactory::create` to return `Result<Option<Arc<dyn MiddlewareDriver>>>` and make the `context`, `dedup_check`, `limits`, `output_limit_recovery` and `parallel_edit_guard` factories return `Ok(None)` when turned off; verify their existing "disabled" unit tests assert `Ok(None)` instead of an error string
- [x] 1.2 Add one `middleware::build_from_entries` function (entries plus auto-added context, output-limit recovery and parallel edit guard defaults) that adds `middleware '<type>'` context to errors; use it from `AgentBuilder::build` and quorum, delete both `msg.contains("disabled")` checks; verify with a test that the same invalid entry fails both build paths with the same message
- [x] 1.3 Make quorum `DelegateFactory`/`PlannerFactory` return `anyhow::Result<Arc<dyn AgentHandle>>`, add `AgentQuorumError::Agent { agent_id, #[source] source }`, make `apply_middleware_from_config` return `Result`, and wrap existing `add_delegate_agent`/`with_planner` callers in `Ok(..)`; verify with a test that an invalid delegate `limits` entry fails `QuorumBuilder::build` naming the delegate
- [x] 1.4 Derive `JsonSchema` and `Default` (Build) for `AgentMode`; add `mode: AgentMode` to `AgentSettings`, `PlannerConfig`, `DelegateConfig` and api `AgentConfig`/builders (`AgentBuilder::mode`), applied via `with_agent_mode`; verify config tests for `[agent] mode = "plan"`, delegate `mode = "review"`, and that `mode = "plna"` on a planner and a delegate fails loading naming `mode`
- [x] 1.5 Remove `default` from `AgentModeFactoryConfig`, add `deny_unknown_fields`, delete `configured_default_mode`, `AgentConfigBuilder::set_agent_mode` and the quorum `if let Ok(..)`; verify a test that an `agent_mode` entry with `default` fails the build naming `default`, and that a new session of a `mode = "plan"` agent starts in plan
- [x] 1.6 Update `agent_modes.md` and `configuration.md` for `mode` and middleware failure behavior; verify the documented TOML snippets load via a config test

## 2. Typed tool selectors and `ToolSet`

- [x] 2.1 In `config/tools.rs`, give `ToolSpec` `FromStr` with a thiserror `ToolSpecError`, `Display` round-trip, serde `try_from`/`into` String and a string `JsonSchema`; delete `parse_tool_spec`; verify unit tests for valid forms, round-trip, and rejection of `""`, `"a."`, `".b"`, `"a.*.b"`
- [x] 2.2 Add `ToolRef<'a>` and `ToolSet` (`ServerTools::{All, Only}`) with `FromIterator`/`Extend<ToolSpec>`, `insert(ToolRef)`, allocation-free `contains(ToolRef)` and `covers(&ToolSpec)`; verify unit tests for bare vs MCP, `server.*`, same tool name on two servers
- [x] 2.3 Switch `tools` in `AgentSettings`, `PlannerConfig`, `DelegateConfig`, api configs and `ResolvedTools` to `Vec<ToolSpec>`; verify existing tool resolution and quorum config tests pass and a malformed `tools` entry fails loading naming the field

## 3. Allow and deny lists on `ToolSet`

- [x] 3.1 Replace `ToolConfig.allowlist/denylist` with `allow: Option<ToolSet>`, `deny: ToolSet` and `ToolConfig::allows(ToolRef)`; delete `is_tool_allowed_with` and `is_mcp_tool_allowed_with`; port `collect_tools` and the dispatch allow check; port the `agent/tools.rs` and `api/quorum.rs` selector tests to `allows`, including the PR #967 server-scoping cases
- [x] 3.2 Change `SetAllowedTools`/`SetDeniedTools` to carry `Vec<ToolSpec>` and update builders (`with_allowed_tools`/`with_denied_tools` take `ToolSpec` iterators), dotagents targets and remote message registration; verify session actor tests that `github.*` deny hides all `github` tools, a bare `lookup` deny keeps `alpha.lookup`, and a malformed selector fails to decode leaving lists unchanged

## 4. Permissions configuration and validation

- [x] 4.1 Add `PermissionsConfig { ask, always_ask, timeout_sec: Option<NonZeroU64> }` (`deny_unknown_fields`) to `[agent]`, `[planner]`, `[[delegates]]`; add runtime `Permissions { ask: ToolSet, always_ask: ToolSet, timeout: Option<Duration> }` with `need(ToolRef) -> ApprovalNeed`; thread it through `AgentConfigBuilder::with_permissions`, `AgentBuilder::permissions`, planner/delegate builders and quorum; verify unit tests for `need` precedence (`always_ask` > `ask` > none)
- [x] 4.2 Remove `mutating_tools`, `assume_mutating`, `confirm_tools`, `confirm_timeout_sec` from all config structs, builders, `AgentConfig` (with `requires_confirmation`, `requires_permission_for_tool`, `should_snapshot_tool`); verify config tests that each removed key fails with an unknown-field error
- [x] 4.3 Add `validate_permissions` to the loader for the single agent, planner and each delegate (servers = quorum-level + delegate `mcp`); verify tests that `ibkr-trade.place_order` with server `ibkr_trade` fails, and `ask = ["shell"]` with `tools = ["read_tool"]` fails naming `tools`
- [x] 4.4 Add `Permissions::unmatched(&McpToolSnapshot)` and warn (structured `server`, `selector`) wherever a snapshot is stored in `build_mcp_state` and the `tools/list_changed` refresh; verify a unit test that `ibkr.placeOrder` against a snapshot exposing `place_order` is reported
- [x] 4.5 Replace `assume_mutating=`/`mutating_tools=` in `crates/py/querymt-agent-py` with `ask=`, `always_ask=`, `approval_timeout_sec=`, `mode=`; verify the Python package tests pass

## 5. Snapshots from declared tool effect

- [x] 5.1 Add `Tool::is_read_only(&self) -> bool { false }` and override it to `true` for the read-only built-ins listed in design D7; verify a test iterating `all_builtin_tools()` that asserts the read-only set exactly (writes, shell, tasks, knowledge ingest/consolidate, delegate and todowrite are not read-only)
- [x] 5.2 Add `McpToolAdapter::is_read_only()` in `crates/querymt` reading `annotations.read_only_hint == Some(true)`; verify unit tests for hint true, false and absent
- [x] 5.3 Snapshot only when the resolved target is not read-only (provider always mutating); verify execution tests that `read_tool` takes no snapshot and `edit` does with a snapshot policy enabled

## 6. Single approval path

- [x] 6.1 Change hook `PreToolPermissionDecision::Ask` to `Ask { reason: Option<String> }`, remove `PreToolUseResult::ask_reason`; verify hooks tests assert the reason on the variant
- [x] 6.2 Add `ToolTarget { Builtin, Mcp, Provider }` resolved during preparation; replace `hook_allows_interactive`/`hook_ask` in `PreparedCall` with `target` and `approval: ApprovalNeed` (hook allow → None, hook ask → EveryCall{reason}, else `Permissions::need`); delete `mcp_server_of`; verify prepared-call tests still pass
- [x] 6.3 Rewrite `ensure_tool_permission` as `request_approval(.., ToolRef, args, ApprovalNeed) -> Result<Approval, _>` with `Denial` (thiserror): session grant check, task-active check, `permission_request` hooks, options per need (`allow_once`, `allow_session`, `reject` / `allow_once`, `reject`), hook reason as title/reason, `select!` with optional timeout sleep, `PermissionGranted` on every exit; replace `permission_cache` with `session_grants: ParkingMutex<ToolSet>`; delete `rejected_by_user`; verify with `start_paused` tests: listed tool times out with `TimedOut` and does not run; hook ask with empty lists prompts and times out; `allow_session` skips later prompts in the same session but not for `always_ask`, hook asks, `beta.lookup`, or a new session; rejection is not remembered; paused task denies without prompting; `permission_request` hook deny surfaces its message
- [x] 6.4 Move dispatch into `run_tool(target) -> ToolOutcome` with `ToolSource`/`Execution` enums (`as_str()` for span attributes); call allow check → `request_approval` → snapshot → `run_tool` for all sources; verify MCP approval test (server not called until granted), denial result text ("…; the tool was not run"), and the existing genai span tests for `permission_denied`/`policy_blocked`
- [x] 6.5 Update ACP/remote/websocket tests and event fixtures for the new option IDs and session-scoped semantics; verify the `session-interactions` scenarios (remote allow-for-session, timeout retires the request so a late answer is not pending) pass
- [x] 6.6 Document approval in `configuration.md` and `hooks.md` (ask vs always_ask, session allow, hook ask/allow, timeout for unattended profiles); verify the documented examples load in a config test

## 7. Examples, docs and integration

- [x] 7.1 Update example configs using removed keys (`single_coder`, `standup_bot`, `research_journal`, `coder_delegate`, `watchdog`, `hook_guarded_coder`, `trading_analyst`, `trading_desk`); `trading_desk` uses `mode = "plan"` and `[agent.permissions] timeout_sec = 120`; verify the example-config load tests, and a test that the built trading desk agent has a 120 s approval timeout
- [x] 7.2 Update `examples.md`, `api_reference.md`, `profiles.md`, `mesh.md`, regenerate config and hook schemas, and add the CHANGELOG migration table (removed key → replacement); verify schema generation is clean in git diff
- [ ] 7.3 Run `cargo nextest run -p querymt-agent -p querymt`, `cargo clippy --all-targets -- -D warnings` and the Python package tests; verify all pass

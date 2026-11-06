# Proposal

## Why

Tool approval is split across three mechanisms that disagree. `mutating_tools` and `assume_mutating` claim to require confirmation but only prompt for provider-run tools, and otherwise just decide snapshots. The uncommitted `confirm_tools` work adds a third path that matches names differently from the tool allowlist. Its prompt bypasses the permission interaction, events and session cache, and its timeout is silently dropped when the list is empty. Tool names are raw strings everywhere, so a typo in a safety list disables it without a warning.

The quorum builder also hides configuration errors. An unknown `agent_mode` default starts a planner or delegate in `build`. A failing middleware is logged and skipped. Both paths treat any error containing "disabled" as an intentional opt-out.

Breaking changes are acceptable, so this change replaces the three mechanisms with one small, typed permission model. Approval, allowlists and snapshots then follow a single rule.

## What Changes

- **BREAKING** Remove `mutating_tools`, `assume_mutating`, `confirm_tools` and `confirm_timeout_sec` from single-agent, planner and delegate configs, the Rust builders and the Python bindings.
- Add a `permissions` section to `[agent]`, `[planner]` and each `[[delegates]]` entry:
  - `ask` lists tools that need approval. The prompt offers "Allow once", "Allow for this session" and "Reject".
  - `always_ask` lists tools that prompt on every call and never offer the session option. `always_ask` wins over `ask`.
  - `timeout_sec` rejects any approval prompt left unanswered for that long. Without it, prompts wait.
- Add one approval path for built-in, MCP and provider tools. It uses the existing permission interaction (ACP `session/request_permission`, `PermissionRequested`/`PermissionGranted` events, recovery, cancellation).
- **BREAKING** A pre-tool hook `ask` becomes a per-call prompt that shows the hook's reason. It never offers the session option.
- **BREAKING** "Always allow" becomes "Allow for this session", keyed by the server-qualified tool. "Always reject" is removed. Provider tools no longer prompt based on a hard-coded tool kind.
- **BREAKING** Tool selectors are parsed when the config is read, everywhere they appear (`tools`, `permissions.ask`, `permissions.always_ask`, runtime allow and deny lists). They share one matching rule: a bare name never matches an MCP tool, and `server.*` matches every tool of that server. Malformed selectors, and approval entries that `tools` does not cover or that name an unconfigured MCP server, fail the config load. After MCP discovery, an approval entry that matches no tool produces a warning.
- **BREAKING** Snapshots follow what each tool declares, which replaces `mutating_tools` and `assume_mutating`:
  - Built-in tools declare whether they are read-only.
  - MCP tools are read-only when the server reports `readOnlyHint: true`.
  - Provider tools are always treated as mutating.
- **BREAKING** The initial session mode moves from the `agent_mode` middleware `default` key to a typed `mode` key on `[agent]`, `[planner]` and `[[delegates]]`. The middleware config rejects unknown keys, so a leftover `default` fails the load.
- Middleware factories report "turned off" as a value instead of an error string. Single-agent and quorum builds share one middleware assembly path that fails on any real error. Quorum agent factories can return errors, and those errors fail the quorum build.

## Capabilities

### New Capabilities

- `tool-approval`: Decides which tool calls need the user's approval and how the prompt behaves: approval lists, hook asks, session-wide allow, timeout, denial results. It applies to built-in, MCP and provider tools.
- `tool-selection`: Typed tool selectors, their validation when the config loads, and the matching rule shared by allowlists, deny lists and approval lists.
- `tool-snapshots`: Decides which tool calls take a filesystem snapshot, based on what each tool declares.
- `agent-configuration`: Defines how the initial mode and the middleware entries become agent settings. Single agents, quorum planners and delegates behave the same, and invalid values fail the build instead of being ignored.

### Modified Capabilities

- `session-interactions`:
  - Permission choices are remembered for the session, not permanently. There is no remembered rejection.
  - A configured approval timeout rejects an unanswered permission request.

## Impact

- **Code**:
  - `crates/agent/src/agent/execution/tool_calls.rs` (approval path, dispatch, outcome types)
  - `agent/agent_config.rs`, `agent/agent_config_builder.rs`, `agent/core.rs` (`ToolConfig`, `SessionRuntime` permission state)
  - `agent/tools.rs`, `agent/session_actor.rs`, `agent/messages.rs` (runtime allow and deny lists)
  - `config/{single,quorum,tools,loader}.rs`, `api/{agent,config,quorum}.rs`, `quorum.rs`
  - `middleware/{factory,modes,context,dedup_check,limits,output_limit,parallel_edit_guard}.rs`
  - `hooks/engine.rs`, `tools/context.rs` and the built-in tool implementations
  - `crates/querymt/src/mcp/adapter.rs` (expose `readOnlyHint`)
  - `crates/py/querymt-agent-py`
- **Wire and UI**:
  - Permission options change to `allow_once`, `allow_session` (ACP kind `allow_always`) and `reject`.
  - Runtime allow and deny messages carry selectors in the same string form; bare names no longer deny MCP tools.
  - All mesh nodes must run the matching version.
- **Config, docs and examples**: every example config that uses the removed keys, plus `configuration.md`, `hooks.md`, `agent_modes.md`, `examples.md`, `api_reference.md`, `profiles.md`, `mesh.md`, generated schemas and the CHANGELOG.
- **Supersedes**: the uncommitted `confirm_tools` work (`rejected_by_user`, `requires_confirmation`, `configured_default_mode`, `set_agent_mode`). The rest of that work stays: hook `mcp_server` matching, `QMT_PROFILE_DIR`, hooks enabled by default, post-tool hook failure handling, and the elicitation capability.

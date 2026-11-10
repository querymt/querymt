# Changelog

## Unreleased

### Behavior changes

- **BREAKING: llama.cpp tool-call grammars use llguidance by default, and tool
  arguments are schema-aware in every format.** New provider parameter
  `grammar_backend` (`llguidance` | `gbnf`, default `llguidance`; set it in
  `[agent.parameters]` or llama-cpp provider parameters). Parameter names, value
  types, enums and required parameters now follow the tool's JSON schema in all
  six formats (Qwen3 JSON, Qwen XML-function, Ministral, LFM2, FunctionGemma,
  Gemma 4); before, only Qwen XML-function did, and the others accepted any JSON
  object or keyword list. Undeclared parameters are allowed only when the schema
  allows additional properties. An uncompilable schema falls back to a
  structure-only llguidance grammar, and a build or mid-request llguidance failure
  falls back to GBNF for the rest of the request (logged once). Set
  `grammar_backend = "gbnf"` for the previous engine.
- **llama.cpp grammars are enforced by rejection sampling**: the sampled token is
  checked and the whole vocabulary is masked only when it is rejected, also for
  speculative (MTP) decoding and thinking-budget forced tokens. Sampling with a
  fixed `seed` can produce different output than before, because a rejected draw
  consumes one extra random number.
- **llama.cpp tool calls are much faster.** A tool-call string value cost about
  0.5 s of CPU per token with GBNF on a 150k vocabulary, which left GPUs idle on
  long `write_file`/`edit` calls. llguidance needs about 0.3 ms, and GBNF text
  values are narrowed so each state has at most four alternatives (13 ms).
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

### Fixed

- **llama.cpp tool calls could not contain `<` in string arguments.** The tool
  grammar for Qwen-function models (Qwen3.5/3.6/3.8, AREX-2) and FunctionGemma
  forbade `<` inside every string parameter, so `write_file`/`edit` content such
  as `row[j] < best_d` came out corrupted (the sampler picked another token).
  String values now accept anything except the exact closing sequence
  (`</parameter>` / `<escape>`).
- **llama.cpp tool-call parsing keeps string arguments verbatim.** Declared
  string parameters are no longer coerced through JSON (`123`, `true`, a JSON
  file body), only the template's one newline on each side is removed
  (indentation and trailing newlines are kept), and values containing
  `</function>`, `</tool_call>` or `[TOOL_CALLS]` no longer end the call early.
  FunctionGemma values with commas or braces and Gemma 4 strings containing `"`
  now round-trip; a parameter of an unknown schema type no longer breaks the
  whole grammar.
- **llama.cpp usage reports `reasoning_tokens`.** Thinking tokens (including a
  thinking-budget close marker) were counted in `output_tokens` with
  `reasoning_tokens` always 0; they are now split out.

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

- **`qmt_llama_cpp::LlamaCppConfig` has a new public field
  `grammar_backend: Option<GrammarBackend>`**; struct literals must add it
  (`None` keeps the default). `qmt_llama_cpp::GrammarBackend` is a new exported
  enum.
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

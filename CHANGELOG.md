# Changelog

## Unreleased

### Added

- **`qmacp` and `qmcp` live in this workspace.** The ACP client CLI (`qmacp`,
  `crates/qmacp`) and the MCP-to-ACP bridge (`qmcp`, `crates/qmcp`) moved from
  their separate repositories, follow the workspace version, and are built for
  every platform and attached to stable and nightly releases. The qmacp agent
  skill moved to `skills/qmacp`.

### Behavior changes

- **Subagents API (Rust, Python, hooks, ACP):** `Agent::builder()`,
  `AgentBuilder::subagent`, `Agent::add_subagent` / `remove_subagent` /
  `subagents` / `has_subagents` / `on_subagents_changed` /
  `subscribe_subagent_changes`; Python `Agent.create`, `add_subagent`,
  `remove_subagent`, `subagents`, `on_subagents_changed`, `SubagentError`.
  New observe-only hooks `subagent_added` / `subagent_removed`;
  `querymt/agents/changed` and the add/remove replies now carry `added` /
  `removed`. Deprecated: `Agent::single`, `Agent::is_multi`,
  `Agent::from_quorum_config*`, `Agent::builder_from_quorum_config`, and in
  Python `Agent.single` and `is_multi()` (`DeprecationWarning`).
- **BREAKING (Rust API, delegation):** every runtime has one root agent and a
  typed roster of members it can delegate to (`unify-agent-runtime`). A plain
  agent is a runtime with an empty roster and now gets the delegation service
  too. Removed: the `AgentHandle` trait and `RemoteAgentHandle`,
  `register_remote_agent*`, `AgentRegistry` / `DefaultAgentRegistry`,
  `ProtocolTargetFactory` and `DotagentsTargetFactory`, `DelegationMiddleware`
  and the `DelegationContextConfig` / `DelegationContextTiming` types and
  builder methods, `AgentQuorum*`, `QuorumBuilder`, `DelegateAgent`,
  `Agent::multi` / `Agent::quorum` / `Agent::planner`, `collect_event_sources`
  (every transport uses the root's single `EventFanout`), the services
  `ToolContext::extensions()` / `with_extension` API and its `http`
  dependency, and `skills::ApprovalMode` with `NonInteractive`. Use
  `Agent::roster()`, `add_member`, `remove_member`, `MemberSpec` and
  `AgentRoster` / `AgentId` (all in the prelude) instead.
- **BREAKING (behavior):** a skill approval requested by a protocol
  (`.agents`) target in an unattended run now waits for an answer instead of
  being refused (`skills::ApprovalMode::NonInteractive` is removed). Unattended
  runs that relied on the refusal need an explicit approval policy.
- **BREAKING (profile catalog):** the embedded `coder-delegate` profile is
  written as `[agent]` with `[[delegates]]`, so its `config_kind` is now
  `single`, not `quorum`.
- **Mesh sharing:** Agent mesh runtimes start with no sharing grants. Joining a
  scope does not grant access in either direction. Use ACP to grant access
  explicitly; pre-upgrade clients require Full access on an upgraded host.
- Configuration: `[agent]` accepts `[[delegates]]`, `[agent.delegation]`
  (`wait_policy`, `wait_timeout_secs`, `cancel_grace_secs`, `max_parallel`,
  `verification`, `summary`) and `snapshot_policy`. `[quorum]` / `[planner]` is
  deprecated: it still loads and builds the same runtime, logs one notice per
  load, and a file mixing it with `[agent]` is rejected. See "Migrating from
  `[quorum]`" in `docs/docs/agent/configuration.md`. The shipped
  `coder_agent`, `coder_delegate`, `learning_pair` and `multi_agent` profiles
  use the new form.
- ACP: new `querymt/agents/list`, `querymt/agents/add` and
  `querymt/agents/remove` (optional `profileId`) and the
  `querymt/agents/changed` notification change the roster of a running
  runtime. Changes are not persisted. Members added this way reject `peer`,
  file system-prompt parts and `isolated` without a workspace service, and do
  not expand `${VAR}` in `api_key`. See "Changing agents while running" in
  `docs/docs/agent/delegation.md`.
- Remote agents are no longer contacted at startup. They are listed whether or
  not the peer is reachable and resolved on first delegation; an unreachable
  peer fails that delegation with an error naming the peer. Remote delegation
  sessions are marked as delegations and are not listed as user sessions. New
  mesh message `CreateRemoteDelegationSession`: nodes in one mesh need the same
  version for remote delegation. New session-database migration
  `0038_remote_bookmark_fork_origin` (nullable column); bookmarks written
  earlier have no recorded origin.
- **BREAKING (log filtering):** the agent is split into `querymt-agent-core`,
  `-index`, `-knowledge`, `-sqlite`, `-services` and `-runtime`, with
  `querymt-agent` as the facade. `log` and `tracing` targets come from module
  paths, so they gain the crate prefix: `querymt_agent::agent::remote` is now
  `querymt_agent_runtime::agent::remote`. A filter on the bare crate name
  (`RUST_LOG=querymt_agent=debug`) is a string-prefix match and still covers
  every agent crate; a filter on a deeper old path matches nothing. Update
  deeper filters (see `docs/docs/agent/mesh.md`).
- **BREAKING (Rust API):** `ViewStore` loses `get_atif`. ATIF export is a
  function over the existing audit-view query.
- **BREAKING (Rust API):** `SessionError::Database` holds a database-neutral
  `DbError` (boxed source plus a `NoRows` / `Busy` / `Other` kind) instead of
  `rusqlite::Error`, and `From<rusqlite::Error>` for `SessionError` and
  `KnowledgeError` is removed. `is_no_rows()`, `is_transient()` and the
  `source()` chain behave as before.
- Existing `querymt_agent::…` module paths keep working through the facade.
  `querymt-agent` features (`remote`, `oauth`, `plugin-loaders`, `api`) forward
  to `querymt-agent-runtime`. Session database schema, config, hooks, ACP wire
  shapes and typeshare output are unchanged.
- **BREAKING (Rust API, journal):** `querymt_agent::events::AgentEventKind` gains
  `ElicitationResolved { elicitation_id, session_id, outcome, selection }`
  (kind `elicitation_resolved`, durable) and the `ElicitationOutcome` enum
  (`accepted`, `declined`, `cancelled`). The enum is not `#[non_exhaustive]`, so
  exhaustive matches downstream stop compiling. Built-in questions, skill
  approvals and MCP server elicitations now journal how they ended; MCP form
  content is never recorded. An older binary cannot decode journals that
  contain the new event, so downgrading over existing sessions is not
  supported. Typeshare output gains the types (UI follow-up for
  querymt-desktop, not done here).
- **BREAKING (Rust API):** `ToolContext::ask_question` takes `&[QuestionOption]`
  instead of `&[(String, String)]` (`QuestionOption` moves to
  `querymt_agent::tools`; the `question` tool re-exports it).
  `SkillCatalog::bind` and the skill tool binding take the session's skill
  grant set.
- Skill approval prompts offer "Allow", "Allow for this session" and "Deny".
  "Allow for this session" skips later prompts for the same skill ID, and its
  supporting-file reads, in that session only. Configured `deny` still wins,
  non-interactive agents still fail at once, and denials and one-time allows
  are never remembered.
- An invalid skill `file` path now fails before any approval prompt, for local
  and MCP skills (a configured `deny` still reports the denial first).
- The skill registry refresh debug log now says "local skills".
- **BREAKING (Rust API):** `querymt_agent::tasks::run_verification` and
  `tasks::extract_verification_commands` are removed. Nothing called them, and
  they ran substrings of model output through `sh -c`. `TaskWatcher` is
  unchanged, and delegation's `run_verification` config option is unaffected.
- **BREAKING (Rust API):** run ids are `TurnId` end to end. `ActiveRun`,
  `SteeringInbox`, `TurnControlError`, `PromptFinished`, `RunPhaseChanged` and
  `RunObjective` carry `TurnId` instead of `String`.
  `ExecutionContext::new` now takes the run's `TurnId` and the context's
  `turn_id` is no longer optional (read it with `turn_id()`).
  `ExecutionContext::turn_id_string` and `with_turn_id` are removed, and
  `impl Default for TurnId` is removed (use `TurnId::new()`). Wire formats are
  unchanged. Hook input `turn_id` is now always a UUID: hooks inside a run
  (including native `/compact` and maintenance compaction) carry that run's id,
  and delegation hooks get a freshly generated one instead of an empty string.
- `ScheduleInfo.state` is typed as `ScheduleState`, and `PendingInputSummary`
  `delivery`/`state` as `InputDelivery`/`PendingInputState`. JSON values are
  unchanged. querymt-desktop should regenerate its typeshare output for
  `ScheduleInfo.state` and `PendingInputSummary` (UI follow-up, not done here).
- **BREAKING (mesh wire, feature `remote`):** the error arm of the
  `querymt::AdmissionRequest` reply is now a structured `AdmissionRejection`
  instead of a string. Nodes on different versions see a decode error instead of
  the rejection reason, so upgrade all mesh nodes together.
- **BREAKING (Rust API):** `WorkspaceIndexError` is replaced by `IndexError`
  (also used by `FunctionIndex::build`, `CodeSearch::search` and the workspace
  actor replies). `HooksConfig::validate`, `Hooks::new` and `Hooks::run_stop`
  return `HookConfigError` / `HookError` instead of `anyhow::Error`.
- **BREAKING: `.gitignore` applies to undo snapshots outside git repositories.**
  Before, it was ignored there, so ignored files were snapshotted and could be
  reverted by undo. `.ignore` and `.git/info/exclude` are honoured too; `.ignore`
  is the way to opt paths out.
- **Snapshot GC is supported and off by default.** `max_snapshots` and
  `max_age_days` default to unset, and nothing is deleted unless one is set.
- **BREAKING: setting `max_snapshots`/`max_age_days` now prunes and deletes old
  snapshots for the whole workspace** (before, the values were ignored).
  Undo/redo past the window fail without changing files.
- **BREAKING: `max_snapshots = 0` or `max_age_days = 0` is a config load error.**
  Remove the key, or use JSON `null`.
- Snapshots cover files of every size (the unreleased `max_file_bytes` key is
  removed; setting it is an unknown-field error). Large files are stored
  uncompressed and restored by streaming.
- New warnings for files of 64 MiB or more entering snapshots and for snapshot
  stores of 5 GiB or more, both pointing to `.ignore`.
- Undo of moved files now restores the original path. Undo and redo report
  restore failures (writes are atomic and keep permissions), and a failed undo
  still records a redo frame. That frame never deletes history and is discarded
  by the next prompt; `undo_stack` entries (`querymt/session/undoStack`, undo
  and redo responses) gain `restore_failed` so UIs can tell it apart.
- **BREAKING (Rust API):** `GitSnapshotBackend::with_snapshot_base` is public;
  `SnapshotError` gains `Io`; `GcResult` gains `deleted_objects`;
  `GcConfig` limits are `NonZero*` and `GcConfig::default()` has no limits.
- Dev note: after running a `fix/snapshot` development build, clear
  `querymt/snapshots` in the user cache dir (stores from that build may lack
  large files).
- **Undo restores files again for turns recorded since steering support.** Undo
  that can't find a turn's pre-turn snapshot fails without changing files
  instead of reporting success. A revert frame left by the broken undo can be
  removed with redo before the next prompt.
- **BREAKING: undo to a message inside a turn (for example a steering message)
  is refused.** Undo to the turn's prompt instead.
- **BREAKING (Rust API):** `snapshot::SnapshotId` is a newtype instead of a
  `String` alias, and turn ids use the new `TurnId` in `MessagePart` turn records
  and `ExecutionContext`. Stored sessions, revert frames and mesh payloads are
  unchanged.

- **BREAKING: default system cache hints cover only the first and last section,
  and the Anthropic provider enforces its cache breakpoint limits.** For
  Anthropic, Codex and OpenAI Responses the agent no longer adds a default `cache`
  hint to middle system sections; add an explicit `cache` hint to keep a boundary
  there (explicit hints and `cache_control` are never changed). The Anthropic
  provider now sends at most four `cache_control` markers per request, keeping the
  last system block, the last two messages, the first system block, then older
  messages and remaining system blocks, and logs one warning with the number
  dropped. It also raises an earlier marker's lifetime to the longest one that
  follows it (a `1h` marker no longer follows a `5m` one), so such a raised marker
  is billed at the `1h` write rate. Requests that previously failed with a 400 for
  too many breakpoints now succeed.
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
- **BREAKING (storage, mesh wire): session events are delivered and replayed in
  production order.** Every event now carries a per-session `session_seq`,
  assigned when the session produces it. Each session has one ordered writer
  that persists durable events in batches (up to 64 per transaction) and
  publishes them after commit. Ephemeral deltas wait only for earlier durable
  events. Replay uses the same order as live delivery, so back-to-back events
  such as a tool-call start and end no longer swap. Relayed events keep the
  host's `session_seq`.
  - **`session/prompt` responses follow their updates.** The prompt reply
    carries an internal `event_tip`, and the connection holds the response until
    it has delivered every update of the run. The wait is released on
    disconnect, lag-recovery failure, connection close or a 30 s safety limit.
    The client-facing `PromptResponse` is unchanged.
  - **Lagging consumers recover.** The WebSocket forwarder used to stop
    delivering session updates for the rest of the connection once it fell
    behind the live buffer. The WebSocket and stdio forwarders and the remote
    relay now reload the missed durable range from the journal (the relay
    fetches it from the host) and keep receiving. Ephemeral deltas missed while
    lagging are not replayed. The recovered durable event supersedes them.
  - **Journal migration `0024_event_journal_session_seq`** adds the
    `session_seq` column and a unique index on `(session_id, session_seq)` for
    locally produced rows, and backfills existing local rows. Relayed legacy
    rows stay unnumbered.
  - **File databases now use WAL with `synchronous=NORMAL`.** The latest commits
    can be lost on power loss, but the database cannot be corrupted.
    In-memory databases are unchanged.
  - **Upgrade all mesh nodes together.** A node that does not send `event_tip`
    is treated as "no watermark", but ordering across mixed versions is not
    guaranteed. An older binary cannot read a migrated journal correctly, so
    roll back by restoring a pre-upgrade database copy together with the
    matching binary.
  - **UI follow-up:** `session_seq` is added to the typeshare types for
    `DurableEvent`, `EphemeralEvent` and `AgentEvent`. querymt-desktop should
    regenerate its types (not done here).
  - See `docs/docs/agent/event-ordering.md`.
- **Session database: read indexes and read cache.** Migration
  `0037_session_message_read_indexes` runs on start. It adds
  `idx_messages_session_order` (`messages(session_id, created_at)`) and a partial
  `idx_message_parts_prompt` (prompt and steering parts only), and drops
  `idx_messages_session`, which the first makes redundant (about 0.2 s on a
  3.6 GB database; no data changes). File-backed session database connections now
  use a 64 MiB page cache (`cache_size = -65536`) and memory-map up to 256 MiB of
  the file (`mmap_size = 268435456`), with or without migrations. Older binaries
  can still open a migrated database.

### Fixed

- **Faster prompt records, and corrupt non-prompt parts no longer fail loads.**
  Structured user prompt records (remote and local session loads, older-history
  pages, peer prompt requests and history replay for clients without snapshot
  support) now come from a targeted read of prompt and steering parts instead of
  decoding the whole message history. A message part that cannot be decoded
  outside a prompt (for example a corrupt tool result) no longer fails the
  request, the load or the replay. Windowed loads include exactly the prompts of
  the window's prompt events, no longer older prompts stored in the same second.
  Messages stored in the same second are now ordered by insertion. The remote
  speed-up applies once the session's owner runs this version.
- **Forked sessions copy conversation events that point at the fork's own
  messages.** A fork gave its copied messages new ids but copied the prompt and
  assistant events with the source's ids, so inherited prompts had no structured
  records in a windowed load and replay fell back to flattened text. The copied
  events now carry the fork's message ids. Forks created before this version keep
  the old ids.
- **llama.cpp Qwen3 (JSON tool-call format) calls were dropped.** The tool
  grammar forced the tool name without JSON quotes (`"name": glob`), which the
  parser cannot read. The name is now a JSON string.
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

- **Git worktree isolation** (opt-in via `[agent.workspace]` /
  `[quorum.workspace]`, `AgentBuilder`/`QuorumBuilder` and Python keyword
  arguments). Sessions can be started in their own linked worktree and managed
  branch (ACP `session/new` metadata, native `/worktree create`, simple API,
  Python `agent.worktrees`), and delegates can run isolated through the
  per-delegate `isolation` setting (`inherit` | `isolated` | `shared`). Isolated
  delegates start from a private baseline that includes the parent's uncommitted
  edits, results are reviewed baseline-relative, and
  `accept_delegation_result` integrates a reviewed delta as one commit on
  explicit request. Existing undo/redo works inside each workspace. Sessions
  without an explicit isolation request keep running locally. Also adds
  `querymt/worktree/*` ACP extensions, handoff to a fresh owner, archive from
  any session, and recovery, with stable error codes on every surface. See
  `docs/docs/agent/worktrees.md`. Typeshare/ACP additions need a querymt-desktop
  follow-up (not done here). `gix` is upgraded to 0.90; isolation creation is
  refused for sparse sources until sparse-checkout support lands.
- **Portable repository knowledge (opt-in).** Index a checkout's Markdown notes
  and source declarations from a committed `.qmt/knowledge.toml` manifest into a
  disposable local cache, and search it with `repo_knowledge_query`,
  `repo_knowledge_get`, `repo_knowledge_list` and `repo_knowledge_stats`. It is
  off unless `[agent.repository_knowledge] enabled = true`; repository files can
  never enable it, widen it or grant tools. Two separate delivery stages can
  attach bounded, cited cards: `orientation` before a prompt's first model
  request and `index_context` after a successful `index` call. Manifest
  `[orientation] retrieval_only` keeps documents searchable but never attached
  automatically. Session creation warms the index in the background when a
  delivery stage is on, and the first prompt waits up to 8 s for it. Notes may
  carry evidence (`knowledge_sources`) that the maintenance example
  (`cargo run -p querymt-agent --example repository_knowledge -- check`) reports
  as changed. See `docs/docs/agent/repository_knowledge.md` and the
  `repository_knowledge_coder.toml` profile. On the five covered questions in
  the pilot, median fresh tokens fell 14% with similar rubric coverage; the
  feature adds a fixed per-request cost (tool schemas, policy text) and, with
  `entrypoints` configured, a map card on first prompts nothing matched.
- **A plain `cargo build/run/test` at the workspace root selects only the native
  crates** (`default-members`); the WASM provider plugins and heavy native
  providers need `-p <package>` or `--workspace`.
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

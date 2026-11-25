# Changelog

## Unreleased

### Added

- **Lean 4 support in the code index.** `.lean` files are now accepted by
  `index`, `get_symbol`, `get_function`, `find_symbol_references` and
  `replace_symbol`, and yield symbol records in the repository knowledge store.
  Declarations map to the usual kinds (`def`/`theorem` functions, `structure`,
  `class`, `inductive`, `instance`, `axiom`, `example`, macro commands).
  Namespaces nest their declarations, symbols are addressable by dotted
  qualified name (`Geometry.dist`), and a declaration's range covers its
  attributes. The `index` outline gains a `theorems` section for top-level
  `theorem`/`lemma` declarations (Lean only; other languages are unchanged).
  The grammar is `Julian/tree-sitter-lean`, pinned by git revision. API:
  `querymt_agent_index::symbol_index::outline_projection::symbols_to_sections`
  takes a new `language: &str` argument (**BREAKING** for direct callers).
- **`qmacp` and `qmcp` live in this workspace.** The ACP client CLI (`qmacp`,
  `crates/qmacp`) and the MCP-to-ACP bridge (`qmcp`, `crates/qmcp`) moved from
  their separate repositories, follow the workspace version, and are built for
  every platform and attached to stable and nightly releases. The qmacp agent
  skill moved to `skills/qmacp`. Use `qmacp mesh` to inspect directional sharing,
  manage grants and quotas, and wait for access changes.
- **llama-cpp: LoRA adapters.** New provider option `lora`, a list with one
  GGUF adapter (`path`: local file or Hugging Face `<repo>:<selector>`,
  `scale`: finite float, default `1.0`, `0` disables). The adapter is loaded
  with the base model and applied to chat, streaming, completion, tool-call,
  multimodal and speculative (target) generation. Changing the adapter path
  reloads the model; changing only the scale does not. New exports
  `LoraAdapterConfig`, `LoraScale` and `LoraScaleError`.

- **Providers declare prompt-cache support; new `[agent.execution.prompt_cache]
  ttl_secs`.** `ChatCapabilities` has a new `prompt_cache` field
  (`explicit_breakpoints`, `max_ttl_secs`), `Copy` and defaulted, so a plugin that
  omits it reads as declaring nothing. New exports `PromptCacheCapabilities`.
  `ttl_secs` (0 to 86400, default 0 for the provider's default) sets the lifetime
  of the agent's default cache hints, capped at the provider's `max_ttl_secs`.
  Anthropic declares 3600 and OpenAI Responses, on a model and endpoint with
  breakpoint support, 1800. A value out of range, not an integer, or an unknown
  key in the table fails the load naming the key.

- **`ChatProvider::output_provenance` and `HTTPChatProvider::output_provenance`.**
  A provider can state the `ChatOutputProvenance` (provider, protocol, model,
  endpoint) it stamps on its output; the default is `None`. `ValidatedLLM`,
  `ToolEnabledProvider`, `LLMProviderFromHTTP` and `MeshChatProvider` forward
  it, `RemoteChatProvider` reports `None` (the provider contract carries no
  provenance), and the OpenAI (Responses mode), xAI (Responses path) and Codex
  providers implement it. The plugin contract is unchanged, so an Extism
  provider reports `None`.

- **ACP: subagent updates for delegations.** For a client that advertises
  `clientCapabilities.subagents`, a delegated child session is announced on the
  parent session with a `subagent_update` (title = target agent, description =
  objective, `running` while the delegation executes, verifies or finalizes,
  `idle` once it ends, `_meta.querymt.stage`, model, node and timestamps), before
  any event of the child. The delegate tool call gains a `session` reference to
  the child. Load snapshots carry `subagentUpdates` and `session/load` replays one
  `subagent_update` per child. The `querymt/session/delegationUpdate`
  notification and `delegationUpdates` are still sent for now, and are removed
  with the desktop switch.

- **ACP: session cursors and fork at a cursor.** For a client that advertises
  `clientCapabilities.session.cursors`, the agent sends a `cursor_update` after
  each user message, agent message, thought, tool call and compaction, live and
  on replay. Ids are derived from the anchor, so they are the same after a
  reconnect or restart. `session/fork` accepts `cursorId` (messages only; an
  unknown, foreign or non-forkable cursor is `-32602` and creates no session),
  which takes precedence over `_meta.querymt.message_id`. Forking a remote-hosted
  session is now done by its owner node. The load snapshot
  (`querymt/sessionLoadSnapshot.v1`) and each `querymt/session/history` page
  carry the cursors of the events they hold in `cursorUpdates`, with the ids
  replay announces; a client without `session.cursors` gets no `cursorUpdates`,
  and one without `compaction` gets no compaction cursors.

- **ACP: `PromptResponse.usage`.** `session/prompt` reports the token counts of
  the execution cycle that answered the prompt (input, output, reasoning, cache
  read and write, and their sum), on every stop reason, and omits it when no
  provider reported usage.

- **ACP: `session/cancel` on a delegation child.** Cancelling a child session
  while its delegation runs cancels that delegation, as a cancel requested by the
  agent does.

### Behavior changes

- **BREAKING (ACP wire shape, additive): `session/list` reports each session's
  plans.** Entries for local sessions with a todo list carry the top-level
  `_meta` key `querymt/plans`, an array with one `items` plan
  (`planId: "querymt-todos"`) holding the same entries a `plan_update` would
  carry; sessions without a todo list omit it. The current todo plan is now
  persisted per session (session database migration `0041_session_todo_plans`,
  which backfills existing sessions from their last `todowrite`). After a
  `todowrite` changes it, connections where the session is active get a
  `session_info_update` whose `_meta` sets `querymt/plans` (`null` when
  emptied), and every connection gets the new extension notification
  `querymt/session/plansChanged` (`session_id`, `plans`), listed in the control
  capabilities. Each connection announces independently and skips plans it
  already announced. A rewind that removes the session's last `todowrite` (when
  the prompt after an undo starts) restores the earlier plan and announces it
  the same way, through the new transient event
  `AgentEventKind::TodoPlanRestored`, and `todoread` returns the restored
  list. New field `SessionListItem::todo_plan`; new typeshare type
  `SessionPlansChangedNotification` (follow-up for querymt-desktop). Clients
  that ignore unknown `_meta` keys and notifications are unaffected.

- **BREAKING (ACP): a delegation that fails or is cancelled fails its delegate
  tool call.** The delegate tool call ends `completed` when the delegation is
  accepted. For a client that advertises `subagents`, a delegation that later
  fails or is cancelled now sends a final `tool_call_update` with status
  `failed` for that call, live and on replay. Clients without the capability see
  no change.

- **BREAKING (ACP): a `session/fork` of a remote-hosted session, and a fork
  `message_id` that is not in the source session.** A fork of a session hosted
  on another mesh node is created by that node and recorded locally as a remote
  session; it used to fail with `source session not found`. Workspace options in
  the fork's `_meta.querymt.sessionWorkspace` are rejected with `-32602` for a
  remote source. A `_meta.querymt.message_id` that is not a message of the source
  session is rejected with `-32602`; before, the store accepted a message of
  another session.

- **Active time is run time, in the summary and on state updates.** The
  whole-session summary of a load snapshot carries `activeMs`: the sum, over
  the session's ended runs, of the time from `RunStarted` to `RunCompleted`
  (paired by run id, over the whole journal; a run still active is reported
  by the runtime status's `run_started_at_ms`). `querymt/session/stateUpdate`
  carries `atMs`, the event's timestamp in milliseconds, on every transition
  the agent derives from an event; the current-state sync after a load has
  none. The session reducer reports both as `SessionHeader.active_time`.

- **A replayed compaction is sent where it happened.** On a load without a
  snapshot the agent used to send every finished compaction before the whole
  conversation. It now sends each one at the position of the event that
  settled it, between the messages around it. A compaction without an id is
  still paired with its start, and the todo-list, tool-kind and workspace
  announcement rules are one projection
  (`querymt_agent_protocol_types::replay`) shared with the session reducer.

- **Compaction updates carry their token counts.** Live, in a history replay
  and in a load snapshot's projection, a `compaction_update` has
  `_meta.querymt.compaction.tokensBefore` (the last context measurement before
  the compaction started, or its token estimate when there is none) and, once
  known, `tokensAfter`. A count that is not known is omitted. The session
  reducer merges `_meta` key by key, so a later update that adds `tokensAfter`
  keeps `tokensBefore` and the run id.

- **The replay identity of a stored reply comes from the provider.** The agent
  used to complete the protocol and endpoint a reply is replayed against from
  the latest stored reply of the same provider and model. When the provider
  states its provenance it now uses all of it (provider, protocol, model and
  endpoint, whatever alias the session configuration gives the provider), so
  after the configured endpoint changes a reply stamped at the old endpoint is
  sent in portable form (no encrypted reasoning or signatures). A stored reply
  with no provenance (stored before provenance existed) is also sent in
  portable form to such a provider, instead of being matched on provider and
  model alone (`OutputTarget::fallback_provider_model` is off for a stated
  identity). Providers that state nothing keep the old completion and the old
  comparison. OpenAI Responses streams are now stamped with the configured
  endpoint, like their non-streamed replies; before, a streamed reply carried an
  empty endpoint and never replayed natively.
- **A run resolves its model once, before any middleware hook.** When the
  session's provider cannot be built (after the provider-setup retries) the run
  fails with the provider-initialization error before any middleware hook runs
  and before any model request, with the user's prompt already in the history;
  no pre-turn snapshot is taken for it. Hooks therefore always see the model's
  real profile. Native `/compact` with a provider that cannot be built still
  runs the `pre_compaction` hook first: a summary the hook supplies is stored
  without a provider, and so is a compaction policy that names another working
  provider. When the summary has to come from the session's own provider, it
  fails with the provider failure before the `compaction_start` event, instead
  of an internal error after it (an empty conversation still answers "nothing
  to compact" without a provider). The
  `agent.execution.provider_init` span is a child of `agent.prompt.execute`.
  The tools a run offers are still collected for every request, so an MCP
  server's `tools/list_changed` still takes effect mid-run.
- **BREAKING (config semantics, compaction): provider errors during compaction
  follow `[agent.execution.rate_limit]`.** `[agent.execution.compaction.retry]`
  (`max_retries`, `initial_backoff_ms`, `backoff_multiplier`) now applies only to
  a reply the agent rejects as a summary (empty, refused or cut off). A provider
  error is retried only when it is retryable (an overloaded or rate-limited
  provider, a 5xx status, a lost connection), after the wait the error calls for
  (the provider's own delay, `transport_wait_secs` or `default_wait_secs`), for
  at most `max(1, rate_limit.max_retries)` requests, and the run's cancellation
  stops the wait. A rejected credential or an invalid request fails the compaction
  after one request; before, it was retried `compaction.retry.max_retries`
  times. A request timeout is still not retried. No key is added or removed.
  A compaction can now send up to `(compaction.retry.max_retries + 1) ×
  max(1, rate_limit.max_retries)` requests (12 with the defaults, 4 before);
  each is in the cost accounting. A response stream that closes before its
  terminal event is a lost connection and is retried like one (it used to count
  as a rejected summary). A request timeout is recorded as a failed attempt.
  Cancelling stops a request in flight, also while its stream is open, keeps the
  usage it had reported, and a summary that finishes after the cancel is not
  stored. The same retry-wait events as for a model request
  (`llm_retry_wait`, `rate_limited` and their resume events) are now sent during
  a compaction. A compaction request span (telemetry) carries
  `gen_ai.request.attempt` and its events. `querymt-desktop` should confirm that
  retry notices during a compaction render like those of a model request. The
  public `compaction::RetryConfig` (re-exported as `CompactionRetryConfig`) is
  removed. `SessionCompaction::process` and `process_with_instructions` are one
  method, `process(messages, max_prompt_bytes, instructions, &CompactionDispatch)`;
  `CompactionDispatch { provider, config, cancel, ledger }` carries the provider,
  the `&AgentConfig`, the `CancellationToken` and the `DispatchLedger`. The model
  and the session of a compaction request (and its span) are now the ledger's
  (`AttemptOperation::model()` / `session_id()`), no longer a `model` argument
  and the first message's session. Retry-wait events of a compaction are
  written to that initiating session (an empty or blank history session used to
  send them to session `""`).
- **Fix (compaction): `[agent.execution.compaction]` `provider` and `model` now
  choose where the compaction request goes.** Before, both were only labels:
  the request always ran on the session's model, while the cost accounting, the
  pricing and the span named the configured `model`, so a cheaper compaction
  model was charged at its rates for a request that ran on the session's model.
  `model` alone is that model on the session's provider (and mesh node);
  `provider` is resolved like any provider and needs a `model` (the session's
  model belongs to another provider), otherwise the compaction fails before a
  request is sent. The session's own settings (system prompt, reasoning effort,
  local-model options) are not carried over to another model. A configuration
  that set only `provider` used to be accepted and is now refused when it
  loads (see the hooks entry below). The worktree handoff summary follows the same policy, and a
  price-limit refusal of it is reported as such (`summary_failed`, "price limit
  reached") instead of as a provider failure. `AttemptOperation` gains
  `model()` / `session_id()`.
- **BREAKING (hooks, compaction): `pre_compaction` and `post_compaction` name
  the compaction model.** Their `model` was the session's model even when
  `[agent.execution.compaction]` sent the request elsewhere; it is now the model
  the summary is requested from, and both payloads gain `provider`.
  `post_compaction` also gains `summary_source` (`model` or `hook`). The
  committed input schemas set `additionalProperties: false`, so a hook that
  validates its stdin against the old schema rejects the new fields.
  `compaction.provider` without `compaction.model` is refused when a
  configuration file loads (`agent`, `planner` and every delegate; a session
  stored by an earlier version still loads, and its compaction then fails before it starts;
  it used to be accepted and silently ignored, and
  afterwards was refused only when a compaction started); a configuration built
  in code is refused when the compaction starts, before any event or hook.
- **Fix (worktree handoff): `session/cancel` stops a handoff summary.** The
  owner's actor is unloaded for a handoff, so the cancel found nothing to stop
  and the summary kept running with the workspace held exclusively. It now
  stops the request in flight, or the provider setup that precedes it, and the
  handoff ends with the new `summary_cancelled` code, ownership unchanged and
  the workspace released. The
  summary still has no time bound beyond the retry budgets.
- **Skipped tool results are stored before the steering that skipped them.**
  When steering is applied before a tool batch starts, the stored order is now
  the assistant message with the calls, one message with the skipped results,
  then the steering message (before, the steering message was stored first and a
  load moved the results behind it, so the request a run sent differed from the
  one a reload sends). Sessions stored with the old order still load: the
  results are moved to directly after their calls, as before.
- **Token figures of a compaction use the request's estimate.** `compaction_start`
  and `compaction_end` events, the `context_tokens` that follow a compaction and
  the delegation summary budget are now the estimate of the request the history
  projects to (steering, reasoning and compaction summaries included, and the
  tool list and tool-change definitions when the session's provider takes tool
  changes as messages), where three different partial estimates were used. The
  numbers change for histories that hold any of those parts; for one with a
  4,000-byte tool-change definition the figure grows by about 1,000 tokens.
- **Duplicate-code review runs in every run that changed files (middleware
  `dedup_check`).** Before, its "already reviewed" flag was cleared only by
  `MiddlewareDriver::reset`, which no run called, so after the first review of
  a process no later run of any session was reviewed; and it continued a run
  from the last context any session had stored, so with concurrent sessions a
  continuation could carry another session's conversation. Now the review
  runs once per run that added or modified files, considers only that run's
  changes (`SessionRuntime::turn_diffs` is cleared when a run starts and when
  it ends), and continues with the reviewed session's own conversation. It is
  still opt-in (`type = "dedup_check"`) and can be turned off with
  `enabled = false`.
- **The context-usage warning is stored at most once per run (middleware
  `context`).** Before, the "already warned" state was reset whenever any
  session used a different model than the one before it, so concurrent
  sessions on different models could store a `context_usage` notice on every
  step, and a session that stayed above the threshold was warned once until
  the model changed. Now each run stores one notice, at the first request at
  or above `warn_at_percent`, and the next run warns again.
- **Scheduled runs record `run_started`.** A run started by a schedule now
  records a durable `run_started` event with `origin = "scheduled"` after the
  run lease is acquired and before its other events, with the same `run_id` as
  its `run_completed`. Before, only interactive and queued runs did. It is one
  more durable event per scheduled run. Follow-up in `querymt-desktop`:
  confirm nothing assumes `run_started.origin` is always `interactive` and
  that a scheduled run renders as a run.
- **BREAKING (Rust API, llama-cpp):** `qmt_llama_cpp::LlamaCppConfig` has a new
  public field `lora: Vec<LoraAdapterConfig>`. Struct literals must add
  `lora: Vec::new()`, or better `..LlamaCppConfig::new(model)` /
  `..Default::default()`: `LlamaCppConfig` now implements `Default` and has
  `LlamaCppConfig::new(model)`, so later new options won't break callers again.
  Config files are unaffected.
- **BREAKING (model requests, prompt caching):** the model request is now an
  append-only prefix of stored history plus one trailing request message.
  - The run objective is no longer sent with every request by default. Set
    `[agent.execution] run_objective = true` to restore it (new key, default
    `false`). When on, it is sent in the trailing message, so it no longer
    changes the cached prefix.
  - Reminders (agent mode, output limit, progress review, Stop-hook and
    task-completion messages, duplicate-call and duplicate-code guidance,
    parallel-edit and context-usage warnings, background-command notices) are
    stored once as `notice` message parts instead of being added to every
    model call. A mode reminder is stored when the mode changes, with a leave
    notice when the session moves to a mode without one.
  - Context hooks now receive durable history only (no run objective and no
    other hooks' `additional_context`). Their `additional_context` is sent in
    the trailing request message and is not stored.
  - Rollback: a session that holds a `notice` part cannot be loaded by a build
    from before this change (unknown message part kind).
- **BREAKING (provider plugins, prompt caching):** the agent now adds default
  system cache hints from the provider's declaration, not from a list of provider
  names. A provider plugin built before this change declares nothing and gets no
  default system hints until it is rebuilt against this version. `qmt-codex`
  declares nothing either: its backend rejects explicit breakpoints and the
  adapter dropped every hint, so no request changes. `qmt-openai` Responses now
  sends a 30-minute lifetime for any cache hint asking for 1800 seconds or less
  (it failed for anything but 1800) and still fails for longer.
- **Retries after transport failures wait 10 seconds, not 60.** A model request
  that fails without an answer from the provider (connection refused or reset,
  DNS failure, timeout, a stream that breaks or cannot be decoded) is retried
  after `[agent.execution.rate_limit] transport_wait_secs` (new key, 0 to 300,
  default 10; then doubled by `backoff_multiplier`). Failures the provider
  answered keep the provider's own retry delay, or `default_wait_secs` (60) when
  it gave none. Set `transport_wait_secs = 60` for the old behaviour. Rollback:
  after a revert the new key is an unknown key in `[agent.execution.rate_limit]`
  and fails the load until removed.
- **Higher starting output cap at high reasoning effort.** With no `max_tokens`
  configured and reasoning effort `high` or `max`, requests start at
  `min(64000, model output limit)` instead of `min(32000, ...)`; the ceiling for
  escalation is unchanged. An unknown output limit stays at 32,000 and an
  explicit `max_tokens` still wins.
- **Path arguments expand a leading `~`.** Every built-in tool argument that
  names a file or directory (`path`, `file_path`, `root`, `paths`, and `workdir`
  of `shell`) treats `~` and `~/...` as the home directory of the user running
  the agent. Before, the path was taken relative to the working directory, so
  `~/x` named a directory called `~`. `~name/...` and a `~` that is not first are
  unchanged. Approval prompts and file-conflict detection use the expanded path;
  the arguments stored in the assistant message are not rewritten. If no home
  directory is known the call fails with an error naming the argument. New
  `ToolContext::resolve_path_arg(arg, value)` (default method; `resolve_path`
  calls it with `path`); `resolve_target` in the built-in helpers takes the
  argument name and returns a `Result`.
- **Fewer exported spans by default (telemetry).** Actor message dispatch spans
  (`kameo`, about 60% of exported spans) are no longer exported unless
  `QMT_TELEMETRY_LEVEL` names `kameo` (for example `info,kameo=info`), and
  eleven per-step bookkeeping spans (`middleware.driver`, `middleware.phase`,
  the `agent.execution.middleware.*` spans, `agent.tool.side_effects`,
  `agent.request.cache_breakpoints`, `model_inventory.get_snapshot`,
  `function_index.remove_file`) are exported only at `debug`. Model request and
  tool call spans are unchanged.
- **Model request spans report their attempts (telemetry).** The `chat {model}`
  span carries `gen_ai.request.attempt` (attempts started) and one event of that
  name per attempt that ended, with `error.type` on failed attempts. Error
  messages are not exported. `otelite retries` lists a request
  retried this way with its final attempt number (checked against a local
  otelite).
- **llama-cpp:** adjacent plain user messages are joined (with a blank line)
  before the chat template is applied, because the agent now stores notices as
  user messages and strict templates (Gemma, Mistral) reject two user messages
  in a row. Media markers keep their order; a message with several nested tool
  result images now yields one user message holding all markers.
- **Request attribution:** `llm_request_end` events carry `prefix_change`
  (`first_request`, `model`, `tools`, `system`, `history` or `none`) and
  `idle_secs`, and the model request span carries `qmt.cache.prefix_change` and
  `qmt.cache.idle_secs`.
- **Tool order:** the tool list sent to the model is sorted by name, and
  `ToolRegistry` keeps its tools in name order too, so the tool prefix is
  byte-stable between requests.
- **Tool changes inside the conversation (Anthropic):** a tool that appears,
  disappears or changes mid-session (an MCP server connecting, `tools/list_changed`,
  a changed `skill` description) no longer rewrites the prompt cache on models
  that accept in-conversation tool changes (Opus 4.8, 5 and 5.5, Sonnet 5.5,
  Haiku 5.5, Fable 5 and Mythos 5 families). The agent stores the offered tools
  as a `tool_baseline` message part (before the first request, after a
  compaction and after a model change; never sent as a message) and later
  differences as `tool_change` parts, sent as `tool_addition` / `tool_removal`
  entries with the `inline-tools-2026-09-15` beta header. The request keeps the
  baseline's tool list and records `prefix_change = "none"`. Other providers are
  unchanged. New `querymt` API: `ChatCapabilities::tool_updates` (default
  `false`, so older provider plugins keep working), `ChatInputPart::ToolChange`,
  `ToolChange`, `ChatMessage::from_tool_change`, `reject_tool_changes`, and
  `LLMProviderFromHTTP::with_name`. Rollback: a session holding a `tool_baseline`
  or `tool_change` part cannot be loaded by a build from before this change
  (unknown message part kind), like `notice` above.
- **Compaction:** the summary is followed by the session's active task, so a
  compacted session keeps its task commitment.
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
- **BREAKING (mesh sharing):** Agent mesh runtimes start with no sharing grants.
  Joining a scope does not grant access in either direction. Use ACP to grant
  access explicitly; pre-upgrade clients require Full access on an upgraded host.
- **Mesh default access:** peers without a saved grant get the union of host
  defaults. `[mesh] lan_access` (CLI `--mesh-lan-access`, env
  `QMT_MESH_LAN_ACCESS`, Python `MeshRuntime.lan(lan_access=...)`) applies to
  peers connected from a private-network address; it defaults to `"none"`.
  `[[mesh.trust]]` scopes give holders of an offline-signed token access over
  any transport; issue tokens with `qmtcode --mesh-trust-issue`. ACP
  `getPeerAccess` reports `outgoing.source`, and the new
  `querymt/mesh/clearPeerAccess` returns a peer to the defaults. Rust:
  `MeshRuntimeConfig` has a new `access` field (**BREAKING** for struct
  literals), `AccessEdit` has a new `Inherit` variant, `GrantStore` gains
  defaulted `grant_source` and `access_defaults`, and the grant-aware wire adds
  a trust presentation exchange.
- **Mesh remote sessions with your own model:** a remote session bound to a
  model on the prompting node no longer fails with `mesh_access_denied`. The
  host sends that inference back to the prompting node, which authorizes and
  meters it, instead of checking its own model grant and quotas. Session access
  is enough to select such a model; selecting one of the host's own models
  still needs Full. A request for an actor that is not
  exported, such as one from before the host restarted, now fails as
  `ActorNotRunning` instead of an unknown message, so clients reattach. Hosts
  log refused mesh requests and denied turns with the reason.
- **Mesh model passes (third-node models):** a hosted session may be bound to a
  model on a node that is neither the session host nor the prompting peer (a
  desktop prompts an agent on a phone that uses a GPU server). Session access is
  enough to select such a model. The prompting node signs a short-lived
  **model pass** (default one hour, no renewal) that names the session host, the
  model node, the provider, the model and the session, and attaches it to the
  `Prompt` or `SubmitSessionInput` when the model node reports that it shares
  the model with it. The host runs the turn under the pass: the model node
  authorizes and meters the host's calls under the prompter's grant and
  quotas, the host checks neither its own model grant nor its own quotas, and
  removing the prompter's access on the model node stops the stream. Without a
  pass, only a peer with Full access can have the host use its own access on a
  third node; below Full, and for scheduled turns, the host refuses the turn. A
  pass that does not match the turn, and a model node that does not confirm
  that it charges the issuer (for example one that predates passes), refuse the
  turn: the host never falls back to its own access. Rust, feature `remote`:
  `querymt_remote::ModelPass`; `GetProviderContractInfo`,
  `ProviderChatRequest` and `ProviderStreamRequest` gain an optional `pass`,
  `ProviderContractInfo` gains `charged_peer` (**BREAKING** for struct
  literals), `Prompt` and `SubmitSessionInput` gain an optional `pass`, and
  `InboundAccess` gains `Handler`.
- **Mesh quotas:** Provider and model quotas apply to all shared models on Custom
  grants. Direct calls and hosted turns use the same prompt-plus-output estimate.
  Actual usage is charged even above the estimate; missing usage is charged at
  the estimate by default. Exhaustion blocks new requests but lets admitted work
  finish. Full access does not spend Custom quotas.
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


### Breaking

- **BREAKING: the middleware trait is replaced by `Middleware` and
  `TurnMiddleware`** (`querymt_agent::middleware`). A middleware is now an
  agent-scoped `Middleware` (configuration and shared services) that creates one
  `TurnMiddleware` per run; all state of a run lives in that instance, so it
  cannot leak into another session or the next run (before, per-run state kept
  in a shared driver was cleared by `reset`, which no run called). Hooks take a
  `HookCx` (reads of the run, `queue_notice`, `set_request_overrides`) and
  return a verdict that names only what the stage allows (`StepVerdict`,
  `ReplyVerdict`, `EndVerdict`, `StopVerdict`) instead of any `ExecutionState`.
  Before and after, a middleware that stops a run at its sixth request:

  ```rust
  // Before
  struct Budget;
  #[async_trait]
  impl MiddlewareDriver for Budget {
      async fn on_step_start(&self, state: ExecutionState, _rt: Option<&Arc<SessionRuntime>>)
          -> Result<ExecutionState> {
          match state {
              ExecutionState::BeforeLlmCall { context } if context.stats.steps >= 5 =>
                  Ok(ExecutionState::Stopped {
                      message: "budget used".into(),
                      stop_type: StopType::Other,
                      context: Some(context),
                  }),
              other => Ok(other),
          }
      }
      fn reset(&self) {}
      fn name(&self) -> &'static str { "budget" }
  }

  // After
  struct Budget;
  impl Middleware for Budget {
      fn name(&self) -> &'static str { "budget" }
      fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> { Box::new(BudgetTurn) }
  }
  struct BudgetTurn;
  #[async_trait]
  impl TurnMiddleware for BudgetTurn {
      async fn on_step_start(&mut self, cx: &mut HookCx<'_>) -> Result<StepVerdict> {
          if cx.stats().steps >= 5 {
              return Ok(StepVerdict::Stop(Stop::new(StopType::Other, "budget used")));
          }
          Ok(StepVerdict::Proceed)
      }
  }
  ```

  Removed from the trait: `reset` (a run gets new instances), the
  `on_processing_tool_calls` hook (only test code implemented it), the
  `Option<&Arc<SessionRuntime>>` parameter (use `HookCx::runtime`), a hook's
  freedom to return any `ExecutionState` (a middleware can no longer complete or
  cancel a run), and `get_limits` (now `Middleware::limits`). `on_turn_start`
  remains and returns a `StepVerdict`. `CompositeDriver::run_*` and `reset` are
  gone; `CompositeDriver::begin_turn` returns the run's `TurnStack`.
  `MiddlewareFactory::create`, `AgentBuilder::middleware`,
  `AgentConfigBuilder::with_middleware(s)` and `AgentConfig::middleware_drivers`
  take `Arc<dyn Middleware>` / `impl Middleware`. See the
  [middleware guide](docs/docs/agent/middleware.md).
- **BREAKING: five unused middleware types are removed:**
  `DelegationGuardMiddleware`, `DuplicateToolCallMiddleware`,
  `MaxStepsMiddleware`, `TurnLimitMiddleware` and `PriceLimitMiddleware`. None
  was built by any preset, assembly or configuration entry. Use
  `LimitsMiddleware` for the three limit types
  (`LimitsConfig::default().max_steps(n)`, `.max_turns(n)`,
  `LimitsConfig::with_manual_pricing(input, output).max_price_usd(cap)`); the
  other two have no replacement. `StopType::DelegationBlocked`,
  `NoticeSource::DuplicateToolCall` and `Delegation::retry_count` stay.
- **`SessionRuntime::turn_diffs` is removed.** The files a run changed are held
  by the run itself and reach middleware through `HookCx::changed_paths`.
- **BREAKING (Rust API): the run no longer holds its history as provider
  messages.** `HookCx::history()` returns `&[AgentMessage]`, the messages as the
  session stores them (a notice with its source, a tool baseline, a steering
  message), not the `&[ChatMessage]` a provider is sent. `ConversationContext`
  is removed, and the `ExecutionState` variants no longer carry a `context`: the
  history, statistics, queued notices and request overrides of a run belong to
  the run and reach a hook through `HookCx` (`history`, `stats`,
  `pending_notices`, `last_applied`, `request_overrides`).
  `SessionHandle::add_message` is `pub(crate)`, and `SessionHandle::history`
  and `SessionHandle::project_history` are removed: the run is the only writer
  of its history, and a model or compaction request is projected from the stored
  messages in one place. Public `SessionRuntime` fields `turn_snapshot` and
  `pre_turn_snapshot_task` are removed with `turn_diffs`.
- **BREAKING (Rust API): the removed public `SessionRuntime` fields.**
  `SessionRuntime::turn_snapshot`, `pre_turn_snapshot_task` and `turn_diffs` no
  longer exist: they were state of one run kept on the session. The pre-turn
  snapshot and the changed paths belong to the run, and a reader fails to
  compile instead of reading a value of another run.
- **BREAKING (Rust API): `BudgetWarnings::take_new` takes `&mut self`**
  (`querymt_agent_services::accounting::BudgetWarnings`). It held a mutex only
  because the run shared it; the run now owns it.
- **BREAKING (Rust API): `ExecutionState` is removed** from
  `querymt_agent::middleware`. It was the loop's position, and middleware stopped
  using it when hooks got typed verdicts; the loop's position is now internal.
  `ExecutionOrigin` is exported from `querymt_agent_runtime::agent::execution`
  (the module `execution_context` is gone), and a run's failure is the internal
  `TurnError`, which keeps its source (a provider error, or the name and source
  of a failed middleware) where the loop used to wrap it in a string.
- **A scheduled run executes like an interactive one.** Both start through one
  `SessionActor::start_run`, and the run reports its phase through a `watch`
  channel, so a burst of phase changes publishes the latest phase instead of
  every step. Events and their order are unchanged. A provider that cannot be
  built now fails the run before the first middleware hook and before the model
  is asked, the provider is built once per run, and a middleware error names the
  middleware.
  A failed write of the stored conversation keeps the store's error as its
  source; the messages that were already prefixed (`Failed to store assistant
  message: ...`, `... wait message: ...`, `... tool results: ...`) are unchanged,
  and the writes that reported the store's bare error (queued notices, steering,
  tool baseline and change records, intent projection) now carry the same kind of
  prefix.
- **A run that panics fails instead of leaving its session stuck.** The panic
  is caught where the run is started and becomes the run's failure: the client
  gets an `Internal` error (`run panicked: ...`), an `Error` event is emitted,
  a scheduled run emits `ScheduledExecutionFailed`, and the run is closed as
  `failed`, so the run lease is released or handed to the next queued prompt.
  Before, the session kept a run that never ended: later prompts queued behind
  it for good and the client was told `prompt execution dropped`. The workspace
  changes a panicking run made are still recorded for undo, and steering the
  run had accepted but not applied gets a `SteeringDiscarded` event
  (`run_failed`) instead of none; a run that fails by an error ends the same
  way.

### Fixed

- **A failed scheduled run is attributed to its own cycle.** The scheduler now
  names the run it fires, and the run's terminal events (including a failure
  and the cycle timeout) carry that id. Before, every failure was reported
  without a run id and the scheduler remembered it per schedule, so after a
  schedule's first failure its later failures were dropped as duplicates (the
  schedule was never re-armed with backoff or marked failed), and the late
  failure of a cycle that had timed out could end the cycle that followed it.
  A terminal event of an earlier cycle is now ignored while a later one is in
  flight, and failures that name no run (startup recovery, the reconciliation
  sweep) are each applied.

- **`shell` returns when its command exits.** A command that leaves a background
  process holding its output (for example `server & echo started`) no longer
  keeps the call open until that process ends: the call waits at most one second
  for the remaining output after the command exits. The leftover process keeps
  running, and output it writes afterwards is not part of the result. The result
  format and its truncation are unchanged.
- **Every started tool call gets one end event.** When a run was cancelled, a
  running tool call could be dropped without a `tool_call_end` event, depending
  on whether the cancel signal or the tool's own cancellation was observed first.
  Such a call now always ends with an error event reporting the cancellation. A
  call that was running when the agent process died is closed as interrupted when
  the run is reconciled on the next load.
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

- **`SessionHandle::chat`, `SessionHandle::submit_request` and
  `SessionHandle::convert_chat_to_agent` removed** from
  `querymt_agent_runtime::session`. `chat` sent the stored history as it was
  and skipped the agent's per-request tool handling, so it failed on any session
  with stored tool lists or tool changes; the other two had no other caller.
  Send prompts with `AgentSession::chat`, `Agent::chat` or ACP
  `session/prompt`.
- **`querymt_agent::middleware::PreparedModelRequest` has a new public field
  `offered_tools`**; struct literals must add it. It holds the tools the model
  can call in the request. `tools` is the request's tool list, which on a
  provider that takes tool changes as messages is the list stored when the
  session started on that model: tools added or removed since are not in it.
  Middleware that asks "can the model call X" must read `offered_tools`.
- **`querymt_agent::middleware::ConversationContext` can no longer be built
  with a struct literal outside its crate**: it has a crate-private field (the
  tool baseline in effect). Use `ConversationContext::new` and the `with_*`
  methods.
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

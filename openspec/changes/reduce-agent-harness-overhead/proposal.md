# Proposal

## Why

Session logs and trace spans for 2026-11-05 to 2026-11-19 (373 sessions, 19.2k model requests, 27.6k tool calls) show the harness wasting tokens, wall time and storage in a few repeatable ways:

- **Prompt cache.** The newest cache breakpoint is never read back. When the first request of a turn is a full cache write, the second is a full write again in 441 of 542 turns (58.2M tokens, 19% of all cache writes), and in steady state 51.2M tokens are written for 28.1M tokens of context growth. Request-scoped context (the run objective) is merged into, or placed before, the newest history message, so that message differs on the next request. Five kinds of harness notice (stop-hook continuations, the task-completion reminder, two duplicate warnings, the parallel-edit note) are added in memory and never stored, so they are missing when the next turn reloads history; this is read from the call sites and not yet shown by a test.
- **Run objective.** In the 14 days to 2026-11-20 (18.9k requests, 1,330 runs) the run objective was the prompt again plus one fixed sentence in over 90% of runs, about 150 tokens per request. Nothing measures whether it changes behaviour.
- **Shell calls.** A call returns only when every holder of its output pipes has exited, not when the command exits, so a command that leaves a detached process behind blocks until that process dies. 39 `shell` starts have no end event, 27 of them followed by a cancellation.
- **Harness overhead.** 1.81M of 3.04M exported spans are sub-millisecond actor dispatch spans, and another 0.44M are per-step bookkeeping spans; together 74% of the export. Clients poll `runtimeState` (173k calls), `session/list` (41.8k, 289 ms average) and `getLifecycle` (31k). Two provider plugins whose files are missing are loaded again on every model listing and authentication status read (444 and 415 attempts, up to 111 in one process), and the failed attempts are exported as successful spans.
- **Journal volume.** 46.8k `progress_recorded` events (18% of journal rows, 28 MB) repeat data that is already in other events.

## What Changes

The groups are independent and can land separately, in the order below.

**A. Prompt caching (all providers)**

- Nothing is merged into or placed before a stored message. What the harness tells the model (mode reminders, output-limit guidance, the progress review, stop-hook continuations, the task-completion reminder, duplicate warnings, the parallel-edit note) is stored once as a message in history instead of being rebuilt or re-added for each request. Default message hints go on the last two durable messages.
- A mode reminder is stored when the mode at request time differs from the last stored one, with a leave notice when a mode that has a reminder is left. **BREAKING** (behaviour): the reminder is no longer repeated on every request.
- The run objective is not sent by default. New key `[agent.execution] run_objective` (boolean, default `false`) sends it again, after the last durable message. **BREAKING** (behaviour): requests no longer carry the objective unless the key is set.
- The only other request-scoped text is `additional_context` from `context` hooks, sent after the last durable message. **BREAKING** (hooks): `context` hooks receive durable history only.
- Tool definitions are sent in name order, so the same tools give the same request after a restart or an MCP refresh (35 of 38 tool-list changes in the 14 days were order only).
- Providers declare prompt-cache support (explicit breakpoints, longest selectable lifetime) in their chat capabilities. Default system hints follow that declaration instead of a provider-name list. **BREAKING** (provider plugins): a plugin built before this change declares nothing and gets no default system hints until rebuilt.
- New config `[agent.execution.prompt_cache] ttl_secs`, requested by default hints, capped at the longest lifetime the provider declares; the provider rounds it to one it supports.
- Every model request records why its cache prefix changed (`prefix_change`) and how long the session was idle (`idle_secs`), in the request-end event and the trace span.

**B. Background commands (moved out)**

- Moved to a separate change, `add-background-commands`. See Non-goals.

**C. Shell execution**

- A call returns when its command exits, even if detached descendants keep its output open.
- A cancelled call always gets an end event (27 cancelled calls in the window have none).
- What a `shell` result contains and how large output is cut stay as they are. See Non-goals.

**D. Retries and output cap**

- Transport failures (timeout, connect failure, broken stream) retry after `[agent.execution.rate_limit] transport_wait_secs` (default 10) instead of `default_wait_secs` (60).
- At reasoning effort `high` or `max` the initial output cap is `min(64000, model output limit)`. 15 of the 16 escalations in the window were at those efforts and each discarded a complete reply; every retried reply then fit within 64,000 tokens.

**E. Tool errors and arguments**

- A call to a tool that was not offered says so and names the closest offered tools, instead of "is not allowed".
- Built-in tool path arguments expand a leading `~`.

**F. Journal content**

- No progress entry or `progress_recorded` event per tool call or per model response. **BREAKING** (event stream): consumers of raw `progress_recorded` events stop receiving them for these two cases.
- `snapshot_start` and `snapshot_end` are delivered live but not journaled.
- `provider_changed` is recorded only when the selection changed (146 of 2,764 repeat the previous value).

**G. Client state reads**

- New notifications `querymt/session/lifecycleChanged` and `querymt/sessions/changed`, so clients need not poll `getLifecycle` and `session/list`.
- A lifecycle request for a session whose owner node is not connected fails within a second instead of waiting out the 10 s remote timeout (73 calls in the window sat at that timeout).
- `session/list` itself: the stored-history aggregate takes 6 ms on the current database, and 213 ms of the 289 ms average is wait time no span accounts for. This change adds the spans needed to find it; no behaviour change is specified for it yet.

**H. Telemetry and plugin loading**

- Per-message actor dispatch spans are not exported at the default telemetry level, and eleven per-step bookkeeping spans move to `debug`.
- A provider plugin that failed to load is not attempted again until its file changes, a wait has passed (remote sources) or a model refresh is requested. Failed attempts are exported as errored spans.
- Model request spans carry `gen_ai.request.attempt`, which is what `otelite retries` reads (it reports no retries today although 33 happened).
- `otelite cache` also reports nothing although `otelite usage` reads our cache tokens; the cause is not established, so this change only investigates it. `otelite agents` rolls up opencode, codex and claude only, so it stays empty until otelite recognises our scope (upstream, not planned here).

**I. Tool changes without a cache rewrite (lands last)**

- A provider can declare that tools may be added, removed or redefined inside the conversation. For such a provider the tool list at the head of the request stays fixed for the session, and a change is stored as a marker message. Other providers get the new list, as today.
- `qmt-anthropic` declares this for the models that support it and sends the markers. It is a beta API whose wire shape we know only from opencode's source; the group's first task confirms it before any code.
- Expected saving is small: 3 real tool-list changes in the 14 days (1 add or remove, 2 `skill` description changes), none of them mid-run.

## Non-goals

- **Background commands and a limit on blocking `shell` calls.** `shell` keeps running without a time limit: 270 calls ran 5 minutes or longer (up to 13.4 h), mostly `sleep N; check` and `ssh … nohup` commands, and 311 requests rewrote the whole context (66.8M tokens) because the 5-minute cache lifetime had passed while a tool was running. An earlier revision of this change added background commands, a 240-second foreground limit, `job_wait` and `job_kill`, exit notices and wake-up runs. They moved to a separate change, `add-background-commands`, together with `QMT_SESSION_ID`, the dedicated-tool hints and `background_ttl_secs`. Design "Deferred to `add-background-commands`" says why, and what that change has to settle first.
- **Changing `shell` output.** The shared truncation step keeps only the head of a tool result, so an oversized `shell` result loses the end of its output and `stderr` and is no longer valid JSON. An earlier revision kept both ends and wrote the complete output to a file; it was dropped because it changes what the tool returns, and how much output to keep is already the model's choice (2,048 of 2,115 cargo invocations are piped through `tail`/`head`/`grep`).
- **Batched image removal.** No code removes images from requests, and tool-result pruning already runs once per turn above a threshold.
- **Removing the run objective code.** The bookkeeping and its events stay; only sending it is switched off.

- **Storing tool results once.** Results are in `tool_call_end` events (82 MB) and in message history (96.6 MB). Removing one copy needs a journal/history storage redesign that overlaps `add-agent-session-core` and `batch-journal-writes-across-sessions`.
- **Cache keep-alive requests** during long waits. See design.
- **Pre-turn snapshot tail.** The 274 s case hashed 3.5 GB, which `workspace-snapshots` requires; `.ignore` is the remedy. The 439 s case came from a node on an older build.
- **Request serialization time** (`http_adapter.prepare_request`, 106 ms average against an 8 s median model call).
- **Re-read detection for `read_tool`.** Only 83 of 4,793 calls repeat identical arguments; the rest read other ranges or follow an edit.
- **Classifying shell commands as read-only** to skip snapshots.
- **Already fixed:** the knowledge tool table errors, and the "maximum of 4 blocks with cache_control" errors (last seen 2026-11-11, the date of `3e203124 fix: cache budget`), and slow prompt-record fetches on remote session loads (193 calls of 5 s or more, all on 2026-11-13 to 2026-11-16, none since `99623996` merged on 2026-11-16).
- **`session_config_changed` repeats.** The 713 identical consecutive payloads follow model switches; the event carries mode and effort only. No change.
- UI work in `querymt-desktop`; see Impact for the follow-ups.

## Capabilities

### New Capabilities

- `shell-command-execution`: when a `shell` call completes and how its cancellation is reported.
- `llm-request-retries`: how long the agent waits before retrying a failed model request.
- `provider-plugin-loading`: when the agent attempts to load a provider plugin again after a failed attempt.
- `session-journal-content`: which session activity is stored in the journal and which is live-only or not recorded.
- `agent-telemetry`: which trace spans are exported at the default telemetry level, and the attempt number on model request spans.

### Modified Capabilities

- `agent-prompt-caching`: hints go on durable messages, system defaults follow provider declarations, the durable prefix is stable between requests, harness notices are stored, the run objective is opt-in, tool definitions have a stable order, tool changes can be sent as markers, lifetimes are configurable, prefix changes are attributed.
- `output-limit-recovery`: the initial output cap depends on reasoning effort; continuation guidance is stored instead of removed after the next complete reply.
- `anthropic-messages-protocol`: tool-change markers on the wire.
- `tool-selection`: error for a call to a tool that was not offered.
- `tool-argument-handling`: `~` expansion in path arguments.
- `acp-session-updates`: change notifications for session lifecycle and for the set of sessions; prompt failure of lifecycle requests when the owner node is not connected.

## Impact

**Packages**

- `querymt`: chat capabilities gain a prompt-cache declaration and a tool-update declaration, and chat messages gain a tool-change kind that crosses the Extism boundary (public Rust API, additive); the plugin registry remembers failed loads (features `extism_host`, `tracing`); this also applies to `querymt-cli` and `querymt-py`, which use the same registry.
- `qmt-anthropic`, `qmt-openai`, `qmt-codex`: declare prompt-cache support (features `native` and `extism`). `qmt-anthropic` also sends tool-change markers.
- `querymt-agent-core`: new `llm_request_end` fields, a new config table and keys, new stored message part kinds for notices and tool changes.
- `querymt-agent-services`: `shell` completion, path resolution, system cache defaults.
- `querymt-agent-runtime`: request assembly, stored notices, mode reminders, hints, context hooks, tool order, retries, output cap, tool dispatch errors, ACP notifications, session listing spans, remote lifecycle requests (feature `remote`).
- `querymt-agent-sqlite`: the lifecycle update reports whether anything changed.
- `querymt-agent-index`: one span moves to debug level.
- `querymt-utils`: default trace filter.
- `querymt-agent`: embedded example configs and prompts, facade tests.

**Contracts**

- Config: new table `[agent.execution.prompt_cache]`; new keys `[agent.execution] run_objective` and `[agent.execution.rate_limit] transport_wait_secs`.
- Extism plugin contract: the capabilities JSON gains an optional field. No contract version bump is planned; an older plugin simply declares no cache support. This is the one compatibility decision to confirm in review.
- typeshare / ACP: two new fields on `llm_request_end`, two new notifications, and two new message part kinds in loaded history (notice, tool change).
- Session DB schema: no migration expected; parts are stored as JSON. A build from before this change may not load a session that contains the new part kinds (see design "Migration Plan").
- Python bindings: no API change.

**querymt-desktop follow-ups (not planned here)**

- Replace polling of `runtimeState`, `models`, `delegateModels`, `getLifecycle` and `session/list` with the existing and new notifications.
- Surface `prefix_change` (for example the cost of a model switch on a large session).
- Render or hide the new notice and tool-change parts in loaded history.
- Confirm nothing reads `progress_recorded` for tool calls or the journaled snapshot events.

**In-flight changes to coordinate with**: `add-agent-session-core` (event types), `batch-journal-writes-across-sessions` (journal writer), `adopt-acp-subagents-cursors-usage` (ACP updates).

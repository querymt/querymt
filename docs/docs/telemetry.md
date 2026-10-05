# Telemetry

QueryMT collects telemetry to help the project maintainers understand how the
software is used and to diagnose issues. This page explains what is collected,
where it is sent, and how you can control it.

The telemetry subsystem lives in `querymt-utils` and is shared by both the
**CLI** (`qmt`) and the **Agent** (`qmtcode`).

---

## What QueryMT Collects

QueryMT uses [OpenTelemetry](https://opentelemetry.io/) (OTLP over gRPC) to
export two kinds of signals: **traces** and **logs**.

### Traces (spans)

Traces are structured timing and execution-flow records. Each operation is
wrapped in a named *span* that captures when it started, how long it took, and
a small set of metadata fields (counts, identifiers, status). Spans are
organised hierarchically so that a single user action (e.g. a chat prompt)
produces a tree of child spans describing every step that was executed.

#### CLI spans (`qmt`)

| Span name | Description |
|---|---|
| `cli.providers` | Listing configured providers |
| `cli.models` | Listing available models |
| `cli.embed` | Generating embeddings |
| `cli.update` | Updating provider plugins |
| `cli.chat.pipe` | Processing a piped / single-shot chat prompt |
| `cli.chat.interactive` | Running an interactive REPL session |

#### Agent spans (`qmtcode`)

**Execution**

| Span name | Description |
|---|---|
| `agent.prompt.execute` | Top-level prompt execution |
| `agent.execution.turn` | A single execution turn |
| `agent.execution.history_load` | Loading session history |
| `agent.execution.middleware.*` | Middleware phases (`turn_start`, `step_start`, `after_llm`, `processing_tool_calls`) |

**Tool calls**

| Span name | Description |
|---|---|
| `agent.tool.execute` | Executing a single tool call (exported as `execute_tool {tool}`) |
| `agent.tool.invoke` | Invoking a single tool (includes `source`: `builtin`, `mcp`, or `provider`) |
| `agent.tool.permission` | Evaluating tool permissions |
| `agent.tool.permission_wait` | Waiting for user permission |
| `agent.tool.side_effects` | Processing tool side-effects |
| `agent.tool.snapshot.*` | Workspace snapshot operations (`prepare`, `diff`, `metadata`) |
| `agent.tools.store_all_results` | Persisting tool results |

**Snapshots**

| Span name | Description |
|---|---|
| `agent.snapshot.pre_turn.ensure` | Ensuring a snapshot exists before a turn |
| `agent.snapshot.pre_turn.resolve` | Resolving the snapshot state |
| `agent.snapshot.pre_turn.track` | Tracking snapshot changes |

**ACP protocol**

| Span name | Description |
|---|---|
| `acp.initialize` | Initializing the ACP connection |
| `acp.authenticate` | Authenticating a client |
| `acp.new_session` | Creating a new session |
| `acp.prompt` | Handling a prompt request |
| `acp.cancel` | Cancelling an in-flight prompt |
| `acp.load_session` | Loading a stored session |
| `acp.list_sessions` | Listing sessions |
| `acp.fork_session` | Forking a session |
| `acp.resume_session` | Resuming a session |
| `acp.set_session_model` | Changing the session model |
| `acp.set_session_mode` | Changing the session mode |
| `acp.set_session_config_option` | Updating a session config option |
| `acp.ext_method` | Handling an extension JSON-RPC method |
| `acp.ext_notification` | Handling an extension notification |

**UI / Dashboard**

| Span name | Description |
|---|---|
| `ui.init` | UI initialisation |
| `ui.handle_list_sessions` | Building the session list view |
| `ui.handle_list_sessions.remote_merge` | Merging remote sessions into the list |

**Middleware**

| Span name | Description |
|---|---|
| `middleware.phase` | Running a middleware phase |
| `middleware.driver` | Middleware driver orchestration |
| `middleware.dedup_check.analyze` | Duplicate-content analysis |
| `middleware.dedup_check.update_index` | Updating the dedup index |
| `middleware.dedup_check.turn_end` | Dedup end-of-turn bookkeeping |

#### GenAI trace conventions

Agent inference, tool execution, and prompt invocation reuse the execution spans
listed here rather than adding a second semantic span. Their exported OTel names
are `chat {model}` (CLIENT), `execute_tool {tool}` (INTERNAL), and `invoke_agent`
(INTERNAL). Compaction has one CLIENT `chat {model}` span around its retry loop.
Delegation summarization has one CLIENT `chat {model}` span only when it calls a
provider; raw-history and existing-compaction shortcuts do not create inference
spans. Its timeout is recorded inside the inference span. Unsuccessful summary
outputs (non-completed status, truncation, filtering, errors, or tool calls) are
rejected without persisting or injecting partial text; available response metadata,
actual finish reason, and billed usage remain recorded before the bounded error.
Each chat span covers all retries and the terminal stream. Normal inference drains
trailing usage; compaction collects already-buffered trailing usage without waiting. Structured output and its legacy projections do not
produce separate semantic spans.

The mapping is pinned to the development conventions at
[`e07f4ebacb08f56db8c4c882d117720333fbca04`](https://github.com/open-telemetry/semantic-conventions-genai/tree/e07f4ebacb08f56db8c4c882d117720333fbca04):
[client spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/e07f4ebacb08f56db8c4c882d117720333fbca04/docs/gen-ai/gen-ai-spans.md)
and [agent spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/e07f4ebacb08f56db8c4c882d117720333fbca04/docs/gen-ai/gen-ai-agent-spans.md).
Attribute strings are local; this does not change dependencies, telemetry
configuration, exporters, or add metrics.

- Operation, known provider/request model, tool identity, and session conversation
  ID are attached at creation. Configured provider aliases are the initial best
  knowledge; canonical provenance can refine the provider afterwards, but never
  changes the requested model or the span name. `codex` maps to `openai`, `google`
  to `gcp.gemini`, `mistral` to `mistral_ai`, `xai` to `x_ai`, and `moonshotai`
  to `moonshot_ai`. Compaction's generic
  `ChatProvider` does not expose a provider name, so it is only recorded when
  canonical provenance supplies it. Unavailable agent names, fixed agent models,
  and response models are not invented. Available provenance models are recorded
  as `gen_ai.response.model`: built-in adapters can supply the server-reported
  response model (including resolved request aliases), while external providers
  may omit provenance.
- `gen_ai.agent.id` uses the actual nonblank configured agent ID on prompt invocation,
  normal chat, and tool execution. It is stable across prompts and sessions within
  that agent configuration, not globally unique: the ordinary API defaults to
  `agent`, and independent servers can use the same ID. It is not an agent name or
  a session UUID. Summary and compaction do not inherit an unknown agent identity.
- A successful built-in skill load adds the resolved catalog `gen_ai.skill.name`
  and refines the existing tool span to `execute_tool skill {skill.name}`. The
  operation remains `execute_tool`; no separate skill span is created. The name
  comes from canonical registered metadata after effective pre-hook arguments,
  validation, permission checks, and successful loading, not from an arbitrary
  request or another tool named `skill`. Skill content, descriptions, paths, and
  source URIs are not attached.
- Normal chat adds `gen_ai.conversation.compacted=true` only when the final request
  retains a nonblank successful summary from typed effective history, matched by
  role and payload (ignoring cache hints). History reloads and successful compaction
  rebuilds refresh this private provenance. Requests alone, removed/modified summaries,
  and unverified summary-like text omit the attribute; `false` is never emitted.
  Compaction and delegation-summary chat spans do not currently attach this flag.
- The built-in shell refines the existing INTERNAL tool span to
  `execute_tool shell {process.executable.name}` when a best-effort Linux
  `/proc/[pid]/exe` lookup started immediately after spawn yields an executable basename.
  The lookup does not delay process waiting or cancellation; identity may be omitted
  if completion or cancellation wins. This identifies the observed executable
  (possibly a script interpreter or shell), not a parsed command or requested symlink.
  Lookup failures and other platforms also omit identity. `process.exit.code` records
  only an actual numeric status, including nonzero exits, without changing tool success
  semantics. Cancellation/drop retains already-recorded identity but omits exit code
  without a status. Pre-spawn
  failures and custom tools named `shell` add no process metadata. No separate process
  span, executable path, command/argv, stdout, or stderr is attached.
- Streaming chat records `gen_ai.response.time_to_first_chunk` as a floating-point
  duration in seconds from the first physical generation request to the first
  received successful chunk, including stream creation time. Metadata, empty,
  usage, and terminal chunks qualify; this is not time to the first visible token.
  Provider construction and waits before the first request are excluded. Retries
  keep the original start and first measurement, even if that first chunk belongs
  to a later-failed attempt. Errors and reconnect controls do not qualify. No
  measurement is recorded for nonstreaming calls or cancellation/drop/error before
  a successful chunk; a measurement already recorded is retained.
- Normal chat can attach typed effective `gen_ai.request.max_tokens`,
  `gen_ai.request.temperature`, `gen_ai.request.top_p`, and
  `gen_ai.request.reasoning.level`. A private scalar snapshot is bound to the
  constructed provider using the final merged, model-defaulted, schema-pruned
  configuration, not the initial agent parameters or a later binding lookup.
  The allowlist uses the implementation factory's name and an official QueryMT
  OCI identity: OpenAI (Chat Completions and Responses), Codex (no max tokens,
  which its request ignores), and Anthropic (temperature 1.0 with reasoning;
  thinking mode/budget is not reported as a reasoning level). OpenAI/Codex `max`
  effort maps to the sent `xhigh`. Extra-body fields other than known storage,
  prompt-cache-key, and verbosity fields conservatively omit all settings. Absent, malformed, unsupported, or unverified values are not
  inferred from server defaults. Custom/static/local-path factories, unidentified
  aliases, mesh providers, Google/XAI mappings, and summary/compaction settings
  remain deferred; no provider rebuild, extra auth lookup, or config/credential
  retention is added for telemetry.
- `session.id` is a collector interoperability alias of `gen_ai.conversation.id`,
  using the same stable session UUID as a string at creation on all five boundaries:
  prompt invocation, normal chat, tool execution, compaction chat, and delegation
  summary chat. Repeated prompts retain that UUID. Parent work, compaction, and
  summaries use the parent ID; a delegate uses its own ID, even in the same trace.
  Unavailable or blank history IDs omit both attributes. OTel does not automatically
  copy parent attributes, so these are explicit span fields, not a global resource
  attribute on a concurrent server. This applies only to newly emitted spans;
  historical data is not backfilled. Broader diagnostic span/log enrichment is deferred.
- Canonical response IDs, when present, and typed string-array finish reasons are
  recorded without content. Inclusive input usage sums the normalized exclusive
  input/cache-read/cache-write buckets; inclusive output sums output/reasoning.
  Arithmetic widens to `i64` first. OpenAI/Codex subtract cache/reasoning during
  normalization, Anthropic reports exclusive cache buckets, and current
  Google/Ollama/mrs/llama-cpp adapters leave these extra buckets zero. External
  providers must respect that usage contract; missing modality usage is not
  inferred.
- Errors use an empty OTel status description and bounded `error.type` values:
  `authentication`, `rate_limited`, `invalid_request`, `response_format`,
  `unsupported_operation`, `transport`, `provider_error`, `tool_error`,
  `tool_pipeline_error`, `hook_blocked`, `invalid_tool_arguments`, `policy_blocked`,
  `permission_denied`, `timeout`, `invalid_prompt`, `client_disconnected`, and
  `agent_error`. Transport channel teardown is an error, not an automatic
  successful cancellation; explicit permission cancellation leaves status unset.
  `querymt.tool.execution` values are `not_started`, `hook_failed`, `hook_blocked`,
  `validation_rejected`, `policy_blocked`, `permission_denied`, and `executed`.
  Actual tool failure is recorded before post-hooks can change the model-facing
  result.
  Agent status follows the invocation result, not an arbitrary child failure.
- Span lifetime closes early returns, `?` exits, cancellation, and polled futures
  that are dropped. Cancellation/drop do not set error status. An unfinished
  expected generation retains a typed `["error"]` finish reason; canonical
  output replaces it; completed output without a finish reason records `unknown`.
  SDK 0.32 retains attribute updates in order, with the final
  value representing the attribute map value.
- Detached and queued prompts use only their own valid ACP `_meta.traceparent`
  parent, installed before span entry (required by tracing-opentelemetry 0.33).
  Without one they start a root span, not a child of the actor's long-lived span.
  Chat/tool spans inherit the active execution parent normally. Malformed metadata
  is extracted against an empty context, so it cannot inherit the consumer's span.
- Local live delegation events carry the original emitter span through a bounded,
  private fanout channel. The existing `delegation.orchestrator.handle_event` boundary
  installs that parent before entry (or explicitly starts a root); there is no extra
  dispatch span. The diagnostic `delegation.execute` worker inherits the handler and
  injects W3C `traceparent`/`tracestate` into the child prompt's transient ACP metadata
  before invocation. Context is never added to event JSON, journal records, or
  delegation/session configuration. Public fanout publication, remote event relay,
  and replay carry no context and start independent delegation roots. Cancellation
  does not replace an already-running worker's parent. Internal tool-driven
  delegation is not classified as `invoke_workflow`.

This is **traces only**. Standalone core `LLMBuilder` calls, cross-node mesh
propagation, GenAI metrics, content capture, and exporter/configuration redesign
are deferred. Verification uses capture-based regression tests and a bounded
live qmtcode run against a local collector with an isolated fixture directory and
temporary session database; production databases are not part of these checks.

#### What metadata is attached to spans

Spans may carry lightweight metadata such as:

- **Session ID** — identifies which session the work belongs to.
- **Timing fields** — durations of sub-operations (e.g. `view_fetch_ms`, `remote_merge_ms`, `total_ms`).
- **Counts** — e.g. `message_count`, `files_checked`, `duplicates_found`.
- **Tool source** — whether a tool is `builtin`, `mcp`, or `provider`.
- **Boolean flags** — e.g. `is_error`, `granted`, `cache_hit`.

!!! important
    The GenAI semantic attributes do not capture prompts, responses, tool
    arguments/results, file contents, or API keys. This is not a blanket privacy
    guarantee for preexisting diagnostic logs or span events: verbose logging
    can include payload previews, so use it only with appropriate data controls.

### Logs

Application log messages at the configured level and above are exported
alongside traces. These include operational events such as:

- Provider plugin downloads and cache status
- Tool invocation outcomes (name only, not arguments or full results)
- Connection lifecycle events (WebSocket open/close, mesh peer activity)
- Warnings and errors

At the default level (`info`) logs are limited to high-level operational
events. Lowering the level to `debug` or `trace` will include more verbose
output such as streaming-chunk sizes and abbreviated tool-result previews.

### Payload metadata

Every telemetry payload includes:

| Field | CLI value | Agent value |
|---|---|---|
| Service name | `querymt-cli` | `qmtcode` |
| Service version | Build version at compile time | Build version at compile time |

---

## Where Telemetry Is Sent

By default, both traces and logs are exported via gRPC to the QueryMT
project's OpenTelemetry collector:

```
https://otel.query.mt
```

This is a standard [OTLP/gRPC](https://opentelemetry.io/docs/specs/otlp/#otlpgrpc)
endpoint. You can redirect telemetry to your own collector by setting the
`OTEL_EXPORTER_OTLP_ENDPOINT` environment variable (see below).

---

## How to Control Telemetry

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `QMT_NO_TELEMETRY` | *(unset)* | Set to **any value** to disable all OTLP export. Only local console logging remains active. |
| `QMT_TELEMETRY_LEVEL` | `info` | Filter level for exported traces and logs. Accepts standard levels: `trace`, `debug`, `info`, `warn`, `error`. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `https://otel.query.mt` | OTLP collector endpoint (gRPC). |
| `RUST_LOG` | `error` | Console output filter. Independent of the OTLP telemetry level. |

### Disabling telemetry entirely

```sh
export QMT_NO_TELEMETRY=1
```

When this variable is set (to any value), **no data is sent to any remote
endpoint**. The only active logging layer is the local console formatter,
controlled by `RUST_LOG`.

### Sending telemetry to your own collector

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
```

Any OTLP-compatible collector (Jaeger, Grafana Alloy, the OpenTelemetry
Collector, etc.) will work.

### Adjusting verbosity

```sh
# Only export warnings and errors
export QMT_TELEMETRY_LEVEL=warn

# Include debug-level spans and logs (more verbose)
export QMT_TELEMETRY_LEVEL=debug
```

The console filter (`RUST_LOG`) is independent — you can keep the console
quiet while still exporting detailed telemetry, or vice versa:

```sh
# Verbose console, minimal telemetry
export RUST_LOG=debug
export QMT_TELEMETRY_LEVEL=error

# Quiet console, detailed telemetry
export RUST_LOG=error
export QMT_TELEMETRY_LEVEL=debug
```

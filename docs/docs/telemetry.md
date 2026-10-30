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
| `agent.execution.driver_create` | Building the middleware driver for the turn |
| `agent.execution.profile` | Resolving the turn's model profile (builds the provider on first use) |
| `agent.execution.context_init` | Building the initial conversation context |
| `agent.provider.build` | Cold provider build for the turn (once per turn; later steps reuse it) |
| `agent.llm.call_with_retry` | Retry wrapper; `operation` is `provider_init` or `chat` |
| `agent.tools.collect` | Collecting and hashing tool definitions (`tool_count`, `tools_bytes`) |
| `agent.hooks.context` | Running context hooks before the request |
| `agent.request.cache_breakpoints` | Applying cache hints to the request messages |
| `agent.accounting.open` | Opening the durable accounting record before dispatch |
| `providers.registry.load` | (Re)parsing the models.dev registry (`reason`: `cold` or `changed`, `bytes`, `parse_ms`) |
| `http_adapter.credential_refresh` | Refreshing the provider credential |
| `http_adapter.prepare_request` | Building the HTTP request (`http.request.body.size`) |
| `http_adapter.send` | Sending the request, up to response headers |
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

Agent spans follow the
[OpenTelemetry GenAI conventions](https://github.com/open-telemetry/semantic-conventions-genai/tree/e07f4ebacb08f56db8c4c882d117720333fbca04).

| Exported span name | Kind | Description |
|---|---|---|
| `invoke_agent` | INTERNAL | Prompt execution |
| `chat {model}` | CLIENT | Inference, including compaction and delegation summarization |
| `execute_tool {tool}` | INTERNAL | Tool calls |

Metadata includes known providers, models, request settings, response IDs,
finish reasons, token usage, errors, and tool status. Input totals include cache
read/write tokens; output totals include reasoning tokens, with breakdowns when available.

When available, `gen_ai.conversation.id` and its `session.id` alias identify the
current session. Delegates use their own session ID.
`gen_ai.agent.id` identifies the configured agent, not the session, and is
not globally unique.

Built-in skill and shell tools may add `gen_ai.skill.name`,
`process.executable.name`, or `process.exit.code`, and refine the tool span name.

Normal chat adds `gen_ai.conversation.compacted=true` when the request retains
a verified compaction summary.

Unavailable fields are omitted. For streaming, these are measured in seconds from
the first generation request (request setup and retry waits count; provider
construction does not):

| Attribute | Meaning |
|---|---|
| `llm.time_to_first_byte` | The response headers arrived |
| `gen_ai.response.time_to_first_chunk` | The first stream chunk of any kind, including metadata |
| `llm.time_to_first_token` | The first chunk carrying model output (text, reasoning or tool-call data) |
| `qmt.stream.chunk_count`, `qmt.stream.wait_ms`, `qmt.stream.process_ms`, `qmt.stream.max_gap_ms` | Aggregates: time waiting on the network vs. handling chunks locally, and the longest gap between chunks |

`llm.time_to_first_token` is the client-side time. It is intentionally not named
`gen_ai.server.time_to_first_token`, which the GenAI conventions reserve for
server-side measurements. Otelite reads `llm.time_to_first_token` for its TTFT column.

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

### Metrics

The agent exports counters and histograms through the same OTLP endpoint as traces and
logs (periodic export; `QMT_NO_TELEMETRY` disables them as well).

**Latency histograms** (seconds; attributes `gen_ai.operation.name`,
`gen_ai.provider.name`, `gen_ai.request.model`):

| Metric | Extra attributes | Meaning |
|---|---|---|
| `gen_ai.client.operation.duration` | `error.type` on failure (`cancelled` or `_OTHER`) | One logical model request, retries included |
| `qmt.gen_ai.client.time_to_first_byte` | | Request dispatch to response headers (streaming only) |
| `qmt.gen_ai.client.time_to_first_token` | | Request dispatch to the first model-output chunk (streaming only) |
| `qmt.agent.step.overhead` | `phase=pre_request` | Agent time between starting a step and dispatching its request |

| Metric | Attributes | Meaning |
|---|---|---|
| `querymt.tool.args_repaired` | `tool`, `rule` | One increment per argument repair actually applied before validation. `rule` is `alias`, `wrap`, `json_string`, `int_string` or `scalar_to_array`. |
| `querymt.tool.calls` | `tool`, `repaired`, `outcome` | One increment per tool call. `repaired` is `true` when at least one repair was applied. `outcome` is `success`, `error` (the tool ran or its pipeline failed) or `rejected` (never ran: conflicting spellings, hook block, validation failure or middleware rejection). |

Repairs are counted once per call, when its arguments are prepared: both the
normalisation of the model's arguments and the normalisation of a pre-tool hook
replacement count, but scheduling, middleware review, execution and re-normalising an
already canonical value do not. A repaired call is also marked on its
`execute_tool` span with `querymt.tool.args_repairs`, the list of applied rule
names (never argument values).

Rule counters alone cannot tell whether a repair helped, because a call can be
repaired and still fail. Use `querymt.tool.calls` for that:

```text
# repaired calls that ran fine vs. repaired calls that still failed, per tool
sum by (tool, outcome) (querymt_tool_calls_total{repaired="true"})

# share of repaired calls that did not succeed
sum(querymt_tool_calls_total{repaired="true", outcome!="success"})
  / sum(querymt_tool_calls_total{repaired="true"})

# which repairs fire most
sum by (tool, rule) (increase(querymt_tool_args_repaired_total[7d]))
```

(Metric names are shown in Prometheus form; other backends may render dots
differently.)

**Retention.** QueryMT does not retain metrics itself. Counters are cumulative and
kept for as long as your collector/backend keeps them; choose a window of at least
seven days to compare release periods. Failure and volume figures that must be
independent of telemetry come from the session execution records (stored tool
results, with `is_error` and the effective arguments), which live as long as the
session history. If the metrics backend is unavailable, report repair outcomes as
unavailable instead of inferring them from failed calls.

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

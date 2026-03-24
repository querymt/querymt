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
| `agent.tool.execute` | Executing a tool call batch |
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

#### What metadata is attached to spans

Spans may carry lightweight metadata such as:

- **Session ID** — identifies which session the work belongs to.
- **Timing fields** — durations of sub-operations (e.g. `view_fetch_ms`, `remote_merge_ms`, `total_ms`).
- **Counts** — e.g. `message_count`, `files_checked`, `duplicates_found`.
- **Tool source** — whether a tool is `builtin`, `mcp`, or `provider`.
- **Boolean flags** — e.g. `is_error`, `granted`, `cache_hit`.

!!! important
    Spans do **not** contain user prompts, LLM responses, API keys, file
    contents, or any other sensitive data.

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
http://otel.query.mt:4317
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
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `http://otel.query.mt:4317` | OTLP collector endpoint (gRPC). |
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

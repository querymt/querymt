# QueryMT Agent - Configuration Guide

This guide covers the TOML configuration format for QueryMT Agent. Configuration can be loaded from files or specified inline.

## Configuration Overview

Agent configuration is defined in TOML format with the following structure:

```toml
# Single agent configuration
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
# ... agent settings

# Optional MCP servers
[[mcp]]
name = "github"
transport = "stdio"
command = "npx"
args = ["@modelcontextprotocol/server-github"]

# Optional middleware
[[middleware]]
type = "limits"
max_steps = 200

# Optional mesh configuration
[mesh]
enabled = true
```

## Loading Configuration

### From File

```rust
use querymt_agent::prelude::*;

let agent = from_config("path/to/config.toml").await?;
```

### From String

```rust
let config = r#"
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
"#;

let agent = from_config(ConfigSource::Toml(config.to_string())).await?;
```

### Embedded Configuration

The `qmtcode` example uses embedded configuration:

```rust
const EMBEDDED_CONFIG: &str = include_str!("confs/single_coder.toml");
let agent = from_config(ConfigSource::Toml(EMBEDDED_CONFIG.to_string())).await?;
```

## Single Agent Configuration

### Agent Settings

```toml
[agent]
# LLM Provider and Model
provider = "anthropic"              # Required: provider name
model = "claude-sonnet-4-5-20250929"  # Required: model identifier
api_key = "${ANTHROPIC_API_KEY}"    # Optional: API key (env var interpolation)

# Working directory
cwd = "."                           # Optional: workspace directory

# Tools to enable
tools = [
    "read_tool", "edit", "write_file",
    "shell", "glob", "search_text",
    "create_task", "todowrite", "todoread",
    "question", "web_fetch"
]

# System prompt (can be string or array of parts)
system = "You are a helpful coding assistant."

# Or use file references:
system = [
    { file = "prompts/default_system.txt" },
    { file = "prompts/code_meta.jinja2" }
]

# Or mixed inline and file:
system = [
    "You are a helpful coding assistant.",
    { file = "prompts/code_meta.jinja2" }
]

# Parameters passed to the LLM provider
[agent.parameters]
temperature = 0.7
max_tokens = 4096

# Assume unknown tools are mutating (requires permission)
assume_mutating = false

# Explicit list of mutating tools
mutating_tools = ["edit", "write_file", "shell"]
```

For reusable system-prefix annotations, TTL behavior, and provider limitations,
see [Prompt Cache Hints](../core/providers.md#prompt-cache-hints).

### Execution Policy

```toml
[agent.execution]

# Tool output truncation (Layer 1)
[agent.execution.tool_output]
max_lines = 2000          # Maximum lines before truncation
max_bytes = 51200         # Maximum bytes before truncation (50 KB)
overflow_storage = "temp_dir"  # discard | temp_dir | data_dir

# Pruning (Layer 2) - runs after every turn
[agent.execution.pruning]
enabled = true            # Enable/disable pruning
protect_tokens = 40000    # Tokens of recent output to protect
minimum_tokens = 20000    # Minimum tokens to clear before pruning
protected_tools = ["skill"]  # Tools never to prune

# AI Compaction (Layer 3) - runs on context overflow
[agent.execution.compaction]
auto = true               # Enable automatic compaction
provider = "anthropic"    # Optional: different provider for compaction
model = "claude-haiku"    # Optional: cheaper model for compaction

[agent.execution.compaction.retry]
max_retries = 3
initial_backoff_ms = 1000
backoff_multiplier = 2.0

# Compaction is a paid request. Every attempt (including a retry after a
# rejected or empty summary) is recorded in the session's cost accounting
# under the "compaction" purpose, charged to the cycle that triggered it.
# See "Cost accounting" in api_reference.md.

# Snapshot backend (undo/redo support)
[agent.execution.snapshot]
backend = "git"           # "git" or "none"
max_snapshots = 100       # Maximum snapshots to keep
max_age_days = 30         # Maximum age of snapshots

# Rate limit retry configuration
[agent.execution.rate_limit]
max_retries = 3
default_wait_secs = 60
backoff_multiplier = 2.0
```

### Skills Configuration

```toml
[agent.skills]
enabled = true
include_external = true   # Check Claude Code, agents conventions paths
paths = ["./skills"]      # Custom skill search paths
urls = []                 # Remote skill URLs (future)
agent_id = "querymt"      # Agent identifier for skills
```

### Hooks Configuration

Hooks run configured local commands at specific agent lifecycle points. They are configured directly in `[agent.hooks]`, which makes them a natural fit for profiles.

```toml
[agent.hooks]
enabled = true

[[agent.hooks.pre_tool_use]]
matcher = "^shell$"

[[agent.hooks.pre_tool_use.hooks]]
type = "command"
command = "sh ./hooks/check-shell.sh"
timeout_sec = 5
status_message = "Checking shell command"

[[agent.hooks.pre_compaction]]

[[agent.hooks.pre_compaction.hooks]]
type = "command"
command = "sh ./hooks/check-compaction.sh"
timeout_sec = 5

[[agent.hooks.pre_delegation]]
matcher = "^coder$"

[[agent.hooks.pre_delegation.hooks]]
type = "command"
command = "sh ./hooks/check-delegation.sh"
timeout_sec = 5

[[agent.hooks.stop]]

[[agent.hooks.stop.hooks]]
type = "command"
command = "sh ./hooks/stop-verify.sh"
timeout_sec = 5
```

Hook commands receive JSON on stdin and return JSON on stdout. Supported events now include compaction hooks and delegation hooks in addition to tool, prompt, session, permission, and stop hooks. See the [Hooks Guide](hooks.md) for the full event list, schema files, and `stop` continuation behavior.

### Workspace (Git worktree) Configuration

Managed Git worktree isolation is disabled by default. Enabling it only makes
isolation *available*; sessions still run locally unless they explicitly ask
for a worktree.

```toml
[agent.workspace]          # [quorum.workspace] for a quorum
enabled = true             # default false
storage_root = "~/.qmt/worktrees"   # default; relative paths resolve against this file
baseline = "commit"        # "commit" (default) | "current_working_tree"
```

`baseline` is a default for explicitly requested top-level isolation that does
not choose its own baseline. Invalid settings (unusable `storage_root`, a
backend without a workspace repository) fail when the agent is built. See
[Git Worktree Workspaces](worktrees.md) for requesting isolation, the error
codes, and the Rust/Python APIs.

## MCP Servers

MCP (Model Context Protocol) servers extend agent capabilities:

```toml
# Stdio transport
[[mcp]]
name = "filesystem"
transport = "stdio"
command = "npx"
args = ["@modelcontextprotocol/server-filesystem", "/path/to/workspace"]
env = { SOME_VAR = "value" }

# HTTP transport
[[mcp]]
name = "context7"
transport = "http"
url = "https://mcp.context7.com/mcp"
headers = { AUTHORIZATION = "Bearer ${CONTEXT7_API_KEY}" }
```

### Tool Selection with MCP

```toml
# Enable all tools from a server
tools = ["filesystem.*"]

# Enable specific tools from a server
tools = ["filesystem.read_file", "filesystem.write_file"]

# Mix with built-in tools
tools = ["read_tool", "filesystem.*", "shell"]
```

## Middleware Configuration

Middleware extends agent behavior through a pluggable stack:

```toml
# Agent mode middleware (build/plan/review modes)
[[middleware]]
type = "agent_mode"
default = "build"
reminder = """<system-reminder>
You are in plan mode. Read-only access.
</system-reminder>"""
review_reminder = """<system-reminder>
You are in review mode. Provide feedback only.
</system-reminder>"""

# Limits middleware
[[middleware]]
type = "limits"
max_steps = 200
max_turns = 50

# Context middleware (token management)
[[middleware]]
type = "context"
warn_at_percent = 80
compact_at_percent = 90
fallback_max_tokens = 128000

# Deduplication check
[[middleware]]
type = "dedup_check"
threshold = 0.85
min_lines = 10

# Parallel edit guard (on in "warn" mode by default; "deny" rejects redundant same-file edits)
[[middleware]]
type = "parallel_edit_guard"
mode = "deny"

# Custom middleware (if registered)
[[middleware]]
type = "custom_middleware"
option1 = "value1"
option2 = 42
```

## Mesh Configuration

Enable cross-machine collaboration:

```toml
[mesh]
enabled = true
listen = "/ip4/0.0.0.0/tcp/9000"  # Multiaddr to listen on
discovery = "mdns"                 # "mdns" | "kademlia" | "none"
auto_fallback = false              # Allow mesh provider discovery

# Explicit peers to connect to
[[mesh.peers]]
name = "dev-gpu"
addr = "/ip4/192.168.1.100/tcp/9000"

# Request timeout for non-streaming calls
request_timeout_secs = 300
```

## Remote Agents

Define agents running on remote mesh nodes:

```toml
[[remote_agents]]
id = "gpu-coder"
name = "GPU Coder"
description = "Coder running on GPU server with fast model"
peer = "dev-gpu"           # References [[mesh.peers]] name
capabilities = ["gpu", "fast-model"]
```

## Sessions Database

Agent and profile TOML configs do not select database paths. A process uses one shared SQLite sessions database for the root agent and all profiles.

Database path precedence:

1. `qmtcode --db <path>`
2. `QMT_SESSIONS_DB` when set to a non-empty value
3. Default `<QMT_CONFIG_DIR or QMT_HOME or ~/.qmt>/sessions.db`

Programmatic builders may still call `.db(...)` as an explicit runtime override.

### Run leases

While a local session run is in progress, the process holds an exclusive OS file lock (a *run lease*) on
`<db-dir>/<db-stem>.run-leases/<session-id>.lock`, e.g. `~/.qmt/sessions.run-leases/`. The directory sits next to the
sessions database, so a separate `--db` gets separate leases.

- The operating system releases the lease when the process exits, including a crash or kill. This works the same on
  Linux, macOS and Windows.
- If a process dies mid-run, the session journal keeps a `run_started` without a `run_completed`. The next process that
  loads, cancels or stops that session sees the free lease and records `run_completed` with outcome `interrupted`, so the
  session no longer shows as running.
- A prompt sent to a session whose lease is held by another process fails with `session is running in another process`.
  A lease held only for a moment (while another process reconciles or deletes the session) delays the prompt briefly
  instead. Queued prompts of one process keep the lease between turns.
- Processes that take no lease (QueryMT versions before run leases, or a run that could not take one) are not
  visible to others: loading such a session elsewhere marks its live run `interrupted`. Restart running instances when
  upgrading.
- The lock files are empty and are kept after a run. Deleting a session removes its file when it is free.
- Leases are per machine. They are not reliable on network filesystems (NFS/SMB); keep the sessions database on a local
  disk. If the lease cannot be taken for any other I/O reason, the run proceeds without one.
- Sessions hosted on a remote node are not reconciled locally; the host closes its own runs.

## Multi-Agent (Quorum) Configuration

For planner-delegate workflows:

```toml
[quorum]
cwd = "."
delegation = true
verification = true
snapshot_policy = "diff"

# Optional delegation summary configuration
[quorum.delegation_summary]
provider = "anthropic"
model = "claude-haiku"
enabled = true
max_tokens = 2000
timeout_secs = 30
min_history_tokens = 2000

# Wait policy for delegations
delegation_wait_policy = "any"  # "all" | "any"
delegation_wait_timeout_secs = 120
delegation_cancel_grace_secs = 5
max_parallel_delegations = 5

# Planner configuration
[planner]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
api_key = "${ANTHROPIC_API_KEY}"
tools = ["delegate", "read_tool", "shell"]

system = "You are a planner agent that coordinates tasks."

[planner.execution]
# Same execution policy structure as [agent.execution]

# Optional planner middleware
[[planner.middleware]]
type = "limits"
max_steps = 100

# Delegate configurations
[[delegates]]
id = "coder"
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
description = "Coder agent that implements changes"
capabilities = ["coding", "filesystem"]
tools = ["edit", "write_file", "shell", "read_tool"]

system = "You are a coder agent. Implement the requested changes."

# Workspace isolation for tasks delegated to this agent (default "inherit").
# isolation = "inherit"   # isolated parents isolate delegates; local parents share
# isolation = "isolated"  # always a private worktree (needs [quorum.workspace])
# isolation = "shared"    # share the parent's checkout (disclosed as shared)

[delegates.execution]
# Same execution policy structure

# Optional delegate middleware
[[delegates.middleware]]
type = "limits"
max_steps = 200

# Remote delegate (runs on different mesh node)
[[delegates]]
id = "remote-coder"
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
description = "Coder on remote GPU machine"
peer = "dev-gpu"  # Routes LLM calls to remote node
tools = ["edit", "write_file", "shell"]
```

## Environment Variable Interpolation

Configuration supports environment variable interpolation:

```toml
[agent]
api_key = "${ANTHROPIC_API_KEY}"           # Required, error if missing
fallback_key = "${FALLBACK_KEY:-default}"  # Optional with default
```

Supported syntax:
- `${VAR}` - Required variable
- `${VAR:-default}` - Optional with default value

## Configuration Validation

The configuration system validates:
- Unique MCP server names
- Valid tool names (built-in or from MCP servers)
- Mesh peer references
- System prompt template syntax
- Required fields (provider, model)

## Example Configurations

See the `examples/confs/` directory for complete examples:
- `single_coder.toml` - Single coder agent
- `coder_agent.toml` - Coder agent with full features
- `multi_agent.toml` - Planner-delegate configuration

## Profiles Configuration

Profiles allow you to save and switch between different agent configurations.

### Profile Metadata

Add optional profile metadata to any configuration:

```toml
[profile]
id = "coder"
name = "Full-Featured Coder"
description = "Complete coding agent with all tools enabled"
tags = ["coding", "development", "full-access"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
# ... rest of agent configuration
```

### Profile Storage

Profiles can be stored in:

1. **User profiles** (global): `~/.qmt/profiles/`
2. **Project profiles** (team-shared): `.qmt/profiles/`
3. **Embedded profiles**: Built-in with QueryMT

### Using Profiles

```bash
# List available profiles
cargo run --example qmtcode --list-profiles

# Use specific profile
cargo run --example qmtcode --profile coder

# Use custom profiles directory
cargo run --example qmtcode --profiles-dir ./my-profiles --profile custom
```

### Profile Configuration Examples

**Minimal profile:**
```toml
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "edit", "write_file"]
```

**Full profile with metadata:**
```toml
[profile]
id = "reviewer"
name = "Code Reviewer"
description = "Read-only code review specialist"
tags = ["review", "read-only"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "glob", "search_text", "question"]
system = "You are a code review specialist."

[[middleware]]
type = "agent_mode"
default = "review"
```

For complete profiles documentation, see [Profiles Guide](profiles.md).

## Slash Commands Configuration

Slash commands extend the CLI with custom functionality.

### Command Locations

1. **User commands** (global): `~/.qmt/commands/`
2. **Project commands** (team-shared): `.qmt/commands/`

### Command File Format

Commands are markdown files with optional YAML frontmatter:

```markdown
---
description: "Analyze and optimize code"
argument-hint: "<file-path>"
model: "claude-sonnet-4-5-20250929"
allowed-tools: ["read_tool", "glob", "search_text"]
---

Please analyze and optimize the code in $1.

Focus on performance, memory efficiency, and readability.
```

### Built-in Commands

QueryMT includes several built-in commands:

- `/help [command]` - Show help information
- `/mcp <subcommand>` - Manage MCP servers
- `/clear` - Clear screen
- `/exit` - Exit application

### Using Slash Commands

```bash
# Type / to see available commands
> /optimize src/main.rs

# Tab completion
> /opt<TAB>  # Completes to /optimize

# Arguments
> /review HEAD --focus security
```

For complete slash commands documentation, see [Slash Commands Guide](slash-commands.md).

## Tool Argument Handling

Builtin tool parameters use `snake_case` names everywhere. The edit tools were the
only exception and were renamed in a single release:

| Tool | Canonical parameters |
|---|---|
| `edit` | `file_path`, `old_string`, `new_string`, `replace_all` |
| `multiedit` | `file_path`, `edits[]` of `old_string`, `new_string`, `replace_all` |
| `replace_symbol` | `replacements[]` of `path`, `symbol`, `kind`, `occurrence`, `expected_hash`, `new_text`; `root` |

**Permanent aliases.** The previous camelCase names (`filePath`, `oldString`,
`newString`, `replaceAll`, `newText`, `expectedHash`) and the short forms `old_str` /
`new_str` are still accepted, at the top level and inside `edits` / `replacements`
items, and are renamed to the canonical names before the call is processed. Aliases
are never advertised to the model and exist only for builtin tools: MCP and provider
tools receive their keys unchanged. Sending two spellings of one parameter (for
example `new_string` and `newString`) is rejected, even when the values are equal.

**Closed schemas.** Every builtin object schema sets `additionalProperties: false`, so
an unknown key fails validation instead of being ignored.

**Unambiguous shape repairs.** Before validation the schema is used to repair
mistakes that have one reading: a JSON string where an array or object is expected, an
integer sent as a string (`"466, "`), a single string where an array of strings is
expected, and a single edit or replacement sent without its `edits` / `replacements`
array. Broken JSON, string-typed content (such as edit text that happens to contain
JSON) and union-typed values are never modified.

`search_text.exclude` accepts either one glob string or an array of globs. A
`question` header longer than 12 characters is accepted and shortened to its first 12
Unicode scalar values before it is shown.

### Raw and effective arguments

Every tool call has two argument representations with separate owners:

| Representation | What it is | Consumed by |
|---|---|---|
| **Raw** | The argument text exactly as the model emitted it (aliases, stringified containers, odd whitespace). Never rewritten. | Stored assistant output/history, provider replay on the next request, diagnostics. |
| **Effective** | Normalised, hook-adjusted and validated arguments of the prepared call. | `pre_tool_use` hooks (the normalised input), validation, file-access scheduling, middleware review (`ParallelEditGuard`), permission checks, tool invocation, snapshots, `ToolCallStart` events, progress records, `post_tool_use` hooks, and the `tool_arguments` stored on tool results. |

A repair therefore never rewrites history: the assistant message keeps what the model
sent, while the matching tool result records what actually ran, including a target
changed by a hook.

### Preparation order

Each call is prepared once, in model order, and the prepared call (not the raw
text) is what gets scheduled and executed:

0. Reject invalid outer input (see [Invalid outer input](#invalid-outer-input)). No
   hook, permission check or tool sees such a call.
1. Normalise the raw arguments (aliases, shape repairs). Conflicting spellings reject
   the call before any hook runs.
2. Run `pre_tool_use` hooks once on the normalised input. A hook `updated_input` is
   normalised the same way; a hook block wins over a conflict inside its replacement.
3. Validate the effective arguments against the tool schema.
4. Declare the call's file access from the effective arguments.
5. Middleware reviews the prepared batch (`on_prepared_tool_calls`) and may reject
   calls, for example `ParallelEditGuard` in `deny` mode.
6. Conflict-aware waves are planned from the declared accesses and executed.

Preparation happens one execution segment at a time. A segment is a run of
parallel-safe calls, or a single stateful/clarification barrier call. Calls after a
barrier are prepared only after it finished, so their hooks see its effects (for
example an updated task state). Calls skipped by cancellation or by a clarification
answer are never prepared: their hooks do not run and they receive a synthetic result
in model order. Every call ends with exactly one result.

### Invalid outer input

For every provider, a call whose raw arguments are malformed JSON, are not a JSON
object, or were marked invalid by the provider is **never executed**. It is rejected
before hooks, permission checks and invocation, and receives exactly one error result
whose text is `{"INVALID_JSON": "<raw input>"}`. The raw input is cut to 2,000 Unicode
scalar values. Invalid input never becomes an executable `{}`.

The Anthropic provider marks a streamed tool call invalid when its completed buffer is
not a JSON object, and when `max_tokens` interrupts the block, even if the partial
buffer happens to parse. Each interrupted block completes exactly once, before the
response terminates. A block that finished normally before the limit is not
invalidated, and a block with no input deltas is the valid zero-argument call `{}`. The
marker travels on the canonical function-call item and is carried onto the prepared
call; it is not inferred from the argument text.

This is different from an unrepaired value *inside* a valid object:

```jsonc
// Invalid outer input: rejected as INVALID_JSON, hooks and tool never run.
{"edits": [

// Valid outer object, unrepairable nested string: reaches schema validation, so hooks
// run once and the error lists the violation with a hint.
// /edits: expected array, got string: "[{\"old_string\": ..."; hint: `edits` must be
// a JSON array of objects, not a string containing JSON
{"file_path": "a.rs", "edits": "[{\"old_string\": \"x\", "}
```

The rejected call keeps its raw arguments in stored assistant history, and the tool
result records them as emitted (there are no effective arguments to record).

### Anthropic history replay

Anthropic requires `tool_use.input` to be an object. When an assistant tool call whose
stored arguments are not a JSON object is replayed, the provider sends `input: {}` and
logs a warning containing the call ID (never the argument text). That `{}` is a protocol
placeholder only: stored history keeps the raw arguments, the matching error result
stays in place, and the placeholder is never executed.

## New Tools Reference

QueryMT includes several new code intelligence tools:

### Code Structure Tools

#### `index`

Produce a compact structural skeleton of source files:

```bash
# Example tool call
index(path="src/main.rs")
```

**Returns:**
- Imports, types, classes, traits, impls, functions, tests
- Line ranges for each item
- Supports Rust, Python, TypeScript, JavaScript, Go, Java, C, C++

#### `get_symbol`

Read structured AST symbols from source files:

```bash
# Example tool call
get_symbol(requests=[{path: "src/lib.rs", symbol: "MyStruct", kind: "struct"}])
```

**Parameters:**
- `path`: File path
- `symbol`: Symbol name or qualified name
- `kind`: Symbol kind filter (function, method, class, struct, enum, trait, impl, type, const, module, test, any)
- `occurrence`: 0-based occurrence when multiple symbols match
- `context_lines`: Number of surrounding lines to include

#### `get_function`

Read one or more functions from source files by name:

```bash
# Example tool call
get_function(paths=["src/lib.rs"], names=["my_function", "helper"])
```

**Returns:**
- Line-numbered function bodies
- Digest metadata
- Context lines (configurable)

#### `replace_symbol`

Replace entire symbol bodies using AST byte ranges:

```bash
# Example tool call
replace_symbol(replacements=[{
    path: "src/lib.rs",
    symbol: "my_function",
    new_text: "fn my_function() -> i32 { 42 }"
}])
```

**Features:**
- AST-aware replacement
- Resolves all replacements before writing
- Rejects overlapping replacements
- Rejects stale writes (hash mismatch)

#### `find_symbol_references`

Find references to symbols across the codebase:

```bash
# Example tool call
find_symbol_references(paths=["src/lib.rs"], symbols=["MyStruct"])
```

**Returns:**
- All references to the symbol
- File paths and line numbers
- Reference context

### Tool Configuration

Tools are configured in the `[agent]` section:

```toml
[agent]
tools = [
    # File operations
    "edit", "read_tool", "write_file", "multiedit", "replace_symbol",
    
    # Search & navigation
    "glob", "search_text", "ls",
    
    # Execution
    "shell",
    
    # Task management
    "create_task", "todowrite", "todoread",
    
    # User interaction
    "question",
    
    # Web browsing
    "browse",
    
    # Code intelligence
    "index", "get_symbol", "get_function", "replace_symbol", "find_symbol_references"
]
```

### Tool Permissions

Configure which tools are mutating:

```toml
[agent]
assume_mutating = false
mutating_tools = ["edit", "multiedit", "write_file", "shell", "replace_symbol"]
```

A permission-required tool call that no cached choice, tool policy or hook decides waits for an explicit user answer, delivered as an ACP `session/request_permission` request. It is never auto-granted, including when no client is attached. Unattended runs (schedules, headless) must restrict `tools` / `mutating_tools` or add `permission_request` hooks, or they pause at the first unanswered permission. See [Session Interactions](mesh.md#session-interactions) for the full lifecycle and the all-nodes-upgrade requirement (`RemoteNodeUpgradeRequired`).

## Scheduled Tasks Configuration

QueryMT supports scheduled tasks for autonomous recurring work. Schedules are created at runtime with the `querymt/schedules/create` ACP extension, not through static TOML configuration. The embedded dashboard discovers schedule capabilities through ACP.

### Schedule Types

#### Interval Schedules

Run tasks at fixed intervals. Trigger JSON format:

```json
{ "type": "interval", "seconds": 3600 }
```

#### Event-Driven Schedules

Run tasks when accumulated events meet a threshold. Trigger JSON format:

```json
{
  "type": "event_driven",
  "event_filter": {
    "event_kinds": ["knowledge_ingested"],
    "threshold": 10,
    "session_public_id": null
  },
  "debounce_seconds": 120
}
```

#### One-Time Schedules

Fire exactly once at a specified time. Trigger JSON format:

```json
{ "type": "once_at", "at": "2026-06-01T09:00:00Z" }
```

### Creating Schedules

Create schedules with the `querymt/schedules/create` ACP extension:

```json
{
  "session_id": "<session-public-id>",
  "prompt": "Check for updates and summarize changes",
  "trigger": { "type": "interval", "seconds": 3600 },
  "max_steps": 50,
  "max_cost_usd": 0.10,
  "max_runs": null
}
```

### Example Configurations for Scheduled Agents

See `examples/confs/watchdog.toml`, `examples/confs/standup_bot.toml`, and `examples/confs/research_journal.toml` for agent configurations designed for scheduled use. These agents are configured with appropriate execution limits for autonomous scheduled cycles.

## VS Code / Language Intelligence

QueryMT can provide language intelligence (LSP-like features) when connected to a VS Code client. This is not configured via TOML; instead, the VS Code extension connects to QueryMT via ACP and provides workspace query support for tools like `language_query`.

The `language_query` tool is available in the built-in tools list and can query the connected editor for:
- Hover information
- Go-to-definition
- Find references
- Completions

This requires a VS Code extension that implements the workspace query bridge.

## Migration Notes

### From Old Config Format

If you have old configurations, note these changes:
- `system` now supports file references and arrays
- `execution` section is nested under agent/planner/delegate
- Middleware uses `type` field instead of direct fields
- Mesh configuration moved to top-level `[mesh]` section
- Profiles added with `[profile]` section
- Slash commands stored in `.qmt/commands/`
- New tools: `index`, `get_symbol`, `get_function`, `replace_symbol`, `find_symbol_references`
- Scheduled tasks created via ACP extensions, not TOML config

### New Features Since Last Documentation Update

1. **Profiles**: Save and switch between configurations
2. **Slash Commands**: Custom commands via markdown files
3. **Internet Mesh**: iroh transport with invite tokens
4. **Remote Sessions**: Forking, resuming, recovery
5. **Code Intelligence Tools**: AST-aware code analysis
6. **Scheduled Tasks**: Autonomous recurring work via ACP extensions
7. **Language Intelligence**: VS Code integration via `language_query` tool
8. **Streaming Stability**: Robust stream handling with reconnection
9. **Multi-Transport**: LAN + Internet mesh simultaneously

## Related Documentation

- [Overview](index.md) - Architecture and concepts
- [Mesh Networking](mesh.md) - Cross-machine collaboration
- [Profiles](profiles.md) - Configuration profiles
- [Slash Commands](slash-commands.md) - Custom commands
- [Delegation](delegation.md) - Multi-agent configuration
- [Middleware](middleware.md) - Processing pipeline
- [Examples](examples.md) - Configuration examples
- [API Reference](api_reference.md) - Rust API documentation

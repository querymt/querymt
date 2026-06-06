# QueryMT Agent - Examples

This document covers example programs, configurations, and usage patterns for QueryMT Agent.

## Table of Contents

- [Example Programs](#example-programs)
- [Configuration Examples](#configuration-examples)
- [Mesh Networking Examples](#mesh-networking-examples)
- [Profile Examples](#profile-examples)
- [Slash Command Examples](#slash-command-examples)
- [Code Intelligence Examples](#code-intelligence-examples)
- [Remote Session Examples](#remote-session-examples)
- [Scheduled Task Examples](#scheduled-task-examples)

---

## Example Programs

The `crates/agent/examples/` directory contains runnable examples:

| Example | Description | Features Required |
|---------|-------------|-------------------|
| `qmtcode` | Full-featured coding agent | `dashboard`, `remote` |
| `acp_agent` | Minimal ACP stdio server | none |
| `auto_delegation_example` | Multi-agent delegation | none |
| `from_config` | Config-file-based agent | none |
| `closure_builder` | Programmatic builder API | none |
| `morning_brief` | Daily summary agent | none |
| `replay_session` | Replay a session from history | none |
| `web_dashboard` | Standalone web dashboard | `dashboard` |
| `event_stream` | Subscribe to agent events | none |
| `export_atif` | Export session to ATIF format | none |
| `export_sft` | Export sessions as SFT training data | none |

---

## qmtcode

The primary example — a full-featured coding assistant with multiple run modes.

```bash
# ACP stdio mode (for subprocess integration)
cargo run --example qmtcode -- --acp

# Web dashboard (default: http://127.0.0.1:3000)
cargo run --example qmtcode --features dashboard -- --dashboard

# Dashboard on custom address
cargo run --example qmtcode --features dashboard -- --dashboard=0.0.0.0:8080

# Mesh-only mode (runs until Ctrl+C)
cargo run --example qmtcode --features remote -- --mesh

# Dashboard + mesh (cross-machine sessions)
cargo run --example qmtcode --features "dashboard remote" -- --dashboard --mesh

# Use a custom config file
cargo run --example qmtcode --features dashboard -- path/to/config.toml --dashboard
```

**Key behaviors:**
- Loads embedded `single_coder.toml` if no config path is given
- ACP mode logs to stderr only (stdout is reserved for JSON-RPC)
- Dashboard mode uses full telemetry
- Mesh mode registers `RemoteNodeManager` and `ProviderHostActor` in the DHT

---

## acp_agent

Minimal example showing how to build and serve an ACP stdio agent.

```bash
cargo run --example acp_agent
```

**What it demonstrates:**
- Building an agent from config
- Serving over ACP stdio
- Minimal setup without dashboard

```rust
use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let runner = from_config("examples/confs/single_agent.toml").await?;
    runner.acp("stdio").await?;
    Ok(())
}
```

---

## auto_delegation_example

Shows how multi-agent delegation works in practice.

```bash
cargo run --example auto_delegation_example
```

**What it demonstrates:**
- Configuring a planner + delegate quorum
- The planner automatically delegating to the coder
- Delegation lifecycle events

```rust
use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Agent::multi()
        .cwd(".")
        .planner(|p| {
            p.provider("anthropic", "claude-sonnet-4-5-20250929")
                .tools(["delegate", "read_tool"])
                .system("You are a planner. Delegate coding tasks to the coder.")
        })
        .delegate("coder", |d| {
            d.provider("anthropic", "claude-sonnet-4-5-20250929")
                .tools(["edit", "write_file", "shell", "read_tool"])
                .capabilities(["coding"])
                .system("You are a coder. Implement the requested changes.")
        })
        .build()
        .await?;

    agent.chat("Add a hello world function to src/lib.rs").await?;
    Ok(())
}
```

---

## from_config

Shows how to load an agent entirely from a TOML config file.

```bash
cargo run --example from_config
cargo run --example from_config -- path/to/config.toml
```

**What it demonstrates:**
- `load_config()` / `from_config()` API
- Config-driven agent setup
- Handling both single and multi-agent configs

```rust
use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "examples/confs/single_agent.toml".to_string());

    let runner = from_config(&path).await?;
    runner.acp("stdio").await?;
    Ok(())
}
```

---

## closure_builder

Shows how to build an agent programmatically using the Rust builder API without a config file.

```bash
cargo run --example closure_builder
```

**What it demonstrates:**
- `Agent::single()` and `Agent::multi()` builder patterns
- Inline system prompts
- Inline middleware configuration
- Callback hooks

```rust
use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Agent::single()
        .provider("anthropic", "claude-sonnet-4-5-20250929")
        .cwd(".")
        .tools(["read_tool", "shell", "edit", "glob", "search_text"])
        .system("You are a helpful coding assistant.")
        .middleware(|stack| {
            stack
                .limits(|l| l.max_steps(100).max_turns(20))
                .context(|c| c.warn_at_percent(80).compact_at_percent(90))
        })
        .on_message(|session_id, content| {
            println!("[{}] {}", session_id, content);
            Ok(())
        })
        .build()
        .await?;

    let response = agent.chat("What files are in the current directory?").await?;
    println!("{}", response);
    Ok(())
}
```

---

## morning_brief

An example agent that generates a daily summary/briefing.

```bash
cargo run --example morning_brief
```

**What it demonstrates:**
- A task-oriented agent (no interactive loop)
- Running a single-shot prompt
- Collecting and printing the result

```rust
use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Agent::single()
        .provider("anthropic", "claude-sonnet-4-5-20250929")
        .tools(["shell", "read_tool", "glob"])
        .system("You are an assistant that creates concise daily briefings.")
        .build()
        .await?;

    let brief = agent
        .chat("Summarize recent git commits and any open TODO items.")
        .await?;

    println!("{}", brief);
    Ok(())
}
```

---

## replay_session

Replays a previously recorded session from the session database.

```bash
cargo run --example replay_session -- <session-id>
```

**What it demonstrates:**
- Loading session history from SQLite
- Replaying events in order
- Useful for debugging and auditing

---

## web_dashboard

Starts a standalone web dashboard without using the `qmtcode` binary.

```bash
cargo run --example web_dashboard --features dashboard
cargo run --example web_dashboard --features dashboard -- --addr=0.0.0.0:8080
```

**What it demonstrates:**
- Starting the dashboard server directly
- Attaching an agent to the dashboard
- Custom bind address

---

## event_stream

Subscribes to and prints all agent events from a running session.

```bash
cargo run --example event_stream
```

**What it demonstrates:**
- `agent.subscribe_events()` broadcast receiver
- Filtering and printing different event kinds
- Non-blocking event consumption

```rust
use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Agent::single()
        .provider("anthropic", "claude-sonnet-4-5-20250929")
        .tools(["read_tool"])
        .build()
        .await?;

    let mut events = agent.subscribe_events();

    // Start a chat in background
    let agent_clone = agent.clone();
    tokio::spawn(async move {
        agent_clone.chat("List files in the current directory").await.ok();
    });

    // Print all events
    while let Ok(envelope) = events.recv().await {
        println!("[{}] {:?}", envelope.session_id(), envelope.kind());
    }

    Ok(())
}
```

---

## export_atif

Exports a session to the ATIF (Agent Tool Interaction Format) format.

```bash
cargo run --example export_atif -- <session-id> output.atif
```

**What it demonstrates:**
- Loading session history
- Exporting to ATIF format
- Session serialization

---

## export_sft

Exports session data as JSONL training data for fine-tuning LLMs via SFT (Supervised Fine-Tuning). Supports OpenAI chat and ShareGPT formats.

```bash
# Show export stats
cargo run --example export_sft -- --stats

# Export all sessions as OpenAI chat format
cargo run --example export_sft -- -o training.jsonl

# Export only Claude Opus sessions as ShareGPT format for unsloth
cargo run --example export_sft -- -f sharegpt --models claude-opus-4-6 --scrub-paths -o training.jsonl

# Export with quality filters
cargo run --example export_sft -- --min-turns 5 --exclude-errored --max-tool-error-rate 0.1 -o training.jsonl
```

**What it demonstrates:**
- Batch export from session database
- Turn materialization from event streams
- Session quality filtering (model, error rate, turn count)
- Path scrubbing for privacy
- Context windowing for long sessions

See the [Export documentation](export.md) for the full guide including fine-tuning workflows and the HTTP API.

---

## Example Config Files

All examples in `examples/confs/`:

### `single_agent.toml` — Minimal single agent

```toml
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "shell"]
system = "You are a helpful assistant."
```

### `single_coder.toml` — Full-featured single coder

```toml
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
assume_mutating = false
mutating_tools = ["edit", "write_file", "shell"]
tools = [
    "edit", "read_tool", "write_file",
    "glob", "search_text", "ls",
    "shell",
    "create_task", "todowrite", "todoread",
    "question", "web_fetch",
]
system = [
    { file = "../prompts/default_system.txt" },
    { file = "../prompts/code_meta.jinja2" },
]

[agent.execution.snapshot]
backend = "git"

[agent.execution.tool_output]
max_lines = 2000
max_bytes = 51200

[agent.execution.pruning]
protect_tokens = 40000

[agent.execution.compaction]
auto = true

[[middleware]]
type = "agent_mode"
default = "build"

[[middleware]]
type = "limits"
max_steps = 200
max_turns = 50

[[middleware]]
type = "context"
warn_at_percent = 80
compact_at_percent = 90

[[middleware]]
type = "dedup_check"
threshold = 0.85
min_lines = 10
```

### `hook_guarded_coder.toml` — Single coder with lifecycle hooks

Use this profile when you want config-level hook commands to inspect shell calls, automate approval decisions, or request one extra step at turn completion.

### `multi_agent.toml` — Planner + delegates

```toml
[quorum]
cwd = "."
delegation = true
verification = true
max_parallel_delegations = 3

[planner]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["delegate", "read_tool", "shell", "glob"]
system = [{ file = "../prompts/planner.md" }]

[[delegates]]
id = "coder"
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
description = "Implements code changes"
capabilities = ["coding", "filesystem"]
tools = ["edit", "write_file", "shell", "read_tool", "glob", "search_text"]
system = [{ file = "../prompts/coder.md" }]
```

---

## Running All Examples

```bash
# Single agent - interactive ACP
cargo run --example qmtcode -- --acp

# Single agent - web dashboard
cargo run --example qmtcode --features dashboard -- --dashboard

# Multi-agent delegation
cargo run --example auto_delegation_example

# From config file
cargo run --example from_config -- examples/confs/single_coder.toml

# Programmatic builder
cargo run --example closure_builder

# Event streaming
cargo run --example event_stream

# Morning brief (single-shot)
cargo run --example morning_brief
```

## Building a Release Binary

```bash
# Build optimized binary
cargo build --release --example qmtcode --features dashboard

# Clear macOS quarantine flag
xattr -dr com.apple.quarantine target/release/examples/qmtcode

# Run the binary
./target/release/examples/qmtcode --dashboard
```

---

## Configuration Examples

### Example: Single Coder Agent (`single_coder.toml`)

```toml
[profile]
id = "default"
name = "Default"
description = "Default single coder profile"
tags = ["coding", "single-agent"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
assume_mutating = false
mutating_tools = ["edit", "multiedit", "write_file", "shell", "replace_symbol"]
tools = [
  "edit", "read_tool", "write_file", "multiedit", "replace_symbol",
  "glob", "search_text", "ls", "index", "get_symbol", "get_function",
  "find_symbol_references", "shell", "create_task", "todowrite", "todoread",
  "question", "browse"
]
system = [
  { file = "../prompts/default_system.txt" },
  { file = "../prompts/code_meta.jinja2" }
]

[agent.execution.snapshot]
backend = "git"

[[middleware]]
type = "agent_mode"
default = "build"

[[middleware]]
type = "limits"
max_steps = 200
max_turns = 50

[[middleware]]
type = "context"
warn_at_percent = 80
compact_at_percent = 90
```

### Example: Multi-Agent Quorum (`multi_agent.toml`)

```toml
[quorum]
cwd = "."
delegation = true
verification = true
max_parallel_delegations = 3

[planner]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["delegate", "read_tool", "shell", "glob"]
system = [{ file = "../prompts/planner.md" }]

[[delegates]]
id = "coder"
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
description = "Implements code changes"
capabilities = ["coding", "filesystem"]
tools = ["edit", "write_file", "shell", "read_tool", "glob", "search_text"]
system = [{ file = "../prompts/coder.md" }]
```

---

## Mesh Networking Examples

### Example: Basic LAN Mesh

```toml
# mesh_lan.toml
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "edit", "write_file", "shell"]

[mesh]
enabled = true
listen = "/ip4/0.0.0.0/tcp/0"
discovery = "mdns"
```

**Usage:**
```bash
# Start mesh node
cargo run --example qmtcode --features remote -- --mesh

# Start on specific port
cargo run --example qmtcode --features remote -- --mesh=/ip4/0.0.0.0/tcp/9001
```

### Example: Internet Mesh with Invite Tokens

```bash
# Machine A: Create invite and host mesh
cargo run --example qmtcode --features remote -- --mesh --mesh-invite="Dev Team"
# Output: Invite token: qmt://mesh/join/eyJpbnZ...

# Machine B: Join via invite token
cargo run --example qmtcode --features remote -- --mesh-join=qmt://mesh/join/eyJpbnZ...
```

### Example: Multi-Transport Mesh (LAN + Internet)

```toml
# mesh_multi.toml
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "edit", "write_file", "shell"]

[mesh]
enabled = true

# LAN transport
[mesh.lan]
enabled = true
listen = "/ip4/0.0.0.0/tcp/0"
discovery = "mdns"

# Internet transport (iroh)
[[mesh.iroh]]
enabled = true
name = "personal"
invite = "${QMT_PERSONAL_INVITE}"

[[mesh.iroh]]
enabled = true
name = "team"
invite = "${QMT_TEAM_INVITE}"
```

### Example: Remote Agents with Delegation

```toml
# mesh_remote.toml
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"

[mesh]
enabled = true

[[mesh.peers]]
name = "gpu-server"
addr = "/ip4/192.168.1.100/tcp/9000"

[[remote_agents]]
id = "gpu-coder"
name = "GPU Coder"
description = "Fast coder on GPU server"
peer = "gpu-server"
capabilities = ["gpu", "fast-model"]

[[delegates]]
id = "remote-coder"
provider = "anthropic"
model = "claude-sonnet-4"
peer = "gpu-server"
tools = ["edit", "write_file", "shell"]
```

**Usage:**
```bash
# Start mesh node with dashboard
cargo run --example qmtcode --features "remote dashboard" -- --mesh --dashboard
```

For complete mesh documentation, see [Mesh Networking Guide](mesh.md).

---

## Profile Examples

### Example: List and Use Profiles

```bash
# List all profiles
cargo run --example qmtcode --list-profiles

# Use specific profile
cargo run --example qmtcode --profile coder

# Use profile with custom directory
cargo run --example qmtcode --profiles-dir ./my-profiles --profile custom

# Use profile with dashboard
cargo run --example qmtcode --features dashboard --profile coder --dashboard
```

### Example: Coder Profile (`~/.qmt/profiles/coder.toml`)

```toml
[profile]
id = "coder"
name = "Full-Featured Coder"
description = "Complete coding agent with all tools"
tags = ["coding", "development", "full-access"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
assume_mutating = false
mutating_tools = ["edit", "multiedit", "write_file", "shell", "replace_symbol"]
tools = [
    "edit", "read_tool", "write_file", "multiedit", "replace_symbol",
    "glob", "search_text", "ls", "index", "get_symbol", "get_function",
    "find_symbol_references", "shell", "create_task", "todowrite", "todoread",
    "question", "browse"
]
system = [
    { file = "prompts/coder_system.txt" },
    { file = "prompts/code_meta.jinja2" }
]

[agent.execution.snapshot]
backend = "git"

[[middleware]]
type = "agent_mode"
default = "build"

[[middleware]]
type = "limits"
max_steps = 200
max_turns = 50
```

### Example: Reviewer Profile (`~/.qmt/profiles/reviewer.toml`)

```toml
[profile]
id = "reviewer"
name = "Code Reviewer"
description = "Read-only code review specialist"
tags = ["review", "read-only"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
assume_mutating = false
tools = [
    "read_tool", "glob", "search_text", "ls", "index",
    "get_symbol", "get_function", "find_symbol_references",
    "question"
]
system = """You are a code review specialist. Analyze code for:
- Correctness and logic errors
- Security vulnerabilities
- Performance issues
- Code style and maintainability

Provide constructive feedback with specific suggestions."""

[[middleware]]
type = "agent_mode"
default = "review"
```

### Example: Team Profiles in Version Control

```bash
# Create project profiles directory
mkdir -p .qmt/profiles

# Create team profile
cat > .qmt/profiles/team-coder.toml << 'EOF'
[profile]
id = "team-coder"
name = "Team Coder"
description = "Standard team coding profile"
tags = ["team", "standard"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "edit", "write_file", "shell", "glob", "search_text"]
system = "You are a helpful coding assistant following team conventions."
EOF

# Commit to version control
git add .qmt/profiles/
git commit -m "Add team coding profile"
```

For complete profiles documentation, see [Profiles Guide](profiles.md).

---

## Slash Command Examples

### Example: Optimize Command (`~/.qmt/commands/optimize.md`)

```markdown
---
description: "Analyze and optimize code for performance"
argument-hint: "<file-path>"
---

Please analyze and optimize the code in $1 for:

- Performance improvements
- Memory efficiency
- Algorithmic complexity
- Code readability

Show before/after comparisons and explain your changes.
```

**Usage:**
```bash
> /optimize src/main.rs
```

### Example: Review Command (`~/.qmt/commands/review.md`)

```markdown
---
description: "Perform code review"
argument-hint: "<file-path> [focus-area]"
allowed-tools: ["read_tool", "glob", "search_text", "question"]
---

Please review the code in $1.

Focus area: $2

Check for:
- Code quality and best practices
- Potential bugs
- Security issues
- Performance concerns
```

**Usage:**
```bash
> /review src/api.rs security
```

### Example: Test Command (`.qmt/commands/test.md`)

```markdown
---
description: "Run project tests"
argument-hint: "[test-filter]"
---

Run the project test suite.

Filter: $1

Execute:
1. `cargo test $1`
2. Report results
3. If failures, analyze and suggest fixes
```

**Usage:**
```bash
> /test
> /test my_module
```

### Example: Commit Command (`~/.qmt/commands/commit.md`)

```markdown
---
description: "Create semantic commit message"
allowed-tools: ["shell", "question"]
---

Create a semantic commit for the current changes.

Steps:
1. Run `git status` and `git diff --cached`
2. Analyze changes and determine commit type (feat, fix, docs, etc.)
3. Generate commit message following Conventional Commits
4. Ask for confirmation
5. Create the commit
```

**Usage:**
```bash
> /commit
```

For complete slash commands documentation, see [Slash Commands Guide](slash-commands.md).

---

## Code Intelligence Examples

### Example: Index Source Files

```bash
# In the agent session, use the index tool
> index(path="src/main.rs")
```

**Output:**
```
Imports: [0-5]
Types: [10-25]
Functions: [30-80]
  - main [30-35]
  - process_input [40-60]
  - handle_error [65-80]
Tests: [85-120]
```

### Example: Get Symbol Definition

```bash
> get_symbol(requests=[{path: "src/lib.rs", symbol: "MyStruct", kind: "struct"}])
```

**Output:**
```
00010| pub struct MyStruct {
00011|     pub name: String,
00012|     pub value: i32,
00013| }
```

### Example: Find All References

```bash
> find_symbol_references(paths=["src/lib.rs"], symbols=["MyStruct"])
```

**Output:**
```
src/lib.rs:10 - Definition
src/main.rs:15 - Usage: let s = MyStruct::new(...)
src/tests.rs:25 - Test: #[test] fn test_my_struct()
```

### Example: Replace Symbol

```bash
> replace_symbol(replacements=[{
    path: "src/lib.rs",
    symbol: "my_function",
    newText: "fn my_function() -> i32 {\n    42\n}"
}])
```

For more on code intelligence tools, see [Configuration Guide](configuration.md#new-tools-reference).

---

## Remote Session Examples

Remote sessions are managed through the mesh network. See the [Mesh Networking Guide](mesh.md) for detailed documentation.

### Example: Remote Session via Dashboard

1. Start a mesh node with dashboard:
   ```bash
   cargo run --example qmtcode --features "remote dashboard" -- --mesh --dashboard
   ```

2. Use the dashboard UI to:
   - List available remote nodes
   - Create sessions on remote nodes
   - Fork and resume remote sessions

### Example: Remote Session via API

```rust
use querymt_agent::prelude::*;

// Remote session operations require a RemoteActorRef<RemoteNodeManager>
// obtained from mesh discovery

// Create session on remote node
let response = agent
    .create_remote_session(&node_manager_ref, Some(cwd))
    .await?;

// Fork remote session at specific message
let forked = agent
    .fork_remote_session(&node_manager_ref, source_session_id, message_id)
    .await?;

// Resume remote session
let resumed = agent
    .resume_remote_session(&node_manager_ref, session_id)
    .await?;

// List available remote nodes
let nodes = agent.list_remote_nodes().await;
```

For complete remote session documentation, see [Mesh Networking Guide](mesh.md#session-management).

---

## Scheduled Task Examples

Schedules are created at runtime through the dashboard UI (Session > Schedules > Create Schedule) or via the WebSocket API. Here are the trigger JSON formats for different schedule types:

### Example: Periodic Health Check (Interval)

```json
{
  "type": "create_schedule",
  "session_id": "<session-public-id>",
  "prompt": "Run system health check: verify services, check disk space, review logs for errors",
  "trigger": { "type": "interval", "seconds": 3600 },
  "max_steps": 50
}
```

### Example: Code Quality Monitor (Event-Driven)

```json
{
  "type": "create_schedule",
  "session_id": "<session-public-id>",
  "prompt": "Review recent file changes for code quality issues",
  "trigger": {
    "type": "event_driven",
    "event_filter": {
      "event_kinds": ["file_modified"],
      "threshold": 10
    },
    "debounce_seconds": 300
  }
}
```

### Example: Daily Standup (Interval)

```json
{
  "type": "create_schedule",
  "session_id": "<session-public-id>",
  "prompt": "Generate daily standup summary: what was done yesterday, what's planned today, any blockers",
  "trigger": { "type": "interval", "seconds": 86400 }
}
```

### Example Scheduled Agent Configurations

For complete agent configurations designed for scheduled use, see:
- `examples/confs/watchdog.toml` - Scheduled codebase health monitor
- `examples/confs/standup_bot.toml` - Daily standup reporter
- `examples/confs/research_journal.toml` - Research journal with event-driven consolidation
- `examples/confs/learning_pair.toml` - Learning pair with scheduled consolidation
- `examples/confs/hook_guarded_coder.toml` - Single coder with config-level lifecycle hooks

---

## Running All Examples

```bash
# Single agent - interactive ACP
cargo run --example qmtcode -- --acp

# Single agent - web dashboard
cargo run --example qmtcode --features dashboard -- --dashboard

# Multi-agent delegation
cargo run --example auto_delegation_example

# From config file
cargo run --example from_config -- examples/confs/single_coder.toml

# Programmatic builder
cargo run --example closure_builder

# Event streaming
cargo run --example event_stream

# Morning brief (single-shot)
cargo run --example morning_brief

# Mesh networking
cargo run --example qmtcode --features remote -- --mesh

# With specific profile
cargo run --example qmtcode --profile coder

# Dashboard with mesh
cargo run --example qmtcode --features "dashboard remote" -- --dashboard --mesh
```

---

## Related Documentation

- [Overview](index.md) — Architecture concepts
- [Configuration Guide](configuration.md) — Full config reference
- [Mesh Networking](mesh.md) — Cross-machine collaboration
- [Profiles](profiles.md) — Configuration profiles
- [Slash Commands](slash-commands.md) — Custom commands
- [Delegation](delegation.md) — Multi-agent coordination
- [Middleware](middleware.md) — Processing pipeline
- [API Reference](api_reference.md) — Rust API docs
- [Agent Modes](agent_modes.md) — Build/Plan/Review modes
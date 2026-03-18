# QueryMT Agent - Examples

This document covers the example programs provided in `crates/agent/examples/`.

## Available Examples

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

## Related Documentation

- [Overview](index.md) — Architecture concepts
- [Configuration Guide](configuration.md) — Full config reference
- [API Reference](api_reference.md) — Public API types
- [Agent Modes](agent_modes.md) — Build/Plan/Review modes
- [Delegation Guide](delegation.md) — Multi-agent workflows
- [Mesh Networking](mesh.md) — Cross-machine collaboration
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

# Database path (SQLite)
db = "./data/agent.db"              # Optional: session history database

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

## Multi-Agent (Quorum) Configuration

For planner-delegate workflows:

```toml
[quorum]
cwd = "."
db = "./data/quorum.db"
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

## Migration Notes

### From Old Config Format

If you have old configurations, note these changes:
- `system` now supports file references and arrays
- `execution` section is nested under agent/planner/delegate
- Middleware uses `type` field instead of direct fields
- Mesh configuration moved to top-level `[mesh]` section

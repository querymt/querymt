# QueryMT Agent - Overview

The `querymt-agent` crate is a high-level agent runtime for QueryMaTe, providing a flexible framework for building AI agents with support for single-agent and multi-agent (quorum) configurations.

## What is QueryMT Agent?

QueryMT Agent is a Rust library that enables you to:

- **Build AI agents** with configurable tools, models, and behaviors
- **Run agents in multiple modes**: ACP stdio, web dashboard, or mesh networking
- **Support multi-agent workflows** with planner-delegate delegation patterns
- **Manage context efficiently** with automatic compaction and pruning
- **Enable cross-machine collaboration** via libp2p mesh networking

## Architecture Overview

```
─────────────────────────────────────────────────────────────────
│                        Agent Runtime                            │
─────────────────────────────────────────────────────────────────
│  ──────────────  ──────────────  ──────────────          │
│  │   Session    │  │   Session    │  │   Session    │          │
│  │    Actor     │  │    Actor     │  │    Actor     │          │
│  ─────────────  ─────────────  ─────────────          │
│         │                 │                 │                   │
│         ──────────────────────────────────                   │
│                           │                                     │
│                  ────────▼────────                            │
│                  │  AgentConfig    │                            │
│                  │  (Shared State) │                            │
│                  ────────────────                            │
│                           │                                     │
│         ──────────────────────────────────                   │
│         │                 │                 │                   │
│  ──────▼──────  ──────▼──────  ──────▼──────            │
│  │  Middleware │  │   Tools     │  │  Provider   │            │
│  │    Stack    │  │  Registry   │  │   System    │            │
│  ─────────────  ─────────────  ─────────────            │
─────────────────────────────────────────────────────────────────
```

### Core Components

#### 1. AgentConfig
The central configuration structure containing:
- **Provider**: LLM provider configuration (Anthropic, OpenAI, etc.)
- **Tool Registry**: Available tools (built-in, MCP, provider-specific)
- **Middleware Stack**: Processing pipeline for agent decisions
- **Event Sink**: Event publishing and subscription
- **Session Provider**: Session management and history storage

#### 2. SessionActor
Per-session runtime state managed as a kameo actor:
- **Execution State**: Current turn, step count, tool usage
- **Conversation Context**: Message history with compaction
- **Runtime State**: MCP tools, workspace index, permissions
- **Execution Permit**: Ensures FIFO ordering of prompts

#### 3. Middleware Stack
A pluggable processing pipeline that intercepts and modifies agent behavior:
- **LimitsMiddleware**: Step and turn limits
- **ContextMiddleware**: Token management and compaction
- **AgentModeMiddleware**: Mode-aware restrictions (build/plan/review)
- **DedupCheckMiddleware**: Duplicate code detection
- **DelegationMiddleware**: Multi-agent coordination

#### 4. Tool Registry
Unified tool system supporting:
- **Built-in tools**: `edit`, `read_tool`, `shell`, `glob`, etc.
- **Provider tools**: Tools exposed by the LLM provider
- **MCP tools**: Model Context Protocol servers

## Agent Modes

QueryMT Agent supports three runtime modes, switchable at runtime:

| Mode | Description | Use Case |
|------|-------------|----------|
| **Build** | Full read/write access | Implementing code changes |
| **Plan** | Read-only, planning focus | Analyzing and planning before implementation |
| **Review** | Read-only, code review | Reviewing code quality and providing feedback |

Switch modes with `Ctrl+M` (or `Cmd+M` on macOS) in dashboard mode.

## Execution Flow

```
User Request
    │
    ▼
─────────────────
│  SessionActor   │  ← Create session via ACP or dashboard
────────────────
         │
         ▼
─────────────────
│  Middleware     │  ← Limits check, mode validation
│     Stack       │
────────────────
         │
         ▼
─────────────────
│  Tool Selection │  ← Agent chooses tools to use
────────────────
         │
         ▼
─────────────────
│  Tool Execution │  ← Execute tools (shell, file ops, etc.)
────────────────
         │
         ▼
─────────────────
│  Context        │  ← Prune/compact if needed
│   Management    │
────────────────
         │
         ▼
─────────────────
│  LLM Response   │  ← Send to LLM, get next action
─────────────────
```

## Multi-Agent (Quorum) Architecture

For complex tasks, QueryMT supports a planner-delegate pattern:

```
──────────────────────────────────────────────────────────────
│                        Planner Agent                          │
│  (Analyzes task, decides which delegate to use)               │
─────────────────────────────────────────────────────────────
                           │
                           │ Delegation Request
                           ▼
        ────────────────────────────────────
        │                                     │
        ▼                                     ▼
─────────────────                   ─────────────────
│   Delegate 1    │                   │   Delegate 2    │
│  (Code Reviewer)│                   │   (Tester)      │
─────────────────                   ─────────────────
```

## Key Features

### Context Management

QueryMT implements a 3-layer context management system:

1. **Tool Output Truncation**: Limits tool output to configurable size
2. **Pruning**: Removes old tool results after every turn
3. **AI Compaction**: Summarizes history when context approaches limits

### Delegation System

- **Automatic delegation**: Agents can delegate tasks to specialized agents
- **Verification**: Optional verification of delegate results
- **Planning context**: Summarized context passed to delegates
- **Parallel execution**: Multiple delegations can run concurrently

### Mesh Networking

- **Cross-machine sessions**: Share sessions across multiple machines
- **Peer discovery**: mDNS or Kademlia DHT for finding peers
- **Remote agents**: Access agents running on other machines
- **Provider routing**: Route LLM calls to specific nodes

## Getting Started

### Quick Start

```bash
# Run the coder agent example with dashboard
cd crates/agent
cargo run --example coder_agent --features dashboard -- --dashboard

# Run as ACP stdio server
cargo run --example coder_agent -- --acp
```

### Programmatic Usage

```rust
use querymt_agent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a single agent
    let agent = Agent::single()
        .provider("anthropic", "claude-sonnet-4-5-20250929")
        .cwd(".")
        .tools(["read_tool", "shell", "edit"])
        .build()
        .await?;

    // Chat with the agent
    let response = agent.chat("Hello!").await?;
    println!("{}", response);

    Ok(())
}
```

## Documentation Structure

- **Overview** (this document): Architecture and concepts
- **Configuration Guide**: TOML configuration reference
- **API Reference**: Rust API documentation
- **Middleware Guide**: Custom middleware development
- **Examples**: Usage examples and patterns

## Related Documentation

- [QueryMaTe Main Documentation](https://docs.query.mt)
- [Agent Examples](examples.md)
- [Configuration Examples](configuration.md)
# QueryMT Agent - Delegation System

The delegation system enables multi-agent workflows where a planner agent can delegate tasks to specialized delegate agents. This allows for division of labor and specialized expertise.

## Overview

Delegation allows agents to:
- **Delegate tasks** to specialized agents
- **Coordinate work** across multiple agents
- **Verify results** from delegates
- **Run in parallel** multiple delegations
- **Route to remote agents** via mesh networking

## Architecture

```
─────────────────────────────────────────────────────────────
│                      Planner Agent                          │
│  (Analyzes task, decides which delegate to use)             │
────────────────────────────────────────────────────────────
                            │
                            │ Delegation Request
                            ▼
        ──────────────────────────────────────────
        │          Delegation Orchestrator          │
        │  (Manages delegation lifecycle)          │
        ────────────────────────────────────────
                      │
        ──────────────────────────
        │                           │
        ▼                           ▼
───────────────          ───────────────
│  Delegate 1   │          │  Delegate 2   │
│  (Coder)      │          │  (Tester)     │
───────────────          ───────────────
```

## Configuration

### Enabling Delegation

Delegation is enabled in the quorum configuration:

```toml
[quorum]
delegation = true
verification = false  # Optional: enable verification
```

### Defining Delegates

```toml
[[delegates]]
id = "coder"
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
description = "Coder agent that implements code changes"
capabilities = ["coding", "filesystem", "shell"]
tools = ["edit", "write_file", "shell", "read_tool", "glob"]

system = """You are a coder agent. Implement the requested changes
efficiently and correctly. Focus on writing clean, maintainable code."""

[delegates.execution]
max_steps = 200
```

### Planner Configuration

```toml
[planner]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["delegate", "read_tool", "shell", "glob"]

system = """You are a planner agent. Your role is to:
1. Analyze user requests
2. Decide if delegation is needed
3. Choose the appropriate delegate
4. Provide clear instructions to delegates
5. Review and integrate delegate results"""
```

## Delegation Lifecycle

### 1. Delegation Request

The planner decides to delegate and creates a delegation request:

```rust
pub struct Delegation {
    pub public_id: String,              // Unique delegation ID
    pub target_agent_id: String,        // Which delegate to use
    pub objective: String,              // What needs to be done
    pub context: Option<String>,        // Additional context
    pub constraints: Option<String>,    // Constraints to follow
    pub expected_output: Option<String>,// Expected result format
    pub task_id: Option<String>,        // Associated task ID
    pub planning_summary: Option<String>,// Summary of planning conversation
    pub verification_spec: Option<VerificationSpec>, // Optional verification
}
```

### 2. Session Creation

The orchestrator creates a new session for the delegate:

```rust
// Planner creates delegation session
let (session_id, session_ref) = target_agent
    .create_delegation_session(cwd)
    .await?;
```

### 3. Context Injection

The planner's context is injected into the delegate session:

```rust
// Planning summary is injected via kameo message
session_ref
    .set_planning_context(planning_summary)
    .await?;
```

### 4. Task Execution

The delegate receives the task and begins execution:

```
Delegate receives:
  - Objective: "Add user authentication"
  - Context: [Planning summary]
  - Constraints: "Use JWT, follow security best practices"
  - Expected Output: "Working authentication with tests"

Delegate:
  - Analyzes requirements
  - Reads existing code
  - Implements changes
  - Writes tests
```

### 5. Result Collection

The delegate's work is collected:

```rust
// Get delegate's history
let history = session_ref.get_history().await?;

// Extract summary
let summary = extract_session_summary_from_history(&history);
```

### 6. Verification (Optional)

If verification is enabled, the result is verified:

```rust
if let Some(verification_spec) = &delegation.verification_spec {
    let passed = verification_service
        .verify(verification_spec, context)
        .await?;
    
    if !passed {
        // Delegation failed verification
        // Error is reported to planner
    }
}
```

### 7. Result Injection

The result is injected back into the planner session:

```rust
let message = format_delegation_completion_message(
    &delegation.public_id,
    &summary
);

planner_session.prompt(message).await?;
```

## Delegation Status

| Status | Description |
|--------|-------------|
| `Pending` | Delegation requested, waiting to start |
| `Running` | Delegate is working on the task |
| `Complete` | Delegate finished successfully |
| `Failed` | Delegate encountered an error |
| `Cancelled` | Delegation was cancelled |

## Verification

### Verification Types

```rust
pub enum VerificationType {
    // Run a shell command and check exit code
    ShellCommand { command: String },
    
    // Check if a file exists
    FileExists { path: String },
    
    // Check if file contains specific content
    FileContains { path: String, content: String },
    
    // Custom verification logic
    Custom { spec: serde_json::Value },
}
```

### Example Verification

```toml
# In delegate configuration
[[delegates]]
id = "coder"
# ... other config

# Verification spec in delegation request
verification_spec = {
    verification_type = "shell_command",
    command = "cargo check && cargo test"
}
```

## Delegation Parameters

### Wait Policy

Controls how the planner waits for delegate results:

```toml
[quorum]
delegation_wait_policy = "any"  # "all" | "any"
delegation_wait_timeout_secs = 120
```

- **any**: Continue when first delegate completes
- **all**: Wait for all delegates to complete

### Parallel Delegations

```toml
[quorum]
max_parallel_delegations = 5
```

Maximum concurrent delegations.

### Grace Period

```toml
[quorum]
delegation_cancel_grace_secs = 5
```

Time to wait for graceful cancellation before force abort.

## Remote Delegation

Delegates can run on remote mesh nodes:

```toml
# Mesh configuration
[mesh]
enabled = true
listen = "/ip4/0.0.0.0/tcp/9000"

[[mesh.peers]]
name = "gpu-server"
addr = "/ip4/192.168.1.100/tcp/9000"

# Remote delegate
[[delegates]]
id = "remote-coder"
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
description = "Coder on GPU server"
peer = "gpu-server"  # Routes LLM calls to remote node
tools = ["edit", "write_file", "shell"]
```

When `peer` is specified:
- LLM calls are routed to the remote node
- Tool execution happens locally
- Enables "remote model, local session" pattern

## Delegation Events

Agents emit events during delegation:

```rust
pub enum AgentEventKind {
    // Delegation lifecycle
    DelegationRequested { delegation: Delegation },
    SessionForked {
        parent_session_id: String,
        child_session_id: String,
        target_agent_id: String,
        origin: ForkOrigin,
        fork_point_type: ForkPointType,
        fork_point_ref: String,
        instructions: String,
    },
    DelegationCompleted {
        delegation_id: String,
        result: Option<String>,
    },
    DelegationFailed {
        delegation_id: String,
        error: String,
    },
    DelegationCancelled {
        delegation_id: String,
    },
}
```

## Programmatic Delegation

### Creating a Delegation

```rust
use querymt_agent::prelude::*;
use agent_client_protocol::Delegation;

let delegation = Delegation {
    public_id: uuid::Uuid::new_v4().to_string(),
    target_agent_id: "coder".to_string(),
    objective: "Implement user authentication".to_string(),
    context: Some("Project uses JWT for authentication".to_string()),
    constraints: Some("Follow security best practices".to_string()),
    expected_output: Some("Working auth with tests".to_string()),
    task_id: None,
    planning_summary: None,
    verification_spec: None,
};

// Request delegation
agent.delegate(delegation).await?;
```

### Subscribing to Delegation Events

```rust
let mut events = agent.subscribe_events();

while let Ok(event) = events.recv().await {
    match event.kind() {
        AgentEventKind::DelegationRequested { delegation } => {
            println!("Delegation requested: {}", delegation.objective);
        }
        AgentEventKind::DelegationCompleted { delegation_id, result } => {
            println!("Delegation {} completed: {:?}", delegation_id, result);
        }
        AgentEventKind::DelegationFailed { delegation_id, error } => {
            println!("Delegation {} failed: {}", delegation_id, error);
        }
        _ => {}
    }
}
```

## Error Handling

### Common Errors

| Error | Cause | Resolution |
|-------|-------|------------|
| `AgentNotFound` | Delegate not registered | Register delegate with correct ID |
| `SessionCreationFailed` | Cannot create delegate session | Check delegate configuration |
| `VerificationFailed` | Verification check failed | Fix the issue or adjust verification |
| `Timeout` | Delegation took too long | Increase timeout or optimize task |
| `Cancelled` | Delegation was cancelled | Retry or handle cancellation |

### Error Classification

The system classifies delegation errors:

```rust
// Patch Application Failure
// → Use read_tool to see current file state
// → Verify context lines match actual file

// Verification Failure  
// → Read verification error output
// → Fix compilation/test errors

// Invalid Working Directory
// → Do NOT specify workdir in patches
// → Verify file paths are correct

// Too Many Retries
// → Current approach not working
// → Try different strategy
```

## Best Practices

### When to Delegate

**Good candidates for delegation:**
- Well-defined, isolated tasks
- Tasks requiring specific expertise
- Parallelizable work
- Tasks with clear success criteria

**Poor candidates for delegation:**
- Highly ambiguous requirements
- Tasks requiring deep context
- Interactive, multi-turn tasks
- Tasks needing human judgment

### Writing Good Delegation Requests

1. **Clear objective**: Be specific about what needs to be done
2. **Relevant context**: Include necessary background information
3. **Explicit constraints**: List any requirements or restrictions
4. **Expected output**: Describe what success looks like
5. **Verification criteria**: If applicable, specify how to verify

### Planning for Delegation

1. **Break down tasks**: Split complex tasks into smaller delegations
2. **Order dependencies**: Plan delegation sequence
3. **Set expectations**: Clearly communicate goals to delegates
4. **Review results**: Always review delegate output before integrating

## Examples

### Simple Delegation

```toml
# Planner delegates coding task to coder
[planner]
tools = ["delegate"]

[[delegates]]
id = "coder"
tools = ["edit", "write_file", "shell"]
```

User: "Add a new API endpoint"
Planner: Delegates to coder with task details
Coder: Implements the endpoint
Planner: Reviews and integrates the changes

### Parallel Delegation

```toml
[[delegates]]
id = "frontend-coder"
tools = ["edit", "write_file"]

[[delegates]]
id = "backend-coder"
tools = ["edit", "write_file", "shell"]
```

User: "Implement feature X"
Planner: Delegates frontend to frontend-coder, backend to backend-coder
Both delegates work in parallel
Planner: Integrates both results

### Verification Example

```toml
[[delegates]]
id = "coder"
# ...

# Verification in delegation request
verification_spec = {
    verification_type = "shell_command",
    command = "cargo test --lib"
}
```

## Troubleshooting

### Delegation Not Starting

1. Check delegate is registered: `agent.agent_registry().list_agents()`
2. Verify delegate configuration is valid
3. Check for middleware errors
4. Review logs for delegation events

### Delegate Not Completing

1. Check delegate has necessary tools
2. Verify delegate can access required files
3. Check for infinite loops in delegate logic
4. Review timeout settings

### Verification Failing

1. Check verification command is correct
2. Verify delegate made expected changes
3. Adjust verification criteria if too strict
4. Review delegate output for issues

## Related Documentation

- [Configuration Guide](configuration.md) - Delegation configuration
- [Mesh Networking](mesh.md) - Remote delegation
- [API Reference](api_reference.md) - Delegation types
- [Agent Modes](agent_modes.md) - Mode-aware delegation
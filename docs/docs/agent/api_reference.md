# QueryMT Agent - API Reference

This document provides a reference for the QueryMT Agent public API.

## Module Structure

```
querymt_agent
── prelude          # Re-exports common types
── api              # Primary public API
│   ├── Agent        # Main agent runtime
│   ├── AgentSession # Per-session interface
│   ── callbacks    # Callback types
── agent            # Agent core types
│   ├── AgentConfig  # Shared configuration
│   ├── SessionActor # Session runtime
│   ── handle       # AgentHandle trait
── acp              # Agent Client Protocol
── delegation       # Multi-agent delegation
── export           # Session data export
│   ├── turns        # Shared turn materialization
│   ├── atif         # ATIF trajectory export
│   ── sft          # SFT training data export
── middleware       # Middleware system
── session          # Session management
── tools            # Tool registry
── events           # Event system
```

## Prelude

The `prelude` module re-exports the most commonly used types:

```rust
use querymt_agent::prelude::*;
```

## Agent Creation

### Single Agent

```rust
use querymt_agent::prelude::*;

let agent = Agent::single()
    .provider("anthropic", "claude-sonnet-4-5-20250929")
    .cwd(".")
    .tools(["read_tool", "shell", "edit"])
    .mesh(Mesh::hybrid())
    .build()
    .await?;
```

Use `Mesh::hybrid()` when you want one process-shared runtime that supports both LAN discovery and later iroh invite joins.

### Multi-Agent (Quorum)

```rust
let agent = Agent::multi()
    .cwd(".")
    .planner(|p| {
        p.provider("anthropic", "claude-sonnet-4-5-20250929")
            .tools(["delegate"])
    })
    .delegate("coder", |d| {
        d.provider("anthropic", "claude-sonnet-4-5-20250929")
            .tools(["shell", "edit"])
            .capabilities(["coding"])
    })
    .build()
    .await?;
```

### From Config

```rust
use querymt_agent::config::{load_config, ConfigSource};

// From file
let config = load_config("config.toml").await?;
let agent = Agent::from_config(config, infra).await?;

// From string
let config = load_config(ConfigSource::Toml(toml_string)).await?;
let agent = Agent::from_config(config, infra).await?;
```

## Agent Types

### Agent

The main agent runtime, supporting both single and multi-agent modes.

```rust
pub enum Agent {
    Single(LocalAgentHandle),
    Multi(QuorumHandle),
}
```

#### Methods

| Method | Description |
|--------|-------------|
| `chat(&self, message: &str) -> Result<String>` | Send a message and get response |
| `new_session(&self) -> Result<AgentSession>` | Create a new session |
| `list_sessions(&self) -> Result<Vec<SessionInfo>>` | List active sessions |
| `mesh(&self) -> Option<AgentMesh>` | Access mesh APIs when the agent was built with mesh support |
| `subscribe_events(&self) -> Receiver<EventEnvelope>` | Subscribe to events |
| `shutdown(&self) -> Result<()>` | Gracefully shutdown |

### AgentMesh

Mesh-aware facade returned by `agent.mesh()`.

```rust
let outcome = agent.mesh().unwrap().join(invite_code).await?;
println!("joined {}", outcome.mesh_id);
```

#### Methods

| Method | Description |
|--------|-------------|
| `join(&self, invite: impl AsRef<str>) -> Result<MeshJoinOutcome>` | Join an iroh mesh scope on the shared runtime |
| `ensure_published(&self) -> Result<()>` | Publish local mesh actors for this agent |
| `runtime(&self) -> &MeshRuntimeHandle` | Access the underlying shared runtime handle |

### AgentSession

Per-session interface for agent interaction.

```rust
pub struct AgentSession {
    // Session-specific operations
}
```

#### Methods

| Method | Description |
|--------|-------------|
| `chat(&self, message: &str) -> Result<String>` | Send message in this session |
| `cancel(&self) -> Result<()>` | Cancel current operation |
| `set_mode(&self, mode: AgentMode) -> Result<()>` | Change agent mode |
| `set_allowed_tools(&self, tools: &[String]) -> Result<()>` | Set allowed tools |
| `set_denied_tools(&self, tools: &[String]) -> Result<()>` | Set denied tools |
| `get_history(&self) -> Result<Vec<AgentMessage>>` | Get conversation history |
| `fork(&self, instructions: &str) -> Result<AgentSession>` | Fork to new session |

## Agent Modes

```rust
pub enum AgentMode {
    Build,   // Full read/write access
    Plan,    // Read-only, planning
    Review,  // Read-only, code review
}
```

### Mode Switching

```rust
// Via session
session.set_mode(AgentMode::Plan)?;

// Via ACP notification
agent.notify_session(SessionNotification::SetAgentMode {
    session_id: "...".to_string(),
    mode: AgentMode::Plan,
})?;
```

## Callbacks

### MessageCallback

```rust
pub type MessageCallback = Arc<dyn Fn(&str, &str) -> Result<()> + Send + Sync>;
```

Called when a message is sent or received.

### ToolCallCallback

```rust
pub type ToolCallCallback = Arc<dyn Fn(&str, &str, &str) -> Result<()> + Send + Sync>;
```

Called before a tool is executed.

### ToolCompleteCallback

```rust
pub type ToolCompleteCallback = Arc<dyn Fn(&str, &str, &str, &str) -> Result<()> + Send + Sync>;
```

Called after a tool completes.

### DelegationCallback

```rust
pub type DelegationCallback = Arc<dyn Fn(&str, &str, &str) -> Result<()> + Send + Sync>;
```

Called during delegation operations.

### ErrorCallback

```rust
pub type ErrorCallback = Arc<dyn Fn(&str, &str) -> Result<()> + Send + Sync>;
```

Called when an error occurs.

## Events

### EventEnvelope

```rust
pub struct EventEnvelope {
    pub session_id: String,
    pub kind: AgentEventKind,
    pub timestamp: u64,
}
```

### AgentEventKind

The concrete event enum evolves over time; consult `crates/agent/src/events.rs` for the canonical Rust definition. Common event families include session lifecycle, prompts/messages, tool execution, permissions, compaction, delegation, and hook diagnostics.

Hook-related diagnostics are surfaced as:

```rust
HookNotice {
    event_name: String,
    message: String,
    is_error: bool,
}
```

`hook_notice` is emitted when a hook exits successfully but returns invalid output that QueryMT ignores for control-flow purposes.

### Cost accounting

Cost is a supported token-cost estimate computed from recorded provider usage and the pricing bound when each request was sent. It is not a provider invoice. Four scopes are used, and every figure belongs to exactly one of them:

| Scope | Meaning | Where it appears |
| --- | --- | --- |
| **attempt** | One physical provider request. A retry is a new attempt. | `llm_attempt_started` / `llm_attempt_settled` (the durable ledger) |
| **operation** | One logical model request, i.e. all of its attempts. | `llm_request_end.cost_usd` and `llm_request_end.accounting` |
| **cycle** | Everything attributed to one execution cycle (one prompt run). A new cycle starts at zero. | `AgentStats` cost fields (a read-only view) |
| **session** | Everything recorded for the session, across cycles and restarts. | `llm_request_end.cumulative_cost_usd` |

`llm_request_end` is derived presentation. It is never an additional ledger entry, and new accounting is recorded only through the attempt events.

```json
{
  "type": "llm_request_end",
  "data": {
    "usage": { "input_tokens": 100, "output_tokens": 10, "reasoning_tokens": 5,
               "cache_read": 50, "cache_write": 20 },
    "tool_calls": 0,
    "finish_reason": "Stop",
    "cost_usd": 0.000615,
    "cumulative_cost_usd": 0.000615,
    "context_tokens": 185,
    "metrics": { "steps": 1, "turns": 1 },
    "accounting": {
      "operation_id": "op-1",
      "attempt_ids": ["att-1"],
      "known_subtotal_usd": 0.000615,
      "complete": true,
      "reasons": []
    }
  }
}
```

- `cost_usd` is the complete supported cost of the whole **operation**, retries included, and includes cache read and write charges. It is `null` unless that total is complete.
- `cumulative_cost_usd` is the **session** total, with the same `null` rule. A session total is complete only when nothing in it is unresolved or incomplete, so a single interrupted attempt clears it. An all-excluded session (for example a subscription provider) has no dollar amount instead of a known zero.
- `accounting.attempt_ids` and `operation_id` match the durable attempt records, so consumers can deduplicate. `known_subtotal_usd` and `reasons` explain a `null` cost. Events written before accounting existed omit `accounting`.
- An execution that ends before a normal `llm_request_end` (cancellation, error) still has its attempts recorded. Its charges and uncertainty are in the attempt events and the session total, but no request-end event is emitted for it.
- Retry attempts are valued separately. A rejected attempt that reports usage is charged; a provider-confirmed non-billable rejection adds nothing; an unconfirmed transport failure stays uncertain rather than zero.

#### Auxiliary generation

Besides the main response, two components make paid provider requests. Both are recorded in the same ledger, with their own `purpose`, and neither changes the main conversation's context size or step counters:

| Purpose | Dispatched by | Charged to |
| --- | --- | --- |
| `main_response` | the execution loop | the executing cycle |
| `compaction` | AI context compaction | the cycle that triggered it (a dedicated `maintenance-…` cycle when compaction runs outside a prompt) |
| `delegation_summary` | the delegation planning summary | the parent cycle that requested the delegation |

Compaction lowers the effective context while increasing spend. A summary refused or rejected by validation (empty, truncated, or with tool calls) still incurs and records its usage, and each retry is a separate attempt.

Every dispatch site is either recorded or explicitly classified. The `AgentRunner::chat` / `Agent::chat` convenience API runs an ordinary prompt cycle and so reaches a provider only through the recorded main loop. `SessionHandle::submit_request` and `SessionHandle::chat` require a ledger argument and have no unrecorded variant. A test (`accounting::dispatch_coverage_tests`) fails if a new provider call appears outside this list. A node that merely hosts a provider for a remote peer does not record the attempt: the requesting node's session does, because the host has no session to charge.

#### Accounting metadata (ACP)

Clients receive the reducer's state as one versioned replacement DTO, `AccountingSnapshot` (generated TypeScript: `target/generated/types.ts`, produced with `PATH=/Users/wiking/.cargo/bin:$PATH ./scripts/generate-types.sh`). The same payload is carried three ways:

| Carrier | When | Where |
| --- | --- | --- |
| `usage_update` | a reliable main-context measurement and the active model's limit are known | `_meta.querymt.accounting` |
| `session_info_update` | otherwise (unknown limit, or a model switch invalidated the last measurement) | `_meta.querymt.accounting`; no `title`/`updatedAt`, no fabricated `used`/`size` |
| `SessionLoadSnapshot` | `session/load` | the required `accounting` field, reduced from the same audit events as `cursor`, present even without historical replay |

Delivery does not depend on negotiated notice support or on the context size. Attempt *starts* are not published (they would flash an incomplete total during every request); each *settlement* that changes state is, and an identical redelivery publishes nothing.

```json
{
  "version": 1,
  "session": {
    "knownSubtotalUsd": 0.000615,
    "completeTotalUsd": 0.000615,
    "complete": true,
    "billing": "metered",
    "reasons": [],
    "pricedAttempts": 1,
    "excludedAttempts": 0,
    "unresolvedAttempts": 0,
    "incompleteAttempts": 0,
    "unaccountedGenerations": 0
  },
  "cycle": {
    "cycleId": "cycle-1",
    "scope": {
      "knownSubtotalUsd": 0.000615,
      "completeTotalUsd": 0.000615,
      "complete": true,
      "billing": "metered",
      "reasons": [],
      "pricedAttempts": 1,
      "excludedAttempts": 0,
      "unresolvedAttempts": 0,
      "incompleteAttempts": 0,
      "unaccountedGenerations": 0
    },
    "budget": {
      "knownSubtotalUsd": 0.000615,
      "completeTotalUsd": 0.000615,
      "reasons": [],
      "incompleteAttempts": 0,
      "bestEffort": false
    }
  }
}
```

A cost that turns incomplete keeps its known subtotal and replaces the total with an explicit `null`:

```json
{
  "version": 1,
  "session": {
    "knownSubtotalUsd": 0.001,
    "completeTotalUsd": null,
    "complete": false,
    "billing": "mixed",
    "reasons": [{ "kind": "valuation", "reason": { "kind": "missing_rate", "category": "cache_write" } }],
    "pricedAttempts": 2,
    "excludedAttempts": 1,
    "unresolvedAttempts": 0,
    "incompleteAttempts": 1,
    "unaccountedGenerations": 0
  },
  "cycle": null
}
```

Semantics:

- **Replacement.** A present payload replaces the previous accounting state atomically. `completeTotalUsd` and `cycle` are always serialized (explicit `null`), never omitted. A message without the payload does not touch accounting. Omission of standard ACP `cost` alone is not a clearing signal.
- **Standard `cost`.** Sent on `usage_update` only from the session's complete metered estimate. It is omitted when the total is partial, unresolved, all-excluded or based on legacy history, and `usage_update`'s `cost` is never taken from `llm_request_end.cumulative_cost_usd`.
- **Scopes.** `session` is lifetime API spend. `cycle` is the cycle of the most recent *main-response* attempt, or `null` when there is none; a delayed compaction or summary settling for an older cycle never selects that cycle as active. `billing` is `empty`, `metered`, `excluded` (never a zero-dollar invoice) or `mixed` (amounts cover metered attempts only).
- **Budget view.** `cycle.budget` is the manual-rate valuation used by `max_price_usd`, kept apart from API spend; `reasons` lists settled gaps only (in-flight attempts are not a reason) and `bestEffort` is true when the cap is enforced against a known subtotal.
- **Token telemetry is separate.** `_meta.querymt.usage` v1 (`latestRequest`, `sessionTotals`, `partial`) keeps its shape and its main-request token scope. Retry and auxiliary attempts do not enter it, so its totals are not the monetary ledger's tokens.
- **Ordering.** Live and replayed delivery reduce the same records, so they agree. Snapshot and captured-live application use the existing local/remote cursors; older state never overwrites newer accounting, and a duplicate is a no-op.
- **Export.** ATIF `final_metrics.total_cost_usd` is the reducer's complete session total or absent. `final_metrics.extra` carries the same `accounting` snapshot plus an `attempts` list covering main, auxiliary, failed and unresolved attempts. A step's `metrics.extra.accounting` gives an incomplete operation's known subtotal and reasons. Auxiliary attempts have no step, and an `llm_request_end` without `accounting` (legacy history) exports no dollar cost.

##### Required client behavior (`querymt-desktop` follow-up)

This repository defines the payload only. The desktop client must be updated together with the backend:

- Apply the payload atomically and treat a present `null` `completeTotalUsd` as "cleared". Do not write `update.cost ?? previous`.
- `mergeMissingSessionUsage` must not refill an intentionally cleared cost from an older candidate. Merge precedence follows the snapshot/live cursors.
- Hydrate from `SessionLoadSnapshot.accounting`. Stop deriving money from `llm_request_end.cumulative_cost_usd`; token-history telemetry may stay as is.
- Read the same DTO from `usage_update` or `session_info_update`, and do not refresh the session catalog for an accounting-only `session_info_update` (it changes no summary field).
- Label figures as "Estimated API cost" (complete), "Known API subtotal - incomplete" (with reasons) and "Subscription usage excluded"; show the manual-rate cycle budget separately.

##### Breaking changes and deployment

- `Usage` gains optional `cache_write_tiers`; `ModelPricing` gains cache and one-hour write rates and applicability metadata; `LlmRequestEnd` gains `accounting`; the events `llm_attempt_started` / `llm_attempt_settled` are new; `SessionLoadSnapshot` gains `accounting`; `AgentStats` costs are views of recorded components and the aggregate-token repricing helpers are removed. There are no legacy adapters.
- Upgrade the backend, providers/plugins, remote peers, bindings and clients together. Mixed-version accounting is not supported.
- Existing sessions keep their history. Local generation recorded before accounting has no basis and is reported as `unaccountedGenerations`, so such a session never claims a complete total, and no historical rates are applied.
- Rollback needs matching binaries and a database copy taken before the upgrade. Do not run an older event decoder against a journal containing the new events, and do not delete accounting events to make a rollback succeed.

## Delegation

### Delegation

```rust
pub struct Delegation {
    pub public_id: String,
    pub target_agent_id: String,
    pub objective: String,
    pub context: Option<String>,
    pub constraints: Option<String>,
    pub expected_output: Option<String>,
    pub task_id: Option<String>,
    pub planning_summary: Option<String>,
    pub verification_spec: Option<VerificationSpec>,
    pub isolation: DelegationIsolation, // Inherit (default) | Shared | Isolated
}
```

### DelegationStatus

```rust
pub enum DelegationStatus {
    Pending,
    Running,
    Complete,
    Failed,
    Cancelled,
}
```

### VerificationSpec

```rust
pub struct VerificationSpec {
    pub verification_type: VerificationType,
    pub parameters: HashMap<String, String>,
}

pub enum VerificationType {
    ShellCommand { command: String },
    FileExists { path: String },
    FileContains { path: String, content: String },
    Custom { spec: serde_json::Value },
}
```

## Middleware

### MiddlewareDriver

```rust
pub trait MiddlewareDriver: Send + Sync {
    fn name(&self) -> &str;
    fn process_request(&self, request: &mut Request) -> Result<()>;
    fn process_response(&self, response: &mut Response) -> Result<()>;
    fn process_tool_call(&self, tool_call: &mut ToolCall) -> Result<()>;
    fn process_tool_result(&self, result: &mut ToolResult) -> Result<()>;
}
```

### CompositeDriver

```rust
pub struct CompositeDriver {
    // Chain of middleware drivers
}

impl CompositeDriver {
    pub fn new(drivers: Vec<Arc<dyn MiddlewareDriver>>) -> Self;
    pub fn add(&mut self, driver: Arc<dyn MiddlewareDriver>);
}
```

## Tools

### ToolRegistry

```rust
pub struct ToolRegistry {
    // Registered tools
}

impl ToolRegistry {
    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>>;
    pub fn names(&self) -> Vec<String>;
    pub fn definitions(&self) -> Vec<chat::Tool>;
    pub fn register(&mut self, tool: Arc<dyn Tool>);
}
```

### Built-in Tools

| Tool | Description |
|------|-------------|
| `read_tool` | Read file contents |
| `edit` | Edit file with patch |
| `write_file` | Write/overwrite file |
| `delete_file` | Delete file |
| `shell` | Execute shell command |
| `glob` | File pattern matching |
| `search_text` | Text search in files |
| `ls` | List directory contents |
| `create_task` | Create task |
| `todowrite` | Update todo list |
| `todoread` | Read todo list |
| `question` | Ask user question |
| `web_fetch` | Fetch web content |

## Configuration Types

### AgentConfig

```rust
pub struct AgentConfig {
    pub provider: Arc<SessionProvider>,
    pub event_sink: Arc<EventSink>,
    pub agent_registry: Arc<dyn AgentRegistry>,
    pub default_mode: Arc<Mutex<AgentMode>>,
    pub tool_config: ToolConfig,
    pub tool_registry: ToolRegistry,
    pub middleware_drivers: Vec<Arc<dyn MiddlewareDriver>>,
    pub mcp_servers: Vec<McpServerConfig>,
    pub max_steps: Option<usize>,
    pub snapshot_policy: SnapshotPolicy,
    pub assume_mutating: bool,
    pub mutating_tools: HashSet<String>,
    pub execution_policy: RuntimeExecutionPolicy,
    // ... more fields
}
```

### ToolConfig

```rust
pub struct ToolConfig {
    pub policy: ToolPolicy,
    pub allowlist: Option<HashSet<String>>,
    pub denylist: HashSet<String>,
}

pub enum ToolPolicy {
    BuiltInOnly,
    ProviderOnly,
    BuiltInAndProvider,
}
```

### ExecutionPolicy

```rust
pub struct ExecutionPolicy {
    pub tool_output: ToolOutputConfig,
    pub pruning: PruningConfig,
    pub compaction: CompactionConfig,
    pub snapshot: SnapshotBackendConfig,
    pub rate_limit: RateLimitConfig,
}
```

## Error Handling

### AgentError

```rust
pub enum AgentError {
    // Core errors
    InternalError(String),
    InvalidConfig(String),
    NotFound(String),
    
    // Session errors
    SessionNotFound(String),
    SessionNotActive(String),
    SessionLimitReached,
    
    // Tool errors
    ToolNotFound(String),
    ToolPermissionDenied(String),
    ToolExecutionFailed(String),
    
    // LLM errors
    LLMProviderError(String),
    LLMRateLimit,
    LLMContextExceeded,
    
    // Delegation errors
    DelegationNotFound(String),
    DelegationFailed(String),
    VerificationFailed(String),
    
    // Permission errors
    PermissionDenied(String),
    PermissionCancelled,
}
```

## ACP (Agent Client Protocol)

### Transport

```rust
// Stdio transport
pub async fn serve_stdio(agent: Arc<LocalAgentHandle>) -> Result<()>;

// WebSocket transport
pub async fn serve_websocket(agent: Arc<LocalAgentHandle>, addr: &str) -> Result<()>;
```

### RPC Messages

```rust
pub struct RpcRequest {
    pub id: String,
    pub method: String,
    pub params: serde_json::Value,
}

pub struct RpcResponse {
    pub id: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<RpcError>,
}
```

## Remote/Mesh API

### Mesh Configuration

```rust
pub struct MeshConfig {
    pub listen: Option<String>,
    pub discovery: MeshDiscovery,
    pub bootstrap_peers: Vec<String>,
    pub directory: DirectoryMode,
    pub request_timeout: Duration,
}

pub enum MeshDiscovery {
    Mdns,
    Kademlia,
    None,
}
```

### Remote Session

```rust
pub struct RemoteSession {
    pub peer_id: String,
    pub session_id: String,
    pub agent_id: String,
}

impl RemoteSession {
    pub async fn create(agent: &Agent, peer_id: &str, config: &str) -> Result<Self>;
    pub async fn attach(agent: &Agent, remote: RemoteSession) -> Result<AgentSession>;
}
```

## Examples

See `examples/` directory for complete usage examples:

- `qmtcode.rs` - Full-featured coder agent
- `acp_agent.rs` - ACP stdio server
- `auto_delegation_example.rs` - Multi-agent delegation
- `from_config.rs` - Configuration-based agent
- `morning_brief.rs` - Daily summary agent
- `replay_session.rs` - Session replay

## Version Compatibility

| QueryMT Agent | QueryMaTe | Rust Edition |
|---------------|-----------|--------------|
| 0.2.x | 0.2.x | 2021 |
| 0.1.x | 0.1.x | 2021 |

## Breaking Changes

### v0.2.0

- `AgentHandle` trait introduced for unified local/remote interface
- Delegation uses `AgentHandle::create_delegation_session()`
- `SessionActor` now uses kameo actor model
- Event system uses `EventFanout` instead of direct subscriptions

### v0.1.0

- Initial release with single-agent support
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
    .build()
    .await?;
```

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
| `subscribe_events(&self) -> Receiver<EventEnvelope>` | Subscribe to events |
| `shutdown(&self) -> Result<()>` | Gracefully shutdown |

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

```rust
pub enum AgentEventKind {
    // Session events
    SessionCreated,
    SessionStarted,
    SessionCancelled,
    SessionClosed,
    
    // Message events
    UserMessage { message: String },
    AssistantMessage { content: String },
    
    // Tool events
    ToolCall { tool_name: String, arguments: String },
    ToolResult { tool_name: String, result: String },
    
    // Delegation events
    DelegationRequested { delegation: Delegation },
    DelegationCompleted { delegation_id: String, result: Option<String> },
    DelegationFailed { delegation_id: String, error: String },
    
    // Mode events
    AgentModeChanged { mode: AgentMode },
    
    // System events
    CompactionStarted,
    CompactionCompleted,
    PruningStarted,
    PruningCompleted,
}
```

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
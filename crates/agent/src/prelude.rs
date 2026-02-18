//! Commonly-used types for agent applications
//!
//! ```no_run
//! use querymt_agent::prelude::*;
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let agent = Agent::single()
//!     .provider("openai", "gpt-4")
//!     .build()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
// High-level APIs
pub use crate::simple::{
    Agent, AgentBuilder, AgentSession, DelegateConfigBuilder, PlannerConfigBuilder, Quorum,
    QuorumBuilder,
};

// Config and runner APIs
pub use crate::config::{
    AgentSettings, Config, ConfigSource, DelegateConfig, McpServerConfig, PlannerConfig,
    QuorumConfig, QuorumSettings, SingleAgentConfig,
};
pub use crate::runner::{
    AgentRunner, ChatRunner, ChatRunnerExt, ChatSession, ChatSessionExt, from_config,
};

// Core agent types
pub use crate::agent::{AgentHandle, SnapshotPolicy, ToolPolicy};
pub use crate::quorum::{AgentQuorum, DelegateAgent};

// Events
pub use crate::event_bus::EventBus;
pub use crate::events::{AgentEvent, AgentEventKind, EventObserver};

// Delegation & Multi-agent
pub use crate::delegation::{AgentInfo, AgentRegistry};

// Configuration types
pub use crate::agent::DelegationContextTiming;
pub use crate::middleware::{ContextConfig, DelegationConfig, LimitsConfig, MiddlewarePresets};

// Session & Storage
pub use crate::session::domain::Task;
pub use crate::session::store::SessionStore;

// Hash
pub use crate::hash::RapidHash;

// Tools
pub use crate::tools::Tool;

// ACP Server
pub use crate::acp::AcpTransport;

// Builder (for advanced use)
pub use crate::agent::AgentConfigBuilder;

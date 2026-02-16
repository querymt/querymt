//! Simplified high-level API for agent creation
//!
//! This module provides an ergonomic interface for creating single agents
//! and multi-agent quorums with minimal boilerplate.
//!
//! # Single Agent Example
//!
//! ```no_run
//! use querymt_agent::simple::Agent;
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let agent = Agent::single()
//!     .provider("openai", "gpt-4")
//!     .cwd(".")
//!     .tools(["shell", "read_tool", "write_file"])
//!     .build()
//!     .await?;
//!
//! let response = agent.chat("Hello!").await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Multi-Agent Quorum Example
//!
//! ```no_run
//! use querymt_agent::simple::Agent;
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let quorum = Agent::multi()
//!     .cwd(".")
//!     .planner(|p| p.provider("openai", "gpt-4").tools(["delegate"]))
//!     .delegate("coder", |d| {
//!         d.provider("anthropic", "claude-3-opus")
//!             .tools(["shell", "edit"])
//!             .capabilities(["coding"])
//!     })
//!     .build()
//!     .await?;
//!
//! let response = quorum.chat("Build a hello world app").await?;
//! # Ok(())
//! # }
//! ```

mod agent;
mod callbacks;
mod config;
mod quorum;
mod session;
mod utils;

// Re-export public API types
pub use agent::{Agent, AgentBuilder};
pub use config::{DelegateConfigBuilder, PlannerConfigBuilder};
pub use quorum::{Quorum, QuorumBuilder};
pub use session::AgentSession;

// Re-export callback types for public use
pub use callbacks::{
    DelegationCallback, ErrorCallback, MessageCallback, ToolCallCallback, ToolCompleteCallback,
};

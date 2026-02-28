//! Primary public API for agent creation and interaction.
//!
//! `Agent` is the single runtime type for both single agents and multi-agent
//! quorums. `Agent::single()` and `Agent::multi()` return different builders
//! that both produce `Agent`.
//!
//! # Single Agent
//!
//! ```no_run
//! use querymt_agent::prelude::*;
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let agent = Agent::single()
//!     .provider("anthropic", "claude-sonnet-4-20250514")
//!     .cwd(".")
//!     .tools(["read_tool", "shell", "edit"])
//!     .build()
//!     .await?;
//!
//! agent.chat("hello").await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Multi-Agent (quorum)
//!
//! ```no_run
//! use querymt_agent::prelude::*;
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let agent = Agent::multi()
//!     .cwd(".")
//!     .planner(|p| p.provider("openai", "gpt-4").tools(["delegate"]))
//!     .delegate("coder", |d| {
//!         d.provider("anthropic", "claude-sonnet-4-20250514")
//!             .tools(["shell", "edit"])
//!             .capabilities(["coding"])
//!     })
//!     .build()
//!     .await?;
//!
//! agent.chat("implement feature X").await?;
//! # Ok(())
//! # }
//! ```
//!
//! # From serializable config (FFI / TOML / JSON)
//!
//! ```no_run
//! use querymt_agent::prelude::*;
//!
//! # async fn example(config: SingleAgentConfig, infra: AgentInfra) {
//! let agent = Agent::from_config(config, infra).await.unwrap();
//! agent.chat("hello").await.unwrap();
//! # }
//! ```

mod agent;
mod callbacks;
mod config;
mod quorum;
mod session;
mod utils;

// Re-export public API types
pub use agent::{Agent, AgentBuilder, AgentInfra};
pub use config::{DelegateConfigBuilder, PlannerConfigBuilder};
pub use quorum::QuorumBuilder;
pub use session::AgentSession;

// Re-export callback types for public use
pub use callbacks::{
    DelegationCallback, ErrorCallback, MessageCallback, ToolCallCallback, ToolCompleteCallback,
};

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
mod config;
mod dotagents_runtime;
mod dotagents_targets;
mod profiles;
mod quorum;
mod utils;
mod worktrees;

/// The session, mesh and callback parts of the API live in the runtime crate.
pub use querymt_agent_runtime::api::{mesh, sessions};

// Re-export public API types
pub use agent::{Agent, AgentBuilder, AgentInfra};
pub use config::{DelegateConfigBuilder, PlannerConfigBuilder};
pub use dotagents_targets::DotagentsTargetFactory;
pub use profiles::{AgentProfiles, ProfileRuntimeHandle};
pub use querymt_agent_runtime::api::AgentSession;
pub use querymt_agent_runtime::api::sessions::AcpSessionListError;
pub use querymt_agent_runtime::api::sessions::{
    AgentLoadedSession, AgentSessions, ArchiveFilter, ListSessionsOptions,
    RemoteSessionConnectionState, RemoteSessionMode, ResolutionFilter, SessionChildrenPage,
    SessionChildrenPageWithLifecycle, SessionGroup, SessionGroupWithLifecycle, SessionLifecycle,
    SessionLifecycleFilter, SessionLifecyclePatch, SessionListMode, SessionListPage,
    SessionListPageWithLifecycle, SessionMeta, SessionSummary, SessionSummaryWithLifecycle,
};
pub use querymt_agent_runtime::api::{AgentMesh, Mesh, MeshJoinOutcome, MeshRuntime, MeshSpec};
pub use quorum::QuorumBuilder;
pub use worktrees::{AgentWorktrees, SessionOptions};

#[cfg(test)]
mod agent_tests;

// Re-export callback types for public use
pub use querymt_agent_runtime::api::callbacks::{
    DelegationCallback, ErrorCallback, MessageCallback, ToolCallCallback, ToolCompleteCallback,
};

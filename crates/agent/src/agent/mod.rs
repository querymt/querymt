//! Agent module - Core agent functionality split across focused submodules
//!
//! This module provides the agent implementation, broken down into logical
//! components for better maintainability and testing. The public API uses
//! `AgentHandle` which wraps the kameo actor-based session management.
//!
//! ## Kameo Actor Architecture
//!
//! The agent uses a one-actor-per-session design:
//! - `AgentConfig`: Shared configuration (not an actor)
//! - `SessionActor`: One per session, owns all session state
//! - `SessionRegistry`: Manages session actors (server layer)
//! - Messages: Type-safe message structs for actor communication

pub mod agent_config;
pub mod agent_config_builder;
pub mod core;
pub mod execution;
pub mod execution_context;
pub mod file_proxy;
pub mod handle;
pub mod mcp;
pub mod messages;
pub mod protocol;
pub mod remote;
pub mod session_actor;
pub mod session_registry;
pub mod snapshots;
pub mod tools;
pub mod undo;
pub mod utils;
#[cfg(feature = "sandbox")]
pub mod worker_manager;

// Re-export main types for convenience
pub use agent_config_builder::AgentConfigBuilder;
pub use core::{
    ClientState, DelegationContextConfig, DelegationContextTiming, SessionRuntime, SnapshotPolicy,
    ToolConfig, ToolPolicy,
};
pub use execution::CycleOutcome;
pub use snapshots::SnapshotState;

// Re-export kameo actor types
pub use agent_config::AgentConfig;
pub use handle::{AgentHandle, LocalAgentHandle};
pub use remote::SessionActorRef;
pub use session_actor::SessionActor;
pub use session_registry::SessionRegistry;

#[cfg(test)]
mod delegation_loop_tests;
#[cfg(test)]
mod execution_tests;
#[cfg(all(test, feature = "sandbox"))]
mod sandbox_integration_tests;
#[cfg(test)]
mod undo_integration_tests;
#[cfg(test)]
mod undo_tests;

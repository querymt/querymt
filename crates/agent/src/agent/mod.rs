//! Agent module - Core agent functionality split across focused submodules
//!
//! This module provides the main QueryMTAgent implementation, broken down into
//! logical components for better maintainability and testing.

pub mod builder;
pub mod core;
pub mod execution;
pub mod mcp;
pub mod protocol;
pub mod snapshots;
pub mod tool_execution;
pub mod tools;
pub mod transitions;
pub mod utils;

// Re-export main types for convenience
pub use builder::AgentBuilderExt;
pub use core::{
    ClientState, DelegationContextConfig, DelegationContextTiming, QueryMTAgent, SessionRuntime,
    SnapshotPolicy, ToolConfig, ToolPolicy,
};
pub use execution::CycleOutcome;
pub use snapshots::SnapshotState;

// Re-export key functionality
pub use core::QueryMTAgent as Agent;

#[cfg(test)]
mod delegation_loop_tests;
#[cfg(test)]
mod execution_tests;

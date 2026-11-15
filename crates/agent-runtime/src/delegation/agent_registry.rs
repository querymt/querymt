//! The `AgentRegistry` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use super::core::*;
use std::sync::Arc;

/// AgentRegistry stores both agent metadata (AgentInfo) and agent handles.
///
/// This enables:
/// 1. Listing available agents for delegation (via AgentInfo)
/// 2. Actually delegating to agents (via AgentHandle)
/// 3. Thread-safe access from multiple sessions
pub trait AgentRegistry: Send + Sync {
    /// List all available agents (metadata only).
    fn list_agents(&self) -> Vec<AgentInfo>;

    /// Get agent metadata by ID.
    fn get_agent(&self, id: &str) -> Option<AgentInfo>;

    /// Get an agent handle for delegation.
    ///
    /// Returns an `Arc<dyn AgentHandle>` that can be used to interact with the agent
    /// via session management, prompting, and event subscription.
    fn get_handle(&self, id: &str) -> Option<Arc<dyn crate::agent::handle::AgentHandle>>;
}

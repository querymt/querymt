#![recursion_limit = "256"]

pub mod api;
pub mod error;
pub mod mcp_auth;
pub mod prelude;

/// Backward-compatible alias for the `api` module.
#[doc(hidden)]
pub use api as simple;

pub mod config;
pub mod control;
pub(crate) use querymt_agent_core as core_staging;
pub mod runner;
pub mod template;

pub mod auth;

// Re-export SecretStore at crate root so it is available without the `oauth`
// feature.  Dashboard and ACP builds use this for plain API-key storage.
pub use querymt_utils::secret_store::SecretStore;

pub use querymt_agent_services::accounting;
pub mod acp;
pub mod agent;
pub mod delegation;
pub mod dotagents;
pub mod elicitation;
pub use querymt_agent_core::events;
pub use querymt_agent_core::hash;
pub use querymt_agent_services::event_fanout;
pub use querymt_agent_services::event_sink;
pub use querymt_agent_services::export;
pub mod hooks;
pub use querymt_agent_index as index;
pub mod knowledge;
pub mod middleware;
pub use querymt_agent_core::model;
pub use querymt_agent_services::model_heuristics;
pub use querymt_agent_services::model_info;
pub mod chat_session;
pub mod model_inventory;
pub mod model_registry;
#[cfg(feature = "plugin-loaders")]
pub mod plugin_update;
pub mod profile_host;
pub mod profiles;
pub mod quorum;
pub mod scheduler;
pub mod send_agent;
#[cfg(feature = "api")]
pub mod server;
pub mod session;
pub use querymt_agent_services::session_event_stream;
pub mod skills;
pub mod slash_commands;
pub use querymt_agent_services::snapshot;
pub use querymt_agent_services::tasks;
/// Tools: the services-layer tools plus the agent-aware built-ins.
pub mod tools {
    pub use querymt_agent_services::tools::*;

    /// Built-in tools: the services-layer set with the runtime's additions.
    pub mod builtins {
        pub use crate::agent::builtin_tools::{
            DelegateTool, RouteDelegationToPeerTool, UseRemoteProviderTool, all_builtin_tools,
        };
        pub use querymt_agent_services::tools::builtins::*;
    }
}
pub use querymt_agent_services::verification;
pub use querymt_agent_services::workspace_query;
/// Managed worktrees: the services layer plus the SQLite registry.
pub mod worktree {
    #[cfg(test)]
    pub use querymt_agent_services::worktree::test_support;
    pub use querymt_agent_services::worktree::*;
    pub use querymt_agent_sqlite::repo_workspace;
    pub use repo_workspace::SqliteWorkspaceRepository;
}

#[cfg(test)]
pub mod test_utils;

/// Runtime tests that exercise the top layer (`api`, `profiles`, websocket).
#[cfg(test)]
mod runtime_tests;

// Re-export top-level error type
pub use error::AgentError;

// Re-export main agent types for backward compatibility
pub use agent::{AgentHandle, DelegationContextConfig, DelegationContextTiming, LocalAgentHandle};
pub use quorum::{AgentQuorum, AgentQuorumBuilder, AgentQuorumError, DelegateAgent};

// Re-export kameo actor types
pub use agent::{AgentConfig, SessionActor, SessionRegistry};

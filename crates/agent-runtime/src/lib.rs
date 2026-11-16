#![recursion_limit = "256"]
//! Runtime layer of the querymt agent.
//!
//! The agent handle and session actors, ACP (except the WebSocket server),
//! mesh and remote sessions, auth, configuration, hooks, middleware and the
//! profile host contract. Depends on the services, knowledge, index and core
//! crates (design D1); the SQLite implementation is a dev-dependency used by
//! tests and by the `test-support` feature.

pub mod acp;
pub mod agent;
pub mod api;
pub mod auth;
pub mod chat_session;
pub mod config;
pub mod control;
pub mod delegation;
pub mod dotagents;
pub mod elicitation;
pub mod error;
pub mod hooks;
pub mod knowledge;
pub mod mcp_auth;
pub mod middleware;
pub mod model_inventory;
pub mod model_registry;
#[cfg(feature = "plugin-loaders")]
pub mod plugin_update;
pub mod profile_host;
pub mod scheduler;
pub mod send_agent;
pub mod session;
pub mod skills;
pub mod slash_commands;
pub mod template;

/// Tools: the services-layer tools plus the agent-aware built-ins.
#[allow(unused_imports)]
pub(crate) mod tools {
    pub use querymt_agent_services::tools::*;

    pub mod builtins {
        #[allow(unused_imports)]
        pub use crate::agent::builtin_tools::{
            DelegateTool, RouteDelegationToPeerTool, UseRemoteProviderTool, all_builtin_tools,
            builtin_tool_names,
        };
        pub use querymt_agent_services::tools::builtins::*;
    }
}

/// Managed worktrees: the services layer (plus the SQLite registry in tests).
#[allow(unused_imports)]
pub(crate) mod worktree {
    #[cfg(any(test, feature = "test-support"))]
    pub use querymt_agent_services::worktree::test_support;
    pub use querymt_agent_services::worktree::*;
    #[cfg(any(test, feature = "test-support"))]
    pub use querymt_agent_sqlite::repo_workspace;
}

#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod test_utils;

pub use agent::{AgentConfig, LocalAgentHandle, SessionActor, SessionRegistry};
pub use error::AgentError;
pub use querymt_utils::secret_store::SecretStore;

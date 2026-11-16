#![recursion_limit = "256"]
//! `querymt-agent`: the public facade of the agent.
//!
//! The implementation lives in the layered crates `querymt-agent-core`,
//! `-index`, `-knowledge`, `-sqlite`, `-services` and `-runtime`; this crate
//! adds the high-level `api`, the WebSocket/dashboard `server`, `quorum`,
//! `profiles` and re-exports every module at its historical path.

pub mod acp;
pub mod api;
pub mod prelude;
pub mod profiles;
pub mod runner;
#[cfg(feature = "api")]
pub mod server;

/// Backward-compatible alias for the `api` module.
#[doc(hidden)]
pub use api as simple;

// Runtime layer.
#[cfg(feature = "plugin-loaders")]
pub use querymt_agent_runtime::plugin_update;
pub use querymt_agent_runtime::{
    agent, auth, chat_session, config, control, delegation, dotagents, elicitation, error, hooks,
    mcp_auth, middleware, model_inventory, model_registry, profile_host, scheduler, send_agent,
    skills, slash_commands, template,
};

// Services, index and core layers.
pub use querymt_agent_core::events;
pub use querymt_agent_core::hash;
pub use querymt_agent_core::model;
pub use querymt_agent_index as index;
pub use querymt_agent_services::{
    accounting, event_fanout, event_sink, export, model_heuristics, model_info,
    session_event_stream, snapshot, tasks, verification, workspace_query,
};

// Re-export SecretStore at crate root so it is available without the `oauth`
// feature.  Dashboard and ACP builds use this for plain API-key storage.
pub use querymt_utils::secret_store::SecretStore;

/// Knowledge: contract and host from the runtime, plus the SQLite store.
pub mod knowledge {
    pub use querymt_agent_runtime::knowledge::*;
    pub use querymt_agent_sqlite::knowledge::sqlite;
}

/// Sessions: core contracts, SQLite storage and the runtime's provider.
pub mod session {
    pub use querymt_agent_runtime::session::*;
    pub use querymt_agent_sqlite::repo_schedule;
    pub use querymt_agent_sqlite::session::{
        repo_artifact, repo_decision, repo_delegation, repo_intent, repo_progress, repo_session,
        repo_task, schema, sqlite_storage,
    };
    pub use repo_artifact::SqliteArtifactRepository;
    pub use repo_decision::SqliteDecisionRepository;
    pub use repo_delegation::SqliteDelegationRepository;
    pub use repo_intent::SqliteIntentRepository;
    pub use repo_progress::SqliteProgressRepository;
    pub use repo_schedule::SqliteScheduleRepository;
    pub use repo_session::SqliteSessionRepository;
    pub use repo_task::SqliteTaskRepository;
    pub use sqlite_storage::SqliteStorage;
}

/// Tools: the services-layer tools plus the agent-aware built-ins.
pub mod tools {
    pub use querymt_agent_services::tools::*;

    /// Built-in tools: the services-layer set with the runtime's additions.
    pub mod builtins {
        pub use querymt_agent_runtime::agent::builtin_tools::{
            DelegateTool, RouteDelegationToPeerTool, UseRemoteProviderTool, all_builtin_tools,
            builtin_tool_names,
        };
        pub use querymt_agent_services::tools::builtins::*;
    }
}

/// Managed worktrees: the services layer plus the SQLite registry.
pub mod worktree {
    #[cfg(test)]
    pub use querymt_agent_services::worktree::test_support;
    pub use querymt_agent_services::worktree::*;
    pub use querymt_agent_sqlite::repo_workspace;
    pub use repo_workspace::SqliteWorkspaceRepository;
}

#[cfg(test)]
pub use querymt_agent_runtime::test_utils;

/// Runtime tests that exercise the top layer (`api`, `profiles`, websocket).
#[cfg(test)]
mod runtime_tests;

// Re-export top-level error type
pub use error::AgentError;

// Re-export main agent types for backward compatibility
pub use agent::{AgentConfig, LocalAgentHandle, SessionActor, SessionRegistry};

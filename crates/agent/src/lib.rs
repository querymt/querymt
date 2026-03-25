pub mod api;
pub mod error;
pub mod prelude;

/// Backward-compatible alias for the `api` module.
#[doc(hidden)]
pub use api as simple;

pub mod config;
pub mod runner;
pub mod template;

pub mod auth;

// Re-export SecretStore at crate root so it is available without the `oauth`
// feature.  Dashboard and ACP builds use this for plain API-key storage.
pub use querymt_utils::secret_store::SecretStore;

pub mod acp;
pub mod agent;
pub mod delegation;
pub mod elicitation;
pub mod event_fanout;
pub mod event_sink;
pub mod events;
pub mod export;
pub mod hash;
pub mod index;
pub mod knowledge;
pub mod middleware;
pub mod model;
pub mod model_heuristics;
pub mod model_info;
pub mod model_registry;
pub mod plugin_update;
pub mod quorum;
pub mod scheduler;
pub mod send_agent;
#[cfg(feature = "api-only")]
pub mod server;
pub mod session;
pub mod skills;
pub mod snapshot;
pub mod tasks;
pub mod tools;
#[cfg(feature = "api-only")]
pub mod ui;
pub mod verification;
pub mod workspace_query;

#[cfg(test)]
pub mod test_utils;

// Re-export top-level error type
pub use error::AgentError;

// Re-export main agent types for backward compatibility
pub use agent::{AgentHandle, DelegationContextConfig, DelegationContextTiming, LocalAgentHandle};
pub use quorum::{AgentQuorum, AgentQuorumBuilder, AgentQuorumError, DelegateAgent};

// Re-export kameo actor types
pub use agent::{AgentConfig, SessionActor, SessionRegistry};

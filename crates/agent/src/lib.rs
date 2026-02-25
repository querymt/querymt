pub mod error;
pub mod prelude;
pub mod simple;

pub mod config;
pub mod runner;

#[cfg(feature = "oauth")]
pub mod auth;

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
pub mod middleware;
pub mod model;
pub mod model_heuristics;
pub mod model_info;
pub mod quorum;
pub mod send_agent;
#[cfg(feature = "dashboard")]
pub mod server;
pub mod session;
pub mod skills;
pub mod snapshot;
pub mod tasks;
pub mod tools;
#[cfg(feature = "dashboard")]
pub mod ui;
pub mod verification;

#[cfg(test)]
pub mod test_utils;

// Re-export top-level error type
pub use error::AgentError;

// Re-export main agent types for backward compatibility
pub use agent::{AgentHandle, LocalAgentHandle, DelegationContextConfig, DelegationContextTiming};
pub use quorum::{AgentQuorum, AgentQuorumBuilder, AgentQuorumError, DelegateAgent};

// Re-export kameo actor types
pub use agent::{AgentConfig, SessionActor, SessionRegistry};

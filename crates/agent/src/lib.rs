pub mod prelude;
pub mod simple;

pub mod config;
pub mod runner;

pub mod acp;
pub mod agent;
pub mod delegation;
pub mod event_bus;
pub mod events;
pub mod export;
pub mod hash;
pub mod index;
pub mod middleware;
pub mod model;
pub mod model_info;
pub mod quorum;
pub mod send_agent;
pub mod server;
pub mod session;
pub mod tasks;
pub mod tools;
pub mod ui;
pub mod verification;

#[cfg(test)]
pub mod test_utils;

// Re-export main agent types for backward compatibility
pub use agent::{DelegationContextConfig, DelegationContextTiming, QueryMTAgent};
pub use event_bus::EventBus;
pub use quorum::{AgentQuorum, AgentQuorumBuilder, AgentQuorumError, DelegateAgent};

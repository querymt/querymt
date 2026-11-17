//! Shared native ACP client and QueryMT session helpers used by qmacp and MCP bridges.
//!
//! Embedded callers should use [`client::AcpClient::connect_with_events`] to receive
//! compact events through a bounded channel without writing to stdout.

pub mod client;
pub mod error;
pub mod events;
pub mod output;
pub mod policy;
pub mod session;
pub mod url;

pub use agent_client_protocol as protocol;

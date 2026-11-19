//! ACP wire types used by the agent events.
//!
//! Re-exports the ACP v1 schema so every consumer names the same types. The
//! agent's own `protocol` module re-exports the same schema through
//! `agent-client-protocol`; `querymt-agent-core` asserts at compile time that both
//! are one type.

pub use agent_client_protocol_schema::MaybeUndefined;
pub use agent_client_protocol_schema::v1::*;

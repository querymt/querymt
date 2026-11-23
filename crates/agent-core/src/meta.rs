//! ACP `_meta` namespace used by QueryMT extensions.

/// QueryMT extension namespace inside ACP's `_meta` escape hatch. Defined with
/// the shared replay projection so the agent and clients name one value.
pub use querymt_agent_protocol_types::replay::QUERYMT_META_NAMESPACE;

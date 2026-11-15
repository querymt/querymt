//! Knowledge layer — pluggable structured memory.
//!
//! The store contract (`KnowledgeStore`, the domain types, `KnowledgeError`,
//! the scope policies) and the text processing live in
//! `core_staging::knowledge`; this module re-exports them at their old paths
//! and adds the SQLite implementation and the host wiring.

pub mod host;
pub use querymt_agent_knowledge as repository;
#[cfg(any(test, feature = "test-support"))]
pub use querymt_agent_sqlite::knowledge::sqlite;

pub use crate::core_staging::knowledge::text_processing;
pub use crate::core_staging::knowledge::*;

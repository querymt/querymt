//! Function index for fast similarity lookups
//!
//! Builds and maintains an index of all functions in a codebase for fast
//! duplicate/similar code detection.

mod core;
mod fingerprint;
mod indexing;
mod types;

#[cfg(test)]
mod tests;

// Public API — unchanged from the old single-file module
pub use core::FunctionIndex;
pub use types::{FunctionIndexConfig, IndexedFunctionEntry, SimilarFunctionMatch};

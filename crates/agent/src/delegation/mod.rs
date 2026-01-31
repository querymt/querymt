// Delegation orchestration module
mod core;
mod summarizer;

// Re-export public items from core
pub use core::*;

// Re-export summarizer
pub use summarizer::DelegationSummarizer;

// Delegation orchestration module
mod agent_registry;
mod core;
pub mod member;
pub mod root;
use querymt_agent_core::delegation::model_overrides;
mod summarizer;

// Re-export public items from core
pub use core::*;
pub(crate) use model_overrides::DelegateRouteOverrides;
pub use model_overrides::{
    DelegateModelOverride, DelegateModelOverrideStore, DelegateReasoningEffort,
};

// Re-export summarizer
pub use root::{RootDelegation, build_root, delegation_enabled};
pub use summarizer::DelegationSummarizer;

#[cfg(test)]
mod root_tests;

// Delegation orchestration module
mod core;
pub mod member;
pub mod root;
pub mod roster;
use querymt_agent_core::delegation::model_overrides;
mod summarizer;

// Re-export public items from core
pub use core::*;
pub(crate) use model_overrides::DelegateRouteOverrides;
pub use model_overrides::{
    DelegateModelOverride, DelegateModelOverrideStore, DelegateReasoningEffort,
};

// Re-export summarizer
#[cfg(feature = "remote")]
pub use root::PeerRoute;
pub use root::{RootParts, build_root, delegation_enabled};
pub use summarizer::DelegationSummarizer;

#[cfg(test)]
mod handoff_tests;
#[cfg(test)]
mod member_tests;
#[cfg(test)]
pub(crate) mod root_tests;
#[cfg(test)]
mod startup_span_tests;

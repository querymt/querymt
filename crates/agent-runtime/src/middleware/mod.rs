// New state machine architecture
mod assembly;
pub mod driver;
pub mod error;
pub mod factory;
mod middleware_driver;
pub(crate) use middleware_driver::HookInputs;
pub mod state;

// Keep existing exports during transition
mod context;
pub mod dedup_check;
mod limits;
mod modes;
pub mod output_limit;
mod parallel_edit_guard;
mod presets;

#[doc(hidden)]
pub use assembly::build_from_entries;

// Re-export new architecture types
pub(crate) use driver::Folded;
pub use driver::{
    CompositeDriver, PreparedBatch, PreparedCallVerdict, PreparedToolCall, TurnStack,
};
pub use error::{MiddlewareError, Result};
pub use middleware_driver::{
    EndVerdict, HookCx, Middleware, ReplyVerdict, StepVerdict, Stop, StopVerdict, TurnInfo,
    TurnMiddleware,
};
pub use state::{
    AgentStats, LlmResponse, ModelProfile, PendingNotice, PreparedModelRequest, ToolCall,
    ToolFunction, ToolResult, WaitCondition, WaitReason, apply_usage_to_stats,
    calculate_context_tokens,
};

// Re-export model info types for convenience
pub use querymt_agent_services::model_info::{CapabilityError, ModelInfoSource};

// Re-export middleware implementations (will be converted to new system)
pub use context::{ContextConfig, ContextMiddleware};
pub use dedup_check::{
    DedupCheckFactory, DedupCheckMiddleware, DuplicateWarning, FunctionLocation, SimilarMatch,
};
pub use factory::{MIDDLEWARE_REGISTRY, MiddlewareFactory, MiddlewareRegistry, enabled_flag};
pub use limits::{LimitsConfig, LimitsMiddleware};
pub use modes::AgentModeMiddleware;
pub use output_limit::{
    OutputLimitConfig, OutputLimitRecoveryFactory, OutputLimitRecoveryMiddleware,
};
pub use parallel_edit_guard::{ParallelEditGuard, ParallelEditGuardFactory, ParallelEditMode};
pub use presets::MiddlewarePresets;

#[cfg(test)]
mod context_warning_tests;
#[cfg(test)]
mod dedup_check_tests;
#[cfg(test)]
mod driver_tests;
#[cfg(test)]
mod flow_tests;
#[cfg(test)]
mod run_state_tests;
#[cfg(test)]
mod state_tests;

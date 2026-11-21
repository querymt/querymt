// New state machine architecture
mod assembly;
pub mod driver;
pub mod error;
pub mod factory;
mod middleware_driver;
pub mod state;

// Keep existing exports during transition
mod context;
pub mod dedup_check;
pub mod delegation_guard;
mod limits;
mod modes;
pub mod output_limit;
mod parallel_edit_guard;
mod presets;
mod specialized;
mod tasks;

#[doc(hidden)]
pub use assembly::build_from_entries;

// Re-export new architecture types
pub use driver::{
    CompositeDriver, MiddlewareDriver, PreparedBatch, PreparedCallVerdict, PreparedToolCall,
};
pub use error::{MiddlewareError, Result};
pub use state::{
    AgentStats, ConversationContext, ExecutionState, LlmResponse, ModelProfile, PendingNotice,
    PreparedModelRequest, ToolCall, ToolFunction, ToolResult, WaitCondition, WaitReason,
    apply_usage_to_stats, calculate_context_tokens,
};

// Re-export model info types for convenience
pub use querymt_agent_services::model_info::{CapabilityError, ModelInfoSource};

// Re-export middleware implementations (will be converted to new system)
pub use context::{ContextConfig, ContextMiddleware};
pub use dedup_check::{
    DedupCheckFactory, DedupCheckMiddleware, DuplicateWarning, FunctionLocation, SimilarMatch,
};
pub use delegation_guard::DelegationGuardMiddleware;
pub use factory::{MIDDLEWARE_REGISTRY, MiddlewareFactory, MiddlewareRegistry, enabled_flag};
pub use limits::{
    LimitsConfig, LimitsMiddleware, MaxStepsMiddleware, PriceLimitMiddleware, TurnLimitMiddleware,
};
pub use output_limit::{
    OutputLimitConfig, OutputLimitRecoveryFactory, OutputLimitRecoveryMiddleware,
};
pub use parallel_edit_guard::{ParallelEditGuard, ParallelEditGuardFactory, ParallelEditMode};
pub use presets::MiddlewarePresets;
pub use specialized::{AgentModeMiddleware, DuplicateToolCallMiddleware};

#[cfg(test)]
mod driver_tests;
#[cfg(test)]
mod flow_tests;
#[cfg(test)]
mod state_tests;

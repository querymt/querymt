// New state machine architecture
pub mod driver;
pub mod error;
pub mod factory;
pub mod state;

// Keep existing exports during transition
mod context;
pub mod dedup_check;
mod delegation;
pub mod delegation_guard;
mod limits;
mod modes;
mod presets;
mod specialized;
mod tasks;

// Re-export new architecture types
pub use driver::{CompositeDriver, MiddlewareDriver};
pub use error::{MiddlewareError, Result};
pub use state::{
    AgentStats, ConversationContext, ExecutionState, LlmResponse, ToolCall, ToolFunction,
    ToolResult, WaitCondition, WaitReason, calculate_context_tokens,
};

// Re-export model info types for convenience
pub use crate::model_info::{CapabilityError, ModelInfoSource};

// Re-export middleware implementations (will be converted to new system)
pub use context::{
    AutoCompactMiddleware, ContextConfig, ContextMiddleware, ContextWarningMiddleware,
};
pub use dedup_check::{
    DedupCheckFactory, DedupCheckMiddleware, DuplicateWarning, FunctionLocation, SimilarMatch,
};
pub use delegation::{DelegationConfig, DelegationContextMiddleware, DelegationMiddleware};
pub use delegation_guard::DelegationGuardMiddleware;
pub use factory::{MIDDLEWARE_REGISTRY, MiddlewareFactory, MiddlewareRegistry};
pub use limits::{
    LimitsConfig, LimitsMiddleware, MaxStepsMiddleware, PriceLimitMiddleware, TurnLimitMiddleware,
};
pub use presets::MiddlewarePresets;
pub use specialized::{
    DuplicateToolCallMiddleware, PlanModeMiddleware, TaskAutoCompletionMiddleware,
};

#[cfg(test)]
mod driver_tests;
#[cfg(test)]
mod flow_tests;
#[cfg(test)]
mod state_tests;

pub mod config;
pub mod engine;
pub mod output_parser;
pub mod runner;
pub mod schema;

#[cfg(test)]
mod tests;

pub use config::{
    HookCommandConfig, HookEventConfig, HookHandlerConfig, HooksConfig, MatcherGroupConfig,
};
pub use engine::{
    HookNotice, Hooks, PermissionRequestDecision, PermissionRequestRequest,
    PermissionRequestResult, PostToolUseRequest, PostToolUseResult, PreToolUseRequest,
    PreToolUseResult, SessionStartRequest, SessionStartResult, StopRequest, StopResult,
    UserPromptSubmitRequest, UserPromptSubmitResult,
};

pub fn permission_mode_label(mode: crate::agent::core::AgentMode) -> &'static str {
    match mode {
        crate::agent::core::AgentMode::Build => "default",
        crate::agent::core::AgentMode::Plan => "plan",
        crate::agent::core::AgentMode::Review => "accept_edits",
    }
}

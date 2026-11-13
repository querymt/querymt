//! Typed errors for hook configuration and execution.

use thiserror::Error;

/// A hook configuration that cannot be used. `event` is the hook event label.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HookConfigError {
    #[error("unsupported hook event(s): {0}")]
    UnsupportedEvents(String),
    #[error("{event} hook id must not be empty")]
    EmptyId { event: &'static str },
    #[error("{event} hook additional_context_limit must be greater than zero")]
    ZeroContextLimit { event: &'static str },
    #[error("{event} matcher group {group} has an invalid matcher regex `{pattern}`: {source}")]
    InvalidMatcher {
        event: &'static str,
        group: usize,
        pattern: String,
        #[source]
        source: regex::Error,
    },
    #[error("{event} matcher group {group} must contain at least one hook")]
    EmptyGroup { event: &'static str, group: usize },
    #[error("{event} hook command must not be empty")]
    EmptyCommand { event: &'static str },
    #[error("{event} does not have execution-scoped MCP access")]
    McpUnsupported { event: &'static str },
    #[error("{event} MCP hook server and tool must not be empty")]
    EmptyMcpTarget { event: &'static str },
}

/// A hook handler that could not be run or that reported failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HookError {
    #[error(transparent)]
    Config(#[from] HookConfigError),
    /// The hook command could not be started or its input could not be written.
    #[error("failed to run hook command: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("hook timed out after {secs}s")]
    Timeout { secs: u64 },
    /// The hook input could not be serialized.
    #[error("failed to serialize hook input: {0}")]
    Input(#[from] serde_json::Error),
    #[error("{event} hook command '{handler}' failed with exit code {code}: {stderr}")]
    Failed {
        event: &'static str,
        handler: String,
        code: i32,
        stderr: String,
    },
    #[error("{event} hook command '{handler}' terminated without an exit code: {stderr}")]
    Terminated {
        event: &'static str,
        handler: String,
        stderr: String,
    },
    #[error("MCP hook '{server}.{tool}' has no execution-scoped MCP state")]
    McpStateMissing { server: String, tool: String },
    #[error("MCP hook tool '{server}.{tool}' is not connected")]
    McpNotConnected { server: String, tool: String },
    #[error("MCP hook timed out")]
    McpTimeout,
    /// The MCP tool call or its input template failed.
    #[error("MCP hook failed: {0}")]
    Mcp(#[source] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{HookEventConfig, HooksConfig};

    #[test]
    fn invalid_matcher_keeps_its_regex_source() {
        let config: HooksConfig = serde_json::from_value(serde_json::json!({
            "pre_tool_use": [{"matcher": "(", "hooks": [{"type": "command", "command": "true"}]}]
        }))
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, HookConfigError::InvalidMatcher { event, group: 0, .. } if event == HookEventConfig::PreToolUse.label()),
            "{err:?}"
        );
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn empty_group_is_a_typed_config_error() {
        let config: HooksConfig = serde_json::from_value(serde_json::json!({
            "stop": [{"hooks": []}]
        }))
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(HookConfigError::EmptyGroup { group: 0, .. })
        ));
    }
}

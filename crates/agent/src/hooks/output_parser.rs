use crate::hooks::schema::{
    BlockDecisionWire, DelegationFailureCommandOutputWire, DelegationStartCommandOutputWire,
    PermissionRequestBehaviorWire, PermissionRequestCommandOutputWire,
    PostCompactionCommandOutputWire, PostDelegationCommandOutputWire, PostToolUseCommandOutputWire,
    PreCompactionCommandOutputWire, PreDelegationCommandOutputWire, PreToolUseCommandOutputWire,
    PreToolUseDecisionWire, PreToolUsePermissionDecisionWire, SessionStartCommandOutputWire,
    StopCommandOutputWire, UpdatedDelegationWire, UserPromptSubmitCommandOutputWire,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedDecision {
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedPermissionDecision {
    Allow,
    Deny { message: String },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedUpdatedDelegation {
    pub target_agent_id: Option<String>,
    pub objective: Option<String>,
    pub context: Option<String>,
    pub constraints: Option<String>,
    pub expected_output: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedUserPromptSubmit {
    pub decision: Option<ParsedDecision>,
    pub reason: Option<String>,
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPreToolUse {
    pub decision: Option<ParsedDecision>,
    pub reason: Option<String>,
    pub permission_decision: Option<ParsedPermissionDecision>,
    pub updated_input: Option<Value>,
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPermissionRequest {
    pub decision: Option<ParsedPermissionDecision>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPostToolUse {
    pub decision: Option<ParsedDecision>,
    pub reason: Option<String>,
    pub continue_processing: bool,
    pub stop_reason: Option<String>,
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSessionStart {
    pub continue_processing: bool,
    pub stop_reason: Option<String>,
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedStop {
    pub decision: Option<ParsedDecision>,
    pub reason: Option<String>,
    pub continue_processing: bool,
    pub stop_reason: Option<String>,
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPreCompaction {
    pub decision: Option<ParsedDecision>,
    pub reason: Option<String>,
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPostCompaction {
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPreDelegation {
    pub decision: Option<ParsedDecision>,
    pub reason: Option<String>,
    pub updated_delegation: Option<ParsedUpdatedDelegation>,
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDelegationLifecycle {
    pub continue_processing: bool,
    pub stop_reason: Option<String>,
    pub additional_context: Option<String>,
}

pub fn looks_like_json(stdout: &str) -> bool {
    let trimmed = stdout.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

pub fn parse_user_prompt_submit(stdout: &str) -> anyhow::Result<Option<ParsedUserPromptSubmit>> {
    let Some(output) = parse_json::<UserPromptSubmitCommandOutputWire>(stdout)? else {
        return Ok(None);
    };
    Ok(Some(ParsedUserPromptSubmit {
        decision: output.decision.map(|_| ParsedDecision::Block),
        reason: output.reason,
        additional_context: output
            .hook_specific_output
            .and_then(|hook| hook.additional_context),
    }))
}

pub fn parse_pre_tool_use(stdout: &str) -> anyhow::Result<Option<ParsedPreToolUse>> {
    let Some(output) = parse_json::<PreToolUseCommandOutputWire>(stdout)? else {
        return Ok(None);
    };

    let hook_specific = output.hook_specific_output;
    let permission_decision = hook_specific
        .as_ref()
        .and_then(|hook| hook.permission_decision.as_ref())
        .map(|decision| match decision {
            PreToolUsePermissionDecisionWire::Allow => ParsedPermissionDecision::Allow,
            PreToolUsePermissionDecisionWire::Deny => ParsedPermissionDecision::Deny {
                message: hook_specific
                    .as_ref()
                    .and_then(|hook| hook.permission_decision_reason.clone())
                    .unwrap_or_else(|| "blocked by hook".to_string()),
            },
            PreToolUsePermissionDecisionWire::Ask => ParsedPermissionDecision::Deny {
                message: hook_specific
                    .as_ref()
                    .and_then(|hook| hook.permission_decision_reason.clone())
                    .unwrap_or_else(|| "hook requested confirmation".to_string()),
            },
        });

    let decision = match output.decision {
        Some(PreToolUseDecisionWire::Block) => Some(ParsedDecision::Block),
        Some(PreToolUseDecisionWire::Approve) | None => None,
    };

    Ok(Some(ParsedPreToolUse {
        decision,
        reason: output.reason.or_else(|| {
            hook_specific
                .as_ref()
                .and_then(|hook| hook.permission_decision_reason.clone())
        }),
        permission_decision,
        updated_input: hook_specific
            .as_ref()
            .and_then(|hook| hook.updated_input.clone()),
        additional_context: hook_specific.and_then(|hook| hook.additional_context),
    }))
}

pub fn parse_permission_request(stdout: &str) -> anyhow::Result<Option<ParsedPermissionRequest>> {
    let Some(output) = parse_json::<PermissionRequestCommandOutputWire>(stdout)? else {
        return Ok(None);
    };
    let decision = output
        .hook_specific_output
        .and_then(|hook| hook.decision)
        .map(|decision| match decision.behavior {
            PermissionRequestBehaviorWire::Allow => ParsedPermissionDecision::Allow,
            PermissionRequestBehaviorWire::Deny => ParsedPermissionDecision::Deny {
                message: decision
                    .message
                    .unwrap_or_else(|| "blocked by hook".to_string()),
            },
        });
    Ok(Some(ParsedPermissionRequest { decision }))
}

pub fn parse_post_tool_use(stdout: &str) -> anyhow::Result<Option<ParsedPostToolUse>> {
    let Some(output) = parse_json::<PostToolUseCommandOutputWire>(stdout)? else {
        return Ok(None);
    };
    Ok(Some(ParsedPostToolUse {
        decision: output
            .decision
            .map(|BlockDecisionWire::Block| ParsedDecision::Block),
        reason: output.reason,
        continue_processing: output.universal.r#continue,
        stop_reason: output.universal.stop_reason,
        additional_context: output
            .hook_specific_output
            .and_then(|hook| hook.additional_context),
    }))
}

pub fn parse_session_start(stdout: &str) -> anyhow::Result<Option<ParsedSessionStart>> {
    let Some(output) = parse_json::<SessionStartCommandOutputWire>(stdout)? else {
        return Ok(None);
    };
    Ok(Some(ParsedSessionStart {
        continue_processing: output.universal.r#continue,
        stop_reason: output.universal.stop_reason,
        additional_context: output
            .hook_specific_output
            .and_then(|hook| hook.additional_context),
    }))
}

pub fn parse_stop(stdout: &str) -> anyhow::Result<Option<ParsedStop>> {
    let Some(output) = parse_json::<StopCommandOutputWire>(stdout)? else {
        return Ok(None);
    };
    Ok(Some(ParsedStop {
        decision: output
            .decision
            .map(|BlockDecisionWire::Block| ParsedDecision::Block),
        reason: output.reason,
        continue_processing: output.universal.r#continue,
        stop_reason: output.universal.stop_reason,
        additional_context: output
            .hook_specific_output
            .and_then(|hook| hook.additional_context),
    }))
}

pub fn parse_pre_compaction(stdout: &str) -> anyhow::Result<Option<ParsedPreCompaction>> {
    let Some(output) = parse_json::<PreCompactionCommandOutputWire>(stdout)? else {
        return Ok(None);
    };
    Ok(Some(ParsedPreCompaction {
        decision: output
            .decision
            .map(|BlockDecisionWire::Block| ParsedDecision::Block),
        reason: output.reason,
        additional_context: output
            .hook_specific_output
            .and_then(|hook| hook.additional_context),
    }))
}

pub fn parse_post_compaction(stdout: &str) -> anyhow::Result<Option<ParsedPostCompaction>> {
    let Some(output) = parse_json::<PostCompactionCommandOutputWire>(stdout)? else {
        return Ok(None);
    };
    Ok(Some(ParsedPostCompaction {
        additional_context: output
            .hook_specific_output
            .and_then(|hook| hook.additional_context),
    }))
}

pub fn parse_pre_delegation(stdout: &str) -> anyhow::Result<Option<ParsedPreDelegation>> {
    let Some(output) = parse_json::<PreDelegationCommandOutputWire>(stdout)? else {
        return Ok(None);
    };
    Ok(Some(ParsedPreDelegation {
        decision: output
            .decision
            .map(|BlockDecisionWire::Block| ParsedDecision::Block),
        reason: output.reason,
        updated_delegation: output
            .hook_specific_output
            .as_ref()
            .and_then(|hook| hook.updated_delegation.as_ref())
            .map(parsed_updated_delegation),
        additional_context: output
            .hook_specific_output
            .and_then(|hook| hook.additional_context),
    }))
}

pub fn parse_delegation_start(stdout: &str) -> anyhow::Result<Option<ParsedDelegationLifecycle>> {
    parse_delegation_lifecycle::<DelegationStartCommandOutputWire>(stdout)
}

pub fn parse_post_delegation(stdout: &str) -> anyhow::Result<Option<ParsedDelegationLifecycle>> {
    parse_delegation_lifecycle::<PostDelegationCommandOutputWire>(stdout)
}

pub fn parse_delegation_failure(stdout: &str) -> anyhow::Result<Option<ParsedDelegationLifecycle>> {
    parse_delegation_lifecycle::<DelegationFailureCommandOutputWire>(stdout)
}

fn parse_delegation_lifecycle<T>(stdout: &str) -> anyhow::Result<Option<ParsedDelegationLifecycle>>
where
    T: DeserializeOwned + DelegationLifecycleOutput,
{
    let Some(output) = parse_json::<T>(stdout)? else {
        return Ok(None);
    };
    Ok(Some(ParsedDelegationLifecycle {
        continue_processing: output.universal_continue(),
        stop_reason: output.universal_stop_reason(),
        additional_context: output.additional_context(),
    }))
}

trait DelegationLifecycleOutput {
    fn universal_continue(&self) -> bool;
    fn universal_stop_reason(&self) -> Option<String>;
    fn additional_context(&self) -> Option<String>;
}

impl DelegationLifecycleOutput for DelegationStartCommandOutputWire {
    fn universal_continue(&self) -> bool {
        self.universal.r#continue
    }

    fn universal_stop_reason(&self) -> Option<String> {
        self.universal.stop_reason.clone()
    }

    fn additional_context(&self) -> Option<String> {
        self.hook_specific_output
            .as_ref()
            .and_then(|hook| hook.additional_context.clone())
    }
}

impl DelegationLifecycleOutput for PostDelegationCommandOutputWire {
    fn universal_continue(&self) -> bool {
        self.universal.r#continue
    }

    fn universal_stop_reason(&self) -> Option<String> {
        self.universal.stop_reason.clone()
    }

    fn additional_context(&self) -> Option<String> {
        self.hook_specific_output
            .as_ref()
            .and_then(|hook| hook.additional_context.clone())
    }
}

impl DelegationLifecycleOutput for DelegationFailureCommandOutputWire {
    fn universal_continue(&self) -> bool {
        self.universal.r#continue
    }

    fn universal_stop_reason(&self) -> Option<String> {
        self.universal.stop_reason.clone()
    }

    fn additional_context(&self) -> Option<String> {
        self.hook_specific_output
            .as_ref()
            .and_then(|hook| hook.additional_context.clone())
    }
}

fn parsed_updated_delegation(wire: &UpdatedDelegationWire) -> ParsedUpdatedDelegation {
    ParsedUpdatedDelegation {
        target_agent_id: wire.target_agent_id.clone(),
        objective: wire.objective.clone(),
        context: wire.context.clone(),
        constraints: wire.constraints.clone(),
        expected_output: wire.expected_output.clone(),
    }
}

fn parse_json<T: DeserializeOwned>(stdout: &str) -> anyhow::Result<Option<T>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if !looks_like_json(trimmed) {
        anyhow::bail!(
            "hook output must be JSON; got: {}",
            summarize_output(trimmed)
        );
    }
    serde_json::from_str(trimmed).map(Some).map_err(|err| {
        anyhow::anyhow!(
            "invalid hook JSON output: {}; stdout: {}",
            err,
            summarize_output(trimmed)
        )
    })
}

fn summarize_output(output: &str) -> String {
    const MAX_CHARS: usize = 160;
    let compact = output.replace('\n', "\\n");
    if compact.chars().count() <= MAX_CHARS {
        compact
    } else {
        let truncated: String = compact.chars().take(MAX_CHARS).collect();
        format!("{}...", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_pre_tool_use_supports_block_and_rewrite() {
        let parsed = parse_pre_tool_use(
            &json!({
                "continue": true,
                "decision": "block",
                "reason": "unsafe",
                "hook_specific_output": {
                    "hook_event_name": "pre_tool_use",
                    "updated_input": {"command": "echo safe"}
                }
            })
            .to_string(),
        )
        .expect("parse")
        .expect("value");

        assert_eq!(parsed.decision, Some(ParsedDecision::Block));
        assert_eq!(parsed.reason.as_deref(), Some("unsafe"));
        assert_eq!(parsed.updated_input, Some(json!({"command": "echo safe"})));
    }

    #[test]
    fn parse_pre_delegation_supports_rewrite() {
        let parsed = parse_pre_delegation(
            &json!({
                "continue": true,
                "decision": "block",
                "reason": "route elsewhere",
                "hook_specific_output": {
                    "hook_event_name": "pre_delegation",
                    "updated_delegation": {
                        "target_agent_id": "coder",
                        "objective": "refined objective"
                    },
                    "additional_context": "remember tests"
                }
            })
            .to_string(),
        )
        .expect("parse")
        .expect("value");

        assert_eq!(parsed.decision, Some(ParsedDecision::Block));
        assert_eq!(
            parsed.updated_delegation,
            Some(ParsedUpdatedDelegation {
                target_agent_id: Some("coder".to_string()),
                objective: Some("refined objective".to_string()),
                context: None,
                constraints: None,
                expected_output: None,
            })
        );
        assert_eq!(parsed.additional_context.as_deref(), Some("remember tests"));
    }

    #[test]
    fn parse_pre_compaction_supports_block() {
        let parsed = parse_pre_compaction(
            &json!({
                "continue": true,
                "decision": "block",
                "reason": "too risky",
                "hook_specific_output": {
                    "hook_event_name": "pre_compaction",
                    "additional_context": "wait for confirmation"
                }
            })
            .to_string(),
        )
        .expect("parse")
        .expect("value");

        assert_eq!(parsed.decision, Some(ParsedDecision::Block));
        assert_eq!(parsed.reason.as_deref(), Some("too risky"));
        assert_eq!(
            parsed.additional_context.as_deref(),
            Some("wait for confirmation")
        );
    }

    #[test]
    fn parse_permission_request_supports_allow_and_deny() {
        let allow = parse_permission_request(
            &json!({
                "continue": true,
                "hook_specific_output": {
                    "hook_event_name": "permission_request",
                    "decision": {"behavior": "allow"}
                }
            })
            .to_string(),
        )
        .expect("parse allow")
        .expect("allow value");
        assert_eq!(allow.decision, Some(ParsedPermissionDecision::Allow));

        let deny = parse_permission_request(
            &json!({
                "continue": true,
                "hook_specific_output": {
                    "hook_event_name": "permission_request",
                    "decision": {"behavior": "deny", "message": "nope"}
                }
            })
            .to_string(),
        )
        .expect("parse deny")
        .expect("deny value");
        assert_eq!(
            deny.decision,
            Some(ParsedPermissionDecision::Deny {
                message: "nope".to_string()
            })
        );
    }

    #[test]
    fn parse_stop_supports_continue_override() {
        let parsed = parse_stop(
            &json!({
                "continue": false,
                "reason": "keep going",
                "hook_specific_output": {
                    "hook_event_name": "stop",
                    "additional_context": "follow-up"
                }
            })
            .to_string(),
        )
        .expect("parse stop")
        .expect("stop value");

        assert!(!parsed.continue_processing);
        assert_eq!(parsed.reason.as_deref(), Some("keep going"));
        assert_eq!(parsed.additional_context.as_deref(), Some("follow-up"));
    }

    #[test]
    fn parse_post_delegation_supports_additional_context() {
        let parsed = parse_post_delegation(
            &json!({
                "continue": true,
                "hook_specific_output": {
                    "hook_event_name": "post_delegation",
                    "additional_context": "Include follow-up checks"
                }
            })
            .to_string(),
        )
        .expect("parse post delegation")
        .expect("value");

        assert!(parsed.continue_processing);
        assert_eq!(
            parsed.additional_context.as_deref(),
            Some("Include follow-up checks")
        );
    }

    #[test]
    fn parse_json_reports_helpful_error_for_non_json_output() {
        let err = parse_pre_tool_use("blocked by script").expect_err("non-json should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("hook output must be JSON"),
            "unexpected: {msg}"
        );
        assert!(msg.contains("blocked by script"), "unexpected: {msg}");
    }

    #[test]
    fn parse_json_reports_helpful_error_for_invalid_json_output() {
        let err = parse_pre_tool_use("{not valid json").expect_err("invalid json should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid hook JSON output"),
            "unexpected: {msg}"
        );
        assert!(msg.contains("stdout:"), "unexpected: {msg}");
    }
}

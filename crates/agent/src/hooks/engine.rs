use crate::hooks::config::{HookCommandConfig, HookEventConfig, HookHandlerConfig, HooksConfig};
use crate::hooks::output_parser::{
    ParsedDecision, ParsedPermissionDecision, parse_delegation_failure, parse_delegation_start,
    parse_permission_request, parse_post_compaction, parse_post_delegation, parse_post_tool_use,
    parse_pre_compaction, parse_pre_delegation, parse_pre_tool_use, parse_session_start,
    parse_stop, parse_user_prompt_submit,
};
use crate::hooks::runner::{CommandHookSpec, run_command_hook};
use crate::hooks::schema::{
    DelegationFailureCommandInput, DelegationStartCommandInput, NullableString,
    PermissionRequestCommandInput, PostCompactionCommandInput, PostDelegationCommandInput,
    PostToolUseCommandInput, PreCompactionCommandInput, PreDelegationCommandInput,
    PreToolUseCommandInput, SessionStartCommandInput, StopCommandInput,
    UserPromptSubmitCommandInput,
};
use log::warn;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct Hooks {
    config: HooksConfig,
}

#[derive(Debug, Clone)]
pub struct HookNotice {
    pub event_name: String,
    pub message: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionRequestDecision {
    Allow,
    Deny { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdatedDelegation {
    pub target_agent_id: Option<String>,
    pub objective: Option<String>,
    pub context: Option<String>,
    pub constraints: Option<String>,
    pub expected_output: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionStartRequest {
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub source: String,
}

#[derive(Debug, Clone, Default)]
pub struct SessionStartResult {
    pub additional_contexts: Vec<String>,
    pub stop_reason: Option<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct UserPromptSubmitRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Default)]
pub struct UserPromptSubmitResult {
    pub should_block: bool,
    pub block_reason: Option<String>,
    pub additional_contexts: Vec<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct PreToolUseRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct PreToolUseResult {
    pub should_block: bool,
    pub block_reason: Option<String>,
    pub updated_input: Option<Value>,
    pub additional_contexts: Vec<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct PermissionRequestRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Value,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionRequestResult {
    pub decision: Option<PermissionRequestDecision>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct PostToolUseRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_response: Value,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct PostToolUseResult {
    pub should_block: bool,
    pub block_reason: Option<String>,
    pub stop_reason: Option<String>,
    pub additional_contexts: Vec<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct StopRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub stop_reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct StopResult {
    pub should_continue: bool,
    pub block_reason: Option<String>,
    pub stop_reason: Option<String>,
    pub additional_contexts: Vec<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct PreCompactionRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub trigger: String,
    pub token_estimate: u32,
    pub message_count: u32,
}

#[derive(Debug, Clone, Default)]
pub struct PreCompactionResult {
    pub should_block: bool,
    pub block_reason: Option<String>,
    pub additional_contexts: Vec<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct PostCompactionRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub trigger: String,
    pub summary: String,
    pub original_token_count: u32,
    pub summary_token_count: u32,
    pub message_count: u32,
}

#[derive(Debug, Clone, Default)]
pub struct PostCompactionResult {
    pub additional_contexts: Vec<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct PreDelegationRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub tool_use_id: String,
    pub target_agent_id: String,
    pub objective: String,
    pub context: Option<String>,
    pub constraints: Option<String>,
    pub expected_output: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PreDelegationResult {
    pub should_block: bool,
    pub block_reason: Option<String>,
    pub updated_delegation: Option<UpdatedDelegation>,
    pub additional_contexts: Vec<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct DelegationStartRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub delegation_id: String,
    pub target_agent_id: String,
    pub objective: String,
    pub context: Option<String>,
    pub constraints: Option<String>,
    pub expected_output: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Default)]
pub struct DelegationStartResult {
    pub additional_contexts: Vec<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct PostDelegationRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub delegation_id: String,
    pub target_agent_id: String,
    pub child_session_id: String,
    pub objective: String,
    pub status: String,
    pub summary: String,
    pub verification_passed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PostDelegationResult {
    pub additional_contexts: Vec<String>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone)]
pub struct DelegationFailureRequest {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub delegation_id: String,
    pub target_agent_id: String,
    pub objective: String,
    pub status: String,
    pub error: String,
    pub error_type: String,
}

#[derive(Debug, Clone, Default)]
pub struct DelegationFailureResult {
    pub additional_contexts: Vec<String>,
    pub notices: Vec<HookNotice>,
}

impl Hooks {
    pub fn new(config: HooksConfig) -> anyhow::Result<Self> {
        config.validate()?;
        Ok(Self { config })
    }

    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub async fn run_session_start(
        &self,
        request: SessionStartRequest,
    ) -> anyhow::Result<SessionStartResult> {
        if !self.is_enabled() {
            return Ok(SessionStartResult::default());
        }
        let input = SessionStartCommandInput {
            session_id: request.session_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::SessionStart.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            source: request.source,
        };

        let mut result = SessionStartResult::default();
        for stdout in self
            .run_event(
                HookEventConfig::SessionStart,
                None,
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            match parse_session_start(&stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = parsed.additional_context {
                        result.additional_contexts.push(context);
                    }
                    if !parsed.continue_processing && result.stop_reason.is_none() {
                        result.stop_reason = parsed
                            .stop_reason
                            .or_else(|| Some("session start blocked by hook".to_string()));
                    }
                }
                Ok(None) => {}
                Err(err) => result
                    .notices
                    .push(invalid_notice(HookEventConfig::SessionStart, err)),
            }
        }
        Ok(result)
    }

    pub async fn run_user_prompt_submit(
        &self,
        request: UserPromptSubmitRequest,
    ) -> anyhow::Result<UserPromptSubmitResult> {
        if !self.is_enabled() {
            return Ok(UserPromptSubmitResult::default());
        }
        let input = UserPromptSubmitCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::UserPromptSubmit.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            prompt: request.prompt,
        };

        let mut result = UserPromptSubmitResult::default();
        for stdout in self
            .run_event(
                HookEventConfig::UserPromptSubmit,
                None,
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            match parse_user_prompt_submit(&stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = parsed.additional_context {
                        result.additional_contexts.push(context);
                    }
                    if matches!(parsed.decision, Some(ParsedDecision::Block)) {
                        result.should_block = true;
                        if result.block_reason.is_none() {
                            result.block_reason = parsed
                                .reason
                                .or_else(|| Some("blocked by hook".to_string()));
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    result
                        .notices
                        .push(invalid_notice(HookEventConfig::UserPromptSubmit, err));
                }
            }
        }
        Ok(result)
    }

    pub async fn run_pre_tool_use(
        &self,
        request: PreToolUseRequest,
    ) -> anyhow::Result<PreToolUseResult> {
        if !self.is_enabled() {
            return Ok(PreToolUseResult::default());
        }
        let input = PreToolUseCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::PreToolUse.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            tool_name: request.tool_name.clone(),
            tool_input: request.tool_input.clone(),
            tool_use_id: request.tool_use_id,
        };

        let mut result = PreToolUseResult::default();
        for stdout in self
            .run_event(
                HookEventConfig::PreToolUse,
                Some(&request.tool_name),
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            match parse_pre_tool_use(&stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = parsed.additional_context {
                        result.additional_contexts.push(context);
                    }
                    if let Some(updated_input) = parsed.updated_input {
                        result.updated_input = Some(updated_input);
                    }
                    match parsed.permission_decision {
                        Some(ParsedPermissionDecision::Deny { message }) => {
                            result.should_block = true;
                            if result.block_reason.is_none() {
                                result.block_reason = Some(message);
                            }
                        }
                        Some(ParsedPermissionDecision::Allow) | None => {}
                    }
                    if matches!(parsed.decision, Some(ParsedDecision::Block)) {
                        result.should_block = true;
                        if result.block_reason.is_none() {
                            result.block_reason = parsed
                                .reason
                                .or_else(|| Some("blocked by hook".to_string()));
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => result
                    .notices
                    .push(invalid_notice(HookEventConfig::PreToolUse, err)),
            }
        }
        Ok(result)
    }

    pub async fn run_permission_request(
        &self,
        request: PermissionRequestRequest,
    ) -> anyhow::Result<PermissionRequestResult> {
        if !self.is_enabled() {
            return Ok(PermissionRequestResult::default());
        }
        let input = PermissionRequestCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::PermissionRequest.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            tool_name: request.tool_name.clone(),
            tool_input: request.tool_input,
        };

        let mut result = PermissionRequestResult::default();
        for stdout in self
            .run_event(
                HookEventConfig::PermissionRequest,
                Some(&request.tool_name),
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            match parse_permission_request(&stdout) {
                Ok(Some(parsed)) => match parsed.decision {
                    Some(ParsedPermissionDecision::Deny { message }) => {
                        result.decision = Some(PermissionRequestDecision::Deny { message });
                        return Ok(result);
                    }
                    Some(ParsedPermissionDecision::Allow) => {
                        result.decision = Some(PermissionRequestDecision::Allow);
                    }
                    None => {}
                },
                Ok(None) => {}
                Err(err) => {
                    result
                        .notices
                        .push(invalid_notice(HookEventConfig::PermissionRequest, err));
                }
            }
        }
        Ok(result)
    }

    pub async fn run_post_tool_use(
        &self,
        request: PostToolUseRequest,
    ) -> anyhow::Result<PostToolUseResult> {
        if !self.is_enabled() {
            return Ok(PostToolUseResult::default());
        }
        let input = PostToolUseCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::PostToolUse.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            tool_name: request.tool_name.clone(),
            tool_input: request.tool_input,
            tool_response: request.tool_response,
            tool_use_id: request.tool_use_id,
        };

        let mut result = PostToolUseResult::default();
        for stdout in self
            .run_event(
                HookEventConfig::PostToolUse,
                Some(&request.tool_name),
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            match parse_post_tool_use(&stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = parsed.additional_context {
                        result.additional_contexts.push(context);
                    }
                    if matches!(parsed.decision, Some(ParsedDecision::Block)) {
                        result.should_block = true;
                        if result.block_reason.is_none() {
                            result.block_reason = parsed
                                .reason
                                .clone()
                                .or_else(|| Some("blocked by hook".to_string()));
                        }
                    }
                    if !parsed.continue_processing && result.stop_reason.is_none() {
                        result.stop_reason = parsed.stop_reason.or(parsed.reason);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    result
                        .notices
                        .push(invalid_notice(HookEventConfig::PostToolUse, err));
                }
            }
        }
        Ok(result)
    }

    pub async fn run_stop(&self, request: StopRequest) -> anyhow::Result<StopResult> {
        if !self.is_enabled() {
            return Ok(StopResult::default());
        }
        let input = StopCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::Stop.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            stop_reason: request.stop_reason,
        };

        let mut result = StopResult::default();
        for stdout in self
            .run_event(
                HookEventConfig::Stop,
                None,
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            match parse_stop(&stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = parsed.additional_context {
                        result.additional_contexts.push(context);
                    }
                    if matches!(parsed.decision, Some(ParsedDecision::Block)) {
                        result.should_continue = true;
                        if result.block_reason.is_none() {
                            result.block_reason = parsed
                                .reason
                                .clone()
                                .or_else(|| Some("continue requested by stop hook".to_string()));
                        }
                    }
                    if !parsed.continue_processing {
                        result.should_continue = true;
                        if result.stop_reason.is_none() {
                            result.stop_reason = parsed.stop_reason.or(parsed.reason);
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => result
                    .notices
                    .push(invalid_notice(HookEventConfig::Stop, err)),
            }
        }
        Ok(result)
    }

    pub async fn run_pre_compaction(
        &self,
        request: PreCompactionRequest,
    ) -> anyhow::Result<PreCompactionResult> {
        if !self.is_enabled() {
            return Ok(PreCompactionResult::default());
        }
        let input = PreCompactionCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::PreCompaction.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            trigger: request.trigger,
            token_estimate: request.token_estimate,
            message_count: request.message_count,
        };

        let mut result = PreCompactionResult::default();
        for stdout in self
            .run_event(
                HookEventConfig::PreCompaction,
                None,
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            match parse_pre_compaction(&stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = parsed.additional_context {
                        result.additional_contexts.push(context);
                    }
                    if matches!(parsed.decision, Some(ParsedDecision::Block)) {
                        result.should_block = true;
                        if result.block_reason.is_none() {
                            result.block_reason = parsed
                                .reason
                                .or_else(|| Some("compaction blocked by hook".to_string()));
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    result
                        .notices
                        .push(invalid_notice(HookEventConfig::PreCompaction, err));
                }
            }
        }
        Ok(result)
    }

    pub async fn run_post_compaction(
        &self,
        request: PostCompactionRequest,
    ) -> anyhow::Result<PostCompactionResult> {
        if !self.is_enabled() {
            return Ok(PostCompactionResult::default());
        }
        let input = PostCompactionCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::PostCompaction.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            trigger: request.trigger,
            summary: request.summary,
            original_token_count: request.original_token_count,
            summary_token_count: request.summary_token_count,
            message_count: request.message_count,
        };

        let mut result = PostCompactionResult::default();
        for stdout in self
            .run_event(
                HookEventConfig::PostCompaction,
                None,
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            match parse_post_compaction(&stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = parsed.additional_context {
                        result.additional_contexts.push(context);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    result
                        .notices
                        .push(invalid_notice(HookEventConfig::PostCompaction, err));
                }
            }
        }
        Ok(result)
    }

    pub async fn run_pre_delegation(
        &self,
        request: PreDelegationRequest,
    ) -> anyhow::Result<PreDelegationResult> {
        if !self.is_enabled() {
            return Ok(PreDelegationResult::default());
        }
        let input = PreDelegationCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::PreDelegation.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            tool_use_id: request.tool_use_id,
            target_agent_id: request.target_agent_id.clone(),
            objective: request.objective,
            context: NullableString::from_string(request.context),
            constraints: NullableString::from_string(request.constraints),
            expected_output: NullableString::from_string(request.expected_output),
        };

        let mut result = PreDelegationResult::default();
        for stdout in self
            .run_event(
                HookEventConfig::PreDelegation,
                Some(&request.target_agent_id),
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            match parse_pre_delegation(&stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = parsed.additional_context {
                        result.additional_contexts.push(context);
                    }
                    if let Some(updated) = parsed.updated_delegation {
                        result.updated_delegation = Some(UpdatedDelegation {
                            target_agent_id: updated.target_agent_id,
                            objective: updated.objective,
                            context: updated.context,
                            constraints: updated.constraints,
                            expected_output: updated.expected_output,
                        });
                    }
                    if matches!(parsed.decision, Some(ParsedDecision::Block)) {
                        result.should_block = true;
                        if result.block_reason.is_none() {
                            result.block_reason = parsed
                                .reason
                                .or_else(|| Some("delegation blocked by hook".to_string()));
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    result
                        .notices
                        .push(invalid_notice(HookEventConfig::PreDelegation, err));
                }
            }
        }
        Ok(result)
    }

    pub async fn run_delegation_start(
        &self,
        request: DelegationStartRequest,
    ) -> anyhow::Result<DelegationStartResult> {
        if !self.is_enabled() {
            return Ok(DelegationStartResult::default());
        }
        let input = DelegationStartCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::DelegationStart.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            delegation_id: request.delegation_id,
            target_agent_id: request.target_agent_id.clone(),
            objective: request.objective,
            context: NullableString::from_string(request.context),
            constraints: NullableString::from_string(request.constraints),
            expected_output: NullableString::from_string(request.expected_output),
            status: request.status,
        };
        self.run_observing_delegation_event(
            HookEventConfig::DelegationStart,
            &request.target_agent_id,
            input,
            request.cwd.as_deref(),
            parse_delegation_start,
        )
        .await
        .map(|out| DelegationStartResult {
            additional_contexts: out.additional_contexts,
            notices: out.notices,
        })
    }

    pub async fn run_post_delegation(
        &self,
        request: PostDelegationRequest,
    ) -> anyhow::Result<PostDelegationResult> {
        if !self.is_enabled() {
            return Ok(PostDelegationResult::default());
        }
        let input = PostDelegationCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::PostDelegation.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            delegation_id: request.delegation_id,
            target_agent_id: request.target_agent_id.clone(),
            child_session_id: request.child_session_id,
            objective: request.objective,
            status: request.status,
            summary: request.summary,
            verification_passed: request.verification_passed,
        };
        self.run_observing_delegation_event(
            HookEventConfig::PostDelegation,
            &request.target_agent_id,
            input,
            request.cwd.as_deref(),
            parse_post_delegation,
        )
        .await
        .map(|out| PostDelegationResult {
            additional_contexts: out.additional_contexts,
            notices: out.notices,
        })
    }

    pub async fn run_delegation_failure(
        &self,
        request: DelegationFailureRequest,
    ) -> anyhow::Result<DelegationFailureResult> {
        if !self.is_enabled() {
            return Ok(DelegationFailureResult::default());
        }
        let input = DelegationFailureCommandInput {
            session_id: request.session_id,
            turn_id: request.turn_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::DelegationFailure.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            delegation_id: request.delegation_id,
            target_agent_id: request.target_agent_id.clone(),
            objective: request.objective,
            status: request.status,
            error: request.error,
            error_type: request.error_type,
        };
        self.run_observing_delegation_event(
            HookEventConfig::DelegationFailure,
            &request.target_agent_id,
            input,
            request.cwd.as_deref(),
            parse_delegation_failure,
        )
        .await
        .map(|out| DelegationFailureResult {
            additional_contexts: out.additional_contexts,
            notices: out.notices,
        })
    }

    async fn run_observing_delegation_event<T, F>(
        &self,
        event: HookEventConfig,
        target_agent_id: &str,
        input: T,
        cwd: Option<&Path>,
        parser: F,
    ) -> anyhow::Result<ObservingHookResult>
    where
        T: Serialize + Clone,
        F: Fn(
            &str,
        )
            -> anyhow::Result<Option<crate::hooks::output_parser::ParsedDelegationLifecycle>>,
    {
        let mut result = ObservingHookResult::default();
        for stdout in self
            .run_event(event, Some(target_agent_id), input, cwd)
            .await?
        {
            match parser(&stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = parsed.additional_context {
                        result.additional_contexts.push(context);
                    }
                    if !parsed.continue_processing || parsed.stop_reason.is_some() {
                        result.notices.push(ignored_control_notice(event));
                    }
                }
                Ok(None) => {}
                Err(err) => result.notices.push(invalid_notice(event, err)),
            }
        }
        Ok(result)
    }

    async fn run_event<T: Serialize>(
        &self,
        event: HookEventConfig,
        matcher: Option<&str>,
        input: T,
        cwd: Option<&Path>,
    ) -> anyhow::Result<Vec<String>> {
        let mut outputs = Vec::new();
        let stdin_json = serde_json::to_string(&input)?;
        let cwd = cwd.unwrap_or_else(|| Path::new("."));

        for group in self.config.groups_for(event) {
            if !matches_group(group.matcher.as_deref(), matcher) {
                continue;
            }
            for hook in &group.hooks {
                match hook {
                    HookHandlerConfig::Command(command) => {
                        let spec = spec_from_command(command);
                        let output = run_command_hook(&spec, cwd, &stdin_json).await?;
                        match output.exit_code {
                            Some(0) | None => outputs.push(output.stdout),
                            Some(2) => outputs.push(output.stdout),
                            Some(code) => anyhow::bail!(
                                "{} hook command failed with exit code {}: {}",
                                event.label(),
                                code,
                                output.stderr.trim()
                            ),
                        }
                    }
                }
            }
        }

        Ok(outputs)
    }
}

#[derive(Default)]
struct ObservingHookResult {
    additional_contexts: Vec<String>,
    notices: Vec<HookNotice>,
}

fn spec_from_command(command: &HookCommandConfig) -> CommandHookSpec {
    CommandHookSpec {
        command: command.command.clone(),
        timeout: Duration::from_secs(command.timeout_sec.unwrap_or(30)),
        env: command.env.clone(),
    }
}

fn matches_group(matcher: Option<&str>, value: Option<&str>) -> bool {
    let Some(matcher) = matcher.filter(|matcher| !matcher.trim().is_empty()) else {
        return true;
    };
    let Some(value) = value else {
        return false;
    };
    regex::Regex::new(matcher)
        .map(|regex| regex.is_match(value))
        .unwrap_or(false)
}

fn cwd_string(cwd: Option<&Path>) -> String {
    cwd.map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".to_string())
}

fn invalid_notice(event: HookEventConfig, err: anyhow::Error) -> HookNotice {
    let message = format!("Ignoring invalid {} hook output: {}", event.label(), err);
    warn!("{}", message);
    HookNotice {
        event_name: event.label().to_string(),
        message,
        is_error: true,
    }
}

fn ignored_control_notice(event: HookEventConfig) -> HookNotice {
    HookNotice {
        event_name: event.label().to_string(),
        message: format!(
            "Ignoring control fields from {} hook output; this hook is observe-only",
            event.label()
        ),
        is_error: false,
    }
}

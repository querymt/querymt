use crate::hooks::config::{
    HookCommandConfig, HookEventConfig, HookHandlerConfig, HookMcpToolConfig, HooksConfig,
};
use crate::hooks::output_parser::{
    ParsedDecision, ParsedPermissionDecision, parse_context, parse_delegation_failure,
    parse_delegation_start, parse_permission_request, parse_post_compaction, parse_post_delegation,
    parse_post_tool_use, parse_pre_compaction, parse_pre_delegation, parse_pre_tool_use,
    parse_session_start, parse_stop, parse_user_prompt_submit,
};
use crate::hooks::runner::{CommandHookSpec, CommandOutput, run_command_hook};
use crate::hooks::schema::{
    ContextCommandInput, DelegationFailureCommandInput, DelegationStartCommandInput,
    NullableString, PermissionRequestCommandInput, PostCompactionCommandInput,
    PostDelegationCommandInput, PostToolUseCommandInput, PreCompactionCommandInput,
    PreDelegationCommandInput, PreToolUseCommandInput, SessionEndCommandInput,
    SessionEndCommandOutputWire, SessionStartCommandInput, StopCommandInput,
    StructuredToolResultWire, UserPromptSubmitCommandInput,
};
use log::warn;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct Hooks {
    config: HooksConfig,
    pending_session_context: Arc<Mutex<HashMap<String, Vec<HookContextContribution>>>>,
    pending_session_stop: Arc<Mutex<HashMap<String, String>>>,
}

#[derive(Debug, Clone)]
pub struct HookNotice {
    pub event_name: String,
    pub message: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookContextContribution {
    pub event_name: String,
    pub handler_id: String,
    pub tool_use_id: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone)]
struct ResolvedHookHandler {
    id: String,
    matcher: Option<String>,
    kind: ResolvedHookKind,
}

#[derive(Debug, Clone)]
enum ResolvedHookKind {
    Command(HookCommandConfig),
    McpTool(HookMcpToolConfig),
}

impl ResolvedHookHandler {
    fn status_message(&self) -> Option<&str> {
        match &self.kind {
            ResolvedHookKind::Command(config) => config.status_message.as_deref(),
            ResolvedHookKind::McpTool(config) => config.status_message.as_deref(),
        }
    }

    fn additional_context_limit(&self) -> Option<u32> {
        match &self.kind {
            ResolvedHookKind::Command(config) => config.additional_context_limit,
            ResolvedHookKind::McpTool(config) => config.additional_context_limit,
        }
    }
}

#[derive(Debug, Clone)]
struct HookInvocationOutcome {
    handler_id: String,
    stdout: String,
    hard_block_reason: Option<String>,
    notices: Vec<HookNotice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionRequestDecision {
    Allow,
    Deny { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreToolPermissionDecision {
    Allow,
    Ask,
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
pub struct SessionEndRequest {
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct SessionEndResult {
    pub notices: Vec<HookNotice>,
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
    pub context_contributions: Vec<HookContextContribution>,
    pub notices: Vec<HookNotice>,
}

#[derive(Clone)]
pub struct PreToolUseRequest {
    pub session_id: String,
    pub mcp_tool_state: Option<Arc<crate::agent::core::McpToolState>>,
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
    pub permission_decision: Option<PreToolPermissionDecision>,
    pub updated_input: Option<Value>,
    pub additional_contexts: Vec<String>,
    pub context_contributions: Vec<HookContextContribution>,
    pub notices: Vec<HookNotice>,
}

#[derive(Clone)]
pub struct PermissionRequestRequest {
    pub session_id: String,
    pub mcp_tool_state: Option<Arc<crate::agent::core::McpToolState>>,
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

#[derive(Clone)]
pub struct ContextHookRequest {
    pub session_id: String,
    pub mcp_tool_state: Option<Arc<crate::agent::core::McpToolState>>,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub trigger: String,
    pub context_window: u32,
    pub messages: Vec<querymt::chat::ChatMessage>,
}

#[derive(Debug, Clone, Default)]
pub struct ContextHookResult {
    pub messages: Option<Vec<querymt::chat::ChatMessage>>,
    pub estimated_tokens: usize,
    pub notices: Vec<HookNotice>,
}

#[derive(Clone)]
pub struct PostToolUseRequest {
    pub session_id: String,
    pub mcp_tool_state: Option<Arc<crate::agent::core::McpToolState>>,
    pub turn_id: String,
    pub cwd: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub content: Vec<querymt::chat::Content>,
    pub is_error: bool,
    pub execution_is_error: bool,
    pub tool_source: String,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct PostToolUseResult {
    pub content: Option<Vec<querymt::chat::Content>>,
    pub is_error: Option<bool>,
    pub should_block: bool,
    pub block_reason: Option<String>,
    pub stop_reason: Option<String>,
    pub additional_contexts: Vec<String>,
    pub context_contributions: Vec<HookContextContribution>,
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
    pub messages: Vec<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct PreCompactionResult {
    pub should_block: bool,
    pub block_reason: Option<String>,
    pub custom_summary: Option<String>,
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
        Ok(Self {
            config,
            pending_session_context: Arc::default(),
            pending_session_stop: Arc::default(),
        })
    }

    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn take_session_context(&self, session_id: &str) -> Vec<HookContextContribution> {
        self.pending_session_context
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(session_id)
            .unwrap_or_default()
    }

    pub async fn run_session_end(
        &self,
        request: SessionEndRequest,
    ) -> anyhow::Result<SessionEndResult> {
        if !self.is_enabled() {
            return Ok(SessionEndResult::default());
        }
        let input = SessionEndCommandInput {
            session_id: request.session_id,
            transcript_path: NullableString::from_string(None),
            cwd: cwd_string(request.cwd.as_deref()),
            hook_event_name: HookEventConfig::SessionEnd.label().to_string(),
            model: request.model,
            permission_mode: request.permission_mode,
            reason: request.reason,
        };
        let mut result = SessionEndResult::default();
        for outcome in self
            .run_event(
                HookEventConfig::SessionEnd,
                None,
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            result.notices.extend(outcome.notices);
            if outcome.hard_block_reason.is_some() {
                result
                    .notices
                    .push(ignored_control_notice(HookEventConfig::SessionEnd));
            }
            if !outcome.stdout.trim().is_empty()
                && let Err(error) =
                    serde_json::from_str::<SessionEndCommandOutputWire>(&outcome.stdout)
            {
                result
                    .notices
                    .push(invalid_notice(HookEventConfig::SessionEnd, error.into()));
            }
        }
        Ok(result)
    }

    pub async fn run_session_start(
        &self,
        request: SessionStartRequest,
    ) -> anyhow::Result<SessionStartResult> {
        if !self.is_enabled() {
            return Ok(SessionStartResult::default());
        }
        let session_id = request.session_id.clone();
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
        let mut contributions = Vec::new();
        for outcome in self
            .run_event(
                HookEventConfig::SessionStart,
                None,
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            result.notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                result.stop_reason.get_or_insert(reason);
                break;
            }
            match parse_session_start(&outcome.stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = limit_context(parsed.additional_context, None) {
                        result.additional_contexts.push(context.clone());
                        contributions.push(HookContextContribution {
                            event_name: HookEventConfig::SessionStart.label().to_string(),
                            handler_id: outcome.handler_id,
                            tool_use_id: None,
                            content: context,
                        });
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
        if let Some(reason) = result.stop_reason.clone() {
            self.pending_session_stop
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .insert(session_id.clone(), reason);
        }
        if !contributions.is_empty() {
            self.pending_session_context
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .entry(session_id)
                .or_default()
                .extend(contributions);
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
        let session_id = request.session_id.clone();
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
        if let Some(reason) = self
            .pending_session_stop
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&session_id)
        {
            result.should_block = true;
            result.block_reason = Some(reason);
            return Ok(result);
        }
        result.context_contributions = self.take_session_context(&session_id);
        for outcome in self
            .run_event(
                HookEventConfig::UserPromptSubmit,
                None,
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            result.notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                result.should_block = true;
                result.block_reason.get_or_insert(reason);
                break;
            }
            match parse_user_prompt_submit(&outcome.stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = limit_context(parsed.additional_context, None) {
                        result.additional_contexts.push(context.clone());
                        result.context_contributions.push(HookContextContribution {
                            event_name: HookEventConfig::UserPromptSubmit.label().to_string(),
                            handler_id: outcome.handler_id,
                            tool_use_id: None,
                            content: context,
                        });
                    }
                    if matches!(parsed.decision, Some(ParsedDecision::Block)) {
                        result.should_block = true;
                        result.block_reason.get_or_insert_with(|| {
                            parsed
                                .reason
                                .unwrap_or_else(|| "blocked by hook".to_string())
                        });
                        break;
                    }
                }
                Ok(None) => {}
                Err(err) => result
                    .notices
                    .push(invalid_notice(HookEventConfig::UserPromptSubmit, err)),
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

        let handlers =
            self.matching_handlers(HookEventConfig::PreToolUse, Some(&request.tool_name));
        let mut current_input = request.tool_input.clone();
        let mut result = PreToolUseResult::default();
        for handler in handlers {
            let input = PreToolUseCommandInput {
                session_id: request.session_id.clone(),
                turn_id: request.turn_id.clone(),
                transcript_path: NullableString::from_string(None),
                cwd: cwd_string(request.cwd.as_deref()),
                hook_event_name: HookEventConfig::PreToolUse.label().to_string(),
                model: request.model.clone(),
                permission_mode: request.permission_mode.clone(),
                tool_name: request.tool_name.clone(),
                tool_input: current_input.clone(),
                tool_use_id: request.tool_use_id.clone(),
            };
            let outcome = self
                .invoke_handler(
                    HookEventConfig::PreToolUse,
                    &handler,
                    &input,
                    request.cwd.as_deref(),
                    request.mcp_tool_state.as_ref(),
                )
                .await?;
            result.notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                result.should_block = true;
                result.block_reason.get_or_insert(reason);
                break;
            }

            match parse_pre_tool_use(&outcome.stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = limit_context(
                        parsed.additional_context,
                        handler.additional_context_limit(),
                    ) {
                        result.additional_contexts.push(context.clone());
                        result.context_contributions.push(HookContextContribution {
                            event_name: HookEventConfig::PreToolUse.label().to_string(),
                            handler_id: outcome.handler_id.clone(),
                            tool_use_id: Some(request.tool_use_id.clone()),
                            content: context,
                        });
                    }
                    match parsed.permission_decision {
                        Some(ParsedPermissionDecision::Deny { message }) => {
                            result.should_block = true;
                            result.block_reason.get_or_insert(message);
                        }
                        Some(ParsedPermissionDecision::Allow) => {
                            result.permission_decision = Some(PreToolPermissionDecision::Allow);
                            if let Some(updated_input) = parsed.updated_input {
                                if updated_input.is_object() {
                                    current_input = updated_input;
                                    result.updated_input = Some(current_input.clone());
                                } else {
                                    result.notices.push(control_notice(
                                        HookEventConfig::PreToolUse,
                                        format!(
                                            "Ignoring non-object updated_input from hook '{}'",
                                            outcome.handler_id
                                        ),
                                    ));
                                }
                            }
                        }
                        Some(ParsedPermissionDecision::Ask) => {
                            result.permission_decision = Some(PreToolPermissionDecision::Ask);
                            if parsed.updated_input.is_some() {
                                result.notices.push(control_notice(
                                    HookEventConfig::PreToolUse,
                                    format!(
                                        "Ignoring updated_input from hook '{}' without permission_decision allow",
                                        outcome.handler_id
                                    ),
                                ));
                            }
                        }
                        None => {
                            if parsed.updated_input.is_some() {
                                result.notices.push(control_notice(
                                    HookEventConfig::PreToolUse,
                                    format!(
                                        "Ignoring updated_input from hook '{}' without permission_decision allow",
                                        outcome.handler_id
                                    ),
                                ));
                            }
                        }
                    }
                    if matches!(parsed.decision, Some(ParsedDecision::Block)) {
                        result.should_block = true;
                        result.block_reason.get_or_insert_with(|| {
                            parsed
                                .reason
                                .unwrap_or_else(|| "blocked by hook".to_string())
                        });
                    }
                    if result.should_block {
                        break;
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
        for handler in
            self.matching_handlers(HookEventConfig::PermissionRequest, Some(&request.tool_name))
        {
            let outcome = self
                .invoke_handler(
                    HookEventConfig::PermissionRequest,
                    &handler,
                    &input,
                    request.cwd.as_deref(),
                    request.mcp_tool_state.as_ref(),
                )
                .await?;
            result.notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                result.decision = Some(PermissionRequestDecision::Deny { message: reason });
                break;
            }
            match parse_permission_request(&outcome.stdout) {
                Ok(Some(parsed)) => match parsed.decision {
                    Some(ParsedPermissionDecision::Deny { message }) => {
                        result.decision = Some(PermissionRequestDecision::Deny { message });
                        return Ok(result);
                    }
                    Some(ParsedPermissionDecision::Allow) => {
                        result.decision = Some(PermissionRequestDecision::Allow);
                    }
                    Some(ParsedPermissionDecision::Ask) | None => {}
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

    pub async fn run_context(
        &self,
        request: ContextHookRequest,
    ) -> anyhow::Result<ContextHookResult> {
        if !self.is_enabled() {
            return Ok(ContextHookResult {
                estimated_tokens: estimate_chat_tokens(&request.messages),
                messages: Some(request.messages),
                notices: Vec::new(),
            });
        }

        let mut messages = request.messages.clone();
        let mut notices = Vec::new();
        for handler in self.matching_handlers(HookEventConfig::Context, None) {
            let input = ContextCommandInput {
                session_id: request.session_id.clone(),
                turn_id: request.turn_id.clone(),
                transcript_path: NullableString::from_string(None),
                cwd: cwd_string(request.cwd.as_deref()),
                hook_event_name: HookEventConfig::Context.label().to_string(),
                model: request.model.clone(),
                permission_mode: request.permission_mode.clone(),
                trigger: request.trigger.clone(),
                context_window: request.context_window,
                estimated_tokens: estimate_chat_tokens(&messages).min(u32::MAX as usize) as u32,
                messages: messages
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()?,
            };
            let outcome = self
                .invoke_handler(
                    HookEventConfig::Context,
                    &handler,
                    &input,
                    request.cwd.as_deref(),
                    request.mcp_tool_state.as_ref(),
                )
                .await?;
            notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                notices.push(control_notice(
                    HookEventConfig::Context,
                    format!(
                        "hook '{}' attempted to block: {}",
                        outcome.handler_id, reason
                    ),
                ));
                continue;
            }
            match parse_context(&outcome.stdout) {
                Ok(Some(parsed)) => {
                    if let Some(replacement) = parsed.messages {
                        match replacement
                            .into_iter()
                            .map(serde_json::from_value)
                            .collect::<Result<Vec<querymt::chat::ChatMessage>, _>>()
                        {
                            Ok(candidate)
                                if validate_message_projection(&candidate).is_ok()
                                    && preserves_tool_identities(&messages, &candidate) =>
                            {
                                messages = candidate;
                            }
                            Ok(candidate) => {
                                let reason = validate_message_projection(&candidate)
                                    .err()
                                    .map(|error| error.to_string())
                                    .unwrap_or_else(|| {
                                        "projection changed or removed tool-call identities"
                                            .to_string()
                                    });
                                notices.push(control_notice(
                                    HookEventConfig::Context,
                                    format!(
                                        "Ignoring invalid message projection from hook '{}': {}",
                                        outcome.handler_id, reason
                                    ),
                                ));
                            }
                            Err(err) => notices.push(control_notice(
                                HookEventConfig::Context,
                                format!(
                                    "Ignoring malformed message projection from hook '{}': {}",
                                    outcome.handler_id, err
                                ),
                            )),
                        }
                    }
                    if let Some(context) = limit_context(
                        parsed.additional_context,
                        handler.additional_context_limit(),
                    ) {
                        append_request_context(&mut messages, &outcome.handler_id, &context);
                    }
                }
                Ok(None) => {}
                Err(err) => notices.push(invalid_notice(HookEventConfig::Context, err)),
            }
        }
        let estimated_tokens = estimate_chat_tokens(&messages);
        Ok(ContextHookResult {
            messages: Some(messages),
            estimated_tokens,
            notices,
        })
    }

    pub async fn run_post_tool_use(
        &self,
        request: PostToolUseRequest,
    ) -> anyhow::Result<PostToolUseResult> {
        if !self.is_enabled() {
            return Ok(PostToolUseResult::default());
        }

        let handlers =
            self.matching_handlers(HookEventConfig::PostToolUse, Some(&request.tool_name));
        let mut current_content = request.content.clone();
        let mut current_is_error = request.is_error;
        let mut result = PostToolUseResult::default();
        for handler in handlers {
            let input = PostToolUseCommandInput {
                session_id: request.session_id.clone(),
                turn_id: request.turn_id.clone(),
                transcript_path: NullableString::from_string(None),
                cwd: cwd_string(request.cwd.as_deref()),
                hook_event_name: HookEventConfig::PostToolUse.label().to_string(),
                model: request.model.clone(),
                permission_mode: request.permission_mode.clone(),
                tool_name: request.tool_name.clone(),
                tool_input: request.tool_input.clone(),
                tool_response: Value::String(flatten_content(&current_content)),
                tool_result: StructuredToolResultWire {
                    content: current_content
                        .iter()
                        .map(serde_json::to_value)
                        .collect::<Result<Vec<_>, _>>()?,
                    is_error: current_is_error,
                    execution_is_error: request.execution_is_error,
                    tool_source: request.tool_source.clone(),
                },
                tool_use_id: request.tool_use_id.clone(),
            };
            let outcome = self
                .invoke_handler(
                    HookEventConfig::PostToolUse,
                    &handler,
                    &input,
                    request.cwd.as_deref(),
                    request.mcp_tool_state.as_ref(),
                )
                .await?;
            result.notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                current_content = vec![querymt::chat::Content::text(reason.clone())];
                current_is_error = true;
                result.should_block = true;
                result.block_reason.get_or_insert(reason);
                break;
            }

            match parse_post_tool_use(&outcome.stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = limit_context(
                        parsed.additional_context,
                        handler.additional_context_limit(),
                    ) {
                        result.additional_contexts.push(context.clone());
                        result.context_contributions.push(HookContextContribution {
                            event_name: HookEventConfig::PostToolUse.label().to_string(),
                            handler_id: outcome.handler_id.clone(),
                            tool_use_id: Some(request.tool_use_id.clone()),
                            content: context,
                        });
                    }
                    if let Some(patch) = parsed.updated_output {
                        if let Some(content) = patch.content {
                            match content
                                .into_iter()
                                .map(serde_json::from_value)
                                .collect::<Result<Vec<querymt::chat::Content>, _>>()
                            {
                                Ok(content) => current_content = content,
                                Err(err) => result.notices.push(control_notice(
                                    HookEventConfig::PostToolUse,
                                    format!(
                                        "Ignoring invalid updated_output content from hook '{}': {}",
                                        outcome.handler_id, err
                                    ),
                                )),
                            }
                        }
                        if let Some(is_error) = patch.is_error {
                            current_is_error = is_error;
                        }
                    }
                    if matches!(parsed.decision, Some(ParsedDecision::Block)) {
                        let reason = parsed
                            .reason
                            .clone()
                            .unwrap_or_else(|| "tool result blocked by hook".to_string());
                        current_content = vec![querymt::chat::Content::text(reason.clone())];
                        current_is_error = true;
                        result.should_block = true;
                        result.block_reason.get_or_insert(reason);
                    }
                    if !parsed.continue_processing {
                        let reason = parsed
                            .stop_reason
                            .or(parsed.reason)
                            .unwrap_or_else(|| "hook suppressed tool result".to_string());
                        current_content = vec![querymt::chat::Content::text(reason.clone())];
                        current_is_error = true;
                        result.stop_reason.get_or_insert(reason);
                    }
                }
                Ok(None) => {}
                Err(err) => result
                    .notices
                    .push(invalid_notice(HookEventConfig::PostToolUse, err)),
            }
        }
        result.content = Some(current_content);
        result.is_error = Some(current_is_error);
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
        for outcome in self
            .run_event(
                HookEventConfig::Stop,
                None,
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            result.notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                result.should_continue = true;
                result.block_reason.get_or_insert(reason);
                break;
            }
            match parse_stop(&outcome.stdout) {
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
            messages: request.messages,
            candidate_summary: NullableString::from_string(None),
        };

        let mut result = PreCompactionResult::default();
        for handler in self.matching_handlers(HookEventConfig::PreCompaction, None) {
            let mut handler_input = input.clone();
            handler_input.candidate_summary =
                NullableString::from_string(result.custom_summary.clone());
            let outcome = self
                .invoke_handler(
                    HookEventConfig::PreCompaction,
                    &handler,
                    &handler_input,
                    request.cwd.as_deref(),
                    None,
                )
                .await?;
            result.notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                result.should_block = true;
                result.block_reason.get_or_insert(reason);
                break;
            }
            match parse_pre_compaction(&outcome.stdout) {
                Ok(Some(parsed)) => {
                    if let Some(context) = parsed.additional_context {
                        result.additional_contexts.push(context);
                    }
                    if let Some(summary) = parsed.summary {
                        if summary.trim().is_empty() {
                            result.notices.push(control_notice(
                                HookEventConfig::PreCompaction,
                                "Ignoring blank hook-provided compaction summary".to_string(),
                            ));
                        } else {
                            result.custom_summary = Some(summary);
                        }
                    }
                    if matches!(parsed.decision, Some(ParsedDecision::Block)) {
                        result.should_block = true;
                        result.block_reason.get_or_insert_with(|| {
                            parsed
                                .reason
                                .unwrap_or_else(|| "compaction blocked by hook".to_string())
                        });
                        break;
                    }
                }
                Ok(None) => {}
                Err(err) => result
                    .notices
                    .push(invalid_notice(HookEventConfig::PreCompaction, err)),
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
        for outcome in self
            .run_event(
                HookEventConfig::PostCompaction,
                None,
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            result.notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                result.notices.push(control_notice(
                    HookEventConfig::PostCompaction,
                    format!(
                        "hook '{}' attempted to block: {}",
                        outcome.handler_id, reason
                    ),
                ));
                break;
            }
            match parse_post_compaction(&outcome.stdout) {
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
        for outcome in self
            .run_event(
                HookEventConfig::PreDelegation,
                Some(&request.target_agent_id),
                input.clone(),
                request.cwd.as_deref(),
            )
            .await?
        {
            result.notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                result.should_block = true;
                result.block_reason.get_or_insert(reason);
                break;
            }
            match parse_pre_delegation(&outcome.stdout) {
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
        for outcome in self
            .run_event(event, Some(target_agent_id), input, cwd)
            .await?
        {
            result.notices.extend(outcome.notices);
            if let Some(reason) = outcome.hard_block_reason {
                result.notices.push(control_notice(
                    event,
                    format!(
                        "hook '{}' attempted to block: {}",
                        outcome.handler_id, reason
                    ),
                ));
                continue;
            }
            match parser(&outcome.stdout) {
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

    fn matching_handlers(
        &self,
        event: HookEventConfig,
        matcher: Option<&str>,
    ) -> Vec<ResolvedHookHandler> {
        let mut handlers = Vec::new();
        for (group_idx, group) in self.config.groups_for(event).iter().enumerate() {
            if !matches_group(group.matcher.as_deref(), matcher) {
                continue;
            }
            for (handler_idx, hook) in group.hooks.iter().enumerate() {
                let (configured_id, kind) = match hook {
                    HookHandlerConfig::Command(command) => (
                        command.id.clone(),
                        ResolvedHookKind::Command(command.clone()),
                    ),
                    HookHandlerConfig::McpTool(config) => {
                        (config.id.clone(), ResolvedHookKind::McpTool(config.clone()))
                    }
                };
                handlers.push(ResolvedHookHandler {
                    id: configured_id.unwrap_or_else(|| {
                        format!("{}-{}-{}", event.label(), group_idx, handler_idx)
                    }),
                    matcher: group.matcher.clone(),
                    kind,
                });
            }
        }
        handlers
    }

    async fn invoke_handler<T: Serialize>(
        &self,
        event: HookEventConfig,
        handler: &ResolvedHookHandler,
        input: &T,
        cwd: Option<&Path>,
        mcp_tool_state: Option<&Arc<crate::agent::core::McpToolState>>,
    ) -> anyhow::Result<HookInvocationOutcome> {
        let stdin_json = serde_json::to_string(input)?;
        let started = Instant::now();
        let span = tracing::info_span!(
            "agent.hook.invoke",
            event_name = event.label(),
            handler_id = %handler.id,
            matcher = handler.matcher.as_deref().unwrap_or(""),
            status_message = handler.status_message().unwrap_or(""),
            input_bytes = stdin_json.len(),
            output_bytes = tracing::field::Empty,
            exit_code = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        );
        let _entered = span.enter();
        let output = match &handler.kind {
            ResolvedHookKind::Command(command) => {
                run_command_hook(
                    &spec_from_command(command),
                    cwd.unwrap_or_else(|| Path::new(".")),
                    &stdin_json,
                )
                .await?
            }
            ResolvedHookKind::McpTool(config) => {
                run_mcp_hook(config, input, mcp_tool_state).await?
            }
        };
        record_hook_output(&span, &output, started.elapsed());

        let mut notices = Vec::new();
        if let Some(system_message) = extract_system_message(&output.stdout) {
            notices.push(HookNotice {
                event_name: event.label().to_string(),
                message: limit_system_message(&system_message),
                is_error: false,
            });
        }
        notices.extend(unsupported_control_notices(event, &output.stdout));

        match output.exit_code {
            Some(0) => Ok(HookInvocationOutcome {
                handler_id: handler.id.clone(),
                stdout: output.stdout,
                hard_block_reason: None,
                notices,
            }),
            Some(2) => Ok(HookInvocationOutcome {
                handler_id: handler.id.clone(),
                stdout: String::new(),
                hard_block_reason: Some(nonempty_reason(
                    output.stderr.trim(),
                    "blocked by hook command",
                )),
                notices,
            }),
            Some(code) => anyhow::bail!(
                "{} hook command '{}' failed with exit code {}: {}",
                event.label(),
                handler.id,
                code,
                output.stderr.trim()
            ),
            None => anyhow::bail!(
                "{} hook command '{}' terminated without an exit code: {}",
                event.label(),
                handler.id,
                output.stderr.trim()
            ),
        }
    }

    async fn run_event<T: Serialize>(
        &self,
        event: HookEventConfig,
        matcher: Option<&str>,
        input: T,
        cwd: Option<&Path>,
    ) -> anyhow::Result<Vec<HookInvocationOutcome>> {
        let mut outcomes = Vec::new();
        for handler in self.matching_handlers(event, matcher) {
            outcomes.push(
                self.invoke_handler(event, &handler, &input, cwd, None)
                    .await?,
            );
        }
        Ok(outcomes)
    }
}

#[derive(Default)]
struct ObservingHookResult {
    additional_contexts: Vec<String>,
    notices: Vec<HookNotice>,
}

async fn run_mcp_hook<T: Serialize>(
    config: &HookMcpToolConfig,
    input: &T,
    mcp_tool_state: Option<&Arc<crate::agent::core::McpToolState>>,
) -> anyhow::Result<CommandOutput> {
    use querymt::tool_decorator::CallFunctionTool;

    let state = mcp_tool_state.ok_or_else(|| {
        anyhow::anyhow!(
            "MCP hook '{}.{}' has no execution-scoped MCP state",
            config.server,
            config.tool
        )
    })?;
    let tool = state
        .load()
        .tools
        .get(&config.tool)
        .filter(|tool| tool.server_name() == config.server)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "MCP hook tool '{}.{}' is not connected",
                config.server,
                config.tool
            )
        })?;
    let event_value = serde_json::to_value(input)?;
    let arguments = match &config.input {
        Some(template) => resolve_mcp_input_template(template, &event_value)?,
        None => event_value,
    };
    let content = tokio::time::timeout(
        Duration::from_secs(config.timeout_sec.unwrap_or(30)),
        tool.call(arguments),
    )
    .await
    .map_err(|_| anyhow::anyhow!("MCP hook timed out"))??;
    Ok(CommandOutput {
        exit_code: Some(0),
        stdout: flatten_content(&content),
        stderr: String::new(),
    })
}

#[cfg(test)]
pub(crate) fn resolve_mcp_input_template_for_test(
    template: &Value,
    event: &Value,
) -> anyhow::Result<Value> {
    resolve_mcp_input_template(template, event)
}

fn resolve_mcp_input_template(template: &Value, event: &Value) -> anyhow::Result<Value> {
    match template {
        Value::String(value) if value.starts_with("$event.") => {
            let path = value.trim_start_matches("$event.");
            let mut current = event;
            for segment in path.split('.') {
                current = current.get(segment).ok_or_else(|| {
                    anyhow::anyhow!("unknown MCP hook event template path '$event.{}'", path)
                })?;
            }
            Ok(current.clone())
        }
        Value::Array(values) => values
            .iter()
            .map(|value| resolve_mcp_input_template(value, event))
            .collect::<anyhow::Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), resolve_mcp_input_template(value, event)?)))
            .collect::<anyhow::Result<serde_json::Map<_, _>>>()
            .map(Value::Object),
        value => Ok(value.clone()),
    }
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
    control_notice(
        event,
        format!(
            "Ignoring control fields from {} hook output; this hook is observe-only",
            event.label()
        ),
    )
}

fn control_notice(event: HookEventConfig, message: String) -> HookNotice {
    HookNotice {
        event_name: event.label().to_string(),
        message,
        is_error: false,
    }
}

fn unsupported_control_notices(event: HookEventConfig, stdout: &str) -> Vec<HookNotice> {
    let Ok(Value::Object(output)) = serde_json::from_str(stdout.trim()) else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    if output.contains_key("continue")
        && !matches!(
            event,
            HookEventConfig::SessionStart | HookEventConfig::PostToolUse | HookEventConfig::Stop
        )
    {
        fields.push("continue");
    }
    if output.contains_key("stop_reason")
        && !matches!(
            event,
            HookEventConfig::SessionStart | HookEventConfig::PostToolUse | HookEventConfig::Stop
        )
    {
        fields.push("stop_reason");
    }
    if fields.is_empty() {
        Vec::new()
    } else {
        vec![control_notice(
            event,
            format!(
                "Ignoring unsupported {} field(s): {}",
                event.label(),
                fields.join(", ")
            ),
        )]
    }
}

fn extract_system_message(stdout: &str) -> Option<String> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    value
        .get("system_message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_string)
}

fn limit_system_message(message: &str) -> String {
    const MAX_CHARS: usize = 2_000;
    if message.chars().count() <= MAX_CHARS {
        message.to_string()
    } else {
        format!("{}...", message.chars().take(MAX_CHARS).collect::<String>())
    }
}

fn limit_context(context: Option<String>, token_limit: Option<u32>) -> Option<String> {
    let context = context?.trim().to_string();
    if context.is_empty() {
        return None;
    }
    let max_chars = token_limit.unwrap_or(2_500) as usize * 4;
    if context.chars().count() <= max_chars {
        return Some(context);
    }
    let half = max_chars.saturating_sub(80) / 2;
    let head: String = context.chars().take(half).collect();
    let tail: String = context
        .chars()
        .rev()
        .take(half)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    Some(format!(
        "{}\n...[hook context truncated]...\n{}",
        head, tail
    ))
}

fn append_request_context(
    messages: &mut Vec<querymt::chat::ChatMessage>,
    handler_id: &str,
    content: &str,
) {
    let rendered = format!(
        "<hook-context event=context handler={}>\n{}\n</hook-context>",
        handler_id, content
    );
    if let Some(last) = messages.last_mut()
        && last.role == querymt::chat::ChatRole::User
    {
        last.content.push(querymt::chat::Content::text(rendered));
    } else {
        messages.push(querymt::chat::ChatMessage {
            role: querymt::chat::ChatRole::User,
            content: vec![querymt::chat::Content::text(rendered)],
            cache: None,
        });
    }
}

fn validate_message_projection(messages: &[querymt::chat::ChatMessage]) -> anyhow::Result<()> {
    if messages.is_empty() {
        anyhow::bail!("projection must contain at least one message");
    }
    let mut pending_tool_ids = std::collections::BTreeSet::new();
    for message in messages {
        if message.content.is_empty() {
            anyhow::bail!("projection contains an empty message");
        }
        for content in &message.content {
            match content {
                querymt::chat::Content::ToolUse { id, .. } => {
                    if !pending_tool_ids.insert(id.clone()) {
                        anyhow::bail!("duplicate tool call id '{}'", id);
                    }
                }
                querymt::chat::Content::ToolResult { id, .. } if !pending_tool_ids.remove(id) => {
                    anyhow::bail!("unmatched tool result id '{}'", id);
                }
                querymt::chat::Content::ToolResult { .. } => {}
                _ => {}
            }
        }
    }
    if let Some(id) = pending_tool_ids.first() {
        anyhow::bail!("unmatched tool call id '{}'", id);
    }
    Ok(())
}

fn preserves_tool_identities(
    current: &[querymt::chat::ChatMessage],
    candidate: &[querymt::chat::ChatMessage],
) -> bool {
    fn identities(
        messages: &[querymt::chat::ChatMessage],
    ) -> std::collections::BTreeSet<(String, String)> {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                querymt::chat::Content::ToolUse { id, .. } => Some(("use".to_string(), id.clone())),
                querymt::chat::Content::ToolResult { id, .. } => {
                    Some(("result".to_string(), id.clone()))
                }
                _ => None,
            })
            .collect()
    }
    identities(candidate).is_subset(&identities(current))
}

fn estimate_chat_tokens(messages: &[querymt::chat::ChatMessage]) -> usize {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .map(|content| serde_json::to_vec(content).map_or(0, |bytes| bytes.len() / 4))
        .sum()
}

fn flatten_content(content: &[querymt::chat::Content]) -> String {
    content
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}

fn nonempty_reason(reason: &str, fallback: &str) -> String {
    if reason.is_empty() {
        fallback.to_string()
    } else {
        reason.to_string()
    }
}

fn record_hook_output(span: &tracing::Span, output: &CommandOutput, duration: Duration) {
    span.record("output_bytes", output.stdout.len() + output.stderr.len());
    if let Some(code) = output.exit_code {
        span.record("exit_code", code);
    }
    span.record("duration_ms", duration.as_millis() as u64);
}

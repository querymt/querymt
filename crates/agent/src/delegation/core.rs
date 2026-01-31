use crate::agent::QueryMTAgent;
use crate::events::{AgentEvent, AgentEventKind, EventObserver};
use crate::model::MessagePart;
use crate::send_agent::SendAgent;
use crate::session::domain::{Delegation, DelegationStatus, ForkOrigin, ForkPointType};
use crate::session::store::SessionStore;
use crate::verification::VerificationSpec;
use crate::verification::service::{VerificationContext, VerificationService};
use agent_client_protocol::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
    ProtocolVersion, TextContent,
};
use async_trait::async_trait;
use log::{debug, error, warn};
use querymt::chat::ChatRole;
use querymt::error::LLMError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// Type alias to simplify complex type signature
type ActiveDelegations = Arc<Mutex<HashMap<String, (String, CancellationToken, JoinHandle<()>)>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<crate::tools::CapabilityRequirement>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<serde_json::Value>,
}

/// AgentRegistry stores both agent metadata (AgentInfo) and agent instances (SendAgent).
///
/// This enables:
/// 1. Listing available agents for delegation (via AgentInfo)
/// 2. Actually delegating to agents (via SendAgent)
/// 3. Thread-safe access from multiple sessions
pub trait AgentRegistry: Send + Sync {
    /// List all available agents (metadata only).
    fn list_agents(&self) -> Vec<AgentInfo>;

    /// Get agent metadata by ID.
    fn get_agent(&self, id: &str) -> Option<AgentInfo>;

    /// Get an agent instance for delegation.
    ///
    /// Returns an Arc<dyn SendAgent> that can be used to interact with the agent
    /// via the full ACP protocol lifecycle (initialize, new_session, prompt, etc.).
    fn get_agent_instance(&self, id: &str) -> Option<Arc<dyn SendAgent>>;
}

/// Builder for verification specifications from structured verification_spec only.
/// Legacy expected_output parsing has been removed as it was unreliable.
struct VerificationSpecBuilder;

impl VerificationSpecBuilder {
    /// Build verification spec from delegation's structured verification_spec field.
    /// Returns None if no verification_spec is set (expected_output is ignored).
    fn from_delegation(delegation: &Delegation) -> Option<VerificationSpec> {
        delegation.verification_spec.clone()
    }
}

#[derive(Clone)]
pub struct DelegationOrchestratorConfig {
    pub cwd: Option<PathBuf>,
    pub inject_results: bool,
    pub run_verification: bool,
}

impl DelegationOrchestratorConfig {
    pub fn new(cwd: Option<PathBuf>) -> Self {
        Self {
            cwd,
            inject_results: false,
            run_verification: false,
        }
    }
}

pub struct DelegationOrchestrator {
    delegator: Arc<QueryMTAgent>,
    store: Arc<dyn SessionStore>,
    agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
    config: DelegationOrchestratorConfig,
    /// Maps delegation_id -> (parent_session_id, cancellation_token, join_handle)
    active_delegations: ActiveDelegations,
    /// Optional summarizer for generating planning context
    delegation_summarizer: Option<Arc<super::summarizer::DelegationSummarizer>>,
}

impl DelegationOrchestrator {
    pub fn new(
        delegator: Arc<QueryMTAgent>,
        store: Arc<dyn SessionStore>,
        agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
        cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            delegator,
            store,
            agent_registry,
            config: DelegationOrchestratorConfig::new(cwd),
            active_delegations: Arc::new(Mutex::new(HashMap::new())),
            delegation_summarizer: None,
        }
    }

    pub fn with_result_injection(mut self, enabled: bool) -> Self {
        self.config.inject_results = enabled;
        self
    }

    pub fn with_verification(mut self, enabled: bool) -> Self {
        self.config.run_verification = enabled;
        self
    }

    pub fn with_summarizer(
        mut self,
        summarizer: Option<Arc<super::summarizer::DelegationSummarizer>>,
    ) -> Self {
        self.delegation_summarizer = summarizer;
        self
    }
}

#[async_trait]
impl EventObserver for DelegationOrchestrator {
    async fn on_event(&self, event: &AgentEvent) -> Result<(), LLMError> {
        match &event.kind {
            AgentEventKind::DelegationRequested { delegation } => {
                let delegator = self.delegator.clone();
                let store = self.store.clone();
                let agent_registry = self.agent_registry.clone();
                let config = self.config.clone();
                let delegation_summarizer = self.delegation_summarizer.clone();
                let parent_session_id = event.session_id.clone();
                let parent_session_id_for_insert = parent_session_id.clone();
                let delegation = delegation.clone();
                let cancel_token = CancellationToken::new();
                let active_delegations = self.active_delegations.clone();
                let active_delegations_for_spawn = active_delegations.clone();

                // Store the cancellation token and join handle
                let delegation_id = delegation.public_id.clone();
                let cancel_token_clone = cancel_token.clone();

                let handle = tokio::spawn(async move {
                    let ctx = DelegationContext {
                        delegator,
                        store,
                        agent_registry,
                        config,
                        active_delegations: active_delegations_for_spawn,
                        delegation_summarizer,
                    };
                    handle_delegation(ctx, parent_session_id, delegation, cancel_token).await;
                });

                let mut active = active_delegations.lock().await;
                active.insert(
                    delegation_id,
                    (parent_session_id_for_insert, cancel_token_clone, handle),
                );
            }
            AgentEventKind::Cancelled => {
                // Cancel all delegations for this session
                let session_id = &event.session_id;
                let mut active = self.active_delegations.lock().await;

                // Find all delegations for this session
                let to_cancel: Vec<(String, tokio::task::JoinHandle<()>)> = active
                    .iter_mut()
                    .filter(|(_, (parent_id, _, _))| parent_id == session_id)
                    .map(|(delegation_id, (_, cancel_token, handle))| {
                        cancel_token.cancel();
                        // Replace the handle with a dummy handle that immediately completes
                        // so we can take ownership of the real handle for timeout monitoring
                        let dummy_handle = tokio::spawn(async {});
                        let real_handle = std::mem::replace(handle, dummy_handle);
                        (delegation_id.clone(), real_handle)
                    })
                    .collect();

                // Drop the lock before spawning watchdog tasks
                drop(active);

                // Spawn watchdog tasks to force-abort delegations that don't terminate within timeout
                for (delegation_id, mut handle) in to_cancel {
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = &mut handle => {
                                debug!("Delegation {} terminated gracefully after cancel", delegation_id);
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                                warn!("Delegation {} did not terminate within 5s timeout, force aborting", delegation_id);
                                handle.abort();
                            }
                        }
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Context structure to group delegation handler parameters
struct DelegationContext {
    delegator: Arc<QueryMTAgent>,
    store: Arc<dyn SessionStore>,
    agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
    config: DelegationOrchestratorConfig,
    active_delegations: ActiveDelegations,
    delegation_summarizer: Option<Arc<super::summarizer::DelegationSummarizer>>,
}

/// Inject planning summary into delegate session's system prompt
///
/// This modifies the delegate session's LLM config to append the planning summary
/// to the system prompt. The summary persists across all turns of the coder's session.
async fn inject_planning_summary(
    store: &Arc<dyn SessionStore>,
    child_session_id: &str,
    summary: &str,
) -> crate::session::error::SessionResult<()> {
    // 1. Get current LLM config for the delegate session
    let config = store
        .get_session_llm_config(child_session_id)
        .await?
        .ok_or_else(|| {
            crate::session::error::SessionError::InvalidOperation(
                "Delegate session has no LLM config".to_string(),
            )
        })?;

    // 2. Extract current params, including system prompt
    let mut params: querymt::LLMParams = if let Some(params_value) = config.params {
        serde_json::from_value(params_value).unwrap_or_default()
    } else {
        querymt::LLMParams::default()
    };

    // Ensure provider and model are set from config
    params.provider = Some(config.provider.clone());
    params.model = Some(config.model.clone());

    // 3. Append planning summary to system prompt
    let summary_context = format!("\n\n<planning-context>\n{}\n</planning-context>", summary);
    params.system.push(summary_context);

    // 4. Create new LLM config with updated params
    let new_config = store.create_or_get_llm_config(&params).await?;

    // 5. Update session to use new config
    store
        .set_session_llm_config(child_session_id, new_config.id)
        .await?;

    Ok(())
}

async fn handle_delegation(
    ctx: DelegationContext,
    parent_session_id: String,
    delegation: Delegation,
    cancel_token: CancellationToken,
) {
    let delegation_id = delegation.public_id.clone();
    // Validate target's capability requirements
    if let Some(target_info) = ctx.agent_registry.get_agent(&delegation.target_agent_id)
        && target_info
            .required_capabilities
            .contains(&crate::tools::CapabilityRequirement::Filesystem)
        && ctx.config.cwd.is_none()
    {
        let error_message = format!(
            "Cannot delegate to '{}': agent requires filesystem access but no working directory is set",
            delegation.target_agent_id
        );
        fail_delegation(
            &ctx.delegator,
            &ctx.store,
            &ctx.config,
            &parent_session_id,
            &delegation.public_id,
            &error_message,
        )
        .await;
        return;
    }

    let Some(delegate_agent) = ctx
        .agent_registry
        .get_agent_instance(&delegation.target_agent_id)
    else {
        let error_message = format!("Unknown agent ID: {}", delegation.target_agent_id);
        warn!("{}", error_message);
        fail_delegation(
            &ctx.delegator,
            &ctx.store,
            &ctx.config,
            &parent_session_id,
            &delegation.public_id,
            &error_message,
        )
        .await;
        return;
    };

    if let Err(e) = ctx
        .store
        .update_delegation_status(&delegation.public_id, DelegationStatus::Running)
        .await
    {
        warn!("Failed to update delegation status to Running: {}", e);
    }

    let init_resp = match delegate_agent
        .initialize(InitializeRequest::new(ProtocolVersion::LATEST))
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            let error_message = format!("Failed to initialize agent: {}", e);
            fail_delegation(
                &ctx.delegator,
                &ctx.store,
                &ctx.config,
                &parent_session_id,
                &delegation.public_id,
                &error_message,
            )
            .await;
            return;
        }
    };

    if !init_resp.auth_methods.is_empty() {
        let error_message =
            "Delegated agent requires authentication, which is not yet supported".to_string();
        fail_delegation(
            &ctx.delegator,
            &ctx.store,
            &ctx.config,
            &parent_session_id,
            &delegation.public_id,
            &error_message,
        )
        .await;
        return;
    }

    let delegate_session = match &ctx.config.cwd {
        Some(cwd) => {
            let mut req = NewSessionRequest::new(cwd.clone());
            // Pass parent_session_id via meta so it gets set in the session record
            req.meta = Some(serde_json::Map::from_iter([(
                "parent_session_id".to_string(),
                serde_json::Value::String(parent_session_id.clone()),
            )]));
            match delegate_agent.new_session(req).await {
                Ok(session) => session,
                Err(e) => {
                    let error_message = format!("Failed to create session: {}", e);
                    fail_delegation(
                        &ctx.delegator,
                        &ctx.store,
                        &ctx.config,
                        &parent_session_id,
                        &delegation.public_id,
                        &error_message,
                    )
                    .await;
                    return;
                }
            }
        }
        None => {
            let mut req = NewSessionRequest::new(PathBuf::new());
            // Pass parent_session_id via meta so it gets set in the session record
            req.meta = Some(serde_json::Map::from_iter([(
                "parent_session_id".to_string(),
                serde_json::Value::String(parent_session_id.clone()),
            )]));
            match delegate_agent.new_session(req).await {
                Ok(session) => session,
                Err(e) => {
                    let error_message = format!("Failed to create session: {}", e);
                    fail_delegation(
                        &ctx.delegator,
                        &ctx.store,
                        &ctx.config,
                        &parent_session_id,
                        &delegation.public_id,
                        &error_message,
                    )
                    .await;
                    return;
                }
            }
        }
    };

    ctx.delegator.emit_event(
        &parent_session_id,
        AgentEventKind::SessionForked {
            parent_session_id: parent_session_id.clone(),
            child_session_id: delegate_session.session_id.to_string(),
            target_agent_id: delegation.target_agent_id.clone(),
            origin: ForkOrigin::Delegation,
            fork_point_type: ForkPointType::ProgressEntry,
            fork_point_ref: delegation.public_id.clone(),
            instructions: delegation.context.clone(),
        },
    );

    // ──── Generate and inject planning summary ────
    let _planning_summary = if let Some(ref summarizer) = ctx.delegation_summarizer {
        match ctx
            .delegator
            .provider
            .history_store()
            .get_history(&parent_session_id)
            .await
        {
            Ok(history) => {
                match summarizer.summarize(&history, &delegation.objective).await {
                    Ok(summary) => {
                        // Persist to delegation record
                        let mut updated_delegation = delegation.clone();
                        updated_delegation.planning_summary = Some(summary.clone());
                        if let Err(e) = ctx.store.update_delegation(updated_delegation).await {
                            warn!("Failed to persist delegation summary: {}", e);
                        }

                        // Inject into delegate session's system prompt
                        if let Err(e) = inject_planning_summary(
                            &ctx.store,
                            &delegate_session.session_id.to_string(),
                            &summary,
                        )
                        .await
                        {
                            warn!(
                                "Failed to inject planning summary into delegate session: {}",
                                e
                            );
                        } else {
                            log::info!(
                                "Injected planning summary into delegate session {}",
                                delegate_session.session_id
                            );
                        }

                        Some(summary)
                    }
                    Err(e) => {
                        warn!("Delegation summary generation failed: {}", e);
                        None // Proceed without summary — graceful degradation
                    }
                }
            }
            Err(e) => {
                warn!("Failed to load parent history for summary: {}", e);
                None
            }
        }
    } else {
        None
    };

    let prompt_text = build_delegation_prompt(&delegation);
    let prompt_req = PromptRequest::new(
        delegate_session.session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(prompt_text))],
    );

    let child_session_id = delegate_session.session_id.to_string();

    // Use select! to race between prompt completion and cancellation
    let prompt_result = tokio::select! {
        result = delegate_agent.prompt(prompt_req) => Some(result),
        _ = cancel_token.cancelled() => {
            // Cancellation requested - cancel the child session and exit
            let cancel_notif = CancelNotification::new(child_session_id.clone());
            let _ = delegate_agent.cancel(cancel_notif).await;

            if let Err(e) = ctx.store
                .update_delegation_status(&delegation_id, DelegationStatus::Cancelled)
                .await
            {
                warn!("Failed to update delegation status to Cancelled: {}", e);
            }

            ctx.delegator.emit_event(
                &parent_session_id,
                AgentEventKind::DelegationCancelled {
                    delegation_id: delegation_id.clone(),
                },
            );

            // Clean up from active_delegations map
            let mut active = ctx.active_delegations.lock().await;
            active.remove(&delegation_id);

            return;
        }
    };

    match prompt_result {
        Some(Ok(_)) => {
            let verification_passed = if ctx.config.run_verification {
                // Use new verification framework if verification_spec is available or legacy expected_output exists
                match VerificationSpecBuilder::from_delegation(&delegation) {
                    Some(verification_spec) => {
                        // Create verification service using the delegator's tool registry
                        let agent_tool_registry = ctx.delegator.tool_registry();
                        let service = VerificationService::new(agent_tool_registry.clone());
                        let verification_context = VerificationContext {
                            session_id: parent_session_id.clone(),
                            task_id: delegation.task_id.map(|id| id.to_string()),
                            delegation_id: Some(delegation.public_id.clone()),
                            cwd: ctx.config.cwd.clone(),
                            tool_registry: agent_tool_registry.clone(),
                        };

                        match service
                            .verify(&verification_spec, &verification_context)
                            .await
                        {
                            Ok(()) => true,
                            Err(err) => {
                                warn!("Verification failed: {}", err);
                                false
                            }
                        }
                    }
                    None => {
                        // No verification spec available, treat as success
                        true
                    }
                }
            } else {
                true
            };

            if !verification_passed {
                let error_message =
                    "Verification failed: The changes did not pass the specified verification checks."
                        .to_string();
                fail_delegation(
                    &ctx.delegator,
                    &ctx.store,
                    &ctx.config,
                    &parent_session_id,
                    &delegation.public_id,
                    &error_message,
                )
                .await;
                return;
            }

            let summary =
                match extract_session_summary(&ctx.store, &delegate_session.session_id.to_string())
                    .await
                {
                    Ok(summary) => summary,
                    Err(err) => {
                        warn!("Error extracting summary: {}", err);
                        "Error extracting summary.".to_string()
                    }
                };

            if let Err(e) = ctx
                .store
                .update_delegation_status(&delegation.public_id, DelegationStatus::Complete)
                .await
            {
                warn!("Failed to persist delegation completion: {}", e);
            }

            ctx.delegator.emit_event(
                &parent_session_id,
                AgentEventKind::DelegationCompleted {
                    delegation_id: delegation.public_id.clone(),
                    result: Some(summary.clone()),
                },
            );

            if ctx.config.inject_results {
                inject_results(
                    &ctx.delegator,
                    &parent_session_id,
                    &delegation.public_id,
                    &summary,
                )
                .await;
            }

            // Clean up from active_delegations map
            let mut active = ctx.active_delegations.lock().await;
            active.remove(&delegation_id);
        }
        Some(Err(e)) => {
            let error_message = format!("Delegation failed: {}", e);
            fail_delegation(
                &ctx.delegator,
                &ctx.store,
                &ctx.config,
                &parent_session_id,
                &delegation_id,
                &error_message,
            )
            .await;

            // Clean up from active_delegations map
            let mut active = ctx.active_delegations.lock().await;
            active.remove(&delegation_id);
        }
        None => {
            // Already handled in the select! cancellation branch
        }
    }
}

async fn fail_delegation(
    delegator: &Arc<QueryMTAgent>,
    store: &Arc<dyn SessionStore>,
    config: &DelegationOrchestratorConfig,
    parent_session_id: &str,
    delegation_id: &str,
    error_message: &str,
) {
    error!("{}", error_message);
    if let Err(e) = store
        .update_delegation_status(delegation_id, DelegationStatus::Failed)
        .await
    {
        warn!("Failed to persist delegation failure: {}", e);
    }

    delegator.emit_event(
        parent_session_id,
        AgentEventKind::DelegationFailed {
            delegation_id: delegation_id.to_string(),
            error: error_message.to_string(),
        },
    );

    if config.inject_results {
        inject_failure(delegator, parent_session_id, delegation_id, error_message).await;
    }
}

fn build_delegation_prompt(delegation: &Delegation) -> String {
    let mut prompt_text = format!("Task: {}\n", delegation.objective);
    if let Some(ctx) = &delegation.context {
        prompt_text.push_str(&format!("Context: {}\n", ctx));
    }
    if let Some(constraints) = &delegation.constraints {
        prompt_text.push_str(&format!("\nConstraints: {}\n", constraints));
    }
    if let Some(expected) = &delegation.expected_output {
        prompt_text.push_str(&format!("\nExpected Output: {}\n", expected));
    }
    prompt_text.push_str("\nPlease complete this task and summarize your work.");
    prompt_text
}

pub(crate) fn format_delegation_completion_message(delegation_id: &str, summary: &str) -> String {
    format!(
        "Delegation completed.\n\nDelegation ID: {}\n\n{}\n\n\
         Please review the changes and determine if:\n\
         1. The task is complete and satisfactory\n\
         2. Additional fixes or improvements are needed\n\
         3. Further delegation is required",
        delegation_id, summary
    )
}

pub(crate) fn format_delegation_failure_message(delegation_id: &str, error: &str) -> String {
    let (error_type, remediation) = classify_delegation_error(error);
    format!(
        "Delegation failed.\n\nDelegation ID: {}\n\n\
         Error Type: {}\n\
         Error Details:\n{}\n\n\
         Suggested Next Steps:\n{}\n\n\
         IMPORTANT: Do NOT immediately retry the same approach. \
         Analyze the error and adjust your strategy.",
        delegation_id, error_type, error, remediation
    )
}

async fn inject_results(
    delegator: &Arc<QueryMTAgent>,
    session_id: &str,
    delegation_id: &str,
    summary: &str,
) {
    let message = format_delegation_completion_message(delegation_id, summary);

    if let Err(e) = delegator
        .run_prompt(PromptRequest::new(
            session_id.to_string(),
            vec![ContentBlock::Text(TextContent::new(message))],
        ))
        .await
    {
        warn!("Failed to inject delegation results: {}", e);
    }
}

async fn inject_failure(
    delegator: &Arc<QueryMTAgent>,
    session_id: &str,
    delegation_id: &str,
    error: &str,
) {
    let message = format_delegation_failure_message(delegation_id, error);

    if let Err(e) = delegator
        .run_prompt(PromptRequest::new(
            session_id.to_string(),
            vec![ContentBlock::Text(TextContent::new(message))],
        ))
        .await
    {
        warn!("Failed to inject delegation failure: {}", e);
    }
}

fn classify_delegation_error(error: &str) -> (&str, &str) {
    if error.contains("Invalid patch")
        || error.contains("patch: ****")
        || error.contains("Line") && error.contains("mismatch")
    {
        (
            "Patch Application Failure",
            "The patch could not be applied to the file.\n\
          -> Use read_file to see the current state of the target file\n\
          -> Verify the lines you want to change actually exist as shown in the file\n\
          -> Create a new patch with correct context lines matching the actual file",
        )
    } else if error.contains("Verification failed")
        || error.contains("cargo check")
        || error.contains("compilation")
    {
        (
            "Verification Failure",
            "The code change was applied but does not compile or pass tests.\n\
          -> Read the verification error output carefully\n\
          -> Understand what is wrong with the code\n\
          -> Fix the compilation/test errors with another delegation",
        )
    } else if error.contains("workdir") || error.contains("does not exist") {
        (
            "Invalid Working Directory",
            "The specified path or directory does not exist.\n\
          -> Do NOT specify workdir parameter in patches\n\
          -> Patches apply relative to current directory by default\n\
          -> Verify file paths are correct",
        )
    } else if error.contains("MAX RETRIES") || error.contains("retry") {
        (
            "Too Many Retries",
            "This delegation has been attempted multiple times and keeps failing.\n\
          -> The current approach is not working\n\
          -> Try a completely different strategy\n\
          -> Break down the task into smaller, simpler steps\n\
          -> Consider delegating to a different agent",
        )
    } else {
        (
            "Unknown Error",
            "Review the error details above carefully.\n\
          -> Look for clues about what went wrong\n\
          -> Consider trying a different approach\n\
          -> If the error is unclear, break the task into smaller steps",
        )
    }
}

async fn extract_session_summary(
    store: &Arc<dyn SessionStore>,
    session_id: &str,
) -> Result<String, LLMError> {
    let history = store
        .get_history(session_id)
        .await
        .map_err(|e| LLMError::ProviderError(e.to_string()))?;

    let mut summary = String::new();
    summary.push_str("=== Delegate Agent Results ===\n\n");

    let mut tools_used = Vec::new();
    let mut files_modified = Vec::new();
    let mut patches = Vec::new();
    let mut agent_responses = Vec::new();

    for message in &history {
        for part in &message.parts {
            match part {
                MessagePart::ToolUse(tool_call) => {
                    let args_preview = extract_tool_args_preview(
                        &tool_call.function.name,
                        &tool_call.function.arguments,
                    );
                    tools_used.push(format!("{} ({})", tool_call.function.name, args_preview));
                }
                MessagePart::ToolResult {
                    tool_name: Some(name),
                    tool_arguments: Some(args),
                    ..
                } => {
                    if matches!(name.as_str(), "write_file" | "apply_patch" | "delete_file")
                        && let Ok(args_json) = serde_json::from_str::<serde_json::Value>(args)
                        && let Some(path) = args_json.get("path").and_then(|v| v.as_str())
                        && !files_modified.contains(&path.to_string())
                    {
                        files_modified.push(path.to_string());
                    }
                }
                MessagePart::Patch { files, diff, .. } => {
                    patches.push(format!("Files: {}\n\n{}", files.join(", "), diff));
                }
                MessagePart::Text { content } if message.role == ChatRole::Assistant => {
                    if !content.trim().is_empty() {
                        agent_responses.push(content.clone());
                    }
                }
                _ => {}
            }
        }
    }

    if !tools_used.is_empty() {
        summary.push_str("Tools used:\n");
        for tool in &tools_used {
            summary.push_str(&format!("  - {}\n", tool));
        }
        summary.push('\n');
    }

    if !files_modified.is_empty() {
        summary.push_str("Files modified:\n");
        for file in &files_modified {
            summary.push_str(&format!("  - {}\n", file));
        }
        summary.push('\n');
    }

    if !patches.is_empty() {
        summary.push_str("Patches applied:\n\n");
        for (i, patch) in patches.iter().enumerate() {
            summary.push_str(&format!("Patch {}:\n{}\n\n", i + 1, patch));
        }
    }

    if !agent_responses.is_empty() {
        summary.push_str("Agent's summary:\n");
        summary.push_str(agent_responses.last().unwrap());
        summary.push('\n');
    }

    if tools_used.is_empty() && files_modified.is_empty() && patches.is_empty() {
        summary.push_str("(No modifications made)\n");
    }

    Ok(summary)
}

fn extract_tool_args_preview(tool_name: &str, args_json: &str) -> String {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return String::new();
    };

    match tool_name {
        "write_file" | "read_file" | "delete_file" | "apply_patch" => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "web_fetch" => args
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "shell" => args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Internal structure to hold both metadata and agent instance.
struct AgentEntry {
    info: AgentInfo,
    instance: Arc<dyn SendAgent>,
}

#[derive(Default)]
pub struct DefaultAgentRegistry {
    agents: std::collections::HashMap<String, AgentEntry>,
}

impl DefaultAgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an agent with its metadata and instance.
    pub fn register(&mut self, info: AgentInfo, instance: Arc<dyn SendAgent>) {
        let id = info.id.clone();
        self.agents.insert(id, AgentEntry { info, instance });
    }
}

impl AgentRegistry for DefaultAgentRegistry {
    fn list_agents(&self) -> Vec<AgentInfo> {
        self.agents
            .values()
            .map(|entry| entry.info.clone())
            .collect()
    }

    fn get_agent(&self, id: &str) -> Option<AgentInfo> {
        self.agents.get(id).map(|entry| entry.info.clone())
    }

    fn get_agent_instance(&self, id: &str) -> Option<Arc<dyn SendAgent>> {
        self.agents.get(id).map(|entry| entry.instance.clone())
    }
}

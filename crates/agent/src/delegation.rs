use crate::agent::QueryMTAgent;
use crate::events::{AgentEvent, AgentEventKind, EventObserver};
use crate::model::MessagePart;
use crate::send_agent::SendAgent;
use crate::session::domain::{Delegation, DelegationStatus, ForkOrigin, ForkPointType};
use crate::session::store::SessionStore;
use crate::verification::VerificationSpec;
use crate::verification::service::{VerificationContext, VerificationService};
use agent_client_protocol::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion, TextContent,
};
use async_trait::async_trait;
use log::{error, warn};
use querymt::chat::ChatRole;
use querymt::error::LLMError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

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
}

#[async_trait]
impl EventObserver for DelegationOrchestrator {
    async fn on_event(&self, event: &AgentEvent) -> Result<(), LLMError> {
        if let AgentEventKind::DelegationRequested { delegation } = &event.kind {
            let delegator = self.delegator.clone();
            let store = self.store.clone();
            let agent_registry = self.agent_registry.clone();
            let config = self.config.clone();
            let parent_session_id = event.session_id.clone();
            let delegation = delegation.clone();

            tokio::spawn(async move {
                handle_delegation(
                    delegator,
                    store,
                    agent_registry,
                    config,
                    parent_session_id,
                    delegation,
                )
                .await;
            });
        }
        Ok(())
    }
}

async fn handle_delegation(
    delegator: Arc<QueryMTAgent>,
    store: Arc<dyn SessionStore>,
    agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
    config: DelegationOrchestratorConfig,
    parent_session_id: String,
    delegation: Delegation,
) {
    // Validate target's capability requirements
    if let Some(target_info) = agent_registry.get_agent(&delegation.target_agent_id)
        && target_info
            .required_capabilities
            .contains(&crate::tools::CapabilityRequirement::Filesystem)
        && config.cwd.is_none()
    {
        let error_message = format!(
            "Cannot delegate to '{}': agent requires filesystem access but no working directory is set",
            delegation.target_agent_id
        );
        fail_delegation(
            &delegator,
            &store,
            &config,
            &parent_session_id,
            &delegation.public_id,
            &error_message,
        )
        .await;
        return;
    }

    let Some(delegate_agent) = agent_registry.get_agent_instance(&delegation.target_agent_id)
    else {
        let error_message = format!("Unknown agent ID: {}", delegation.target_agent_id);
        warn!("{}", error_message);
        fail_delegation(
            &delegator,
            &store,
            &config,
            &parent_session_id,
            &delegation.public_id,
            &error_message,
        )
        .await;
        return;
    };

    if let Err(e) = store
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
                &delegator,
                &store,
                &config,
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
            &delegator,
            &store,
            &config,
            &parent_session_id,
            &delegation.public_id,
            &error_message,
        )
        .await;
        return;
    }

    let delegate_session = match &config.cwd {
        Some(cwd) => {
            match delegate_agent
                .new_session(NewSessionRequest::new(cwd.clone()))
                .await
            {
                Ok(session) => session,
                Err(e) => {
                    let error_message = format!("Failed to create session: {}", e);
                    fail_delegation(
                        &delegator,
                        &store,
                        &config,
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
            match delegate_agent
                .new_session(NewSessionRequest::new(PathBuf::new()))
                .await
            {
                Ok(session) => session,
                Err(e) => {
                    let error_message = format!("Failed to create session: {}", e);
                    fail_delegation(
                        &delegator,
                        &store,
                        &config,
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

    delegator.emit_event(
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

    let prompt_text = build_delegation_prompt(&delegation);
    let prompt_req = PromptRequest::new(
        delegate_session.session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(prompt_text))],
    );

    match delegate_agent.prompt(prompt_req).await {
        Ok(_) => {
            let verification_passed = if config.run_verification {
                // Use new verification framework if verification_spec is available or legacy expected_output exists
                match VerificationSpecBuilder::from_delegation(&delegation) {
                    Some(verification_spec) => {
                        // Create verification service using the delegator's tool registry
                        let agent_tool_registry = delegator.tool_registry();
                        let service = VerificationService::new(agent_tool_registry.clone());
                        let verification_context = VerificationContext {
                            session_id: parent_session_id.clone(),
                            task_id: delegation.task_id.map(|id| id.to_string()),
                            delegation_id: Some(delegation.public_id.clone()),
                            cwd: config.cwd.clone(),
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
                    &delegator,
                    &store,
                    &config,
                    &parent_session_id,
                    &delegation.public_id,
                    &error_message,
                )
                .await;
                return;
            }

            let summary =
                match extract_session_summary(&store, &delegate_session.session_id.to_string())
                    .await
                {
                    Ok(summary) => summary,
                    Err(err) => {
                        warn!("Error extracting summary: {}", err);
                        "Error extracting summary.".to_string()
                    }
                };

            if let Err(e) = store
                .update_delegation_status(&delegation.public_id, DelegationStatus::Complete)
                .await
            {
                warn!("Failed to persist delegation completion: {}", e);
            }

            delegator.emit_event(
                &parent_session_id,
                AgentEventKind::DelegationCompleted {
                    delegation_id: delegation.public_id.clone(),
                    result: Some(summary.clone()),
                },
            );

            if config.inject_results {
                inject_results(
                    &delegator,
                    &parent_session_id,
                    &delegation.public_id,
                    &summary,
                )
                .await;
            }
        }
        Err(e) => {
            let error_message = format!("Delegation failed: {}", e);
            fail_delegation(
                &delegator,
                &store,
                &config,
                &parent_session_id,
                &delegation.public_id,
                &error_message,
            )
            .await;
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

use crate::event_fanout::EventFanout;
use crate::event_sink::EventSink;
use crate::events::{AgentEventKind, EventEnvelope};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::{Delegation, DelegationStatus, ForkOrigin, ForkPointType};
use crate::session::store::SessionStore;
use crate::tools::ToolRegistry;
use crate::verification::VerificationSpec;
use crate::verification::service::{VerificationContext, VerificationService};
use agent_client_protocol::{ContentBlock, PromptRequest, TextContent};
use log::{debug, error, warn};
use querymt::chat::ChatRole;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::num::NonZeroUsize;
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

/// AgentRegistry stores both agent metadata (AgentInfo) and agent handles.
///
/// This enables:
/// 1. Listing available agents for delegation (via AgentInfo)
/// 2. Actually delegating to agents (via AgentHandle)
/// 3. Thread-safe access from multiple sessions
pub trait AgentRegistry: Send + Sync {
    /// List all available agents (metadata only).
    fn list_agents(&self) -> Vec<AgentInfo>;

    /// Get agent metadata by ID.
    fn get_agent(&self, id: &str) -> Option<AgentInfo>;

    /// Get an agent handle for delegation.
    ///
    /// Returns an `Arc<dyn AgentHandle>` that can be used to interact with the agent
    /// via session management, prompting, and event subscription.
    fn get_handle(&self, id: &str) -> Option<Arc<dyn crate::agent::handle::AgentHandle>>;
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
    pub wait_policy: crate::config::DelegationWaitPolicy,
    pub wait_timeout_secs: u64,
    pub cancel_grace_secs: u64,
    pub max_parallel_delegations: NonZeroUsize,
}

impl DelegationOrchestratorConfig {
    pub fn new(cwd: Option<PathBuf>) -> Self {
        Self {
            cwd,
            inject_results: false,
            run_verification: false,
            wait_policy: crate::config::DelegationWaitPolicy::default(),
            wait_timeout_secs: 120,
            cancel_grace_secs: 5,
            max_parallel_delegations: NonZeroUsize::new(5).expect("non-zero default"),
        }
    }
}

pub struct DelegationOrchestrator {
    delegator: Arc<dyn crate::agent::handle::AgentHandle>,
    event_sink: Arc<EventSink>,
    store: Arc<dyn SessionStore>,
    agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
    tool_registry: Arc<ToolRegistry>,
    config: DelegationOrchestratorConfig,
    max_parallel: Arc<tokio::sync::Semaphore>,
    /// Maps delegation_id -> (parent_session_id, cancellation_token, join_handle)
    active_delegations: ActiveDelegations,
    /// Optional summarizer for generating planning context
    delegation_summarizer: Option<Arc<super::summarizer::DelegationSummarizer>>,
    /// Optional routing snapshot for per-agent routing decisions.
    routing_snapshot: Option<crate::agent::remote::RoutingSnapshotHandle>,
}

impl DelegationOrchestrator {
    pub fn new(
        delegator: Arc<dyn crate::agent::handle::AgentHandle>,
        event_sink: Arc<EventSink>,
        store: Arc<dyn SessionStore>,
        agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
        tool_registry: Arc<ToolRegistry>,
        cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            delegator,
            event_sink,
            store,
            agent_registry,
            tool_registry,
            config: DelegationOrchestratorConfig::new(cwd),
            max_parallel: Arc::new(tokio::sync::Semaphore::new(5)),
            active_delegations: Arc::new(Mutex::new(HashMap::new())),
            delegation_summarizer: None,
            routing_snapshot: None,
        }
    }

    /// Legacy constructor for backward compatibility with QueryMTAgent.
    pub fn with_result_injection(mut self, enabled: bool) -> Self {
        self.config.inject_results = enabled;
        self
    }

    pub fn with_verification(mut self, enabled: bool) -> Self {
        self.config.run_verification = enabled;
        self
    }

    pub fn with_wait_policy(mut self, policy: crate::config::DelegationWaitPolicy) -> Self {
        self.config.wait_policy = policy;
        self
    }

    pub fn with_wait_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.config.wait_timeout_secs = timeout_secs;
        self
    }

    pub fn with_cancel_grace_secs(mut self, grace_secs: u64) -> Self {
        self.config.cancel_grace_secs = grace_secs;
        self
    }

    pub fn with_max_parallel_delegations(mut self, max_parallel: NonZeroUsize) -> Self {
        self.config.max_parallel_delegations = max_parallel;
        self.max_parallel = Arc::new(tokio::sync::Semaphore::new(max_parallel.get()));
        self
    }

    pub fn with_summarizer(
        mut self,
        summarizer: Option<Arc<super::summarizer::DelegationSummarizer>>,
    ) -> Self {
        self.delegation_summarizer = summarizer;
        self
    }

    /// Set the routing snapshot handle for per-agent routing decisions.
    ///
    /// When set, the orchestrator will consult the routing table before creating
    /// delegation sessions and write `provider_node_id` from the routing policy
    /// to the session's DB row.
    pub fn with_routing_snapshot(
        mut self,
        snapshot: crate::agent::remote::RoutingSnapshotHandle,
    ) -> Self {
        self.routing_snapshot = Some(snapshot);
        self
    }

    /// Start listening for events on the given `EventFanout`.
    ///
    /// Spawns a background task that subscribes to the fanout and dispatches
    /// delegation-related events. Returns the `JoinHandle` for the listener task.
    pub fn start_listening(self: &Arc<Self>, fanout: &Arc<EventFanout>) -> JoinHandle<()> {
        let this = Arc::clone(self);
        let mut rx = fanout.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(envelope) => {
                        this.handle_envelope(&envelope).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("DelegationOrchestrator: lagged, skipped {} events", n);
                    }
                }
            }
        })
    }

    /// Process a single event envelope.
    async fn handle_envelope(&self, envelope: &EventEnvelope) {
        let session_id = envelope.session_id();
        match envelope.kind() {
            AgentEventKind::DelegationRequested { delegation } => {
                let delegator = self.delegator.clone();
                let event_sink = self.event_sink.clone();
                let store = self.store.clone();
                let tool_registry = self.tool_registry.clone();
                let config = self.config.clone();
                let max_parallel = self.max_parallel.clone();
                let delegation_summarizer = self.delegation_summarizer.clone();
                let routing_snapshot = self.routing_snapshot.clone();
                let parent_session_id = session_id.to_string();
                let parent_session_id_for_insert = parent_session_id.clone();
                let delegation = delegation.clone();
                let cancel_token = CancellationToken::new();
                let active_delegations = self.active_delegations.clone();
                let active_delegations_for_spawn = active_delegations.clone();

                // Store the cancellation token and join handle
                let delegation_id = delegation.public_id.clone();
                let cancel_token_clone = cancel_token.clone();

                // Get the agent handle for this agent — try new `get_handle` first,
                // fall back to `get_agent_handle` for backward compatibility.
                let target_handle: Option<Arc<dyn crate::agent::handle::AgentHandle>> =
                    self.agent_registry.get_handle(&delegation.target_agent_id);

                let handle = tokio::spawn(async move {
                    let _permit = match max_parallel.acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => {
                            fail_delegation(
                                &event_sink,
                                &delegator,
                                &store,
                                &config,
                                &parent_session_id,
                                &delegation.public_id,
                                "Delegation queue closed before execution could start",
                            )
                            .await;
                            return;
                        }
                    };

                    let ctx = DelegationContext {
                        delegator,
                        event_sink,
                        store,
                        tool_registry,
                        config,
                        active_delegations: active_delegations_for_spawn,
                        delegation_summarizer,
                        routing_snapshot,
                    };
                    match target_handle {
                        Some(target) => {
                            execute_delegation(
                                ctx,
                                target,
                                parent_session_id,
                                delegation,
                                cancel_token,
                            )
                            .await;
                        }
                        None => {
                            // No AgentHandle registered for this agent.
                            let error_message = format!(
                                "Agent '{}' is not registered with a handle. \
                                 Register it via register_handle() in the AgentRegistry.",
                                delegation.target_agent_id
                            );
                            fail_delegation(
                                &ctx.event_sink,
                                &ctx.delegator,
                                &ctx.store,
                                &ctx.config,
                                &parent_session_id,
                                &delegation.public_id,
                                &error_message,
                            )
                            .await;
                        }
                    }
                });

                let mut active = active_delegations.lock().await;
                active.insert(
                    delegation_id,
                    (parent_session_id_for_insert, cancel_token_clone, handle),
                );
            }
            AgentEventKind::DelegationCancelRequested { delegation_id } => {
                let delegation_id_owned = delegation_id.clone();
                let mut active = self.active_delegations.lock().await;
                let entry = active.remove(&delegation_id_owned);
                drop(active);

                if let Some((_parent_id, cancel_token, mut handle)) = entry {
                    let grace_secs = self.config.cancel_grace_secs;
                    cancel_token.cancel();
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = &mut handle => {
                                debug!("Delegation {} terminated gracefully after cancel request", delegation_id_owned);
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(grace_secs)) => {
                                warn!("Delegation {} did not terminate within {}s timeout after cancel request, force aborting", delegation_id_owned, grace_secs);
                                handle.abort();
                            }
                        }
                    });
                }
            }
            AgentEventKind::Cancelled => {
                // Cancel all delegations for this session
                let mut active = self.active_delegations.lock().await;

                // Find all delegations for this session
                let to_cancel: Vec<(String, tokio::task::JoinHandle<()>)> = active
                    .iter_mut()
                    .filter(|(_, (parent_id, _, _))| parent_id == session_id)
                    .map(|(delegation_id, (_, cancel_token, handle))| {
                        cancel_token.cancel();
                        let dummy_handle = tokio::spawn(async {});
                        let real_handle = std::mem::replace(handle, dummy_handle);
                        (delegation_id.clone(), real_handle)
                    })
                    .collect();

                drop(active);

                let grace_secs = self.config.cancel_grace_secs;
                for (delegation_id, mut handle) in to_cancel {
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = &mut handle => {
                                debug!("Delegation {} terminated gracefully after cancel", delegation_id);
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(grace_secs)) => {
                                warn!("Delegation {} did not terminate within {}s timeout, force aborting", delegation_id, grace_secs);
                                handle.abort();
                            }
                        }
                    });
                }
            }
            _ => {}
        }
    }
}

/// Context structure to group delegation handler parameters
struct DelegationContext {
    delegator: Arc<dyn crate::agent::handle::AgentHandle>,
    event_sink: Arc<EventSink>,
    store: Arc<dyn SessionStore>,
    tool_registry: Arc<ToolRegistry>,
    config: DelegationOrchestratorConfig,
    active_delegations: ActiveDelegations,
    delegation_summarizer: Option<Arc<super::summarizer::DelegationSummarizer>>,
    routing_snapshot: Option<crate::agent::remote::RoutingSnapshotHandle>,
}

// ══════════════════════════════════════════════════════════════════════════
//  Kameo-native delegation path (Phase 5)
// ══════════════════════════════════════════════════════════════════════════

/// Execute delegation using the `AgentHandle` trait.
///
/// This creates sessions via `AgentHandle::create_delegation_session()`. History and
/// planning context are exchanged via kameo messages (`GetHistory`,
/// `SetPlanningContext`), so this path works for both local and remote sessions.
async fn execute_delegation(
    ctx: DelegationContext,
    target: Arc<dyn crate::agent::handle::AgentHandle>,
    parent_session_id: String,
    delegation: Delegation,
    cancel_token: CancellationToken,
) {
    let delegation_id = delegation.public_id.clone();

    if let Err(e) = ctx
        .store
        .update_delegation_status(&delegation.public_id, DelegationStatus::Running)
        .await
    {
        warn!("Failed to update delegation status to Running: {}", e);
    }

    // 1. Create session via AgentHandle trait
    let cwd_string = ctx.config.cwd.as_ref().map(|p| p.display().to_string());
    let (child_session_id, session_ref) = match target.create_delegation_session(cwd_string).await {
        Ok(result) => result,
        Err(e) => {
            let error_message = format!("Failed to create session via kameo: {}", e);
            fail_delegation(
                &ctx.event_sink,
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

    // 1b. Apply routing policy from the routing table (if present).
    //     When provider_target = Peer(name) and the peer is resolved,
    //     write the node_id to the session's DB row so SessionProvider
    //     constructs a MeshChatProvider for this session.
    if let Some(ref snap_handle) = ctx.routing_snapshot {
        let snap = snap_handle.load();
        if let Some(policy) = snap.get(&delegation.target_agent_id) {
            use crate::agent::remote::routing::RouteTarget;
            if let RouteTarget::Peer(_) = &policy.provider_target
                && let Some(ref node_id) = policy.resolved_provider_node_id
            {
                if let Err(e) = ctx
                    .store
                    .set_session_provider_node_id(&child_session_id, Some(node_id.as_str()))
                    .await
                {
                    warn!(
                        "execute_delegation: failed to set provider_node_id='{}' \
                             on session {} from routing table: {}",
                        node_id, child_session_id, e
                    );
                } else {
                    debug!(
                        "execute_delegation: set provider_node_id='{}' on session {} \
                             for agent '{}' from routing table",
                        node_id, child_session_id, delegation.target_agent_id
                    );
                }
            }
        }
    }

    emit_delegation_event(
        &ctx.delegator,
        &ctx.event_sink,
        &parent_session_id,
        AgentEventKind::SessionForked {
            parent_session_id: parent_session_id.clone(),
            child_session_id: child_session_id.clone(),
            target_agent_id: delegation.target_agent_id.clone(),
            origin: ForkOrigin::Delegation,
            fork_point_type: ForkPointType::ProgressEntry,
            fork_point_ref: delegation.public_id.clone(),
            instructions: delegation.context.clone(),
        },
    );

    // 2. Generate and inject planning summary via kameo message
    if let Some(ref summarizer) = ctx.delegation_summarizer {
        match ctx.store.get_history(&parent_session_id).await {
            Ok(history) => {
                match summarizer.summarize(&history, &delegation.objective).await {
                    Ok(summary) => {
                        // Persist to delegation record
                        let mut updated_delegation = delegation.clone();
                        updated_delegation.planning_summary = Some(summary.clone());
                        if let Err(e) = ctx.store.update_delegation(updated_delegation).await {
                            warn!("Failed to persist delegation summary: {}", e);
                        }

                        // Inject via SetPlanningContext kameo message
                        let formatted_summary =
                            format!("\n\n<planning-context>\n{}\n</planning-context>", summary);
                        if let Err(e) = session_ref.set_planning_context(formatted_summary).await {
                            warn!(
                                "Failed to inject planning summary via SetPlanningContext: {}",
                                e
                            );
                        } else {
                            log::info!(
                                "Injected planning summary into delegate session {} via kameo",
                                child_session_id
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Delegation summary generation failed: {}", e);
                        // Proceed without summary — graceful degradation
                    }
                }
            }
            Err(e) => {
                warn!("Failed to load parent history for summary: {}", e);
            }
        }
    }

    // 3. Send prompt directly via kameo
    let prompt_text = build_delegation_prompt(&delegation);
    let prompt_req = PromptRequest::new(
        child_session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(prompt_text))],
    );

    let prompt_result = tokio::select! {
        result = session_ref.prompt(prompt_req) => Some(result),
        _ = cancel_token.cancelled() => {
            // Cancellation — cancel the child session
            let _ = session_ref.cancel().await;

            if let Err(e) = ctx.store
                .update_delegation_status(&delegation_id, DelegationStatus::Cancelled)
                .await
            {
                warn!("Failed to update delegation status to Cancelled: {}", e);
            }

            emit_delegation_event(
                &ctx.delegator,
                &ctx.event_sink,
                &parent_session_id,
                AgentEventKind::DelegationCancelled {
                    delegation_id: delegation_id.clone(),
                },
            );

            let mut active = ctx.active_delegations.lock().await;
            active.remove(&delegation_id);
            return;
        }
    };

    match prompt_result {
        Some(Ok(_)) => {
            // 4. Verification (same as legacy path)
            let verification_passed = if ctx.config.run_verification {
                match VerificationSpecBuilder::from_delegation(&delegation) {
                    Some(verification_spec) => {
                        let agent_tool_registry = ctx.tool_registry.clone();
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
                    None => true,
                }
            } else {
                true
            };

            if !verification_passed {
                let error_message =
                    "Verification failed: The changes did not pass the specified verification checks."
                        .to_string();
                fail_delegation(
                    &ctx.event_sink,
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

            // 5. Get history for summary via kameo (works for local and remote)
            let summary = match session_ref.get_history().await {
                Ok(history) => extract_session_summary_from_history(&history),
                Err(err) => {
                    warn!("Error extracting summary via GetHistory: {}", err);
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

            emit_delegation_event(
                &ctx.delegator,
                &ctx.event_sink,
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

            let mut active = ctx.active_delegations.lock().await;
            active.remove(&delegation_id);
        }
        Some(Err(e)) => {
            let error_message = format!("Delegation failed: {}", e);
            fail_delegation(
                &ctx.event_sink,
                &ctx.delegator,
                &ctx.store,
                &ctx.config,
                &parent_session_id,
                &delegation_id,
                &error_message,
            )
            .await;

            let mut active = ctx.active_delegations.lock().await;
            active.remove(&delegation_id);
        }
        None => {
            // Already handled in the select! cancellation branch
        }
    }
}

async fn fail_delegation(
    event_sink: &Arc<EventSink>,
    delegator: &Arc<dyn crate::agent::handle::AgentHandle>,
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

    emit_delegation_event(
        delegator,
        event_sink,
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

fn emit_delegation_event(
    delegator: &Arc<dyn crate::agent::handle::AgentHandle>,
    _event_sink: &Arc<EventSink>,
    session_id: &str,
    kind: AgentEventKind,
) {
    // emit_event is now on the AgentHandle trait — no downcast needed.
    delegator.emit_event(session_id, kind);
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
    delegator: &Arc<dyn crate::agent::handle::AgentHandle>,
    session_id: &str,
    delegation_id: &str,
    summary: &str,
) {
    let message = format_delegation_completion_message(delegation_id, summary);

    if let Err(e) = delegator
        .prompt(PromptRequest::new(
            session_id.to_string(),
            vec![ContentBlock::Text(TextContent::new(message))],
        ))
        .await
    {
        warn!("Failed to inject delegation results: {}", e);
    }
}

async fn inject_failure(
    delegator: &Arc<dyn crate::agent::handle::AgentHandle>,
    session_id: &str,
    delegation_id: &str,
    error: &str,
) {
    let message = format_delegation_failure_message(delegation_id, error);

    if let Err(e) = delegator
        .prompt(PromptRequest::new(
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
          -> Use read_tool to see the current state of the target file\n\
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

/// Extract a delegation summary directly from a history slice.
///
/// Works for both local and remote sessions — the caller provides the history.
/// For local sessions, read from store. For remote, via `GetHistory` message.
pub fn extract_session_summary_from_history(history: &[AgentMessage]) -> String {
    let mut summary = String::new();
    summary.push_str("=== Delegate Agent Results ===\n\n");

    let mut tools_used = Vec::new();
    let mut files_modified = Vec::new();
    let mut patches = Vec::new();
    let mut agent_responses = Vec::new();

    for message in history {
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

    summary
}

fn extract_tool_args_preview(tool_name: &str, args_json: &str) -> String {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return String::new();
    };

    match tool_name {
        "write_file" | "read_tool" | "delete_file" | "apply_patch" => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "web_fetch" | "browse" => args
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

/// Internal structure to hold both metadata and agent handle.
struct AgentEntry {
    info: AgentInfo,
    handle: Arc<dyn crate::agent::handle::AgentHandle>,
}

#[derive(Default)]
pub struct DefaultAgentRegistry {
    agents: std::collections::HashMap<String, AgentEntry>,
}

impl DefaultAgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an agent with its metadata and a unified `AgentHandle`.
    pub fn register(
        &mut self,
        info: AgentInfo,
        handle: Arc<dyn crate::agent::handle::AgentHandle>,
    ) {
        let id = info.id.clone();
        self.agents.insert(id, AgentEntry { info, handle });
    }

    /// Alias for `register` — kept for backward compatibility during migration.
    pub fn register_handle(
        &mut self,
        info: AgentInfo,
        handle: Arc<dyn crate::agent::handle::AgentHandle>,
    ) {
        self.register(info, handle);
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

    fn get_handle(&self, id: &str) -> Option<Arc<dyn crate::agent::handle::AgentHandle>> {
        self.agents.get(id).map(|entry| entry.handle.clone())
    }
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::handle::AgentHandle;
    use crate::agent::remote::SessionActorRef;
    use crate::event_fanout::EventFanout;
    use crate::events::EventEnvelope;
    use agent_client_protocol::{
        CancelNotification, Error, NewSessionRequest, NewSessionResponse, PromptRequest,
        PromptResponse,
    };
    use async_trait::async_trait;

    // ── Minimal stub AgentHandle ──────────────────────────────────────────────

    struct StubAgentHandle {
        name: String,
        event_fanout: Arc<EventFanout>,
    }

    impl StubAgentHandle {
        fn new(name: &str) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_string(),
                event_fanout: Arc::new(EventFanout::new()),
            })
        }
    }

    #[async_trait]
    impl AgentHandle for StubAgentHandle {
        async fn new_session(
            &self,
            _req: NewSessionRequest,
        ) -> std::result::Result<NewSessionResponse, Error> {
            Ok(NewSessionResponse::new(format!("{}-session", self.name)))
        }

        async fn prompt(&self, _req: PromptRequest) -> std::result::Result<PromptResponse, Error> {
            Ok(PromptResponse::new(
                agent_client_protocol::StopReason::EndTurn,
            ))
        }

        async fn cancel(&self, _notif: CancelNotification) -> std::result::Result<(), Error> {
            Ok(())
        }

        async fn create_delegation_session(
            &self,
            _cwd: Option<String>,
        ) -> std::result::Result<(String, SessionActorRef), Error> {
            Err(Error::internal_error().data("stub: not implemented"))
        }

        fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<EventEnvelope> {
            self.event_fanout.subscribe()
        }

        fn event_fanout(&self) -> &Arc<EventFanout> {
            &self.event_fanout
        }

        fn emit_event(&self, _session_id: &str, _kind: AgentEventKind) {}

        fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
            Arc::new(DefaultAgentRegistry::new())
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn make_agent_info(id: &str) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: format!("Description for {id}"),
            capabilities: vec!["coding".to_string()],
            required_capabilities: vec![],
            meta: None,
        }
    }

    // ── DefaultAgentRegistry tests ───────────────────────────────────────────

    #[test]
    fn test_new_registry_is_empty() {
        let registry = DefaultAgentRegistry::new();
        assert!(registry.list_agents().is_empty());
        assert!(registry.get_agent("any").is_none());
        assert!(registry.get_handle("any").is_none());
    }

    #[test]
    fn test_register_and_list_agent() {
        let mut registry = DefaultAgentRegistry::new();
        let info = make_agent_info("agent-1");
        let agent = StubAgentHandle::new("agent-1");
        registry.register(info, agent);

        let agents = registry.list_agents();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "agent-1");
        assert_eq!(agents[0].name, "Agent agent-1");
    }

    #[test]
    fn test_get_agent_by_id() {
        let mut registry = DefaultAgentRegistry::new();
        registry.register(make_agent_info("alpha"), StubAgentHandle::new("alpha"));
        registry.register(make_agent_info("beta"), StubAgentHandle::new("beta"));

        let alpha = registry.get_agent("alpha");
        assert!(alpha.is_some());
        assert_eq!(alpha.unwrap().id, "alpha");

        let beta = registry.get_agent("beta");
        assert!(beta.is_some());
        assert_eq!(beta.unwrap().id, "beta");

        assert!(registry.get_agent("gamma").is_none());
    }

    #[test]
    fn test_get_handle() {
        let mut registry = DefaultAgentRegistry::new();
        registry.register(make_agent_info("worker"), StubAgentHandle::new("worker"));

        assert!(registry.get_handle("worker").is_some());
        assert!(registry.get_handle("missing").is_none());
    }

    #[test]
    fn test_register_multiple_agents() {
        let mut registry = DefaultAgentRegistry::new();
        for i in 0..5 {
            registry.register(
                make_agent_info(&format!("agent-{i}")),
                StubAgentHandle::new(&format!("agent-{i}")),
            );
        }
        assert_eq!(registry.list_agents().len(), 5);
    }

    #[test]
    fn test_register_overwrites_same_id() {
        let mut registry = DefaultAgentRegistry::new();
        let mut info1 = make_agent_info("x");
        info1.description = "first".to_string();
        let mut info2 = make_agent_info("x");
        info2.description = "second".to_string();

        registry.register(info1, StubAgentHandle::new("x"));
        registry.register(info2, StubAgentHandle::new("x"));

        // Still only one entry
        assert_eq!(registry.list_agents().len(), 1);
        let got = registry.get_agent("x").unwrap();
        assert_eq!(got.description, "second");
    }

    // ── AgentInfo serialization ──────────────────────────────────────────────

    #[test]
    fn test_agent_info_serde_round_trip() {
        let info = AgentInfo {
            id: "coder".to_string(),
            name: "Coder Agent".to_string(),
            description: "Writes code".to_string(),
            capabilities: vec!["rust".to_string(), "python".to_string()],
            required_capabilities: vec![],
            meta: Some(serde_json::json!({"version": 2})),
        };

        let json = serde_json::to_string(&info).expect("serialize AgentInfo");
        let restored: AgentInfo = serde_json::from_str(&json).expect("deserialize AgentInfo");
        assert_eq!(restored.id, info.id);
        assert_eq!(restored.name, info.name);
        assert_eq!(restored.capabilities, info.capabilities);
        assert!(restored.meta.is_some());
    }

    #[test]
    fn test_agent_info_meta_none_omitted_in_json() {
        let info = AgentInfo {
            id: "minimal".to_string(),
            name: "Minimal".to_string(),
            description: "No meta".to_string(),
            capabilities: vec![],
            required_capabilities: vec![],
            meta: None,
        };

        let json = serde_json::to_string(&info).expect("serialize");
        // _meta field should be absent when None
        assert!(
            !json.contains("_meta"),
            "meta=None should be omitted from JSON"
        );
    }

    // ── DelegationOrchestratorConfig ─────────────────────────────────────────

    #[test]
    fn test_orchestrator_config_defaults() {
        let config = DelegationOrchestratorConfig::new(None);
        assert!(!config.inject_results);
        assert!(!config.run_verification);
        assert_eq!(config.wait_policy, crate::config::DelegationWaitPolicy::Any);
        assert_eq!(config.wait_timeout_secs, 120);
        assert_eq!(config.cancel_grace_secs, 5);
        assert_eq!(config.max_parallel_delegations.get(), 5);
        assert!(config.cwd.is_none());
    }

    #[test]
    fn test_orchestrator_config_with_cwd() {
        use std::path::PathBuf;
        let path = PathBuf::from("/workspace");
        let config = DelegationOrchestratorConfig::new(Some(path.clone()));
        assert_eq!(config.cwd, Some(path));
    }
}

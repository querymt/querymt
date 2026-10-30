use crate::acp::protocol::{ContentBlock, PromptRequest, StopReason, TextContent};
use crate::event_fanout::EventFanout;
use crate::event_sink::EventSink;
use crate::events::{AgentEventKind, EventEnvelope};
use crate::hooks::{
    DelegationFailureRequest, DelegationStartRequest, HookNotice, Hooks, PostDelegationRequest,
};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::{
    Delegation, DelegationClaim, DelegationStatus, ForkOrigin, ForkPointType,
};
use crate::session::store::SessionStore;
use crate::tools::ToolRegistry;
use crate::verification::VerificationSpec;
use crate::verification::service::{VerificationContext, VerificationService};
use log::{debug, error, warn};
use querymt::chat::{ChatRole, ReasoningEffort};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, instrument};
use tracing_opentelemetry::OpenTelemetrySpanExt;

type ActiveDelegations = Arc<Mutex<HashMap<String, ActiveDelegation>>>;

struct ActiveDelegation {
    parent_session_id: String,
    cancel_token: CancellationToken,
    handle: JoinHandle<()>,
    cancellation: Arc<DelegationCancellation>,
    activity: WorkflowActivity,
}

#[derive(Default)]
struct DelegationCancellation {
    child: OnceLock<(Arc<dyn crate::agent::handle::AgentHandle>, String)>,
    finalized: AtomicBool,
}

impl DelegationCancellation {
    async fn resolve(&self, delegation_id: &str) {
        if let Some((target, child_session_id)) = self.child.get()
            && !self.finalized.swap(true, Ordering::AcqRel)
        {
            let target = target.clone();
            let child_session_id = child_session_id.clone();
            let delegation_id = delegation_id.to_owned();
            // A grace-period abort of the execution task must not drop the owner patch.
            let _ = tokio::spawn(async move {
                resolve_terminal_delegate_attempt(
                    target.as_ref(),
                    &child_session_id,
                    &delegation_id,
                )
                .await;
            })
            .await;
        }
    }

    async fn resolve_if_cancelled(&self, store: &dyn SessionStore, delegation_id: &str) {
        if self.finalized.load(Ordering::Acquire) {
            return;
        }
        match store.get_delegation(delegation_id).await {
            Ok(Some(delegation)) if delegation.status == DelegationStatus::Cancelled => {
                // Timeout cleanup can own the terminal claim before the task sees
                // cancellation. The active attempt transfers that finalization here;
                // its shared one-shot guard prevents replays from undoing a reopen.
                self.resolve(delegation_id).await;
            }
            Ok(_) => {}
            Err(error) => warn!(
                "Failed to read cancelled delegation {} for child finalization: {}",
                delegation_id, error
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<crate::tools::CapabilityRequirement>,
    /// Configured isolation for tasks delegated to this agent (`inherit` unless set).
    #[serde(default)]
    pub isolation: crate::worktree::DelegationIsolation,
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
    hooks: Hooks,
    config: DelegationOrchestratorConfig,
    max_parallel: Arc<tokio::sync::Semaphore>,
    /// Maps delegation_id -> (parent_session_id, cancellation_token, join_handle)
    active_delegations: ActiveDelegations,
    shutting_down: AtomicBool,
    /// Optional summarizer for generating planning context
    delegation_summarizer: Option<Arc<super::summarizer::DelegationSummarizer>>,
    delegate_model_overrides: super::DelegateModelOverrideStore,
    profile_id: Option<String>,
    /// Optional routing snapshot for per-agent routing decisions.
    routing_snapshot: Option<crate::agent::remote::RoutingSnapshotHandle>,
    /// Workspace service for isolated delegation. `None` disables isolated
    /// execution: delegates resolving to `isolated` fail closed instead of
    /// silently sharing the parent's checkout.
    workspace_service: Option<Arc<crate::worktree::WorkspaceService>>,
    /// Manual budget pricing snapshotted onto delegation-summary attempts.
    budget_pricing: Option<querymt::providers::ModelPricing>,
    /// Per-cycle price cap applied before each summary dispatch.
    budget_cap_usd: Option<f64>,
}

impl DelegationOrchestrator {
    pub fn new(
        delegator: Arc<dyn crate::agent::handle::AgentHandle>,
        event_sink: Arc<EventSink>,
        store: Arc<dyn SessionStore>,
        agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
        tool_registry: Arc<ToolRegistry>,
        hooks: Hooks,
        cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            delegator,
            event_sink,
            store,
            agent_registry,
            tool_registry,
            hooks,
            config: DelegationOrchestratorConfig::new(cwd),
            max_parallel: Arc::new(tokio::sync::Semaphore::new(5)),
            active_delegations: Arc::new(Mutex::new(HashMap::new())),
            shutting_down: AtomicBool::new(false),
            delegation_summarizer: None,
            delegate_model_overrides: super::DelegateModelOverrideStore::default(),
            profile_id: None,
            routing_snapshot: None,
            workspace_service: None,
            budget_pricing: None,
            budget_cap_usd: None,
        }
    }

    /// Apply this per-cycle price cap before each delegation-summary dispatch.
    pub fn with_budget_cap(mut self, cap_usd: Option<f64>) -> Self {
        self.budget_cap_usd = cap_usd;
        self
    }

    /// Snapshot this manual budget pricing onto delegation-summary attempts.
    pub fn with_budget_pricing(
        mut self,
        pricing: Option<querymt::providers::ModelPricing>,
    ) -> Self {
        self.budget_pricing = pricing;
        self
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

    pub fn with_delegate_model_overrides(
        mut self,
        overrides: super::DelegateModelOverrideStore,
    ) -> Self {
        self.delegate_model_overrides = overrides;
        self
    }

    pub fn with_profile_id(mut self, profile_id: impl Into<String>) -> Self {
        self.profile_id = Some(profile_id.into());
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

    /// Enable isolated delegation through the given workspace service.
    pub fn with_workspace_service(
        mut self,
        service: Arc<crate::worktree::WorkspaceService>,
    ) -> Self {
        self.workspace_service = Some(service);
        self
    }

    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    #[cfg(test)]
    async fn wait_until_registered(&self, delegation_id: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if self
                    .active_delegations
                    .lock()
                    .await
                    .contains_key(delegation_id)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delegation was not registered");
    }

    pub async fn cancel_active_delegations(&self) {
        self.begin_shutdown();
        let active = std::mem::take(&mut *self.active_delegations.lock().await);
        for (delegation_id, entry) in active {
            let ActiveDelegation {
                cancel_token,
                mut handle,
                cancellation,
                activity,
                ..
            } = entry;
            activity.request_cancel();
            cancel_token.cancel();
            let timed_out = tokio::time::timeout(std::time::Duration::from_secs(1), &mut handle)
                .await
                .is_err();
            handle.abort();
            if self
                .store
                .transition_delegation_status(
                    &delegation_id,
                    DelegationStatus::Running,
                    DelegationStatus::Cancelled,
                )
                .await
                .is_err()
            {
                warn!(
                    "Failed to persist cancellation for delegation {} during shutdown",
                    delegation_id
                );
            }
            let store = self.store.clone();
            // Owner finalization must not extend shutdown's execution grace period.
            tokio::spawn(async move {
                let _activity = activity;
                if timed_out {
                    let _ = handle.await;
                }
                cancellation
                    .resolve_if_cancelled(store.as_ref(), &delegation_id)
                    .await;
            });
        }
    }

    /// Start listening for events on the given `EventFanout`.
    ///
    /// Spawns a background task that subscribes to the fanout and dispatches
    /// delegation-related events. Returns the `JoinHandle` for the listener task.
    pub fn start_listening(self: &Arc<Self>, fanout: &Arc<EventFanout>) -> JoinHandle<()> {
        let this = Arc::clone(self);
        let mut rx = fanout.subscribe_delegation();
        tokio::spawn(async move {
            while !this.shutting_down.load(Ordering::Acquire) {
                match rx.recv().await {
                    Ok((envelope, parent)) => {
                        // Replay/remote events are roots, not children of the listener's context.
                        let handler = tracing::info_span!(
                            parent: None,
                            "delegation.orchestrator.handle_event",
                            session_id = %envelope.session_id(),
                            event_kind = tracing::field::Empty,
                        );
                        if let Some(parent) = parent {
                            let context = parent.context();
                            let _ = handler.set_parent(context);
                        }
                        this.handle_envelope(&envelope).instrument(handler).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("DelegationOrchestrator: lagged, skipped {} events", n);
                    }
                }
            }
        })
    }

    async fn fail_unclaimed_delegation(
        &self,
        delegation: &Delegation,
        parent_session_id: &str,
        error_message: &str,
    ) {
        match self
            .store
            .transition_delegation_status(
                &delegation.public_id,
                DelegationStatus::Requested,
                DelegationStatus::Failed,
            )
            .await
        {
            Ok(true) => {
                emit_delegation_event(
                    &self.delegator,
                    &self.event_sink,
                    parent_session_id,
                    AgentEventKind::DelegationFailed {
                        delegation_id: delegation.public_id.clone(),
                        error: error_message.to_string(),
                    },
                );
                if self.config.inject_results {
                    inject_failure(
                        &self.delegator,
                        parent_session_id,
                        &delegation.public_id,
                        error_message,
                    )
                    .await;
                }
            }
            Ok(false) => {}
            Err(_) => warn!("Failed to persist delegation claim failure"),
        }
    }

    async fn claim_delegation(&self, delegation_id: &str) -> Result<DelegationClaim, String> {
        let mut last_error = None;
        for attempt in 0..3 {
            match self.store.claim_delegation(delegation_id).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    last_error = Some(err.to_string());
                    if attempt < 2 {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| "unknown claim error".to_string()))
    }

    /// Process a single event envelope.
    async fn handle_envelope(&self, envelope: &EventEnvelope) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let session_id = envelope.session_id();
        match envelope.kind() {
            AgentEventKind::DelegationRequested {
                delegation,
                batch_id,
                cycle_id,
                ..
            } => {
                tracing::Span::current().record("event_kind", "DelegationRequested");
                if let Some(profile_id) = self.profile_id.as_deref() {
                    let mut last_error = None;
                    let mut binding_result = None;
                    for attempt in 0..3 {
                        match self.store.get_profile_binding(session_id).await {
                            Ok(result) => {
                                binding_result = Some(result);
                                break;
                            }
                            Err(err) => {
                                last_error = Some(err);
                                if attempt < 2 {
                                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                                }
                            }
                        }
                    }

                    match binding_result {
                        Some(Some(owner)) if owner == profile_id => {}
                        Some(Some(_)) => return,
                        // Live profile sessions must already be bound. Legacy support belongs in
                        // session loading, where one deterministic profile could be backfilled.
                        Some(None) => {
                            error!(
                                "Ignoring delegation {} from unbound session {}",
                                delegation.public_id, session_id
                            );
                            return;
                        }
                        None => {
                            let error_message = format!(
                                "Failed to resolve profile owner for delegation {} after retries: {}",
                                delegation.public_id,
                                last_error
                                    .as_ref()
                                    .map(|e| e.to_string())
                                    .unwrap_or_else(|| "unknown error".to_string())
                            );
                            self.fail_unclaimed_delegation(delegation, session_id, &error_message)
                                .await;
                            return;
                        }
                    }
                }
                match self.claim_delegation(&delegation.public_id).await {
                    Ok(DelegationClaim::Claimed) => {}
                    Ok(DelegationClaim::AlreadyClaimed) => {
                        debug!(
                            "Delegation {} was already claimed; ignoring duplicate request",
                            delegation.public_id
                        );
                        return;
                    }
                    Ok(DelegationClaim::NotFound) => {
                        emit_delegation_event(
                            &self.delegator,
                            &self.event_sink,
                            session_id,
                            AgentEventKind::DelegationFailed {
                                delegation_id: delegation.public_id.clone(),
                                error: "Delegation record was not found".to_string(),
                            },
                        );
                        return;
                    }
                    Ok(DelegationClaim::InvalidState(status)) => {
                        debug!(
                            "Delegation {} is already terminal ({status:?}); ignoring request",
                            delegation.public_id
                        );
                        return;
                    }
                    Err(err) => {
                        self.fail_unclaimed_delegation(
                            delegation,
                            session_id,
                            &format!("Failed to claim delegation before execution: {err}"),
                        )
                        .await;
                        return;
                    }
                }

                let Some(target_handle) =
                    self.agent_registry.get_handle(&delegation.target_agent_id)
                else {
                    fail_delegation(
                        DelegationFailureContext {
                            event_sink: &self.event_sink,
                            delegator: &self.delegator,
                            store: &self.store,
                            hooks: None,
                            config: &self.config,
                            parent_session_id: session_id,
                            delegation_id: &delegation.public_id,
                            target_agent_id: Some(&delegation.target_agent_id),
                            objective: Some(&delegation.objective),
                            child_session: None,
                            execution_cwd: None,
                        },
                        &format!(
                            "Agent '{}' is not registered with a handle",
                            delegation.target_agent_id
                        ),
                    )
                    .await;
                    return;
                };

                let delegator = self.delegator.clone();
                let event_sink = self.event_sink.clone();
                let store = self.store.clone();
                let tool_registry = self.tool_registry.clone();
                let config = self.config.clone();
                let max_parallel = self.max_parallel.clone();
                let delegation_summarizer = self.delegation_summarizer.clone();
                let delegate_model_overrides = self.delegate_model_overrides.clone();
                let routing_snapshot = self.routing_snapshot.clone();
                let profile_id = self.profile_id.clone();
                let parent_session_id = session_id.to_string();
                let parent_session_id_for_insert = parent_session_id.clone();
                let delegation = delegation.clone();
                let cancel_token = CancellationToken::new();
                let cancellation = Arc::new(DelegationCancellation::default());
                let cancellation_for_spawn = cancellation.clone();
                let active_delegations = self.active_delegations.clone();
                let active_delegations_for_spawn = active_delegations.clone();
                let hooks = self.hooks.clone();
                let workspace_service = self.workspace_service.clone();
                let batch_id = batch_id.clone();
                let requesting_cycle_id = cycle_id.clone();
                let budget_pricing = self.budget_pricing.clone();
                let budget_cap_usd = self.budget_cap_usd;
                let (start_tx, start_rx) = oneshot::channel();

                // Store the cancellation token and join handle
                let delegation_id = delegation.public_id.clone();
                let cancel_token_clone = cancel_token.clone();

                let activity = WorkflowActivity::new(
                    event_sink.clone(),
                    parent_session_id.clone(),
                    delegation.public_id.clone(),
                );
                let activity_for_entry = activity.clone();
                let handle = tokio::spawn(
                    async move {
                        if start_rx.await.is_err() {
                            return;
                        }
                        let _permit = match max_parallel.acquire_owned().await {
                            Ok(permit) => permit,
                            Err(_) => {
                                fail_delegation(
                                    DelegationFailureContext {
                                        event_sink: &event_sink,
                                        delegator: &delegator,
                                        store: &store,
                                        hooks: Some(&hooks),
                                        config: &config,
                                        parent_session_id: &parent_session_id,
                                        delegation_id: &delegation.public_id,
                                        target_agent_id: Some(&delegation.target_agent_id),
                                        objective: Some(&delegation.objective),
                                        child_session: None,
                                        execution_cwd: None,
                                    },
                                    "Delegation queue closed before execution could start",
                                )
                                .await;
                                active_delegations_for_spawn
                                    .lock()
                                    .await
                                    .remove(&delegation.public_id);
                                return;
                            }
                        };

                        activity.stage(Some(
                            crate::control::delegation_notifications::DelegationStage::Executing,
                        ));
                        let ctx = DelegationContext {
                            activity,
                            delegator,
                            event_sink,
                            store,
                            tool_registry,
                            config,
                            hooks,
                            active_delegations: active_delegations_for_spawn,
                            delegation_summarizer,
                            delegate_model_overrides,
                            profile_id,
                            routing_snapshot,
                            cancellation: cancellation_for_spawn,
                            workspace_service,
                            batch_id,
                            requesting_cycle_id,
                            budget_pricing,
                            budget_cap_usd,
                        };
                        execute_delegation(
                            ctx,
                            target_handle,
                            parent_session_id,
                            delegation,
                            cancel_token,
                        )
                        .await;
                    }
                    .in_current_span(),
                );

                let mut active = active_delegations.lock().await;
                if self.shutting_down.load(Ordering::Acquire) {
                    handle.abort();
                    drop(active);
                    fail_delegation(
                        DelegationFailureContext {
                            event_sink: &self.event_sink,
                            delegator: &self.delegator,
                            store: &self.store,
                            hooks: None,
                            config: &self.config,
                            parent_session_id: session_id,
                            delegation_id: &delegation_id,
                            target_agent_id: None,
                            objective: None,
                            child_session: None,
                            execution_cwd: None,
                        },
                        "Delegation cancelled because the orchestrator is shutting down",
                    )
                    .await;
                    return;
                }
                active.insert(
                    delegation_id,
                    ActiveDelegation {
                        parent_session_id: parent_session_id_for_insert,
                        cancel_token: cancel_token_clone,
                        handle,
                        cancellation,
                        activity: activity_for_entry,
                    },
                );
                drop(active);
                let _ = start_tx.send(());
            }
            AgentEventKind::DelegationCancelRequested { delegation_id } => {
                tracing::Span::current().record("event_kind", "DelegationCancelRequested");
                let delegation_id_owned = delegation_id.clone();
                let mut active = self.active_delegations.lock().await;
                let entry = active.remove(&delegation_id_owned);
                drop(active);

                if let Some(entry) = entry {
                    let ActiveDelegation {
                        cancel_token,
                        mut handle,
                        cancellation,
                        activity,
                        ..
                    } = entry;
                    let store = self.store.clone();
                    let grace_secs = self.config.cancel_grace_secs;
                    activity.request_cancel();
                    cancel_token.cancel();
                    tokio::spawn(async move {
                        let _activity = activity;
                        tokio::select! {
                            _ = &mut handle => {
                                debug!("Delegation {} terminated gracefully after cancel request", delegation_id_owned);
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(grace_secs)) => {
                                warn!("Delegation {} did not terminate within {}s timeout after cancel request, force aborting", delegation_id_owned, grace_secs);
                                handle.abort();
                                let _ = handle.await;
                            }
                        }
                        cancellation
                            .resolve_if_cancelled(store.as_ref(), &delegation_id_owned)
                            .await;
                    });
                }
            }
            AgentEventKind::Cancelled => {
                tracing::Span::current().record("event_kind", "Cancelled");
                // Cancel all delegations for this session
                let mut active = self.active_delegations.lock().await;

                // Find all delegations for this session
                let to_cancel: Vec<_> = active
                    .iter_mut()
                    .filter(|(_, entry)| entry.parent_session_id == session_id)
                    .map(|(delegation_id, entry)| {
                        entry.activity.request_cancel();
                        entry.cancel_token.cancel();
                        let dummy_handle = tokio::spawn(async {});
                        let real_handle = std::mem::replace(&mut entry.handle, dummy_handle);
                        (
                            delegation_id.clone(),
                            real_handle,
                            entry.cancellation.clone(),
                            entry.activity.clone(),
                        )
                    })
                    .collect();

                drop(active);

                let grace_secs = self.config.cancel_grace_secs;
                for (delegation_id, mut handle, cancellation, activity) in to_cancel {
                    let store = self.store.clone();
                    let active = self.active_delegations.clone();
                    tokio::spawn(async move {
                        let _activity = activity;
                        tokio::select! {
                            _ = &mut handle => {
                                debug!("Delegation {} terminated gracefully after cancel", delegation_id);
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(grace_secs)) => {
                                warn!("Delegation {} did not terminate within {}s timeout, force aborting", delegation_id, grace_secs);
                                handle.abort();
                                let _ = handle.await;
                            }
                        }
                        cancellation
                            .resolve_if_cancelled(store.as_ref(), &delegation_id)
                            .await;
                        active.lock().await.remove(&delegation_id);
                    });
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
struct WorkflowActivity(Arc<WorkflowActivityInner>);

struct WorkflowActivityInner {
    cancelled: parking_lot::Mutex<bool>,
    sink: Arc<EventSink>,
    parent: String,
    delegation: String,
    epoch: String,
    #[cfg(test)]
    before_publish: parking_lot::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl WorkflowActivity {
    fn new(sink: Arc<EventSink>, parent: String, delegation: String) -> Self {
        let activity = Self(Arc::new(WorkflowActivityInner {
            cancelled: parking_lot::Mutex::new(false),
            sink,
            parent,
            delegation,
            epoch: uuid::Uuid::new_v4().to_string(),
            #[cfg(test)]
            before_publish: parking_lot::Mutex::new(None),
        }));
        activity.stage(Some(
            crate::control::delegation_notifications::DelegationStage::Queued,
        ));
        activity
    }

    fn request_cancel(&self) {
        self.stage(Some(
            crate::control::delegation_notifications::DelegationStage::CancelRequested,
        ));
    }

    fn stage(&self, stage: Option<crate::control::delegation_notifications::DelegationStage>) {
        // Stage choice and ordered publication must be one operation: an older
        // normal choice must never publish after a cancellation at a newer seq.
        let mut cancelled = self.0.cancelled.lock();
        if stage == Some(crate::control::delegation_notifications::DelegationStage::CancelRequested)
        {
            *cancelled = true;
        }
        let stage = if stage.is_some() && *cancelled {
            Some(crate::control::delegation_notifications::DelegationStage::CancelRequested)
        } else {
            stage
        };
        #[cfg(test)]
        {
            let hook = self.0.before_publish.lock().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        self.0.sink.session_stream(&self.0.parent).emit_ephemeral(
            AgentEventKind::DelegationStageChanged {
                delegation_id: self.0.delegation.clone(),
                stage,
                source_epoch: self.0.epoch.clone(),
            },
        );
    }
}

impl Drop for WorkflowActivityInner {
    fn drop(&mut self) {
        self.sink.session_stream(&self.parent).emit_ephemeral(
            AgentEventKind::DelegationStageChanged {
                delegation_id: self.delegation.clone(),
                stage: None,
                source_epoch: self.epoch.clone(),
            },
        );
    }
}

/// Context structure to group delegation handler parameters
struct DelegationContext {
    activity: WorkflowActivity,
    delegator: Arc<dyn crate::agent::handle::AgentHandle>,
    event_sink: Arc<EventSink>,
    store: Arc<dyn SessionStore>,
    tool_registry: Arc<ToolRegistry>,
    hooks: Hooks,
    config: DelegationOrchestratorConfig,
    active_delegations: ActiveDelegations,
    delegation_summarizer: Option<Arc<super::summarizer::DelegationSummarizer>>,
    delegate_model_overrides: super::DelegateModelOverrideStore,
    profile_id: Option<String>,
    routing_snapshot: Option<crate::agent::remote::RoutingSnapshotHandle>,
    cancellation: Arc<DelegationCancellation>,
    workspace_service: Option<Arc<crate::worktree::WorkspaceService>>,
    batch_id: Option<String>,
    /// Parent execution cycle that requested this delegation. The planning
    /// summary is charged to it even if it settles after newer cycles begin.
    requesting_cycle_id: Option<String>,
    /// Manual budget pricing for the summary attempt, if configured.
    budget_pricing: Option<querymt::providers::ModelPricing>,
    /// Per-cycle price cap checked before the summary dispatch.
    budget_cap_usd: Option<f64>,
}

struct DelegationFailureContext<'a> {
    event_sink: &'a Arc<EventSink>,
    delegator: &'a Arc<dyn crate::agent::handle::AgentHandle>,
    store: &'a Arc<dyn SessionStore>,
    hooks: Option<&'a Hooks>,
    config: &'a DelegationOrchestratorConfig,
    parent_session_id: &'a str,
    delegation_id: &'a str,
    target_agent_id: Option<&'a str>,
    objective: Option<&'a str>,
    child_session: Option<(&'a Arc<dyn crate::agent::handle::AgentHandle>, &'a str)>,
    /// The delegate's effective execution cwd once its workspace/checkout
    /// has been allocated; `None` for pre-allocation failures, which keep
    /// explicit parent context only.
    execution_cwd: Option<PathBuf>,
}

async fn teardown_failed_delegate_session(
    target: &Arc<dyn crate::agent::handle::AgentHandle>,
    child_session_id: &str,
    session_ref: &crate::agent::remote::SessionActorRef,
    bridge_connection_id: Option<&Arc<str>>,
) {
    if let (Some(target), Some(connection_id)) = (
        target
            .as_any()
            .downcast_ref::<crate::agent::LocalAgentHandle>(),
        bridge_connection_id,
    ) {
        target
            .clear_session_bridge(child_session_id, connection_id.clone())
            .await;
    }
    let _ = session_ref.shutdown().await;
}

// ══════════════════════════════════════════════════════════════════════════
//  Kameo-native delegation path (Phase 5)
// ══════════════════════════════════════════════════════════════════════════

/// Execute delegation using the `AgentHandle` trait.
///
/// This creates sessions via `AgentHandle::create_delegation_session()`. History and
/// planning context are exchanged via kameo messages (`GetHistory`,
/// `SetPlanningContext`), so this path works for both local and remote sessions.
#[cfg(feature = "remote")]
fn resolve_delegate_provider_node(
    override_node_id: Option<&str>,
    routed_node_id: Option<String>,
) -> Result<Option<String>, String> {
    match override_node_id {
        Some(node_id) => crate::agent::remote::NodeId::parse(node_id)
            .map(|node_id| Some(node_id.to_string()))
            .map_err(|err| {
                format!("Invalid provider node '{node_id}' for delegate model override: {err}")
            }),
        None => Ok(routed_node_id),
    }
}

fn reasoning_effort_from_llm_config(
    config: &crate::session::store::LLMConfig,
) -> Result<Option<ReasoningEffort>, serde_json::Error> {
    let Some(value) = config
        .params
        .as_ref()
        .and_then(|params| params.get("reasoning_effort"))
    else {
        return Ok(None);
    };

    serde_json::from_value(value.clone())
}

/// Build the accounting operation for one delegation summary.
async fn summary_operation(
    ctx: &DelegationContext,
    summarizer: &super::summarizer::DelegationSummarizer,
    parent_session_id: &str,
    cycle_id: Option<&str>,
) -> Result<crate::accounting::AttemptOperation, crate::accounting::AccountingError> {
    let accounting = ctx.event_sink.session_accounting(parent_session_id).await?;
    let cycle = match cycle_id {
        Some(id) => crate::accounting::CycleAccounting::with_id(accounting.clone(), id),
        None => crate::accounting::CycleAccounting::begin_maintenance(accounting.clone()),
    };
    let recorder = Arc::new(crate::accounting::AttemptRecorder::with_projection(
        ctx.event_sink.session_stream(parent_session_id),
        accounting,
    ));
    Ok(summarizer
        .attempt_operation(recorder, &cycle)
        .with_budget_pricing(ctx.budget_pricing.clone())
        .with_budget_cap(ctx.budget_cap_usd))
}

#[instrument(
    name = "delegation.execute",
    skip(ctx, target, delegation, cancel_token),
    fields(
        parent_session_id = %parent_session_id,
        delegation_id = %delegation.public_id,
        target_agent = %delegation.target_agent_id,
        child_session_id = tracing::field::Empty,
        is_remote = tracing::field::Empty,
        has_summarizer = ctx.delegation_summarizer.is_some(),
        has_routing = ctx.routing_snapshot.is_some(),
    )
)]
async fn execute_delegation(
    ctx: DelegationContext,
    target: Arc<dyn crate::agent::handle::AgentHandle>,
    parent_session_id: String,
    delegation: Delegation,
    cancel_token: CancellationToken,
) {
    let delegation_id = delegation.public_id.clone();
    let mut planning_context_additions = String::new();

    // Isolated delegation fails closed without a workspace service: a
    // requested isolation never falls back to sharing the parent checkout,
    // and no child session is launched.
    if delegation.isolation == crate::worktree::DelegationIsolation::Isolated
        && (ctx.workspace_service.is_none() || !target.supports_local_workspaces())
    {
        fail_delegation(
            DelegationFailureContext {
                event_sink: &ctx.event_sink,
                delegator: &ctx.delegator,
                store: &ctx.store,
                hooks: Some(&ctx.hooks),
                config: &ctx.config,
                parent_session_id: &parent_session_id,
                delegation_id: &delegation.public_id,
                target_agent_id: Some(&delegation.target_agent_id),
                objective: Some(&delegation.objective),
                execution_cwd: None,
                child_session: None,
            },
            "isolated delegate execution requires a workspace service and local filesystem execution; no workspace service or unsupported remote target; requested isolation never falls back to shared execution",
        )
        .await;
        ctx.active_delegations.lock().await.remove(&delegation_id);
        return;
    }

    // Snapshot the parent's current per-session setting. UI changes persist this value
    // before returning, so delegation inherits the value active when it starts.
    let parent_reasoning_effort = match ctx.store.get_session_llm_config(&parent_session_id).await {
        Ok(Some(config)) => match reasoning_effort_from_llm_config(&config) {
            Ok(effort) => effort,
            Err(err) => {
                let error_message = format!(
                    "Failed to read reasoning effort from parent session '{}': {err}",
                    parent_session_id
                );
                fail_delegation(
                    DelegationFailureContext {
                        event_sink: &ctx.event_sink,
                        delegator: &ctx.delegator,
                        store: &ctx.store,
                        hooks: Some(&ctx.hooks),
                        config: &ctx.config,
                        parent_session_id: &parent_session_id,
                        delegation_id: &delegation.public_id,
                        target_agent_id: Some(&delegation.target_agent_id),
                        objective: Some(&delegation.objective),
                        child_session: None,
                        execution_cwd: None,
                    },
                    &error_message,
                )
                .await;
                ctx.active_delegations.lock().await.remove(&delegation_id);
                return;
            }
        },
        Ok(None) => {
            let error_message = format!(
                "Parent session '{}' has no LLM configuration",
                parent_session_id
            );
            fail_delegation(
                DelegationFailureContext {
                    event_sink: &ctx.event_sink,
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
                    config: &ctx.config,
                    parent_session_id: &parent_session_id,
                    delegation_id: &delegation.public_id,
                    target_agent_id: Some(&delegation.target_agent_id),
                    objective: Some(&delegation.objective),
                    child_session: None,
                    execution_cwd: None,
                },
                &error_message,
            )
            .await;
            ctx.active_delegations.lock().await.remove(&delegation_id);
            return;
        }
        Err(err) => {
            let error_message = format!(
                "Failed to load LLM configuration for parent session '{}': {err}",
                parent_session_id
            );
            fail_delegation(
                DelegationFailureContext {
                    event_sink: &ctx.event_sink,
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
                    config: &ctx.config,
                    parent_session_id: &parent_session_id,
                    delegation_id: &delegation.public_id,
                    target_agent_id: Some(&delegation.target_agent_id),
                    objective: Some(&delegation.objective),
                    child_session: None,
                    execution_cwd: None,
                },
                &error_message,
            )
            .await;
            ctx.active_delegations.lock().await.remove(&delegation_id);
            return;
        }
    };

    // 1. Create session via AgentHandle trait
    // Resolve the parent's effective cwd through the single session-cwd
    // resolution path (worktree::WorkspaceService::resolve_session_cwd):
    // the durable workspace binding wins over the persisted row cwd, which
    // a fork copies from its source checkout. A bound parent whose
    // workspace cannot be resolved fails the delegation closed — the child
    // is never launched into a shared or source checkout as a fallback.
    let persisted_parent_cwd = match ctx.store.get_session(&parent_session_id).await {
        Ok(Some(session)) => session.cwd,
        Ok(None) => {
            warn!("Parent session not found for cwd lookup, using orchestrator default");
            None
        }
        Err(_) => {
            warn!("Failed to lookup parent session cwd, using orchestrator default");
            None
        }
    };
    let effective_cwd = match ctx.workspace_service.as_ref() {
        Some(service) => {
            match service
                .resolve_session_cwd(&parent_session_id, persisted_parent_cwd.clone())
                .await
            {
                Ok(resolved) => resolved,
                Err(err) => {
                    fail_delegation(
                        DelegationFailureContext {
                            event_sink: &ctx.event_sink,
                            delegator: &ctx.delegator,
                            store: &ctx.store,
                            hooks: Some(&ctx.hooks),
                            config: &ctx.config,
                            parent_session_id: &parent_session_id,
                            delegation_id: &delegation.public_id,
                            target_agent_id: Some(&delegation.target_agent_id),
                            objective: Some(&delegation.objective),
                            execution_cwd: None,
                            child_session: None,
                        },
                        &format!("failed to resolve parent execution workspace: {err}"),
                    )
                    .await;
                    ctx.active_delegations.lock().await.remove(&delegation_id);
                    return;
                }
            }
        }
        None => persisted_parent_cwd,
    }
    .or(ctx.config.cwd.clone());
    let source_parent_cwd = effective_cwd.clone();

    // 1a. Isolated delegates allocate their private workspace BEFORE the
    // child session exists; allocation errors fail the delegation closed —
    // the child is never launched into shared execution. The parent's
    // mutation gate is held only around batch baseline capture, never while
    // waiting on the delegate.
    let child_workspace = match (
        delegation.isolation == crate::worktree::DelegationIsolation::Isolated,
        ctx.workspace_service.as_ref(),
    ) {
        (true, Some(service)) => {
            let Some(parent_cwd) = effective_cwd.clone() else {
                fail_delegation(
                    DelegationFailureContext {
                        event_sink: &ctx.event_sink,
                        delegator: &ctx.delegator,
                        store: &ctx.store,
                        hooks: Some(&ctx.hooks),
                        config: &ctx.config,
                        parent_session_id: &parent_session_id,
                        delegation_id: &delegation.public_id,
                        target_agent_id: Some(&delegation.target_agent_id),
                        objective: Some(&delegation.objective),
                        execution_cwd: None,
                        child_session: None,
                    },
                    "isolated delegation requested but the parent session has no working directory",
                )
                .await;
                ctx.active_delegations.lock().await.remove(&delegation_id);
                return;
            };
            let parent_record = match service
                .registry()
                .get_workspace_by_session(&parent_session_id)
                .await
            {
                Ok(record) => record,
                Err(err) => {
                    fail_delegation(
                        DelegationFailureContext {
                            event_sink: &ctx.event_sink,
                            delegator: &ctx.delegator,
                            store: &ctx.store,
                            hooks: Some(&ctx.hooks),
                            config: &ctx.config,
                            parent_session_id: &parent_session_id,
                            delegation_id: &delegation.public_id,
                            target_agent_id: Some(&delegation.target_agent_id),
                            objective: Some(&delegation.objective),
                            execution_cwd: Some(parent_cwd.clone()),
                            child_session: None,
                        },
                        &format!("cannot resolve parent workspace: {err}"),
                    )
                    .await;
                    ctx.active_delegations.lock().await.remove(&delegation_id);
                    return;
                }
            };
            let parent_gate = parent_record.as_ref().and_then(|record| {
                crate::worktree::CheckoutMutationGate::new(
                    service.storage_root(),
                    &record.checkout_path,
                )
                .ok()
            });
            let batch_id = ctx
                .batch_id
                .clone()
                .unwrap_or_else(|| delegation.public_id.clone());
            match service
                .allocate_for_delegation(
                    &parent_session_id,
                    parent_record.as_ref(),
                    &parent_cwd,
                    parent_gate.as_ref(),
                    &delegation.public_id,
                    Some(batch_id),
                )
                .await
            {
                Ok(binding) => Some(binding),
                Err(err) => {
                    fail_delegation(
                        DelegationFailureContext {
                            event_sink: &ctx.event_sink,
                            delegator: &ctx.delegator,
                            store: &ctx.store,
                            hooks: Some(&ctx.hooks),
                            config: &ctx.config,
                            parent_session_id: &parent_session_id,
                            delegation_id: &delegation.public_id,
                            target_agent_id: Some(&delegation.target_agent_id),
                            objective: Some(&delegation.objective),
                            execution_cwd: None,
                            child_session: None,
                        },
                        &format!("isolated workspace allocation failed: {err}"),
                    )
                    .await;
                    ctx.active_delegations.lock().await.remove(&delegation_id);
                    return;
                }
            }
        }
        (true, None) => unreachable!(
            "the fail-closed gate above rejects isolated delegation without a workspace service"
        ),
        (false, _) => None,
    };
    // The delegate session always materializes in its own workspace when
    // isolated — never in the parent's checkout.
    let effective_cwd = child_workspace
        .as_ref()
        .map(|binding| binding.effective_cwd.clone())
        .or(effective_cwd);
    let cwd_string = effective_cwd.as_ref().map(|p| p.display().to_string());

    // delegation_start is a post-allocation hook: it identifies the
    // delegate's actual execution root (the child workspace when isolated,
    // the parent's checkout otherwise) rather than the orchestrator default.
    // Its additional context feeds the delegate's planning prompt below.
    let start_hook = ctx
        .hooks
        .run_delegation_start(DelegationStartRequest {
            session_id: parent_session_id.clone(),
            parent_cwd: source_parent_cwd.clone(),
            turn_id: String::new(),
            cwd: effective_cwd.clone(),
            model: String::new(),
            permission_mode: "default".to_string(),
            delegation_id: delegation.public_id.clone(),
            target_agent_id: delegation.target_agent_id.clone(),
            objective: delegation.objective.clone(),
            context: delegation.context.clone(),
            constraints: delegation.constraints.clone(),
            expected_output: delegation.expected_output.clone(),
            status: "requested".to_string(),
        })
        .await;
    match start_hook {
        Ok(result) => {
            emit_hook_notices(&ctx.delegator, &parent_session_id, result.notices);
            if !result.additional_contexts.is_empty() {
                planning_context_additions = format!(
                    "\n\n<hook-added-context>\n{}\n</hook-added-context>",
                    result.additional_contexts.join("\n\n")
                );
            }
        }
        Err(_) => warn!("Delegation start hook failed"),
    }
    let (child_session_id, session_ref) = match async {
        match child_workspace.as_ref() {
            Some(binding) => {
                target
                    .create_isolated_delegation_session(binding.clone(), parent_session_id.clone())
                    .await
            }
            None => {
                target
                    .create_delegation_session(cwd_string, parent_session_id.clone())
                    .await
            }
        }
    }
    .instrument(tracing::info_span!("delegation.create_session"))
    .await
    {
        Ok(result) => result,
        Err(e) => {
            // The child workspace was allocated before session creation. No
            // session owns it, so mark it failed (resources retained for
            // explicit recovery/removal) instead of leaving an orphaned
            // Ready workspace, and disclose it in the failure.
            if let (Some(binding), Some(service)) =
                (child_workspace.as_ref(), ctx.workspace_service.as_ref())
            {
                let _ = service
                    .mark_owner_binding_failed(
                        &binding.record.id,
                        &format!(
                            "delegate session creation failed; workspace retained unused: {e}"
                        ),
                    )
                    .await;
            }
            let error_message = format!(
                "Failed to create session via kameo: {}{}",
                e,
                workspace_retention_note(child_workspace.as_ref())
            );
            fail_delegation(
                DelegationFailureContext {
                    event_sink: &ctx.event_sink,
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
                    config: &ctx.config,
                    parent_session_id: &parent_session_id,
                    delegation_id: &delegation.public_id,
                    target_agent_id: Some(&delegation.target_agent_id),
                    objective: Some(&delegation.objective),
                    child_session: None,
                    execution_cwd: child_workspace.as_ref().map(|b| b.effective_cwd.clone()),
                },
                &error_message,
            )
            .await;
            ctx.active_delegations.lock().await.remove(&delegation_id);
            return;
        }
    };
    let _ = ctx
        .cancellation
        .child
        .set((target.clone(), child_session_id.clone()));
    let child_session = Some((&target, child_session_id.as_str()));

    let mut child_bridge_connection_id = None;
    let parent_bridge = ctx
        .delegator
        .as_any()
        .downcast_ref::<crate::agent::LocalAgentHandle>()
        .and_then(|handle| {
            handle
                .config
                .session_bridges
                .lock()
                .ok()
                .and_then(|bridges| {
                    bridges
                        .get(&parent_session_id)
                        .map(|route| route.bridge.clone())
                })
        });
    // Locally hosted children inherit the parent's connection route so the
    // workspace-query capability follows the delegation. Remote children never
    // get a route (workspace queries are unavailable for remote sessions).
    if let Some(bridge) = parent_bridge
        && !session_ref.is_remote()
        && let Some(target) = target
            .as_any()
            .downcast_ref::<crate::agent::LocalAgentHandle>()
        && let Ok(mut bridges) = target.config.session_bridges.lock()
    {
        child_bridge_connection_id = bridge.connection_id().map(Arc::from);
        bridges.insert(
            child_session_id.clone(),
            crate::agent::agent_config::SessionBridgeRoute { bridge },
        );
    }

    if let Some(profile_id) = ctx.profile_id.as_deref()
        && let Err(err) = persist_delegate_runtime_binding(
            ctx.store.as_ref(),
            &parent_session_id,
            &child_session_id,
            profile_id,
            &delegation.target_agent_id,
        )
        .await
    {
        teardown_failed_delegate_session(
            &target,
            &child_session_id,
            &session_ref,
            child_bridge_connection_id.as_ref(),
        )
        .await;
        fail_delegation(
            DelegationFailureContext {
                event_sink: &ctx.event_sink,
                delegator: &ctx.delegator,
                store: &ctx.store,
                hooks: Some(&ctx.hooks),
                config: &ctx.config,
                parent_session_id: &parent_session_id,
                delegation_id: &delegation.public_id,
                target_agent_id: Some(&delegation.target_agent_id),
                objective: Some(&delegation.objective),
                child_session,
                execution_cwd: effective_cwd.clone(),
            },
            &format!("Failed to persist delegate session route: {err}"),
        )
        .await;
        ctx.active_delegations.lock().await.remove(&delegation_id);
        return;
    }

    // Record child session info on the parent span.
    let span = tracing::Span::current();
    span.record("child_session_id", child_session_id.as_str());
    span.record("is_remote", session_ref.is_remote());

    // 1b. Apply routing policy from the routing table (if present).
    //     When provider_target = Peer(name) and the peer is resolved,
    //     write the node_id to the session's DB row so SessionProvider
    //     constructs a MeshChatProvider for this session.
    #[cfg(feature = "remote")]
    let mut routed_provider_node_id: Option<String> = None;
    if let Some(ref snap_handle) = ctx.routing_snapshot {
        let routing_span = tracing::info_span!(
            "delegation.apply_routing",
            provider_node_id = tracing::field::Empty,
        );
        async {
            let snap = snap_handle.load();
            if let Some(policy) = snap.get(&delegation.target_agent_id) {
                use crate::agent::remote::routing::RouteTarget;
                if let RouteTarget::Peer(_) = &policy.provider_target
                    && let Some(ref node_id) = policy.resolved_provider_node_id
                {
                    tracing::Span::current().record("provider_node_id", node_id.as_str());
                    #[cfg(feature = "remote")]
                    {
                        routed_provider_node_id = Some(node_id.clone());
                    }
                    if ctx
                        .store
                        .set_session_provider_node_id(&child_session_id, Some(node_id.as_str()))
                        .await
                        .is_err()
                    {
                        warn!(
                            "execute_delegation: failed to set provider_node_id='{}' \
                                 on session {} from routing table",
                            node_id, child_session_id
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
        .instrument(routing_span)
        .await;
    }

    let route_overrides = match ctx
        .delegate_model_overrides
        .resolve(
            ctx.store.as_ref(),
            &parent_session_id,
            &delegation.target_agent_id,
        )
        .await
    {
        Ok(overrides) => overrides,
        Err(error) => {
            teardown_failed_delegate_session(
                &target,
                &child_session_id,
                &session_ref,
                child_bridge_connection_id.as_ref(),
            )
            .await;
            fail_delegation(
                DelegationFailureContext {
                    event_sink: &ctx.event_sink,
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
                    config: &ctx.config,
                    parent_session_id: &parent_session_id,
                    delegation_id: &delegation.public_id,
                    target_agent_id: Some(&delegation.target_agent_id),
                    objective: Some(&delegation.objective),
                    child_session,
                    execution_cwd: effective_cwd.clone(),
                },
                &format!("Failed to read delegate model assignment: {error}"),
            )
            .await;
            ctx.active_delegations.lock().await.remove(&delegation_id);
            return;
        }
    };
    if let Some(model_override) = route_overrides.model {
        #[cfg(feature = "remote")]
        let provider_node_id = match resolve_delegate_provider_node(
            model_override.node_id.as_deref(),
            routed_provider_node_id,
        ) {
            Ok(provider_node_id) => provider_node_id,
            Err(error_message) => {
                teardown_failed_delegate_session(
                    &target,
                    &child_session_id,
                    &session_ref,
                    child_bridge_connection_id.as_ref(),
                )
                .await;
                fail_delegation(
                    DelegationFailureContext {
                        event_sink: &ctx.event_sink,
                        delegator: &ctx.delegator,
                        store: &ctx.store,
                        hooks: Some(&ctx.hooks),
                        config: &ctx.config,
                        parent_session_id: &parent_session_id,
                        delegation_id: &delegation.public_id,
                        target_agent_id: Some(&delegation.target_agent_id),
                        objective: Some(&delegation.objective),
                        child_session,
                        execution_cwd: effective_cwd.clone(),
                    },
                    &error_message,
                )
                .await;
                ctx.active_delegations.lock().await.remove(&delegation_id);
                return;
            }
        };
        #[cfg(not(feature = "remote"))]
        let provider_node_id = None;
        let model_selection = crate::agent::session_control::SessionModelSelection {
            model_id: model_override.model_id.clone(),
            provider_node_id,
        };
        if let Err(err) = session_ref.set_session_model(model_selection).await {
            let error_message = format!(
                "Failed to apply model '{}' to delegate '{}': {err}",
                model_override.model_id, delegation.target_agent_id
            );
            teardown_failed_delegate_session(
                &target,
                &child_session_id,
                &session_ref,
                child_bridge_connection_id.as_ref(),
            )
            .await;
            fail_delegation(
                DelegationFailureContext {
                    event_sink: &ctx.event_sink,
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
                    config: &ctx.config,
                    parent_session_id: &parent_session_id,
                    delegation_id: &delegation.public_id,
                    target_agent_id: Some(&delegation.target_agent_id),
                    objective: Some(&delegation.objective),
                    child_session,
                    execution_cwd: effective_cwd.clone(),
                },
                &error_message,
            )
            .await;
            ctx.active_delegations.lock().await.remove(&delegation_id);
            return;
        }
    }

    // Apply this after any model override because SetSessionModel rebuilds the
    // child's LLM configuration. No role override inherits the parent's current setting.
    let delegate_reasoning_effort = route_overrides
        .reasoning_effort
        .map_or(parent_reasoning_effort, |effort| effort.session_effort());
    if let Err(err) = session_ref
        .set_reasoning_effort(delegate_reasoning_effort)
        .await
    {
        let error_message = format!(
            "Failed to apply reasoning effort for delegate '{}': {err}",
            delegation.target_agent_id
        );
        teardown_failed_delegate_session(
            &target,
            &child_session_id,
            &session_ref,
            child_bridge_connection_id.as_ref(),
        )
        .await;
        fail_delegation(
            DelegationFailureContext {
                event_sink: &ctx.event_sink,
                delegator: &ctx.delegator,
                store: &ctx.store,
                hooks: Some(&ctx.hooks),
                config: &ctx.config,
                parent_session_id: &parent_session_id,
                delegation_id: &delegation.public_id,
                target_agent_id: Some(&delegation.target_agent_id),
                objective: Some(&delegation.objective),
                child_session,
                execution_cwd: effective_cwd.clone(),
            },
            &error_message,
        )
        .await;
        ctx.active_delegations.lock().await.remove(&delegation_id);
        return;
    }

    let selected_model = match session_ref.get_session_control().await {
        Ok(control) => (
            Some(control.effective_model.model_id),
            control.effective_model.provider_node_id,
        ),
        Err(_) => {
            // Provenance only: the child is already created, routed, and configured.
            // Missing selected_model_id is allowed on SessionForked. If confirmed
            // model identity later becomes a hard execution/audit precondition,
            // consider failing the delegation here instead of degrading.
            tracing::warn!(
                delegation_id = %delegation.public_id,
                child_session_id = %child_session_id,
                target_agent_id = %delegation.target_agent_id,
                "Failed to confirm delegate model"
            );
            (None, None)
        }
    };

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
            selected_model_id: selected_model.0,
            selected_provider_node_id: selected_model.1,
        },
    );

    // 2. Generate and inject planning summary via kameo message
    if let Some(ref summarizer) = ctx.delegation_summarizer {
        let history = async {
            match ctx.store.get_effective_history(&parent_session_id).await {
                Ok(history) => {
                    tracing::Span::current().record("message_count", history.len() as u64);
                    Some(history)
                }
                Err(_) => {
                    warn!("Failed to load parent history for summary");
                    None
                }
            }
        }
        .instrument(tracing::info_span!(
            "delegation.fetch_parent_history",
            message_count = tracing::field::Empty,
        ))
        .await;

        if let Some(history) = history {
            // Explicit ownership: the summary charges the parent cycle that
            // requested the delegation. Events from before cycle ids existed
            // have none, so such a summary is charged to a dedicated
            // maintenance cycle rather than to whichever cycle is current.
            let operation = match summary_operation(
                &ctx,
                summarizer,
                &parent_session_id,
                ctx.requesting_cycle_id.as_deref(),
            )
            .await
            {
                Ok(operation) => Some(operation),
                Err(error) => {
                    warn!("Delegation summary skipped: accounting unavailable: {error}");
                    None
                }
            };
            let summary = async {
                let operation = operation.as_ref()?;
                match summarizer
                    .summarize(&history, &delegation.objective, operation)
                    .await
                {
                    Ok(summary) => {
                        tracing::Span::current().record("summary_bytes", summary.len() as u64);
                        Some(summary)
                    }
                    Err(_) => {
                        // The parent cycle's price cap refused the paid summary
                        // dispatch: nothing was sent. Not a generation failure.
                        if let Some((cap, spent)) = operation.budget_refusal() {
                            warn!(
                                "Delegation summary skipped: price limit reached: ${spent:.4} >= ${cap:.2}"
                            );
                            return None;
                        }
                        warn!("Delegation summary generation failed");
                        // Proceed without summary — graceful degradation
                        None
                    }
                }
            }
            .instrument(tracing::info_span!(
                "delegation.summarize",
                history_messages = history.len(),
                summary_bytes = tracing::field::Empty,
                strategy = tracing::field::Empty,
            ))
            .await;

            if let Some(summary) = summary {
                // Persist only the summary; the event copy still has the pre-claim status.
                if ctx
                    .store
                    .set_delegation_planning_summary(&delegation.public_id, &summary)
                    .await
                    .is_err()
                {
                    warn!("Failed to persist delegation summary");
                }

                // Inject via SetPlanningContext kameo message
                let formatted_summary = format!(
                    "\n\n<planning-context>\n{}{}\n</planning-context>",
                    summary, planning_context_additions
                );
                let summary_bytes = formatted_summary.len();
                async {
                    if session_ref
                        .set_planning_context(formatted_summary)
                        .await
                        .is_err()
                    {
                        warn!("Failed to inject planning summary via SetPlanningContext");
                    } else {
                        log::info!(
                            "Injected planning summary into delegate session {} via kameo",
                            child_session_id
                        );
                    }
                }
                .instrument(tracing::info_span!(
                    "delegation.inject_planning_context",
                    summary_bytes = summary_bytes,
                ))
                .await;
            }
        }
    }

    // 3. Send prompt directly via kameo
    let prompt_text = build_delegation_prompt(&delegation);
    let mut prompt_req = PromptRequest::new(
        child_session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(prompt_text))],
    );
    prompt_req.meta = crate::acp::trace_context::inject_current_acp_trace_context();

    let prompt_result = tokio::select! {
        result = session_ref.prompt(prompt_req) => Some(result),
        _ = cancel_token.cancelled() => {
            ctx.activity.stage(Some(crate::control::delegation_notifications::DelegationStage::CancelRequested));
            // Cancellation — cancel the child session
            let _ = session_ref.cancel().await;

            match ctx
                .store
                .transition_delegation_status(
                    &delegation_id,
                    DelegationStatus::Running,
                    DelegationStatus::Cancelled,
                )
                .await
            {
                Ok(true) => {
                    ctx.cancellation.resolve(&delegation_id).await;
                }
                Ok(false) => {
                    let mut active = ctx.active_delegations.lock().await;
                    active.remove(&delegation_id);
                    return;
                }
                Err(_) => {
                    warn!("Failed to update delegation status to Cancelled");
                    let mut active = ctx.active_delegations.lock().await;
                    active.remove(&delegation_id);
                    return;
                }
            }

            emit_delegation_event(
                &ctx.delegator,
                &ctx.event_sink,
                &parent_session_id,
                AgentEventKind::DelegationCancelled {
                    delegation_id: delegation_id.clone(),
                },
            );

            // Surface where the cancelled delegate's files remain
            // recoverable; nothing is integrated or removed.
            let retention = workspace_retention_note(child_workspace.as_ref());
            if !retention.is_empty() {
                emit_hook_notices(
                    &ctx.delegator,
                    &parent_session_id,
                    vec![crate::hooks::HookNotice {
                        event_name: "delegation_cancelled".to_string(),
                        message: retention.trim_start().to_string(),
                        is_error: false,
                    }],
                );
            }

            let mut active = ctx.active_delegations.lock().await;
            active.remove(&delegation_id);
            return;
        }
    };

    match prompt_result {
        Some(Ok(ref response)) if response.stop_reason != StopReason::EndTurn => {
            // Delegate stopped prematurely (e.g. context threshold, max turns, refusal).
            // This is NOT a successful completion — the delegate was cut short and its
            // output is likely truncated/incomplete.
            let error_message = format!(
                "Delegate stopped prematurely: {:?}{}",
                response.stop_reason,
                workspace_retention_note(child_workspace.as_ref())
            );
            warn!(
                "Delegation {} for agent '{}' stopped with {:?} instead of EndTurn",
                delegation_id, delegation.target_agent_id, response.stop_reason,
            );
            fail_delegation(
                DelegationFailureContext {
                    event_sink: &ctx.event_sink,
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
                    config: &ctx.config,
                    parent_session_id: &parent_session_id,
                    delegation_id: &delegation_id,
                    target_agent_id: Some(&delegation.target_agent_id),
                    objective: Some(&delegation.objective),
                    child_session,
                    execution_cwd: effective_cwd.clone(),
                },
                &error_message,
            )
            .await;

            let mut active = ctx.active_delegations.lock().await;
            active.remove(&delegation_id);
        }
        Some(Ok(_)) => {
            ctx.activity.stage(Some(
                crate::control::delegation_notifications::DelegationStage::Verifying,
            ));
            // 4. Verification (same as legacy path)
            let verification_passed = async {
                if ctx.config.run_verification {
                    match VerificationSpecBuilder::from_delegation(&delegation) {
                        Some(verification_spec) => {
                            tracing::Span::current().record("has_spec", true);
                            let agent_tool_registry = ctx.tool_registry.clone();
                            let service = VerificationService::new(agent_tool_registry.clone());
                            let verification_context = VerificationContext {
                                session_id: child_session_id.clone(),
                                task_id: delegation.task_id.map(|id| id.to_string()),
                                delegation_id: Some(delegation.public_id.clone()),
                                // Verification runs in the delegate's own
                                // checkout — never the orchestrator default.
                                cwd: effective_cwd.clone(),
                                tool_registry: agent_tool_registry.clone(),
                            };
                            let result = match service
                                .verify(&verification_spec, &verification_context)
                                .await
                            {
                                Ok(()) => true,
                                Err(_) => {
                                    warn!("Verification failed");
                                    false
                                }
                            };
                            tracing::Span::current().record("passed", result);
                            result
                        }
                        None => {
                            tracing::Span::current().record("has_spec", false);
                            tracing::Span::current().record("passed", true);
                            true
                        }
                    }
                } else {
                    tracing::Span::current().record("has_spec", false);
                    tracing::Span::current().record("passed", true);
                    true
                }
            }
            .instrument(tracing::info_span!(
                "delegation.verify",
                has_spec = tracing::field::Empty,
                passed = tracing::field::Empty,
            ))
            .await;

            if !verification_passed {
                let error_message = format!(
                    "Verification failed: The changes did not pass the specified verification checks.{}",
                    workspace_retention_note(child_workspace.as_ref())
                );
                fail_delegation(
                    DelegationFailureContext {
                        event_sink: &ctx.event_sink,
                        delegator: &ctx.delegator,
                        store: &ctx.store,
                        hooks: Some(&ctx.hooks),
                        config: &ctx.config,
                        parent_session_id: &parent_session_id,
                        delegation_id: &delegation.public_id,
                        target_agent_id: Some(&delegation.target_agent_id),
                        objective: Some(&delegation.objective),
                        child_session,
                        execution_cwd: effective_cwd.clone(),
                    },
                    &error_message,
                )
                .await;
                ctx.active_delegations.lock().await.remove(&delegation_id);
                return;
            }

            ctx.activity.stage(Some(
                crate::control::delegation_notifications::DelegationStage::Finalizing,
            ));
            // 5. Get history for summary via kameo (works for local and remote)
            let mut summary = async {
                let s = match session_ref.get_history().await {
                    Ok(history) => extract_session_summary_from_history(&history),
                    Err(_) => {
                        warn!("Error extracting summary via GetHistory");
                        "Error extracting summary.".to_string()
                    }
                };
                tracing::Span::current().record("summary_bytes", s.len() as u64);
                s
            }
            .instrument(tracing::info_span!(
                "delegation.extract_summary",
                summary_bytes = tracing::field::Empty,
            ))
            .await;

            match ctx
                .hooks
                .run_post_delegation(PostDelegationRequest {
                    session_id: parent_session_id.clone(),
                    parent_cwd: source_parent_cwd.clone(),
                    turn_id: String::new(),
                    // Execution hook: identifies the delegate's actual
                    // execution root, not the orchestrator default.
                    cwd: effective_cwd.clone(),
                    model: String::new(),
                    permission_mode: "default".to_string(),
                    delegation_id: delegation.public_id.clone(),
                    target_agent_id: delegation.target_agent_id.clone(),
                    child_session_id: child_session_id.clone(),
                    objective: delegation.objective.clone(),
                    status: "complete".to_string(),
                    summary: summary.clone(),
                    verification_passed,
                })
                .await
            {
                Ok(result) => {
                    emit_hook_notices(&ctx.delegator, &parent_session_id, result.notices);
                    if !result.additional_contexts.is_empty() {
                        summary =
                            format!("{}\n\n{}", summary, result.additional_contexts.join("\n\n"));
                    }
                }
                Err(_) => warn!("Post-delegation hook failed"),
            }

            // 4b. Retain the delegate's uncommitted work as an immutable,
            // baseline-relative review artifact before announcing
            // completion. A capture error is disclosed but never fails the
            // delegation or removes the workspace: the checkout and its
            // files stay retained for manual recovery.
            if let (Some(binding), Some(service)) =
                (child_workspace.as_ref(), ctx.workspace_service.as_ref())
            {
                match service
                    .publish_result(
                        &binding.record,
                        &binding.effective_cwd,
                        Some(&binding.mutation_gate),
                        Some(
                            if ctx.config.run_verification
                                && VerificationSpecBuilder::from_delegation(&delegation).is_some()
                            {
                                crate::worktree::ResultVerificationStatus::Passed
                            } else {
                                crate::worktree::ResultVerificationStatus::Skipped
                            },
                        ),
                    )
                    .await
                {
                    Ok(provenance) => {
                        // Completion output carries the baseline-relative
                        // review (changed paths and kinds) or an explicit
                        // reference to it — never just an opaque commit id —
                        // and never claims the result was integrated.
                        let review = service
                            .review_result(&binding.record, &provenance)
                            .await
                            .ok();
                        match review {
                            Some(review) => {
                                let next_step = if review.changed_paths.is_empty() {
                                    "The delegate made NO changes relative to its baseline, so \
                                     there is nothing to accept: completion of the run does not \
                                     mean the objective was achieved. Read its summary above; \
                                     if it reports being blocked, resolve that and delegate \
                                     again."
                                        .to_string()
                                } else {
                                    format!(
                                        "After reviewing, integrate it with \
                                         accept_delegation_result(delegation_id=\"{}\", \
                                         result_commit=\"{}\"); do not delegate patch/merge work.",
                                        delegation.public_id, review.result_commit,
                                    )
                                };
                                summary.push_str(&format!(
                                    "\n\nWorkspace result (retained for review; nothing was merged): \
                                     workspace {}, branch {}, result {} (ref {}), captured HEAD {}, \
                                     verification {}. Nothing was integrated yet; review \
                                     remains available through /worktree result {}. {}\n{}",
                                    binding.record.id,
                                    binding.record.branch.as_str(),
                                    review.result_commit,
                                    review.result_ref.as_deref().unwrap_or("(none)"),
                                    review.captured_head,
                                    review
                                        .verification
                                        .map(|v| v.to_string())
                                        .unwrap_or_else(|| "not reported".to_string()),
                                    binding.record.id,
                                    next_step,
                                    review.summary_text(),
                                ));
                            }
                            None => {
                                summary.push_str(&format!(
                                    "\n\nWorkspace result (retained for review; nothing was merged): \
                                     workspace {}, branch {}, result commit {} (ref {}). Review \
                                     via /worktree result {}.",
                                    binding.record.id,
                                    binding.record.branch.as_str(),
                                    provenance.result_commit.as_deref().unwrap_or("(none)"),
                                    provenance.result_ref.as_deref().unwrap_or("(none)"),
                                    binding.record.id,
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        summary.push_str(&format!(
                            "\n\nWorkspace result capture failed: {err}. The isolated \
                             checkout and its files are retained unchanged for manual \
                             recovery; nothing was integrated."
                        ));
                    }
                }
            }

            match ctx
                .store
                .transition_delegation_status(
                    &delegation.public_id,
                    DelegationStatus::Running,
                    DelegationStatus::Complete,
                )
                .await
            {
                Ok(true) => {
                    resolve_terminal_delegate_attempt(
                        target.as_ref(),
                        &child_session_id,
                        &delegation_id,
                    )
                    .await;
                }
                Ok(false) => {
                    let mut active = ctx.active_delegations.lock().await;
                    active.remove(&delegation_id);
                    return;
                }
                Err(_) => {
                    warn!("Failed to persist delegation completion");
                    let mut active = ctx.active_delegations.lock().await;
                    active.remove(&delegation_id);
                    return;
                }
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
            let error_message = format!(
                "Delegation failed: {}{}",
                e,
                workspace_retention_note(child_workspace.as_ref())
            );
            fail_delegation(
                DelegationFailureContext {
                    event_sink: &ctx.event_sink,
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
                    config: &ctx.config,
                    parent_session_id: &parent_session_id,
                    delegation_id: &delegation_id,
                    target_agent_id: Some(&delegation.target_agent_id),
                    objective: Some(&delegation.objective),
                    child_session,
                    execution_cwd: effective_cwd.clone(),
                },
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

/// Retention note for terminal lifecycle outputs: where the isolated
/// delegate's files remain recoverable after failure, cancellation, or a
/// result-capture error. Empty for shared execution — nothing is retained.
fn workspace_retention_note(child_workspace: Option<&crate::worktree::WorkspaceBinding>) -> String {
    match child_workspace {
        Some(binding) => format!(
            " The isolated workspace {} at {} (branch {}) is retained with all its \
             edits; nothing was integrated and the files remain recoverable.",
            binding.record.id,
            binding.record.checkout_path.display(),
            binding.record.branch.as_str(),
        ),
        None => String::new(),
    }
}

async fn persist_delegate_runtime_binding(
    store: &dyn SessionStore,
    parent_session_id: &str,
    child_session_id: &str,
    profile_id: &str,
    agent_id: &str,
) -> crate::session::error::SessionResult<()> {
    let parent_binding = store.get_session_runtime_binding(parent_session_id).await?;
    let matching_binding = parent_binding
        .as_ref()
        .filter(|binding| binding.profile_id == profile_id);
    store
        .set_session_runtime_binding(
            child_session_id,
            profile_id,
            Some(agent_id),
            matching_binding.and_then(|binding| binding.profile_fingerprint.as_deref()),
            matching_binding.and_then(|binding| binding.provider_lock_digest.as_deref()),
            matching_binding.and_then(|binding| binding.provider_locks_json.as_deref()),
        )
        .await
}

async fn resolve_terminal_delegate_attempt(
    target: &dyn crate::agent::handle::AgentHandle,
    child_session_id: &str,
    delegation_id: &str,
) {
    // Resolution marks the one-shot attempt as finished, not as successful execution.
    if let Err(error) = target
        .set_session_lifecycle(
            child_session_id,
            crate::session::lifecycle::SessionLifecyclePatch {
                resolved: Some(true),
                archived: None,
            },
        )
        .await
    {
        warn!(
            "Failed to resolve terminal delegation child session for delegation {} child {}: {}",
            delegation_id, child_session_id, error
        );
    }
}

#[instrument(
    name = "delegation.fail",
    skip(ctx, error_message),
    fields(
        delegation_id = %ctx.delegation_id,
        parent_session_id = %ctx.parent_session_id,
    )
)]
async fn fail_delegation(ctx: DelegationFailureContext<'_>, error_message: &str) {
    error!("Delegation failed");
    let transitioned = match ctx
        .store
        .transition_delegation_status(
            ctx.delegation_id,
            DelegationStatus::Running,
            DelegationStatus::Failed,
        )
        .await
    {
        Ok(true) => true,
        Ok(false) => match ctx
            .store
            .transition_delegation_status(
                ctx.delegation_id,
                DelegationStatus::Requested,
                DelegationStatus::Failed,
            )
            .await
        {
            Ok(transitioned) => transitioned,
            Err(_) => {
                warn!("Failed to persist delegation failure");
                false
            }
        },
        Err(_) => {
            warn!("Failed to persist delegation failure");
            false
        }
    };
    if !transitioned {
        return;
    }

    if let Some((target, child_session_id)) = ctx.child_session {
        resolve_terminal_delegate_attempt(target.as_ref(), child_session_id, ctx.delegation_id)
            .await;
    }

    let mut failure_message = error_message.to_string();
    if let Some(hooks) = ctx.hooks {
        match hooks
            .run_delegation_failure(DelegationFailureRequest {
                session_id: ctx.parent_session_id.to_string(),
                turn_id: String::new(),
                // The delegate's execution root once allocated; `None` keeps
                // explicit parent context for pre-allocation failures.
                cwd: ctx.execution_cwd.clone(),
                model: String::new(),
                permission_mode: "default".to_string(),
                delegation_id: ctx.delegation_id.to_string(),
                target_agent_id: ctx.target_agent_id.unwrap_or_default().to_string(),
                objective: ctx.objective.unwrap_or_default().to_string(),
                status: "failed".to_string(),
                error: error_message.to_string(),
                error_type: classify_delegation_error(error_message).0.to_string(),
            })
            .await
        {
            Ok(result) => {
                emit_hook_notices(ctx.delegator, ctx.parent_session_id, result.notices);
                if !result.additional_contexts.is_empty() {
                    failure_message = format!(
                        "{}\n\n{}",
                        failure_message,
                        result.additional_contexts.join("\n\n")
                    );
                }
            }
            Err(_) => warn!("Delegation failure hook failed"),
        }
    }

    emit_delegation_event(
        ctx.delegator,
        ctx.event_sink,
        ctx.parent_session_id,
        AgentEventKind::DelegationFailed {
            delegation_id: ctx.delegation_id.to_string(),
            error: failure_message.clone(),
        },
    );

    if ctx.config.inject_results {
        inject_failure(
            ctx.delegator,
            ctx.parent_session_id,
            ctx.delegation_id,
            &failure_message,
        )
        .await;
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

fn emit_hook_notices(
    delegator: &Arc<dyn crate::agent::handle::AgentHandle>,
    session_id: &str,
    notices: Vec<HookNotice>,
) {
    for notice in notices {
        delegator.emit_event(
            session_id,
            AgentEventKind::HookNotice {
                event_name: notice.event_name,
                message: notice.message,
                is_error: notice.is_error,
            },
        );
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
    delegator: &Arc<dyn crate::agent::handle::AgentHandle>,
    session_id: &str,
    delegation_id: &str,
    summary: &str,
) {
    let message = format_delegation_completion_message(delegation_id, summary);

    if delegator
        .prompt(PromptRequest::new(
            session_id.to_string(),
            vec![ContentBlock::Text(TextContent::new(message))],
        ))
        .await
        .is_err()
    {
        warn!("Failed to inject delegation results");
    }
}

async fn inject_failure(
    delegator: &Arc<dyn crate::agent::handle::AgentHandle>,
    session_id: &str,
    delegation_id: &str,
    error: &str,
) {
    let message = format_delegation_failure_message(delegation_id, error);

    if delegator
        .prompt(PromptRequest::new(
            session_id.to_string(),
            vec![ContentBlock::Text(TextContent::new(message))],
        ))
        .await
        .is_err()
    {
        warn!("Failed to inject delegation failure");
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
        // Function calls from both legacy parts and canonical structured output.
        for tool_call in message.function_calls() {
            let args_preview =
                extract_tool_args_preview(&tool_call.function.name, &tool_call.function.arguments);
            tools_used.push(format!("{} ({})", tool_call.function.name, args_preview));
        }
        for part in &message.parts {
            match part {
                MessagePart::ToolResult {
                    tool_name: Some(name),
                    tool_arguments: Some(args),
                    ..
                } => {
                    if matches!(name.as_str(), "write_file" | "delete_file")
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
                MessagePart::Text { content }
                    if message.role == ChatRole::Assistant && !content.trim().is_empty() =>
                {
                    agent_responses.push(content.clone());
                }
                MessagePart::Output { output } if message.role == ChatRole::Assistant => {
                    // Structured turn: the visible assistant text lives in the
                    // canonical output; project it the same way
                    // DelegationSummarizer::format_conversation does.
                    if let Some(text) = output.text()
                        && !text.trim().is_empty()
                    {
                        agent_responses.push(text);
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
        "write_file" | "read_tool" | "delete_file" => args
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
    use crate::acp::protocol::{
        CancelNotification, Error, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
        NewSessionResponse, PromptRequest, PromptResponse,
    };
    use crate::agent::handle::AgentHandle;
    use crate::agent::remote::SessionActorRef;
    use crate::event_fanout::EventFanout;
    use crate::event_sink::EventSink;
    use crate::events::EventEnvelope;
    use crate::hooks::Hooks;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::test_utils::MockSessionStore;
    use async_trait::async_trait;

    #[tokio::test]
    async fn workflow_producer_orders_stages_and_keeps_cleanup_visible_with_idle_parent() {
        use crate::agent::messages::{GetRuntimeStatus, SessionRuntimePhase};
        use crate::control::delegation_notifications::{DelegationActivity, DelegationStage};
        use kameo::actor::Spawn;
        let (plugins, dir) = crate::test_utils::empty_plugin_registry().unwrap();
        let storage = Arc::new(
            SqliteStorage::connect(dir.path().join("workflow.db"))
                .await
                .unwrap(),
        );
        let session = storage
            .session_store()
            .create_session(None, None, None, None)
            .await
            .unwrap();
        let config = Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::new(
                Arc::new(plugins),
                storage,
                querymt::LLMParams::new()
                    .provider("mock")
                    .model("mock-model"),
            )
            .build(),
        );
        let runtime = crate::agent::core::SessionRuntime::new(
            None,
            Default::default(),
            crate::agent::core::McpToolState::empty(),
        );
        let actor = crate::agent::session_actor::SessionActor::spawn(
            crate::agent::session_actor::SessionActor::new(
                config.clone(),
                session.public_id.clone(),
                runtime,
            ),
        );
        let mut rx = config.subscribe_events();
        let delegation = Delegation {
            isolation: Default::default(),
            id: 1,
            public_id: "workflow-1".into(),
            session_id: session.id,
            task_id: None,
            target_agent_id: "coder".into(),
            objective: "test workflow observation".into(),
            objective_hash: crate::hash::RapidHash::default(),
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: None,
            planning_summary: None,
            status: DelegationStatus::Requested,
            retry_count: 0,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };
        config.emit_event(
            &session.public_id,
            AgentEventKind::DelegationRequested {
                delegation,
                tool_call_id: None,
                batch_id: None,
            },
        );
        let activity = WorkflowActivity::new(
            config.event_sink.clone(),
            session.public_id.clone(),
            "workflow-1".into(),
        );
        let cleanup_owner = activity.clone();
        activity.stage(Some(DelegationStage::Executing));
        activity.stage(Some(DelegationStage::Verifying));
        config
            .event_sink
            .session_stream(&session.public_id)
            .flush()
            .await;
        let status = actor.ask(GetRuntimeStatus).await.unwrap();
        assert_eq!(status.phase, SessionRuntimePhase::Idle);
        assert!(status.active_run_id.is_none());
        assert_eq!(status.delegation_activity.len(), 1);
        assert_eq!(
            status.delegation_activity[0].stage,
            Some(DelegationStage::Verifying)
        );
        config.emit_event(
            &session.public_id,
            AgentEventKind::DelegationCancelled {
                delegation_id: "workflow-1".into(),
            },
        );
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let enter_hook = entered.clone();
        let release_hook = release.clone();
        *activity.0.before_publish.lock() = Some(Box::new(move || {
            enter_hook.wait();
            release_hook.wait();
        }));
        let normal = activity.clone();
        let normal_thread =
            std::thread::spawn(move || normal.stage(Some(DelegationStage::Finalizing)));
        entered.wait();
        let cancel = activity.clone();
        let (published, observed) = std::sync::mpsc::channel();
        let cancel_thread = std::thread::spawn(move || {
            cancel.request_cancel();
            let _ = published.send(());
        });
        // Old producer can publish cancel while the selected normal stage is paused.
        // A serialized producer instead keeps cancel waiting for that publication.
        let _ = observed.recv_timeout(std::time::Duration::from_millis(100));
        release.wait();
        normal_thread.join().unwrap();
        cancel_thread.join().unwrap();
        config
            .event_sink
            .session_stream(&session.public_id)
            .flush()
            .await;
        let status = actor.ask(GetRuntimeStatus).await.unwrap();
        assert_eq!(
            status.delegation_activity[0].activity,
            DelegationActivity::Live
        );
        assert_eq!(
            status.delegation_activity[0].stage,
            Some(DelegationStage::CancelRequested)
        );
        drop(activity);
        // Async owner cleanup is still live after the workflow task itself leaves.
        assert_eq!(
            config
                .event_sink
                .live_delegation_activity(&session.public_id)
                .len(),
            1
        );
        drop(cleanup_owner);
        config
            .event_sink
            .session_stream(&session.public_id)
            .flush()
            .await;
        assert!(
            actor
                .ask(GetRuntimeStatus)
                .await
                .unwrap()
                .delegation_activity
                .is_empty()
        );
        let mut seqs = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if matches!(event.kind(), AgentEventKind::DelegationStageChanged { .. }) {
                seqs.push(event.session_seq());
            }
        }
        assert_eq!(seqs.len(), 6);
        assert!(seqs[0] > 0 && seqs.windows(2).all(|pair| pair[0] < pair[1]));
        actor.kill();
        actor.wait_for_shutdown().await;
    }

    #[cfg(feature = "remote")]
    fn random_provider_node_id() -> String {
        crate::agent::remote::NodeId::from_peer_id(
            libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id(),
        )
        .to_string()
    }

    #[cfg(feature = "remote")]
    #[test]
    fn model_override_preserves_routed_provider_node_when_unspecified() {
        let routed_node_id = random_provider_node_id();

        assert_eq!(
            resolve_delegate_provider_node(None, Some(routed_node_id.clone())),
            Ok(Some(routed_node_id))
        );
    }

    #[cfg(feature = "remote")]
    #[test]
    fn model_override_prefers_explicit_provider_node_over_route() {
        let explicit_node_id = random_provider_node_id();
        let routed_node_id = random_provider_node_id();

        assert_eq!(
            resolve_delegate_provider_node(Some(explicit_node_id.as_str()), Some(routed_node_id),),
            Ok(Some(explicit_node_id))
        );
    }

    #[cfg(feature = "remote")]
    #[test]
    fn model_override_rejects_invalid_explicit_provider_node() {
        let error = resolve_delegate_provider_node(
            Some("not-a-provider-node"),
            Some(random_provider_node_id()),
        )
        .expect_err("invalid explicit provider node must fail");

        assert!(error.contains("Invalid provider node 'not-a-provider-node'"));
    }

    #[tokio::test]
    async fn locked_profile_delegation_persists_restorable_provider_lock() {
        let storage = SqliteStorage::connect(":memory:".into())
            .await
            .expect("in-memory storage");
        storage
            .set_session_runtime_binding(
                "parent",
                "locked-profile",
                None,
                Some("profile-fingerprint"),
                Some("provider-lock"),
                Some("{\"llama_cpp\":{}}"),
            )
            .await
            .expect("persist parent binding");

        persist_delegate_runtime_binding(&storage, "parent", "child", "locked-profile", "coder")
            .await
            .expect("persist delegate binding");

        let restored = storage
            .get_session_runtime_binding("child")
            .await
            .expect("restore delegate binding")
            .expect("delegate binding");
        assert_eq!(restored.profile_id, "locked-profile");
        assert_eq!(restored.agent_id.as_deref(), Some("coder"));
        assert_eq!(
            restored.profile_fingerprint.as_deref(),
            Some("profile-fingerprint")
        );
        assert_eq!(
            restored.provider_lock_digest.as_deref(),
            Some("provider-lock")
        );
        assert_eq!(
            restored.provider_locks_json.as_deref(),
            Some("{\"llama_cpp\":{}}")
        );
    }

    #[tokio::test]
    async fn unbound_profile_delegation_persists_null_provider_lock() {
        let storage = SqliteStorage::connect(":memory:".into())
            .await
            .expect("in-memory storage");
        persist_delegate_runtime_binding(
            &storage,
            "missing-parent",
            "child",
            "legacy-profile",
            "coder",
        )
        .await
        .expect("persist legacy delegate binding");

        let restored = storage
            .get_session_runtime_binding("child")
            .await
            .expect("restore delegate binding")
            .expect("delegate binding");
        assert_eq!(restored.profile_id, "legacy-profile");
        assert_eq!(restored.profile_fingerprint, None);
        assert_eq!(restored.provider_lock_digest, None);
        assert_eq!(restored.provider_locks_json, None);
    }

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
                crate::acp::protocol::StopReason::EndTurn,
            ))
        }

        async fn cancel(&self, _notif: CancelNotification) -> std::result::Result<(), Error> {
            Ok(())
        }

        async fn load_session(
            &self,
            _req: LoadSessionRequest,
        ) -> std::result::Result<LoadSessionResponse, Error> {
            Ok(LoadSessionResponse::new())
        }

        async fn create_delegation_session(
            &self,
            _cwd: Option<String>,
            _parent_session_id: String,
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

    #[tokio::test]
    async fn cancelled_child_finalization_uses_owner_once() {
        let fixture = crate::test_utils::helpers::DelegateTestFixture::new()
            .await
            .expect("separate planner and owner stores");
        let owner_store = fixture.delegate_storage.session_store();
        let child = owner_store
            .create_session(None, None, None, None)
            .await
            .expect("owner child");
        let cancellation = DelegationCancellation::default();
        cancellation.resolve("attempt").await;
        assert!(
            cancellation
                .child
                .set((fixture.delegate.clone(), child.public_id.clone()))
                .is_ok()
        );
        tokio::join!(
            cancellation.resolve("attempt"),
            cancellation.resolve("attempt")
        );
        assert!(
            owner_store
                .get_session_lifecycle(&child.public_id)
                .await
                .expect("owner lifecycle")
                .resolved
        );
        assert!(
            fixture
                .planner_storage
                .session_store()
                .get_session(&child.public_id)
                .await
                .expect("planner lookup")
                .is_none()
        );
        owner_store
            .patch_session_lifecycle(
                &child.public_id,
                &crate::session::lifecycle::SessionLifecyclePatch {
                    resolved: Some(false),
                    archived: Some(true),
                },
            )
            .await
            .expect("explicit owner reopen");
        cancellation.resolve("attempt").await;
        let lifecycle = owner_store
            .get_session_lifecycle(&child.public_id)
            .await
            .expect("owner lifecycle after replay");
        assert!(!lifecycle.resolved);
        assert!(lifecycle.archived);
    }

    #[tokio::test]
    async fn shutdown_waits_for_claimed_delegation_registration() {
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("storage"),
        );
        let session = storage
            .create_session(None, None, None, None)
            .await
            .expect("session");
        let delegation = storage
            .create_delegation(Delegation {
                isolation: Default::default(),
                id: 0,
                public_id: String::new(),
                session_id: session.id,
                task_id: None,
                target_agent_id: "coder".to_string(),
                objective: "test shutdown registration".to_string(),
                objective_hash: crate::hash::RapidHash::new(b"test shutdown registration"),
                context: None,
                constraints: None,
                expected_output: None,
                verification_spec: None,
                planning_summary: None,
                status: DelegationStatus::Requested,
                retry_count: 0,
                created_at: time::OffsetDateTime::now_utc(),
                completed_at: None,
            })
            .await
            .expect("delegation");
        let fanout = Arc::new(EventFanout::new());
        let event_sink = Arc::new(EventSink::new(storage.event_journal(), fanout.clone()));
        let mut registry = DefaultAgentRegistry::new();
        registry.register(make_agent_info("coder"), StubAgentHandle::new("coder"));
        let orchestrator = Arc::new(
            DelegationOrchestrator::new(
                StubAgentHandle::new("delegator"),
                event_sink,
                storage.session_store().clone(),
                Arc::new(registry),
                Arc::new(ToolRegistry::new()),
                Hooks::default(),
                None,
            )
            .with_max_parallel_delegations(NonZeroUsize::new(1).unwrap()),
        );
        let held_permit = orchestrator
            .max_parallel
            .clone()
            .acquire_owned()
            .await
            .expect("held permit");
        let listener = orchestrator.start_listening(&fanout);

        let requested = storage
            .event_journal()
            .append_durable(&crate::session::projection::NewDurableEvent {
                session_seq: None,
                session_id: session.public_id.clone(),
                origin: crate::events::EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                kind: AgentEventKind::DelegationRequested {
                    delegation: delegation.clone(),
                    tool_call_id: None,
                    batch_id: None,
                    cycle_id: None,
                },
            })
            .await
            .expect("requested event");
        fanout.publish(EventEnvelope::Durable(requested));
        orchestrator
            .wait_until_registered(&delegation.public_id)
            .await;

        orchestrator
            .event_sink
            .session_stream(&session.public_id)
            .flush()
            .await;
        assert_eq!(
            fanout.live_delegation_activity(&session.public_id)[0].stage,
            Some(crate::control::delegation_notifications::DelegationStage::Queued)
        );
        let mut stages = fanout.subscribe();
        orchestrator.begin_shutdown();
        listener.abort();
        listener.await.expect_err("listener should be aborted");
        let shutdown = orchestrator.cancel_active_delegations();
        tokio::pin!(shutdown);
        tokio::select! {
            _ = &mut shutdown => panic!("queued task still owns its permit wait"),
            _ = async {
                loop {
                    if matches!(stages.recv().await.unwrap().kind(), AgentEventKind::DelegationStageChanged { stage: Some(crate::control::delegation_notifications::DelegationStage::CancelRequested), .. }) { break; }
                }
            } => {}
        }
        assert_eq!(
            fanout.live_delegation_activity(&session.public_id)[0].stage,
            Some(crate::control::delegation_notifications::DelegationStage::CancelRequested)
        );
        shutdown.await;
        drop(held_permit);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    stages.recv().await.unwrap().kind(),
                    AgentEventKind::DelegationStageChanged { stage: None, .. }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(
            fanout
                .live_delegation_activity(&session.public_id)
                .is_empty()
        );

        assert!(orchestrator.active_delegations.lock().await.is_empty());
        assert_eq!(
            storage
                .get_delegation(&delegation.public_id)
                .await
                .expect("delegation lookup")
                .expect("delegation")
                .status,
            DelegationStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn claim_error_does_not_fail_running_winner() {
        let mut store = MockSessionStore::new();
        store
            .expect_transition_delegation_status()
            .withf(|id, expected, next| {
                id == "delegation-1"
                    && *expected == DelegationStatus::Requested
                    && *next == DelegationStatus::Failed
            })
            .times(1)
            .returning(|_, _, _| Ok(false));
        let store: Arc<dyn SessionStore> = Arc::new(store);
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("storage"),
        );
        let fanout = Arc::new(EventFanout::new());
        let event_sink = Arc::new(EventSink::new(storage.event_journal(), fanout));
        let orchestrator = DelegationOrchestrator::new(
            StubAgentHandle::new("delegator"),
            event_sink,
            store,
            Arc::new(DefaultAgentRegistry::new()),
            Arc::new(ToolRegistry::new()),
            Hooks::default(),
            None,
        );
        let delegation = Delegation {
            isolation: Default::default(),
            id: 1,
            public_id: "delegation-1".to_string(),
            session_id: 1,
            task_id: None,
            target_agent_id: "coder".to_string(),
            objective: "test".to_string(),
            objective_hash: crate::hash::RapidHash::new(b"test"),
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: None,
            planning_summary: None,
            status: DelegationStatus::Requested,
            retry_count: 0,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };

        orchestrator
            .fail_unclaimed_delegation(&delegation, "session-1", "claim failed")
            .await;
    }

    /// A stub handle that records delegation child-session launches.
    struct CountingStubHandle {
        child_launches: Arc<std::sync::atomic::AtomicUsize>,
        event_fanout: Arc<EventFanout>,
    }

    impl CountingStubHandle {
        fn new(child_launches: Arc<std::sync::atomic::AtomicUsize>) -> Arc<Self> {
            Arc::new(Self {
                child_launches,
                event_fanout: Arc::new(EventFanout::new()),
            })
        }
    }

    #[async_trait]
    impl AgentHandle for CountingStubHandle {
        async fn new_session(
            &self,
            _req: NewSessionRequest,
        ) -> std::result::Result<NewSessionResponse, Error> {
            Ok(NewSessionResponse::new("counting-session".to_string()))
        }

        async fn prompt(&self, _req: PromptRequest) -> std::result::Result<PromptResponse, Error> {
            Ok(PromptResponse::new(
                crate::acp::protocol::StopReason::EndTurn,
            ))
        }

        async fn cancel(&self, _notif: CancelNotification) -> std::result::Result<(), Error> {
            Ok(())
        }

        async fn load_session(
            &self,
            _req: LoadSessionRequest,
        ) -> std::result::Result<LoadSessionResponse, Error> {
            Ok(LoadSessionResponse::new())
        }

        async fn create_delegation_session(
            &self,
            _cwd: Option<String>,
            _parent_session_id: String,
        ) -> std::result::Result<(String, SessionActorRef), Error> {
            self.child_launches.fetch_add(1, Ordering::SeqCst);
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

    /// Failure/cancellation outputs retain workspace references: the note
    /// identifies the isolated workspace, its path, and its branch; shared
    /// execution produces no retention note.
    #[test]
    fn workspace_retention_note_identifies_the_isolated_workspace() {
        // A real (temp) checkout path: the mutation gate canonicalizes it.
        let checkout = tempfile::TempDir::new().unwrap();
        let storage = tempfile::TempDir::new().unwrap();
        let binding = crate::worktree::WorkspaceBinding {
            record: crate::worktree::WorkspaceRecord {
                id: crate::worktree::WorkspaceId::from_raw("ws-retain"),
                repository: crate::worktree::RepositoryIdentity {
                    common_dir: std::path::PathBuf::from("/repo/.git"),
                    origin_path: std::path::PathBuf::from("/repo"),
                    project_name: "repo".to_string(),
                },
                checkout_path: checkout.path().to_path_buf(),
                branch: crate::worktree::ManagedBranch::for_workspace(
                    &crate::worktree::WorkspaceId::from_raw("ws-retain"),
                ),
                relative_cwd: std::path::PathBuf::from(""),
                source_cwd: String::new(),
                execution: crate::worktree::ExecutionMode::Worktree,
                baseline: None,
                lifecycle: crate::worktree::WorkspaceLifecycle::Ready,
                ownership: crate::worktree::WorkspaceOwnership::default(),
                last_error: None,
                requested_baseline_mode: crate::worktree::BaselineMode::Commit,
                requested_base_ref: None,
                operation_id: "op-retain".to_string(),
                provisioning_phase: None,
                created_at: time::OffsetDateTime::now_utc(),
                updated_at: time::OffsetDateTime::now_utc(),
            },
            effective_cwd: checkout.path().to_path_buf(),
            mutation_gate: crate::worktree::CheckoutMutationGate::new(
                storage.path(),
                checkout.path(),
            )
            .unwrap()
            .into(),
        };

        let note = workspace_retention_note(Some(&binding));
        assert!(note.contains("ws-retain"), "note: {note}");
        assert!(
            note.contains(&checkout.path().to_string_lossy().to_string()),
            "note: {note}"
        );
        assert!(
            note.contains("retained with all its edits"),
            "note must promise retention: {note}"
        );
        assert!(
            note.contains("nothing was integrated"),
            "note must not claim integration: {note}"
        );

        // Shared execution retains nothing and claims nothing.
        assert_eq!(workspace_retention_note(None), "");
    }

    /// Isolated delegation fails closed when no workspace service is
    /// configured: the failure is reported and no child session is ever
    /// created — requested isolation never falls back to shared execution.
    #[tokio::test]
    async fn isolated_delegation_fails_closed_without_workspace_service() {
        isolation_preflight_fails_closed(false).await;
    }

    #[tokio::test]
    async fn isolated_delegation_rejects_remote_execution_before_allocation() {
        isolation_preflight_fails_closed(true).await;
    }

    async fn isolation_preflight_fails_closed(configure_service: bool) {
        let child_launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (failed_tx, failed_rx) = tokio::sync::oneshot::channel::<()>();
        let failed_tx = Arc::new(std::sync::Mutex::new(Some(failed_tx)));
        let mut store = MockSessionStore::new();
        store
            .expect_claim_delegation()
            .withf(|id| id == "delegation-1")
            .times(1)
            .returning(|_| Ok(DelegationClaim::Claimed));
        store
            .expect_transition_delegation_status()
            .withf(|id, expected, next| {
                id == "delegation-1"
                    && *expected == DelegationStatus::Running
                    && *next == DelegationStatus::Failed
            })
            .times(1)
            .returning(move |_, _, _| {
                if let Some(tx) = failed_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                Ok(true)
            });
        let store: Arc<dyn SessionStore> = Arc::new(store);
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("storage"),
        );
        let fanout = Arc::new(EventFanout::new());
        let event_sink = Arc::new(EventSink::new(storage.event_journal(), fanout));

        let mut registry = DefaultAgentRegistry::new();
        registry.register(
            make_agent_info("coder"),
            CountingStubHandle::new(child_launches.clone()),
        );
        let mut orchestrator = DelegationOrchestrator::new(
            StubAgentHandle::new("delegator"),
            event_sink,
            store,
            Arc::new(registry),
            Arc::new(ToolRegistry::new()),
            Hooks::default(),
            None,
        );
        let storage_root = tempfile::TempDir::new().unwrap();
        if configure_service {
            orchestrator = orchestrator.with_workspace_service(Arc::new(
                crate::worktree::WorkspaceService::new(
                    Arc::new(crate::worktree::GixRepoOps::new()),
                    storage.workspace_repository().unwrap(),
                    storage_root.path(),
                ),
            ));
        }

        let delegation = Delegation {
            isolation: crate::worktree::DelegationIsolation::Isolated,
            id: 1,
            public_id: "delegation-1".to_string(),
            session_id: 1,
            task_id: None,
            target_agent_id: "coder".to_string(),
            objective: "test".to_string(),
            objective_hash: crate::hash::RapidHash::new(b"test"),
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: None,
            planning_summary: None,
            status: DelegationStatus::Requested,
            retry_count: 0,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };

        let envelope = EventEnvelope::from(crate::events::AgentEvent {
            seq: 0,
            session_seq: 0,
            timestamp: 0,
            session_id: "session-1".to_string(),
            origin: crate::events::EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::DelegationRequested {
                delegation,
                tool_call_id: None,
                batch_id: None,
                cycle_id: None,
            },
        });
        orchestrator.handle_envelope(&envelope).await;

        // The delegation is durably failed.
        tokio::time::timeout(std::time::Duration::from_secs(5), failed_rx)
            .await
            .expect("delegation must fail closed in time")
            .expect("failure signal sender dropped early");

        assert_eq!(
            child_launches.load(Ordering::SeqCst),
            0,
            "no child session may be launched without a workspace service or local execution capability"
        );
        assert!(
            storage
                .workspace_repository()
                .unwrap()
                .list_workspaces()
                .await
                .unwrap()
                .is_empty(),
            "remote filesystem isolation fails before any allocation"
        );
    }

    fn make_agent_info(id: &str) -> AgentInfo {
        AgentInfo {
            isolation: Default::default(),
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
            isolation: Default::default(),
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
            isolation: Default::default(),
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

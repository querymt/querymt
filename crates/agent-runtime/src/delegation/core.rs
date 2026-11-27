use crate::acp::protocol::{ContentBlock, PromptRequest, StopReason, TextContent};
use crate::agent::session_actor::TurnAuthority;
use crate::delegation::roster::{AgentRoster, MemberAgent};
use crate::hooks::{
    DelegationFailureRequest, DelegationStartRequest, HookNotice, Hooks, PostDelegationRequest,
};
use crate::session::domain::{
    Delegation, DelegationClaim, DelegationStatus, ForkOrigin, ForkPointType,
};
use crate::session::error::SessionResult;
use crate::session::store::SessionStore;
use crate::tools::ToolRegistry;
use log::{debug, error, warn};
use querymt::chat::{ChatRole, ReasoningEffort};
use querymt_agent_core::events::{AgentEventKind, EventEnvelope};
use querymt_agent_core::model::{AgentMessage, MessagePart};
use querymt_agent_services::event_fanout::EventFanout;
use querymt_agent_services::event_sink::EventSink;
use querymt_agent_services::verification::VerificationSpec;
use querymt_agent_services::verification::service::{VerificationContext, VerificationService};
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

/// The child session of a delegation, once it exists.
struct DelegationChild {
    target: MemberAgent,
    session_id: String,
    /// The root that owns the delegation. Weak: a running delegation must not
    /// keep an abandoned runtime alive.
    root: std::sync::Weak<crate::agent::LocalAgentHandle>,
}

#[derive(Default)]
struct DelegationCancellation {
    child: OnceLock<DelegationChild>,
    finalized: AtomicBool,
}

impl DelegationCancellation {
    async fn resolve(&self, delegation_id: &str) {
        if let Some(child) = self.child.get()
            && !self.finalized.swap(true, Ordering::AcqRel)
        {
            let Some(root) = child.root.upgrade() else {
                return;
            };
            let target = child.target.clone();
            let child_session_id = child.session_id.clone();
            let delegation_id = delegation_id.to_owned();
            // A grace-period abort of the execution task must not drop the owner patch.
            let _ = tokio::spawn(async move {
                resolve_terminal_delegate_attempt(
                    &root,
                    &target,
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
    pub id: super::roster::AgentId,
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
    pub run_verification: bool,
    pub wait_policy: crate::config::DelegationWaitPolicy,
    pub wait_timeout_secs: u64,
    pub cancel_grace_secs: u64,
    pub max_parallel_delegations: NonZeroUsize,
}

impl DelegationOrchestratorConfig {
    /// The behaviour `settings` describe, working in `cwd`.
    pub fn new(cwd: Option<PathBuf>, settings: &crate::config::DelegationSettings) -> Self {
        Self {
            cwd,
            run_verification: settings.verification,
            wait_policy: settings.wait_policy.clone(),
            wait_timeout_secs: settings.wait_timeout_secs,
            cancel_grace_secs: settings.cancel_grace_secs,
            max_parallel_delegations: settings.max_parallel,
        }
    }
}

pub struct DelegationOrchestrator {
    /// The root that owns this service. Weak: the root owns the service.
    root: std::sync::Weak<crate::agent::LocalAgentHandle>,
    event_sink: Arc<EventSink>,
    store: Arc<dyn SessionStore>,
    roster: Arc<AgentRoster>,
    pub(crate) tool_registry: Arc<ToolRegistry>,
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
    /// A service for `root`, delegating to the members of `roster` as
    /// `settings` describe. Event sink, store, tool registry and hooks are
    /// those of the root's config.
    pub fn new(
        root: &Arc<crate::agent::LocalAgentHandle>,
        roster: Arc<AgentRoster>,
        settings: &crate::config::DelegationSettings,
        cwd: Option<PathBuf>,
    ) -> Self {
        let config = &root.config;
        Self {
            root: Arc::downgrade(root),
            event_sink: config.event_sink.clone(),
            store: config.provider.history_store(),
            roster,
            tool_registry: config.tool_registry_arc(),
            hooks: config.hooks.clone(),
            config: DelegationOrchestratorConfig::new(cwd, settings),
            max_parallel: Arc::new(tokio::sync::Semaphore::new(settings.max_parallel.get())),
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
    #[must_use]
    pub fn with_budget_cap(mut self, cap_usd: Option<f64>) -> Self {
        self.budget_cap_usd = cap_usd;
        self
    }

    /// Snapshot this manual budget pricing onto delegation-summary attempts.
    #[must_use]
    pub fn with_budget_pricing(
        mut self,
        pricing: Option<querymt::providers::ModelPricing>,
    ) -> Self {
        self.budget_pricing = pricing;
        self
    }

    #[must_use]
    pub fn with_summarizer(
        mut self,
        summarizer: Option<Arc<super::summarizer::DelegationSummarizer>>,
    ) -> Self {
        self.delegation_summarizer = summarizer;
        self
    }

    #[must_use]
    pub fn with_delegate_model_overrides(
        mut self,
        overrides: super::DelegateModelOverrideStore,
    ) -> Self {
        self.delegate_model_overrides = overrides;
        self
    }

    #[must_use]
    pub fn with_profile_id(mut self, profile_id: impl Into<String>) -> Self {
        self.profile_id = Some(profile_id.into());
        self
    }

    /// Set the routing snapshot handle for per-agent routing decisions.
    ///
    /// When set, the orchestrator will consult the routing table before creating
    /// delegation sessions and write `provider_node_id` from the routing policy
    /// to the session's DB row.
    #[must_use]
    pub fn with_routing_snapshot(
        mut self,
        snapshot: crate::agent::remote::RoutingSnapshotHandle,
    ) -> Self {
        self.routing_snapshot = Some(snapshot);
        self
    }

    /// Enable isolated delegation through the given workspace service.
    #[must_use]
    pub fn with_workspace_service(
        mut self,
        service: Arc<crate::worktree::WorkspaceService>,
    ) -> Self {
        self.workspace_service = Some(service);
        self
    }

    /// The settings this service runs with.
    pub fn config(&self) -> &DelegationOrchestratorConfig {
        &self.config
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
                self.event_sink.session_stream(parent_session_id).emit(
                    AgentEventKind::DelegationFailed {
                        delegation_id: delegation.public_id.clone(),
                        error: error_message.to_string(),
                    },
                );
            }
            Ok(false) => {}
            Err(_) => warn!("Failed to persist delegation claim failure"),
        }
    }

    /// Claims a delegation, retrying only transient storage errors (database
    /// busy or locked) up to three attempts in total. Any other error is
    /// returned after the first attempt with its kind intact.
    async fn claim_delegation(&self, delegation_id: &str) -> SessionResult<DelegationClaim> {
        const MAX_ATTEMPTS: usize = 3;
        let mut attempt = 1;
        loop {
            match self.store.claim_delegation(delegation_id).await {
                Ok(result) => return Ok(result),
                Err(err) if err.is_transient() && attempt < MAX_ATTEMPTS => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Process a single event envelope.
    async fn handle_envelope(&self, envelope: &EventEnvelope) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let session_id = envelope.session_id();
        let Some(delegator) = self.root.upgrade() else {
            return;
        };
        match envelope.kind() {
            AgentEventKind::DelegationRequested {
                delegation,
                batch_id,
                cycle_id,
                handoff_from,
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
                            &delegator,
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

                let target_handle = self
                    .roster
                    .snapshot()
                    .get(&delegation.target_agent_id)
                    .cloned()
                    .ok_or_else(|| format!("Unknown agent '{}'", delegation.target_agent_id))
                    .and_then(|member| {
                        self.roster
                            .resolve(&member)
                            .map_err(|error| format!("{:#}", anyhow::Error::from(error)))
                    });
                let target_handle = match target_handle {
                    Ok(handle) => handle,
                    Err(message) => {
                        fail_delegation(
                            DelegationFailureContext {
                                delegator: &delegator,
                                store: &self.store,
                                hooks: None,
                                parent_session_id: session_id,
                                delegation_id: &delegation.public_id,
                                target_agent_id: Some(&delegation.target_agent_id),
                                objective: Some(&delegation.objective),
                                child_session: None,
                                execution_cwd: None,
                            },
                            &message,
                        )
                        .await;
                        return;
                    }
                };

                let delegator_for_task = delegator.clone();
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
                let handoff_from = handoff_from.clone();
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
                        let delegator = delegator_for_task;
                        if start_rx.await.is_err() {
                            return;
                        }
                        let _permit = match max_parallel.acquire_owned().await {
                            Ok(permit) => permit,
                            Err(_) => {
                                fail_delegation(
                                    DelegationFailureContext {
                                        delegator: &delegator,
                                        store: &store,
                                        hooks: Some(&hooks),
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
                            handoff_from,
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
                            delegator: &delegator,
                            store: &self.store,
                            hooks: None,
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
    delegator: Arc<crate::agent::LocalAgentHandle>,
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
    /// Earlier delegation of the parent whose isolated worktree this
    /// delegation takes over by ownership handoff.
    handoff_from: Option<String>,
    /// Parent execution cycle that requested this delegation. The planning
    /// summary is charged to it even if it settles after newer cycles begin.
    requesting_cycle_id: Option<String>,
    /// Manual budget pricing for the summary attempt, if configured.
    budget_pricing: Option<querymt::providers::ModelPricing>,
    /// Per-cycle price cap checked before the summary dispatch.
    budget_cap_usd: Option<f64>,
}

struct DelegationFailureContext<'a> {
    delegator: &'a Arc<crate::agent::LocalAgentHandle>,
    store: &'a Arc<dyn SessionStore>,
    hooks: Option<&'a Hooks>,
    parent_session_id: &'a str,
    delegation_id: &'a str,
    target_agent_id: Option<&'a str>,
    objective: Option<&'a str>,
    child_session: Option<(&'a MemberAgent, &'a str)>,
    /// The delegate's effective execution cwd once its workspace/checkout
    /// has been allocated; `None` for pre-allocation failures, which keep
    /// explicit parent context only.
    execution_cwd: Option<PathBuf>,
}

async fn teardown_failed_delegate_session(
    target: &MemberAgent,
    child_session_id: &str,
    session_ref: &crate::agent::remote::SessionActorRef,
    bridge_connection_id: Option<&Arc<str>>,
) {
    if let (Some(target), Some(connection_id)) = (target.as_local(), bridge_connection_id) {
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
) -> Result<
    querymt_agent_services::accounting::AttemptOperation,
    querymt_agent_services::accounting::AccountingError,
> {
    let accounting = ctx.event_sink.session_accounting(parent_session_id).await?;
    let cycle = match cycle_id {
        Some(id) => {
            querymt_agent_services::accounting::CycleAccounting::with_id(accounting.clone(), id)
        }
        None => querymt_agent_services::accounting::CycleAccounting::begin_maintenance(
            accounting.clone(),
        ),
    };
    let recorder = Arc::new(
        querymt_agent_services::accounting::AttemptRecorder::with_projection(
            ctx.event_sink.session_stream(parent_session_id),
            accounting,
        ),
    );
    Ok(summarizer
        .attempt_operation(recorder, &cycle)
        .with_budget_pricing(ctx.budget_pricing.clone())
        .with_budget_cap(ctx.budget_cap_usd))
}

/// Boxed like [`execute_delegation_inner`].
fn execute_delegation(
    ctx: DelegationContext,
    target: MemberAgent,
    parent_session_id: String,
    delegation: Delegation,
    cancel_token: CancellationToken,
) -> futures_util::future::BoxFuture<'static, ()> {
    Box::pin(execute_delegation_unboxed(
        ctx,
        target,
        parent_session_id,
        delegation,
        cancel_token,
    ))
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
async fn execute_delegation_unboxed(
    ctx: DelegationContext,
    target: MemberAgent,
    parent_session_id: String,
    delegation: Delegation,
    cancel_token: CancellationToken,
) {
    let admission = async {
        let origin = ctx.store.delegation_origin(&delegation.public_id).await?;
        #[cfg(feature = "remote")]
        let mesh = ctx.delegator.mesh();
        let authority = TurnAuthority::from_stored_origin(
            origin,
            #[cfg(feature = "remote")]
            mesh.as_ref(),
        )
        .await?;
        #[cfg(not(feature = "remote"))]
        let restricted = false;
        #[cfg(feature = "remote")]
        let restricted = if let TurnAuthority::Peer(peer) = &authority {
            let member = target
                .as_local()
                .ok_or(crate::error::AgentError::MeshAccessDenied)?;
            let grant = peer.grant().await?;
            if member.config.hooks.is_enabled() || ctx.hooks.is_enabled() {
                return Err(crate::error::AgentError::MeshAccessDenied.into());
            }
            let restricted = grant != querymt_remote::peer_access::PeerGrant::Full;
            if restricted
                && (ctx.workspace_service.is_some()
                    || delegation.isolation != crate::worktree::DelegationIsolation::Shared
                    || !member.supports_confined_delegation())
            {
                return Err(crate::error::AgentError::MeshAccessDenied.into());
            }
            restricted
        } else {
            false
        };
        Ok::<_, anyhow::Error>((authority, restricted))
    }
    .await;
    let (authority, restricted) = match admission {
        Ok(admitted) => admitted,
        Err(error) => {
            fail_delegation(
                DelegationFailureContext {
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: None,
                    parent_session_id: &parent_session_id,
                    delegation_id: &delegation.public_id,
                    target_agent_id: None,
                    objective: None,
                    execution_cwd: None,
                    child_session: None,
                },
                &format!("delegation authority unavailable: {error}"),
            )
            .await;
            ctx.active_delegations
                .lock()
                .await
                .remove(&delegation.public_id);
            return;
        }
    };
    #[cfg(feature = "remote")]
    if let TurnAuthority::Peer(peer) = &authority {
        let mut ctx = ctx;
        // These optional operations cannot inherit the caller's model or process authority.
        ctx.routing_snapshot = None;
        ctx.delegation_summarizer = None;
        ctx.config.run_verification = false;
        let run = execute_delegation_inner(
            ctx,
            target,
            parent_session_id,
            delegation,
            cancel_token.clone(),
            &authority,
            restricted,
        );
        tokio::pin!(run);
        tokio::select! {
            biased;
            _ = peer.revoked(None) => {
                cancel_token.cancel();
                run.await;
            }
            () = &mut run => {},
        }
        return;
    }
    execute_delegation_inner(
        ctx,
        target,
        parent_session_id,
        delegation,
        cancel_token,
        &authority,
        restricted,
    )
    .await;
}

/// The delegation run, built and boxed here so that callers hold a pointer
/// rather than the run's whole state (large in unoptimized builds, where it
/// would add to the stack frame of every caller).
fn execute_delegation_inner(
    ctx: DelegationContext,
    target: MemberAgent,
    parent_session_id: String,
    delegation: Delegation,
    cancel_token: CancellationToken,
    authority: &TurnAuthority,
    restricted: bool,
) -> futures_util::future::BoxFuture<'_, ()> {
    Box::pin(execute_delegation_inner_unboxed(
        ctx,
        target,
        parent_session_id,
        delegation,
        cancel_token,
        authority,
        restricted,
    ))
}

async fn execute_delegation_inner_unboxed(
    ctx: DelegationContext,
    target: MemberAgent,
    parent_session_id: String,
    delegation: Delegation,
    cancel_token: CancellationToken,
    authority: &TurnAuthority,
    _restricted: bool,
) {
    let delegation_id = delegation.public_id.clone();
    let mut planning_context_additions = String::new();
    if let Err(error) = authority.check().await {
        fail_delegation(
            DelegationFailureContext {
                delegator: &ctx.delegator,
                store: &ctx.store,
                hooks: None,
                parent_session_id: &parent_session_id,
                delegation_id: &delegation_id,
                target_agent_id: None,
                objective: None,
                execution_cwd: None,
                child_session: None,
            },
            &error.to_string(),
        )
        .await;
        ctx.active_delegations.lock().await.remove(&delegation_id);
        return;
    }

    // Isolated delegation fails closed without a workspace service: a
    // requested isolation never falls back to sharing the parent checkout,
    // and no child session is launched.
    if delegation.isolation == crate::worktree::DelegationIsolation::Isolated
        && (ctx.workspace_service.is_none() || !target.supports_local_workspaces())
    {
        fail_delegation(
            DelegationFailureContext {
                delegator: &ctx.delegator,
                store: &ctx.store,
                hooks: Some(&ctx.hooks),
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

    // A handoff continues an isolated worktree: it never degrades to a shared
    // checkout, whatever isolation was recorded with the request.
    if ctx.handoff_from.is_some()
        && delegation.isolation != crate::worktree::DelegationIsolation::Isolated
    {
        fail_delegation(
            DelegationFailureContext {
                delegator: &ctx.delegator,
                store: &ctx.store,
                hooks: Some(&ctx.hooks),
                parent_session_id: &parent_session_id,
                delegation_id: &delegation.public_id,
                target_agent_id: Some(&delegation.target_agent_id),
                objective: Some(&delegation.objective),
                execution_cwd: None,
                child_session: None,
            },
            "delegation handoff refused: handoff_from requires isolated execution",
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
                        delegator: &ctx.delegator,
                        store: &ctx.store,
                        hooks: Some(&ctx.hooks),
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
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
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
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
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
                            delegator: &ctx.delegator,
                            store: &ctx.store,
                            hooks: Some(&ctx.hooks),
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
    let mut pending_handoff: Option<crate::worktree::PendingHandoff> = None;
    let child_workspace = match (
        delegation.isolation == crate::worktree::DelegationIsolation::Isolated,
        ctx.workspace_service.as_ref(),
    ) {
        // Delegation by handoff: the delegate takes over the worktree of an
        // earlier delegation instead of allocating one. Every refusal fails
        // the delegation before any session exists; the previous owner is
        // untouched and nothing is allocated.
        (true, Some(service)) if ctx.handoff_from.is_some() => {
            let source = ctx.handoff_from.as_deref().unwrap_or_default();
            let refusal = if _restricted {
                Some(
                    "handoff_unavailable: a restricted mesh delegation cannot take over a worktree"
                        .to_string(),
                )
            } else if cancel_token.is_cancelled() {
                Some("delegation cancelled".to_string())
            } else {
                None
            };
            let begun = match refusal {
                Some(message) => Err(message),
                None => begin_delegation_handoff(&ctx, service, &parent_session_id, source).await,
            };
            match begun {
                Ok((binding, pending)) => {
                    pending_handoff = Some(pending);
                    Some(binding)
                }
                Err(message) => {
                    fail_delegation(
                        DelegationFailureContext {
                            delegator: &ctx.delegator,
                            store: &ctx.store,
                            hooks: Some(&ctx.hooks),
                            parent_session_id: &parent_session_id,
                            delegation_id: &delegation.public_id,
                            target_agent_id: Some(&delegation.target_agent_id),
                            objective: Some(&delegation.objective),
                            execution_cwd: None,
                            child_session: None,
                        },
                        &format!("delegation handoff from '{source}' refused: {message}"),
                    )
                    .await;
                    ctx.active_delegations.lock().await.remove(&delegation_id);
                    return;
                }
            }
        }
        (true, Some(service)) => {
            let Some(parent_cwd) = effective_cwd.clone() else {
                fail_delegation(
                    DelegationFailureContext {
                        delegator: &ctx.delegator,
                        store: &ctx.store,
                        hooks: Some(&ctx.hooks),
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
                            delegator: &ctx.delegator,
                            store: &ctx.store,
                            hooks: Some(&ctx.hooks),
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
                .allocate_for_delegation_checked(
                    &parent_session_id,
                    parent_record.as_ref(),
                    &parent_cwd,
                    parent_gate.as_ref(),
                    &delegation.public_id,
                    Some(batch_id),
                    || async {
                        let cancelled = || {
                            crate::worktree::WorktreeError::InvalidOperation(
                                "delegation cancelled".into(),
                            )
                        };
                        if cancel_token.is_cancelled() {
                            return Err(cancelled());
                        }
                        authority.check().await.map_err(|error| {
                            crate::worktree::WorktreeError::InvalidOperation(error.to_string())
                        })?;
                        if cancel_token.is_cancelled() {
                            return Err(cancelled());
                        }
                        Ok(())
                    },
                )
                .await
            {
                Ok(binding) => Some(binding),
                Err(err) => {
                    fail_delegation(
                        DelegationFailureContext {
                            delegator: &ctx.delegator,
                            store: &ctx.store,
                            hooks: Some(&ctx.hooks),
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
            turn_id: querymt_agent_core::model::TurnId::new().to_string(),
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
    let mut handoff_previous_owner: Option<String> = None;
    let (child_session_id, session_ref) = match async {
        #[cfg(feature = "remote")]
        if _restricted {
            let member = target.as_local().ok_or_else(|| {
                crate::acp::protocol::Error::from(crate::error::AgentError::MeshAccessDenied)
            })?;
            return member
                .create_confined_delegation_session(parent_session_id.clone(), authority)
                .await;
        }
        match (child_workspace.as_ref(), pending_handoff.take()) {
            (Some(binding), Some(pending)) => {
                // Ownership moves inside this call, before the actor starts.
                handoff_previous_owner = Some(pending.previous().to_owned());
                target
                    .create_handoff_delegation_session(
                        binding.clone(),
                        parent_session_id.clone(),
                        pending,
                    )
                    .await
            }
            (Some(binding), None) => {
                target
                    .create_isolated_delegation_session(binding.clone(), parent_session_id.clone())
                    .await
            }
            (None, _) => {
                target
                    .create_delegation_session(
                        &ctx.delegator,
                        cwd_string,
                        parent_session_id.clone(),
                    )
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
            // A handoff never fails the workspace: it already had an owner,
            // and the materializer returned or kept ownership consistently.
            if let (Some(binding), Some(service), None) = (
                child_workspace.as_ref(),
                ctx.workspace_service.as_ref(),
                handoff_previous_owner.as_deref(),
            ) {
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
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
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
    // Hold the delegate's checkout until the delegation reports its terminal
    // state: verification, the history read and the result capture below run
    // there after the delegate's actor went idle. Taken before anything else
    // can wait, so no handoff, archive or snapshot merge slips in between.
    let _delegation_hold = hold_delegate_checkout(
        &target,
        ctx.workspace_service.as_ref(),
        child_workspace.as_ref(),
        &child_session_id,
    );
    // Ownership moved to the delegate: record it on the previous owner's
    // stream (best effort, like `/worktree handoff`) and refresh the binding
    // so later steps see the delegate as the owner.
    let child_workspace = match (child_workspace, handoff_previous_owner.as_deref()) {
        (Some(binding), Some(previous)) => {
            use crate::worktree::HandoffHost as _;
            ctx.delegator
                .announce_handoff(
                    previous,
                    binding.record.id.as_str(),
                    &child_session_id,
                    false,
                )
                .await;
            Some(match ctx.workspace_service.as_ref() {
                Some(service) => match service.registry().get_workspace(&binding.record.id).await {
                    Ok(Some(record)) => service.binding_for(record).unwrap_or(binding),
                    _ => binding,
                },
                None => binding,
            })
        }
        (binding, _) => binding,
    };
    let _ = ctx.cancellation.child.set(DelegationChild {
        target: target.clone(),
        session_id: child_session_id.clone(),
        root: Arc::downgrade(&ctx.delegator),
    });
    let child_session = Some((&target, child_session_id.as_str()));

    let mut child_bridge_connection_id = None;
    let parent_bridge = ctx
        .delegator
        .config
        .session_bridges
        .lock()
        .ok()
        .and_then(|bridges| {
            bridges
                .get(&parent_session_id)
                .map(|route| route.bridge.clone())
        });
    // Locally hosted children inherit the parent's connection route so the
    // workspace-query capability follows the delegation. Remote children never
    // get a route (workspace queries are unavailable for remote sessions).
    if let Some(bridge) = parent_bridge
        && !session_ref.is_remote()
        && let Some(target) = target.as_local()
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
                delegator: &ctx.delegator,
                store: &ctx.store,
                hooks: Some(&ctx.hooks),
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
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
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
                        delegator: &ctx.delegator,
                        store: &ctx.store,
                        hooks: Some(&ctx.hooks),
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
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
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
                delegator: &ctx.delegator,
                store: &ctx.store,
                hooks: Some(&ctx.hooks),
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
        result = session_ref.prompt_with_authority(prompt_req, authority) => Some(result),
        _ = cancel_token.cancelled() => {
            ctx.activity.stage(Some(crate::control::delegation_notifications::DelegationStage::CancelRequested));
            // Cancellation — cancel the child session
            let _ = session_ref.cancel().await;
            release_delegate_checkout(&target, &child_session_id);

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

            emit_delegation_event(&ctx.delegator,
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
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
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
                        delegator: &ctx.delegator,
                        store: &ctx.store,
                        hooks: Some(&ctx.hooks),
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
                    turn_id: querymt_agent_core::model::TurnId::new().to_string(),
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
                    .publish_delegation_result(
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
                        // This run published it; after a handoff it is not
                        // the worktree's own delegation, and acceptance
                        // judges this run's status.
                        Some(&delegation.public_id),
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
                                        // The worktree stays named by the
                                        // delegation that created it, also
                                        // after a delegation by handoff.
                                        binding
                                            .record
                                            .ownership
                                            .delegation_id
                                            .as_deref()
                                            .unwrap_or(delegation.public_id.as_str()),
                                        review.result_commit,
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

            // Verification, the history read and the result capture are done.
            release_delegate_checkout(&target, &child_session_id);
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
                        &ctx.delegator,
                        &target,
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
                &parent_session_id,
                AgentEventKind::DelegationCompleted {
                    delegation_id: delegation.public_id.clone(),
                    result: Some(summary.clone()),
                },
            );

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
                    delegator: &ctx.delegator,
                    store: &ctx.store,
                    hooks: Some(&ctx.hooks),
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

/// Holds an isolated local delegate's session and checkout for the rest of the
/// delegation (see [`crate::agent::handle::delegation_hold`]). `None` for a
/// shared or remote delegate: nothing of theirs can be handed off.
fn hold_delegate_checkout(
    target: &MemberAgent,
    orchestrator_service: Option<&Arc<crate::worktree::WorkspaceService>>,
    child_workspace: Option<&crate::worktree::WorkspaceBinding>,
    child_session_id: &str,
) -> Option<crate::agent::handle::delegation_hold::DelegationHold> {
    let binding = child_workspace?;
    let member = target.as_local()?;
    // The lease lives under the storage root of the service the delegate's
    // own actor and the worktree operations use: the member's.
    let lease = member
        .config
        .workspace_service()
        .or_else(|| orchestrator_service.cloned())
        .and_then(|service| {
            crate::worktree::gate::CheckoutActivityLease::acquire(
                service.storage_root(),
                &binding.record.checkout_path,
                false,
            )
            .inspect_err(|err| {
                warn!(
                    "delegation: could not take the shared activity lease of {}: {err}",
                    binding.record.checkout_path.display()
                );
            })
            .ok()
        });
    Some(member.delegation_holds.hold(child_session_id, lease))
}

/// Ends the hold of [`hold_delegate_checkout`] before the delegation reports
/// its terminal state: the delegating session may hand the worktree on as
/// soon as it learns that the delegation ended.
fn release_delegate_checkout(target: &MemberAgent, child_session_id: &str) {
    if let Some(member) = target.as_local() {
        member.delegation_holds.release(child_session_id);
    }
}

/// First half of a delegation by handoff: find the worktree of `source`, check
/// that the delegating session made it, stop its idle owner and hold the
/// checkout. Nothing is created or changed; every refusal names its code.
///
/// The worktree stays recorded under the delegation that created it, so
/// `source` is always that delegation (the id the orchestrator was shown); a
/// handoff delegation's own id has no worktree of its own.
async fn begin_delegation_handoff(
    ctx: &DelegationContext,
    service: &Arc<crate::worktree::WorkspaceService>,
    parent_session_id: &str,
    source: &str,
) -> Result<
    (
        crate::worktree::WorkspaceBinding,
        crate::worktree::PendingHandoff,
    ),
    String,
> {
    match ctx.store.get_delegation(source).await {
        Ok(Some(_)) => {}
        Ok(None) => return Err(format!("unknown_delegation: no delegation '{source}'")),
        Err(err) => return Err(format!("cannot look up delegation '{source}': {err}")),
    }
    let record = match service.registry().get_workspace_by_delegation(source).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return Err(format!(
                "no_isolated_workspace: delegation '{source}' has no isolated worktree \
                 (it ran in a shared checkout, or only continued another delegation's worktree)"
            ));
        }
        Err(err) => return Err(format!("cannot look up the worktree of '{source}': {err}")),
    };
    if record.ownership.parent_session_id.as_deref() != Some(parent_session_id) {
        return Err(format!(
            "not_parent_session: delegation '{source}' was not made by this session"
        ));
    }
    let ops = crate::worktree::WorktreeOps::new(service.clone());
    let pending = ops
        .begin_handoff(
            &*ctx.delegator,
            &crate::worktree::WorkspaceTarget::Workspace(record.id.to_string()),
        )
        .await
        .map_err(|err| format!("{}: {err}", err.code()))?;
    let binding = service
        .binding_for(pending.record().clone())
        .map_err(|err| format!("cannot bind the worktree: {err}"))?;
    Ok((binding, pending))
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
    root: &crate::agent::LocalAgentHandle,
    target: &MemberAgent,
    child_session_id: &str,
    delegation_id: &str,
) {
    // Resolution marks the one-shot attempt as finished, not as successful execution.
    if let Err(error) = target
        .set_session_lifecycle(
            root,
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

/// Record a failed delegation. Boxed: the delegation run fails at many points,
/// and an unboxed future would take its own stack slot in the run's frame at
/// each of them in unoptimized builds.
fn fail_delegation<'a>(
    ctx: DelegationFailureContext<'a>,
    error_message: &'a str,
) -> futures_util::future::BoxFuture<'a, ()> {
    Box::pin(fail_delegation_unboxed(ctx, error_message))
}

#[instrument(
    name = "delegation.fail",
    skip(ctx, error_message),
    fields(
        delegation_id = %ctx.delegation_id,
        parent_session_id = %ctx.parent_session_id,
    )
)]
async fn fail_delegation_unboxed(ctx: DelegationFailureContext<'_>, error_message: &str) {
    error!("Delegation failed");
    if let Some((target, child_session_id)) = ctx.child_session {
        release_delegate_checkout(target, child_session_id);
    }
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
        resolve_terminal_delegate_attempt(
            ctx.delegator,
            target,
            child_session_id,
            ctx.delegation_id,
        )
        .await;
    }

    let mut failure_message = error_message.to_string();
    if let Some(hooks) = ctx.hooks {
        match hooks
            .run_delegation_failure(DelegationFailureRequest {
                session_id: ctx.parent_session_id.to_string(),
                turn_id: querymt_agent_core::model::TurnId::new().to_string(),
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
        ctx.parent_session_id,
        AgentEventKind::DelegationFailed {
            delegation_id: ctx.delegation_id.to_string(),
            error: failure_message.clone(),
        },
    );
}

fn emit_delegation_event(
    delegator: &Arc<crate::agent::LocalAgentHandle>,
    session_id: &str,
    kind: AgentEventKind,
) {
    delegator.emit_event(session_id, kind);
}

fn emit_hook_notices(
    delegator: &Arc<crate::agent::LocalAgentHandle>,
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

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::roster::{Member, MemberOrigin, test_roster_on};
    use crate::hooks::Hooks;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::test_utils::MockSessionStore;
    use crate::test_utils::delegation::{DelegateBehavior, agent_info, build_delegate_handle};
    use querymt_agent_core::events::EventEnvelope;
    use querymt_agent_services::event_fanout::EventFanout;
    use querymt_agent_services::event_sink::EventSink;

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
            objective_hash: querymt_agent_core::hash::RapidHash::default(),
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
                cycle_id: None,
                handoff_from: None,
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

    /// A root handle and an empty roster over the same storage.
    async fn root_and_roster() -> (
        Arc<crate::agent::LocalAgentHandle>,
        Arc<AgentRoster>,
        crate::test_utils::TestAgent,
        tempfile::TempDir,
    ) {
        let agent = crate::test_utils::TestAgent::with_mock_provider(
            crate::test_utils::SharedLlmProvider::new(
                crate::test_utils::MockLlmProvider::new(),
                vec![],
            ),
        )
        .await;
        let (plugins, dir) = crate::test_utils::helpers::empty_plugin_registry().unwrap();
        let roster = test_roster_on(Arc::new(plugins), agent.storage.clone());
        (agent.handle.clone(), roster, agent, dir)
    }

    /// An orchestrator for `root` whose store and event sink are replaced.
    fn orchestrator_on(
        root: &Arc<crate::agent::LocalAgentHandle>,
        roster: Arc<AgentRoster>,
        store: Arc<dyn SessionStore>,
        event_sink: Arc<EventSink>,
    ) -> DelegationOrchestrator {
        orchestrator_with(root, roster, &Default::default(), store, event_sink)
    }

    /// [`orchestrator_on`] with explicit delegation settings.
    fn orchestrator_with(
        root: &Arc<crate::agent::LocalAgentHandle>,
        roster: Arc<AgentRoster>,
        settings: &crate::config::DelegationSettings,
        store: Arc<dyn SessionStore>,
        event_sink: Arc<EventSink>,
    ) -> DelegationOrchestrator {
        let mut orchestrator = DelegationOrchestrator::new(root, roster, settings, None);
        orchestrator.store = store;
        orchestrator.event_sink = event_sink;
        orchestrator.tool_registry = Arc::new(ToolRegistry::new());
        orchestrator.hooks = Hooks::default();
        orchestrator
    }

    async fn built_member(
        id: &str,
        store: Arc<dyn SessionStore>,
    ) -> (Member, Arc<crate::agent::LocalAgentHandle>) {
        let handle = build_delegate_handle(DelegateBehavior::AlwaysOk, store, None).await;
        (
            Member::built(
                agent_info(id),
                MemberOrigin::Config,
                MemberAgent::Local(handle.clone()),
            ),
            handle,
        )
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
                .set(DelegationChild {
                    target: MemberAgent::Local(fixture.delegate.clone()),
                    session_id: child.public_id.clone(),
                    root: Arc::downgrade(&fixture.delegate),
                })
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
                objective_hash: querymt_agent_core::hash::RapidHash::new(
                    b"test shutdown registration",
                ),
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
        let (root, roster, _agent, _dir) = root_and_roster().await;
        let (member, _handle) = built_member("coder", storage.session_store().clone()).await;
        roster.add(member).unwrap();
        let settings = crate::config::DelegationSettings {
            max_parallel: NonZeroUsize::new(1).unwrap(),
            ..Default::default()
        };
        let orchestrator = Arc::new(orchestrator_with(
            &root,
            roster,
            &settings,
            storage.session_store().clone(),
            event_sink,
        ));
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
                origin: querymt_agent_core::events::EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                kind: AgentEventKind::DelegationRequested {
                    delegation: delegation.clone(),
                    tool_call_id: None,
                    batch_id: None,
                    cycle_id: None,
                    handoff_from: None,
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
    async fn protocol_target_that_cannot_be_built_fails_the_delegation() {
        let (root, roster, agent, _dir) = root_and_roster().await;
        let storage = agent.storage.clone();
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
                target_agent_id: "writer".to_string(),
                objective: "write".to_string(),
                objective_hash: querymt_agent_core::hash::RapidHash::new(b"write"),
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
        // A protocol member whose build always fails.
        roster
            .add(Member::unbuildable(
                agent_info("writer"),
                MemberOrigin::Protocol,
                &"model preset `fast` rejected",
            ))
            .unwrap();
        let mut events = root.config.subscribe_events();
        let orchestrator = orchestrator_on(
            &root,
            roster.clone(),
            storage.session_store(),
            root.config.event_sink.clone(),
        );
        let envelope = EventEnvelope::from(querymt_agent_core::events::AgentEvent {
            seq: 0,
            session_seq: 0,
            timestamp: 0,
            session_id: session.public_id.clone(),
            origin: querymt_agent_core::events::EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::DelegationRequested {
                delegation: delegation.clone(),
                tool_call_id: None,
                batch_id: None,
                cycle_id: None,
                handoff_from: None,
            },
        });
        orchestrator.handle_envelope(&envelope).await;

        let error = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let AgentEventKind::DelegationFailed { error, .. } =
                    events.recv().await.unwrap().kind()
                {
                    break error.clone();
                }
            }
        })
        .await
        .expect("the delegation fails instead of staying requested");
        assert!(error.contains("writer"), "{error}");
        assert!(error.contains("model preset `fast` rejected"), "{error}");
        assert_eq!(
            storage
                .get_delegation(&delegation.public_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            DelegationStatus::Failed
        );

        // The failed build left the member unbuilt, so the next use tries again.
        let snapshot = roster.snapshot();
        let member = snapshot.get("writer").unwrap();
        assert!(!member.is_built());
        assert!(roster.resolve(member).is_err());
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
        let (root, roster, _agent, _dir) = root_and_roster().await;
        let orchestrator = orchestrator_on(&root, roster, store, event_sink);
        let delegation = Delegation {
            isolation: Default::default(),
            id: 1,
            public_id: "delegation-1".to_string(),
            session_id: 1,
            task_id: None,
            target_agent_id: "coder".to_string(),
            objective: "test".to_string(),
            objective_hash: querymt_agent_core::hash::RapidHash::new(b"test"),
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

        let (root, roster, _agent, _dir) = root_and_roster().await;
        let (member, member_handle) = built_member("coder", storage.session_store()).await;
        roster.add(member).unwrap();
        let mut orchestrator = orchestrator_on(&root, roster, store, event_sink);
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
            objective_hash: querymt_agent_core::hash::RapidHash::new(b"test"),
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

        let envelope = EventEnvelope::from(querymt_agent_core::events::AgentEvent {
            seq: 0,
            session_seq: 0,
            timestamp: 0,
            session_id: "session-1".to_string(),
            origin: querymt_agent_core::events::EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::DelegationRequested {
                delegation,
                tool_call_id: None,
                batch_id: None,
                cycle_id: None,
                handoff_from: None,
            },
        });
        orchestrator.handle_envelope(&envelope).await;

        // The delegation is durably failed.
        tokio::time::timeout(std::time::Duration::from_secs(5), failed_rx)
            .await
            .expect("delegation must fail closed in time")
            .expect("failure signal sender dropped early");

        assert!(
            member_handle.registry.lock().await.is_empty(),
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

    // ── claim_delegation retry ───────────────────────────────────────────────

    /// Builds an orchestrator over a store whose `claim_delegation` pops
    /// scripted results and counts attempts.
    async fn claim_orchestrator(
        script: Vec<SessionResult<DelegationClaim>>,
    ) -> (DelegationOrchestrator, Arc<std::sync::atomic::AtomicUsize>) {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let script = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            script,
        )));
        let mut store = MockSessionStore::new();
        let counter = attempts.clone();
        store.expect_claim_delegation().returning(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            script
                .lock()
                .unwrap()
                .pop_front()
                .expect("claim attempted more often than scripted")
        });
        let store: Arc<dyn SessionStore> = Arc::new(store);
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("storage"),
        );
        let event_sink = Arc::new(EventSink::new(
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        ));
        let (root, roster, _agent, _dir) = root_and_roster().await;
        let orchestrator = orchestrator_on(&root, roster, store, event_sink);
        (orchestrator, attempts)
    }

    fn busy_error() -> crate::session::error::SessionError {
        querymt_agent_sqlite::error::session_db_error(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".to_string()),
        ))
    }

    #[tokio::test]
    async fn busy_database_claim_is_retried() {
        let (orchestrator, attempts) =
            claim_orchestrator(vec![Err(busy_error()), Ok(DelegationClaim::Claimed)]).await;
        let claim = orchestrator.claim_delegation("delegation-1").await;
        assert!(matches!(claim, Ok(DelegationClaim::Claimed)), "{claim:?}");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn busy_database_claim_gives_up_after_three_attempts() {
        let (orchestrator, attempts) = claim_orchestrator(vec![
            Err(busy_error()),
            Err(busy_error()),
            Err(busy_error()),
        ])
        .await;
        let claim = orchestrator.claim_delegation("delegation-1").await;
        assert!(claim.unwrap_err().is_transient());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn non_transient_claim_failure_is_not_retried() {
        let undecodable = rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown delegation status",
            )),
        );
        let (orchestrator, attempts) = claim_orchestrator(vec![Err(
            querymt_agent_sqlite::error::session_db_error(undecodable),
        )])
        .await;
        let err = orchestrator
            .claim_delegation("delegation-1")
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::session::error::SessionError::Database(_)),
            "{err:?}"
        );
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_delegation_is_a_claim_outcome_not_a_failure() {
        let (orchestrator, attempts) =
            claim_orchestrator(vec![Ok(DelegationClaim::NotFound)]).await;
        let claim = orchestrator.claim_delegation("delegation-1").await;
        assert!(matches!(claim, Ok(DelegationClaim::NotFound)), "{claim:?}");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // ── AgentInfo serialization ──────────────────────────────────────────────

    #[test]
    fn test_agent_info_serde_round_trip() {
        let info = AgentInfo {
            isolation: Default::default(),
            id: crate::delegation::roster::AgentId::try_from("coder").unwrap(),
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
            id: crate::delegation::roster::AgentId::try_from("minimal").unwrap(),
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
        let config = DelegationOrchestratorConfig::new(None, &Default::default());
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
        let config = DelegationOrchestratorConfig::new(Some(path.clone()), &Default::default());
        assert_eq!(config.cwd, Some(path));
    }
}

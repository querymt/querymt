//! Post-construction protocol activation.
//!
//! The builders resolve a manifest and apply passive configuration (prompts,
//! model overlays, MCP plans, skills, delegation targets), but protocol tasks
//! and memories have durable side effects that must not happen inside
//! `build()`: task records need a session and a scheduler, workspace tasks need
//! a host trust decision, and profiles may not be attached yet.
//!
//! [`DotagentsRuntimeCoordinator`] owns those side effects. It is:
//!
//! - **shared**: single-agent, quorum, and profile runtimes all activate through
//!   the same path, so behavior cannot drift between them;
//! - **repeatable**: activation can run again later (for example once a host has
//!   approved a pending workspace task) without duplicating records;
//! - **fail-soft**: a missing approver, knowledge store, or scheduler produces a
//!   diagnostic and leaves unrelated protocol features active, unless the caller
//!   chose strict mode.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::knowledge::KnowledgeStore;
use crate::session::repo_schedule::ScheduleRepository;
use crate::session::store::SessionStore;

use super::automation::{DotagentsAutomationIdentity, DotagentsAutomationRepository};
use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::manifest::DotagentsManifest;
use super::memory::{DotagentsMemoryPlan, DotagentsMemoryReconcileReport, reconcile_memories};
use super::options::DotagentsLoadOptions;
use super::reconcile::{DotagentsTaskApplied, DotagentsTaskReconciler};
use super::tasks::{
    DotagentsAutomationBinding, DotagentsStartupSkipReason, DotagentsTaskApproval,
    DotagentsTaskApprovalDecision, DotagentsTaskApprovalRequest, DotagentsTaskIdentity,
    DotagentsTaskStateRepository, DotagentsTaskTrustOutcome, DotagentsTaskTrustPolicy,
    decide_task_reconciliation, evaluate_task_trust, plan_startup_fire, plan_task_activation,
    plan_task_retirement,
};

/// A binding used only to plan activation; the real binding is supplied once a
/// task is known to be trusted, so an untrusted task never provisions a session.
static NO_BINDING: std::sync::LazyLock<DotagentsAutomationBinding> =
    std::sync::LazyLock::new(|| DotagentsAutomationBinding::new(String::new(), None::<String>));

/// A set of protocol tasks that share one automation session.
struct TaskGroup {
    target: DotagentsExecutionTarget,
    layer: super::layer::DotagentsLayer,
    workspace: Option<std::path::PathBuf>,
    identities: Vec<DotagentsTaskIdentity>,
}

/// The outcome of preparing one task, before any durable write.
enum PreparedTask {
    /// The task is allowed to reconcile and has everything it needs.
    Reconcile {
        activation: Box<super::tasks::DotagentsTaskActivation>,
        existing: Box<Option<super::tasks::DotagentsTaskOwnership>>,
    },
    /// The task must not cause durable records to be created or updated.
    Stop,
}

/// The canonical workspace a protocol layer's records belong to.
///
/// Workspace-layer records are bound to the workspace so approval and ownership
/// stay stable across machines; the global layer has no workspace identity.
fn canonical_workspace_for(
    layer: super::layer::DotagentsLayer,
    workspace: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    match layer {
        super::layer::DotagentsLayer::Workspace => workspace.map(canonicalize_workspace),
        super::layer::DotagentsLayer::Global => None,
    }
}

/// Best-effort canonicalization so equivalent paths share one workspace identity.
fn canonicalize_workspace(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Where reconciled protocol tasks execute.
///
/// A target pairs the plan decisions with a concrete runtime and the profile
/// that runtime represents, so a task naming `profileId` can be routed to the
/// runtime that actually owns that profile.
#[derive(Clone)]
pub struct DotagentsExecutionTarget {
    /// The profile this target executes under, when profile-scoped.
    pub profile_id: Option<String>,
    /// Whether this target can activate schedules.
    pub scheduler_available: bool,
}

impl DotagentsExecutionTarget {
    /// Create an execution target.
    pub fn new(profile_id: Option<String>, scheduler_available: bool) -> Self {
        Self {
            profile_id,
            scheduler_available,
        }
    }
}

/// Resolves the runtime targets protocol tasks may execute on.
///
/// Implementations are provided by the runtime that owns protocol activation.
/// Returning `None` for a profile means that profile is not available to this
/// runtime, which is reported rather than silently substituted.
#[async_trait::async_trait]
pub trait DotagentsExecutionTargetResolver: Send + Sync {
    /// The default target for tasks that do not name a profile.
    async fn default_target(&self) -> Option<DotagentsExecutionTarget>;

    /// The target for a profile-scoped task.
    async fn target_for_profile(&self, profile_id: &str) -> Option<DotagentsExecutionTarget>;

    /// Whether the named profile can be resolved at all.
    async fn profile_available(&self, profile_id: &str) -> bool {
        self.target_for_profile(profile_id).await.is_some()
    }
}

/// Everything protocol activation needs from the runtime.
pub struct DotagentsActivationContext {
    /// The resolved manifest driving activation.
    pub manifest: Arc<DotagentsManifest>,
    /// Load options supplying strictness, workspace, and trust policy.
    pub options: DotagentsLoadOptions,
    /// Session storage, used for task records.
    pub sessions: Arc<dyn SessionStore>,
    /// Schedule storage, used for interval schedules.
    pub schedules: Arc<dyn ScheduleRepository>,
    /// Protocol ownership and approval persistence.
    pub state: Arc<dyn DotagentsTaskStateRepository>,
    /// Protocol automation session provisioning.
    pub automation: Arc<dyn DotagentsAutomationRepository>,
    /// Knowledge storage for memories, when configured.
    pub knowledge: Option<Arc<dyn KnowledgeStore>>,
    /// The knowledge scope memories are imported into.
    pub knowledge_scope: String,
    /// Optional host approval mechanism.
    pub approver: Option<Arc<dyn super::tasks::DotagentsTaskApprover>>,
    /// Triggers an owned schedule once, standing in for a startup fire.
    pub trigger: Option<Arc<dyn DotagentsStartupTrigger>>,
}

/// Fires a reconciled protocol schedule once for `runOnStartup`.
#[async_trait::async_trait]
pub trait DotagentsStartupTrigger: Send + Sync {
    /// Trigger the schedule identified by `schedule_public_id`.
    async fn trigger_schedule(&self, schedule_public_id: &str) -> Result<(), String>;
}

/// The result of one activation pass.
#[derive(Debug, Clone, Default)]
pub struct DotagentsActivationReport {
    /// Every non-fatal diagnostic from this pass, deterministically ordered.
    pub diagnostics: Vec<DotagentsDiagnostic>,
    /// Memory reconciliation results, when memory support ran.
    pub memory: Option<DotagentsMemoryReconcileReport>,
    /// Applied task outcomes, paired with their protocol source key.
    pub tasks: Vec<DotagentsActivatedTask>,
    /// Workspace tasks awaiting a host trust decision.
    pub pending_approvals: Vec<DotagentsTaskApprovalRequest>,
    /// Whether memories were reconciled during this pass.
    pub memories_reconciled: bool,
    /// Whether a fatal condition stopped activation under strict mode.
    pub failed: bool,
}

impl DotagentsActivationReport {
    /// Whether any problem (fatal or not) was reported.
    pub fn has_diagnostics(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    /// Whether activation stopped early because strict mode demanded a facility.
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// The number of protocol tasks that reconciled to executable records.
    pub fn armed_tasks(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.applied.is_armed())
            .count()
    }

    /// Restore deterministic ordering after a host appends passive diagnostics.
    pub(crate) fn sort_diagnostics(&mut self) {
        self.diagnostics.sort_by(diagnostic_order);
    }
}

/// One protocol task's applied outcome.
#[derive(Debug, Clone)]
pub struct DotagentsActivatedTask {
    /// The protocol source key that owns the records.
    pub source_key: String,
    /// The task ID from the protocol document.
    pub task_id: String,
    /// What was applied.
    pub applied: DotagentsTaskApplied,
    /// Whether a startup fire was performed for this task.
    pub fired_on_startup: bool,
}

/// Runs post-construction protocol activation against a runtime.
pub struct DotagentsRuntimeCoordinator {
    context: DotagentsActivationContext,
}

impl DotagentsRuntimeCoordinator {
    /// Create a coordinator from a runtime activation context.
    pub fn new(context: DotagentsActivationContext) -> Self {
        Self { context }
    }

    /// Whether protocol support is enabled for this runtime.
    pub fn is_enabled(&self) -> bool {
        self.context.options.is_enabled()
    }

    /// Reconcile memories and tasks.
    ///
    /// Memories reconcile first so a task firing at startup can already read the
    /// workspace's declared memory. Tasks then reconcile per execution target,
    /// and only successfully reconciled tasks can fire on startup.
    pub async fn activate(
        &self,
        resolver: &dyn DotagentsExecutionTargetResolver,
    ) -> DotagentsActivationReport {
        let mut report = DotagentsActivationReport::default();

        if !self.context.options.is_enabled() {
            return report;
        }

        // Memories are not trust-gated: they are read-only knowledge, and the
        // spec requires them to import while a missing store stays non-fatal.
        let memory_report = reconcile_memories(
            DotagentsMemoryPlan::from_manifest(&self.context.manifest),
            self.context.knowledge.as_ref(),
            &self.context.knowledge_scope,
        )
        .await;
        report.memories_reconciled = memory_report.store_available;
        report.diagnostics.extend(memory_report.diagnostics.clone());
        report.memory = Some(memory_report);

        let task_report = self.reconcile_tasks(resolver).await;
        report.diagnostics.extend(task_report.diagnostics);
        report.tasks = task_report.tasks;
        report.pending_approvals = task_report.pending_approvals;
        report.failed = task_report.failed;

        report.diagnostics.sort_by(diagnostic_order);
        report
    }

    /// Reconcile protocol tasks across every execution target.
    async fn reconcile_tasks(
        &self,
        resolver: &dyn DotagentsExecutionTargetResolver,
    ) -> DotagentsActivationReport {
        let mut report = DotagentsActivationReport::default();

        let strictness = self.context.options.strictness();
        let policy = self.context.options.workspace_task_trust();

        // Group by protocol scope and target profile. Global and workspace tasks
        // intentionally receive different automation sessions even when they run
        // on the same profile.
        let mut groups: Vec<TaskGroup> = Vec::new();
        let mut live_source_keys = BTreeSet::new();
        let mut disabled_source_keys = BTreeSet::new();

        for task in self.context.manifest.tasks.values() {
            let workspace =
                canonical_workspace_for(task.source.layer, self.context.options.workspace());
            let identity =
                DotagentsTaskIdentity::new(task.source.layer, workspace.clone(), &task.id);
            let source_key = identity.source_key();
            if !task.enabled {
                disabled_source_keys.insert(source_key);
                continue;
            }
            live_source_keys.insert(source_key);

            let target_profile = task.profile_id.as_deref();
            let target = match target_profile {
                Some(profile_id) => resolver.target_for_profile(profile_id).await,
                None => resolver.default_target().await,
            };
            let Some(target) = target else {
                let detail = match target_profile {
                    Some(profile_id) => {
                        format!("profile `{profile_id}` is not available in this runtime")
                    }
                    None => "no default execution target is available".to_string(),
                };
                let disposition = super::tasks::classify_activation_unavailable(
                    strictness,
                    super::tasks::DotagentsActivationFacility::TargetProfile,
                    detail,
                );
                report.diagnostics.push(disposition.diagnostic().clone());
                if disposition.is_fatal() {
                    report.failed = true;
                    return report;
                }
                continue;
            };

            match groups.iter_mut().find(|group| {
                group.target.profile_id == target.profile_id
                    && group.layer == task.source.layer
                    && group.workspace == workspace
            }) {
                Some(group) => group.identities.push(identity),
                None => groups.push(TaskGroup {
                    target,
                    layer: task.source.layer,
                    workspace,
                    identities: vec![identity],
                }),
            }
        }

        for group in groups {
            let TaskGroup {
                target,
                layer,
                workspace,
                identities,
            } = group;

            if !target.scheduler_available {
                let disposition = super::tasks::classify_activation_unavailable(
                    strictness,
                    super::tasks::DotagentsActivationFacility::Scheduler,
                    match &target.profile_id {
                        Some(profile_id) => {
                            format!("no scheduler is available for profile `{profile_id}`")
                        }
                        None => "no scheduler is available for the default runtime".to_string(),
                    },
                );
                report.diagnostics.push(disposition.diagnostic().clone());
                if disposition.is_fatal() {
                    report.failed = true;
                    return report;
                }
                continue;
            }

            let automation_identity =
                DotagentsAutomationIdentity::new(layer, workspace, target.profile_id.clone());

            // Trust is resolved before any durable record exists. A workspace
            // task that is pending or denied must not provision an automation
            // session: the session is created lazily by the first task in this
            // group that is actually allowed to reconcile.
            let mut binding: Option<DotagentsAutomationBinding> = None;
            let mut session_failed = false;

            for identity in identities {
                let Some(task) = self.task_for_identity(&identity) else {
                    continue;
                };

                let prepared = self
                    .prepare_group_task(task, &identity, policy, strictness, &mut report)
                    .await;
                let PreparedTask::Reconcile {
                    activation,
                    existing,
                } = prepared
                else {
                    continue;
                };

                // A durable record is now certain to be written, so the target's
                // automation session is provisioned on first use.
                if binding.is_none() {
                    match self
                        .context
                        .automation
                        .ensure_automation_session(&automation_identity)
                        .await
                    {
                        Ok(created) => {
                            if let Some(profile_id) = &target.profile_id
                                && let Err(error) =
                                    super::automation::bind_automation_session_profile(
                                        self.context.sessions.as_ref(),
                                        &created.session_public_id,
                                        profile_id,
                                    )
                                    .await
                            {
                                report.diagnostics.push(DotagentsDiagnostic::warning(
                                    DotagentsDiagnosticCode::Other,
                                    format!(
                                        "could not bind protocol automation session to profile `{profile_id}`: {error}"
                                    ),
                                ));
                                session_failed = true;
                                break;
                            }
                            binding = Some(created);
                        }
                        Err(error) => {
                            let disposition = super::tasks::classify_activation_unavailable(
                                strictness,
                                super::tasks::DotagentsActivationFacility::ScheduleStorage,
                                format!("could not provision the automation session: {error}"),
                            );
                            report.diagnostics.push(disposition.diagnostic().clone());
                            if disposition.is_fatal() {
                                report.failed = true;
                                return report;
                            }
                            session_failed = true;
                            break;
                        }
                    }
                }

                let Some(binding) = binding.as_ref() else {
                    continue;
                };

                let reconciler = DotagentsTaskReconciler::new(
                    self.context.sessions.clone(),
                    self.context.schedules.clone(),
                    self.context.state.clone(),
                );
                self.apply_reconciliation(
                    &reconciler,
                    binding,
                    task,
                    &identity,
                    activation,
                    existing,
                    &mut report,
                )
                .await;
                if report.failed {
                    return report;
                }
            }

            if session_failed {
                continue;
            }
        }

        // Records whose sources vanished are retired. Disabled definitions are
        // paused so their ownership remains available for re-enablement.
        match self.context.state.list_ownerships().await {
            Ok(stored) => {
                let outcomes = plan_task_retirement(
                    &stored,
                    &live_source_keys,
                    &disabled_source_keys,
                    &BTreeSet::new(),
                );
                let reconciler = DotagentsTaskReconciler::new(
                    self.context.sessions.clone(),
                    self.context.schedules.clone(),
                    self.context.state.clone(),
                );
                for outcome in outcomes {
                    report.diagnostics.push(outcome.diagnostic.clone());
                    if let Err(error) = reconciler.retire(&outcome).await {
                        report.diagnostics.push(DotagentsDiagnostic::warning(
                            DotagentsDiagnosticCode::Other,
                            format!(
                                "could not retire protocol task `{}`: {error}",
                                outcome.ownership.source_key
                            ),
                        ));
                    }
                }
            }
            Err(error) => report.diagnostics.push(DotagentsDiagnostic::warning(
                DotagentsDiagnosticCode::Other,
                format!("could not list protocol task ownership: {error}"),
            )),
        }

        report
    }

    /// Reconcile a single protocol task against its target.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    /// Resolve everything needed to reconcile one task, without writing anything.
    ///
    /// Trust is evaluated here, before any durable record exists. Returning
    /// [`PreparedTask::Stop`] means the task is reported (pending, denied, or
    /// awaiting renewal) and must not cause an automation session or schedule to
    /// be created on its behalf.
    async fn prepare_group_task(
        &self,
        task: &super::manifest::DotagentsTask,
        identity: &DotagentsTaskIdentity,
        policy: DotagentsTaskTrustPolicy,
        strictness: super::options::DotagentsStrictness,
        report: &mut DotagentsActivationReport,
    ) -> PreparedTask {
        let activation = {
            // The resolver already proved this target exists before a task in the
            // group reaches here, so any referenced profile resolves.
            let profile_available = |_profile_id: &str| true;
            match plan_task_activation(task, identity, &NO_BINDING, &profile_available) {
                Ok(activation) => activation,
                Err(diagnostic) => {
                    report.diagnostics.push(*diagnostic);
                    return PreparedTask::Stop;
                }
            }
        };

        let existing = match self
            .context
            .state
            .get_ownership(&identity.source_key())
            .await
        {
            Ok(existing) => existing,
            Err(error) => {
                report.diagnostics.push(DotagentsDiagnostic::warning(
                    DotagentsDiagnosticCode::Other,
                    format!("could not read protocol task ownership: {error}"),
                ));
                return PreparedTask::Stop;
            }
        };

        let Some(approved) = self
            .trust_decision(task, identity, policy, strictness, report)
            .await
        else {
            // Pending or denied: no durable record may be created or updated.
            return PreparedTask::Stop;
        };

        let outcome = decide_task_reconciliation(&activation, existing.as_ref(), approved);
        if let Some(diagnostic) = &outcome.diagnostic {
            report.diagnostics.push(diagnostic.clone());
        }

        if outcome.action == super::tasks::DotagentsTaskReconcileAction::AwaitingApproval {
            // A changed task must stop running immediately, but pausing only
            // touches records the protocol already owns, so no new session is
            // provisioned for it.
            if let Some(ownership) = outcome.ownership.clone()
                && ownership.schedule_public_id.is_some()
            {
                let reconciler = DotagentsTaskReconciler::new(
                    self.context.sessions.clone(),
                    self.context.schedules.clone(),
                    self.context.state.clone(),
                );
                let retirement = super::tasks::DotagentsTaskRetireOutcome {
                    ownership,
                    action: super::tasks::DotagentsTaskRetireAction::Pause,
                    reason: super::tasks::DotagentsTaskRetireReason::ChangedUnapproved,
                    diagnostic: DotagentsDiagnostic::info(
                        DotagentsDiagnosticCode::Other,
                        format!(
                            "protocol task `{}` changed and requires renewed approval; its schedule is paused",
                            identity.task_id
                        ),
                    ),
                };
                if let Err(error) = reconciler.retire(&retirement).await {
                    report.diagnostics.push(DotagentsDiagnostic::warning(
                        DotagentsDiagnosticCode::Other,
                        format!(
                            "could not pause protocol task `{}`: {error}",
                            identity.task_id
                        ),
                    ));
                } else {
                    report.diagnostics.push(retirement.diagnostic.clone());
                }
            }
            return PreparedTask::Stop;
        }

        PreparedTask::Reconcile {
            activation: Box::new(activation),
            existing: Box::new(existing),
        }
    }

    /// Apply a prepared reconciliation against a provisioned session.
    #[allow(clippy::too_many_arguments)]
    async fn apply_reconciliation(
        &self,
        reconciler: &DotagentsTaskReconciler,
        binding: &DotagentsAutomationBinding,
        task: &super::manifest::DotagentsTask,
        identity: &DotagentsTaskIdentity,
        activation: Box<super::tasks::DotagentsTaskActivation>,
        existing: Box<Option<super::tasks::DotagentsTaskOwnership>>,
        report: &mut DotagentsActivationReport,
    ) {
        let activation = *activation;
        let existing = *existing;
        let approved = true;
        let outcome = decide_task_reconciliation(&activation, existing.as_ref(), approved);

        let applied = match reconciler.apply(&outcome, Some(&activation), binding).await {
            Ok(applied) => applied,
            Err(error) => {
                report.diagnostics.push(DotagentsDiagnostic::warning(
                    DotagentsDiagnosticCode::Other,
                    format!(
                        "could not reconcile protocol task `{}`: {error}",
                        identity.task_id
                    ),
                ));
                return;
            }
        };

        let mut fired_on_startup = false;
        if task.run_on_startup {
            // Reconciliation assigns the durable record IDs. Feed those IDs back
            // into the pure startup planner instead of the pre-write ownership
            // snapshot, which is intentionally empty for a Create action.
            let mut startup_outcome = outcome.clone();
            let mut ownership = startup_outcome.ownership.take().unwrap_or_else(|| {
                super::tasks::DotagentsTaskOwnership::pending(
                    identity,
                    activation.fingerprint.clone(),
                )
            });
            ownership.task_public_id = applied.task_public_id().map(ToOwned::to_owned);
            ownership.schedule_public_id = applied.schedule_public_id().map(ToOwned::to_owned);
            startup_outcome.ownership = Some(ownership);
            let plan = plan_startup_fire(&activation, &startup_outcome, true);
            match plan.fire {
                Some(fire) => match &self.context.trigger {
                    Some(trigger) => match trigger.trigger_schedule(&fire.schedule_public_id).await {
                        Ok(()) => fired_on_startup = true,
                        Err(error) => report.diagnostics.push(DotagentsDiagnostic::warning(
                            DotagentsDiagnosticCode::Other,
                            format!(
                                "protocol task `{}` requested runOnStartup but could not be triggered: {error}",
                                identity.task_id
                            ),
                        )),
                    },
                    None => report.diagnostics.push(DotagentsDiagnostic::warning(
                        DotagentsDiagnosticCode::Other,
                        format!(
                            "protocol task `{}` requests runOnStartup but no startup trigger is configured",
                            identity.task_id
                        ),
                    )),
                },
                None => {
                    if let Some(skipped) = plan.skipped {
                        report.diagnostics.push(DotagentsDiagnostic::info(
                            DotagentsDiagnosticCode::Other,
                            format!(
                                "protocol task `{}` did not run on startup: {}",
                                identity.task_id,
                                describe_skip_reason(skipped)
                            ),
                        ));
                    }
                }
            }
        }

        report.tasks.push(DotagentsActivatedTask {
            source_key: identity.source_key(),
            task_id: identity.task_id.clone(),
            applied,
            fired_on_startup,
        });
    }

    /// Resolve whether a task is trusted enough to persist and execute.
    ///
    /// Returns `None` when the task must stay pending or was denied (in which
    /// case the caller stops). Global tasks follow the same policy switch but
    /// have no workspace to bind an approval to.
    async fn trust_decision(
        &self,
        task: &super::manifest::DotagentsTask,
        identity: &DotagentsTaskIdentity,
        policy: DotagentsTaskTrustPolicy,
        strictness: super::options::DotagentsStrictness,
        report: &mut DotagentsActivationReport,
    ) -> Option<bool> {
        if !matches!(task.source.layer, super::layer::DotagentsLayer::Workspace) {
            // The global layer is user-controlled; the policy still governs it,
            // but there is no repository content to approve.
            return match policy {
                DotagentsTaskTrustPolicy::Deny => {
                    report.diagnostics.push(DotagentsDiagnostic::info(
                        DotagentsDiagnosticCode::Other,
                        format!(
                            "global protocol task `{}` is not activated because the trust policy is `deny`",
                            identity.task_id
                        ),
                    ));
                    None
                }
                DotagentsTaskTrustPolicy::Allow => Some(true),
                DotagentsTaskTrustPolicy::Prompt => {
                    // Prompting for a user-controlled global layer would be noise;
                    // treat it as pre-approved.
                    Some(true)
                }
            };
        }

        let Some(workspace) = identity.canonical_workspace.clone() else {
            report.diagnostics.push(DotagentsDiagnostic::warning(
                DotagentsDiagnosticCode::Other,
                format!(
                    "workspace protocol task `{}` has no canonical workspace and cannot be approved",
                    identity.task_id
                ),
            ));
            return None;
        };

        let fingerprint = super::tasks::task_execution_fingerprint(task);

        // A previously recorded decision for this exact fingerprint wins.
        match self
            .context
            .state
            .get_approval(&workspace, &identity.task_id, &fingerprint)
            .await
        {
            Ok(Some(approval)) => {
                return match approval.decision {
                    DotagentsTaskApprovalDecision::Approve => Some(true),
                    DotagentsTaskApprovalDecision::Deny => {
                        report.diagnostics.push(DotagentsDiagnostic::info(
                            DotagentsDiagnosticCode::Other,
                            format!(
                                "workspace protocol task `{}` was previously denied and stays inactive",
                                identity.task_id
                            ),
                        ));
                        None
                    }
                };
            }
            Ok(None) => {}
            Err(error) => report.diagnostics.push(DotagentsDiagnostic::warning(
                DotagentsDiagnosticCode::Other,
                format!("could not read protocol task approval: {error}"),
            )),
        }

        let request = DotagentsTaskApprovalRequest::from_task(task, &workspace);

        if matches!(policy, DotagentsTaskTrustPolicy::Prompt) && self.context.approver.is_none() {
            // Headless hosts keep workspace tasks pending and explain how to fix it.
            report.pending_approvals.push(request.clone());
            let disposition = super::tasks::classify_activation_unavailable(
                strictness,
                super::tasks::DotagentsActivationFacility::Trust,
                format!(
                    "workspace task `{}` requires approval and no approval mechanism is configured",
                    identity.task_id
                ),
            );
            report.diagnostics.push(disposition.diagnostic().clone());
            if disposition.is_fatal() {
                report.failed = true;
            }
            return None;
        }

        let outcome =
            evaluate_task_trust(policy, request.clone(), self.context.approver.as_deref()).await;
        match outcome {
            DotagentsTaskTrustOutcome::Approved { .. } => {
                if matches!(policy, DotagentsTaskTrustPolicy::Allow) {
                    report.diagnostics.push(DotagentsDiagnostic::warning(
                        DotagentsDiagnosticCode::Other,
                        format!(
                            "workspace protocol task `{}` was activated with the unsafe `allow` trust policy; the trust check was bypassed",
                            identity.task_id
                        ),
                    ));
                }
                // Persist the decision so a restart does not re-prompt.
                let approval = DotagentsTaskApproval {
                    canonical_workspace: workspace,
                    task_id: identity.task_id.clone(),
                    fingerprint,
                    decision: DotagentsTaskApprovalDecision::Approve,
                    decided_at: time::OffsetDateTime::now_utc(),
                };
                if let Err(error) = self.context.state.record_approval(approval).await {
                    report.diagnostics.push(DotagentsDiagnostic::warning(
                        DotagentsDiagnosticCode::Other,
                        format!("could not record protocol task approval: {error}"),
                    ));
                }
                Some(true)
            }
            DotagentsTaskTrustOutcome::Denied => {
                let approval = DotagentsTaskApproval {
                    canonical_workspace: workspace,
                    task_id: identity.task_id.clone(),
                    fingerprint,
                    decision: DotagentsTaskApprovalDecision::Deny,
                    decided_at: time::OffsetDateTime::now_utc(),
                };
                if let Err(error) = self.context.state.record_approval(approval).await {
                    report.diagnostics.push(DotagentsDiagnostic::warning(
                        DotagentsDiagnosticCode::Other,
                        format!("could not record protocol task denial: {error}"),
                    ));
                }
                None
            }
            DotagentsTaskTrustOutcome::Pending { .. } => {
                report.pending_approvals.push(request);
                None
            }
        }
    }

    /// Find the manifest task for a protocol identity.
    fn task_for_identity(
        &self,
        identity: &DotagentsTaskIdentity,
    ) -> Option<&super::manifest::DotagentsTask> {
        self.context.manifest.tasks.get(&identity.task_id)
    }
}

/// A human-readable description of why a startup fire was skipped.
fn describe_skip_reason(reason: DotagentsStartupSkipReason) -> &'static str {
    match reason {
        DotagentsStartupSkipReason::NotRequested => "runOnStartup is not requested",
        DotagentsStartupSkipReason::NotReconciled => "the task is not active and trusted yet",
        DotagentsStartupSkipReason::Inactive(_) => "the task is not active this startup",
        DotagentsStartupSkipReason::MissingSchedule => "no owned schedule exists yet",
    }
}

/// Deterministic diagnostic ordering shared with memory reconciliation.
fn diagnostic_order(a: &DotagentsDiagnostic, b: &DotagentsDiagnostic) -> std::cmp::Ordering {
    let key = |d: &DotagentsDiagnostic| {
        (
            d.code.as_str(),
            d.source
                .as_ref()
                .map(|s| (s.layer, s.lexical_path.clone(), s.entry_id.clone())),
            d.message.clone(),
        )
    };
    key(a).cmp(&key(b))
}

/// Ensure a target's automation session exists, returning its binding.
pub async fn ensure_target_session(
    automation: &dyn DotagentsAutomationRepository,
    options: &DotagentsLoadOptions,
    layer: super::layer::DotagentsLayer,
    profile_id: Option<&str>,
) -> Option<DotagentsAutomationBinding> {
    let identity = DotagentsAutomationIdentity::new(
        layer,
        canonical_workspace_for(layer, options.workspace()),
        profile_id,
    );
    automation.ensure_automation_session(&identity).await.ok()
}

//! Application of neutral protocol task plans to durable QueryMT storage.
//!
//! [`super::tasks`] decides *what* should happen to a protocol task;
//! [`DotagentsTaskReconciler`] performs it. It is the only place in protocol
//! support that writes task and schedule records, and it never touches a record
//! that is not referenced by a matching [`DotagentsTaskOwnership`] row — so
//! user-created tasks and schedules are structurally out of reach.
//!
//! Reconciliation is idempotent:
//!
//! - **Create** writes one recurring task keyed by the protocol creation key and
//!   one interval schedule, then records both public IDs in the ownership row.
//!   The `(session_id, creation_key)` uniqueness guarantee makes a replayed
//!   create reuse the original task instead of inserting a duplicate.
//! - **Update** rewrites the owned task and schedule in place.
//! - **Unchanged** performs no writes at all.
//! - **AwaitingApproval** pauses the owned schedule before returning, so a
//!   changed task can never keep running on the strength of a stale approval.
//! - **Retire** deletes the owned records; **Pause** leaves them for inspection
//!   while stopping execution.

use std::sync::Arc;

use crate::session::domain::{Task, TaskKind, TaskStatus};
use crate::session::domain_schedule::{Schedule, ScheduleState, ScheduleTrigger};
use crate::session::error::{SessionError, SessionResult};
use crate::session::repo_schedule::ScheduleRepository;
use crate::session::store::SessionStore;

use super::tasks::{
    DotagentsAutomationBinding, DotagentsTaskActivation, DotagentsTaskOwnership,
    DotagentsTaskReconcileAction, DotagentsTaskReconcileOutcome, DotagentsTaskRetireAction,
    DotagentsTaskRetireOutcome, DotagentsTaskStateRepository,
};

/// The applied result of one reconciliation action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotagentsTaskApplied {
    /// A new task and schedule were created.
    Created {
        /// The created task's public ID.
        task_public_id: String,
        /// The created schedule's public ID.
        schedule_public_id: String,
    },
    /// The owned task and schedule were updated in place.
    Updated {
        /// The updated task's public ID.
        task_public_id: String,
        /// The updated schedule's public ID.
        schedule_public_id: String,
    },
    /// Nothing needed to change.
    Unchanged {
        /// The existing task's public ID.
        task_public_id: String,
        /// The existing schedule's public ID.
        schedule_public_id: String,
    },
    /// The owned schedule was paused because renewed approval is required.
    PausedAwaitingApproval,
    /// The owned schedule was paused and its records retained.
    Paused,
    /// The owned records were deleted.
    Retired,
    /// Ownership existed but nothing owned remained to act on.
    Absent,
}

impl DotagentsTaskApplied {
    /// The task public ID this outcome refers to, when one exists.
    pub fn task_public_id(&self) -> Option<&str> {
        match self {
            Self::Created { task_public_id, .. }
            | Self::Updated { task_public_id, .. }
            | Self::Unchanged { task_public_id, .. } => Some(task_public_id),
            Self::PausedAwaitingApproval | Self::Paused | Self::Retired | Self::Absent => None,
        }
    }

    /// The schedule public ID this outcome refers to, when one exists.
    pub fn schedule_public_id(&self) -> Option<&str> {
        match self {
            Self::Created {
                schedule_public_id, ..
            }
            | Self::Updated {
                schedule_public_id, ..
            }
            | Self::Unchanged {
                schedule_public_id, ..
            } => Some(schedule_public_id),
            Self::PausedAwaitingApproval | Self::Paused | Self::Retired | Self::Absent => None,
        }
    }

    /// Whether the outcome left an executable, armed schedule behind.
    pub fn is_armed(&self) -> bool {
        matches!(
            self,
            Self::Created { .. } | Self::Updated { .. } | Self::Unchanged { .. }
        )
    }
}

/// Applies protocol task plans to durable QueryMT storage.
pub struct DotagentsTaskReconciler {
    sessions: Arc<dyn SessionStore>,
    schedules: Arc<dyn ScheduleRepository>,
    state: Arc<dyn DotagentsTaskStateRepository>,
}

impl DotagentsTaskReconciler {
    /// Create a reconciler over the runtime's storage.
    pub fn new(
        sessions: Arc<dyn SessionStore>,
        schedules: Arc<dyn ScheduleRepository>,
        state: Arc<dyn DotagentsTaskStateRepository>,
    ) -> Self {
        Self {
            sessions,
            schedules,
            state,
        }
    }

    /// Apply a reconciliation decision for one protocol task.
    ///
    /// `activation` is required for every action except `AwaitingApproval`, which
    /// only needs the existing ownership to pause what it owns.
    pub async fn apply(
        &self,
        outcome: &DotagentsTaskReconcileOutcome,
        activation: Option<&DotagentsTaskActivation>,
        binding: &DotagentsAutomationBinding,
    ) -> SessionResult<DotagentsTaskApplied> {
        match outcome.action {
            DotagentsTaskReconcileAction::Create => {
                let activation = require_activation(activation)?;
                self.create(activation, binding).await
            }
            DotagentsTaskReconcileAction::Update => {
                let activation = require_activation(activation)?;
                let existing = self.existing_ownership(outcome).await?;
                self.update(activation, existing.as_ref(), binding).await
            }
            DotagentsTaskReconcileAction::Unchanged => {
                let existing = self.existing_ownership(outcome).await?;
                match existing {
                    Some(ownership) => match (
                        ownership.task_public_id.clone(),
                        ownership.schedule_public_id.clone(),
                    ) {
                        (Some(task_public_id), Some(schedule_public_id)) => {
                            // A record can be deleted outside the protocol. Repairing
                            // it is still an idempotent no-op when it exists.
                            match self.owned_records_survive(&ownership).await? {
                                true => Ok(DotagentsTaskApplied::Unchanged {
                                    task_public_id,
                                    schedule_public_id,
                                }),
                                false => {
                                    let activation = require_activation(activation)?;
                                    self.update(activation, Some(&ownership), binding).await
                                }
                            }
                        }
                        _ => {
                            let activation = require_activation(activation)?;
                            self.update(activation, Some(&ownership), binding).await
                        }
                    },
                    None => {
                        let activation = require_activation(activation)?;
                        self.create(activation, binding).await
                    }
                }
            }
            DotagentsTaskReconcileAction::AwaitingApproval => {
                let existing = self.existing_ownership(outcome).await?;
                match existing {
                    Some(ownership) => {
                        self.pause_owned_schedule(&ownership).await?;
                        Ok(DotagentsTaskApplied::PausedAwaitingApproval)
                    }
                    None => Ok(DotagentsTaskApplied::Absent),
                }
            }
        }
    }

    /// Apply a retirement decision, touching only protocol-owned records.
    pub async fn retire(
        &self,
        outcome: &DotagentsTaskRetireOutcome,
    ) -> SessionResult<DotagentsTaskApplied> {
        let ownership = &outcome.ownership;
        let has_records =
            ownership.task_public_id.is_some() || ownership.schedule_public_id.is_some();
        if !has_records {
            return Ok(DotagentsTaskApplied::Absent);
        }

        match outcome.action {
            DotagentsTaskRetireAction::Pause => {
                self.pause_owned_schedule(ownership).await?;
                Ok(DotagentsTaskApplied::Paused)
            }
            DotagentsTaskRetireAction::Retire => {
                // Delete the schedule first: it references the task.
                if let Some(schedule_public_id) = &ownership.schedule_public_id {
                    self.schedules.delete_schedule(schedule_public_id).await?;
                }
                if let Some(task_public_id) = &ownership.task_public_id {
                    self.sessions.delete_task(task_public_id).await?;
                }
                Ok(DotagentsTaskApplied::Retired)
            }
        }
    }

    /// Look up the persisted ownership for an outcome, if any.
    async fn existing_ownership(
        &self,
        outcome: &DotagentsTaskReconcileOutcome,
    ) -> SessionResult<Option<DotagentsTaskOwnership>> {
        match &outcome.ownership {
            // The planner already attached the long-lived record IDs.
            Some(ownership) if ownership.task_public_id.is_some() => Ok(Some(ownership.clone())),
            _ => {
                self.state
                    .get_ownership(&outcome.identity.source_key())
                    .await
            }
        }
    }

    /// Whether both owned records still exist.
    async fn owned_records_survive(
        &self,
        ownership: &DotagentsTaskOwnership,
    ) -> SessionResult<bool> {
        if let Some(task_public_id) = &ownership.task_public_id
            && self.sessions.get_task(task_public_id).await?.is_none()
        {
            return Ok(false);
        }
        if let Some(schedule_public_id) = &ownership.schedule_public_id
            && self
                .schedules
                .get_schedule(schedule_public_id)
                .await?
                .is_none()
        {
            return Ok(false);
        }
        Ok(true)
    }

    /// Create the owned task and schedule, reusing existing records when a
    /// previous attempt already committed them.
    async fn create(
        &self,
        activation: &DotagentsTaskActivation,
        binding: &DotagentsAutomationBinding,
    ) -> SessionResult<DotagentsTaskApplied> {
        let task = self.ensure_task(activation, binding).await?;

        let schedule_public_id = match self
            .schedules
            .get_schedule_for_task(&task.public_id)
            .await?
        {
            Some(existing) => existing.public_id,
            None => {
                let schedule = self.build_schedule(activation, &task, binding).await?;
                let schedule_public_id = schedule.public_id.clone();
                self.schedules.create_schedule(schedule).await?;
                schedule_public_id
            }
        };

        self.record_ownership(activation, binding, &task.public_id, &schedule_public_id)
            .await?;

        Ok(DotagentsTaskApplied::Created {
            task_public_id: task.public_id,
            schedule_public_id,
        })
    }

    /// Update the owned records in place, repairing any that were removed
    /// outside the protocol.
    async fn update(
        &self,
        activation: &DotagentsTaskActivation,
        existing: Option<&DotagentsTaskOwnership>,
        binding: &DotagentsAutomationBinding,
    ) -> SessionResult<DotagentsTaskApplied> {
        let task = self.ensure_task(activation, binding).await?;

        let schedule_public_id = match existing.and_then(|o| o.schedule_public_id.clone()) {
            Some(schedule_public_id) => {
                match self.schedules.get_schedule(&schedule_public_id).await? {
                    Some(schedule) => {
                        let updated = self
                            .updated_schedule(activation, &task, binding, schedule)
                            .await?;
                        self.schedules.update_schedule(updated).await?;
                        schedule_public_id
                    }
                    None => {
                        let schedule = self.build_schedule(activation, &task, binding).await?;
                        let schedule_public_id = schedule.public_id.clone();
                        self.schedules.create_schedule(schedule).await?;
                        schedule_public_id
                    }
                }
            }
            None => match self
                .schedules
                .get_schedule_for_task(&task.public_id)
                .await?
            {
                Some(existing) => existing.public_id,
                None => {
                    let schedule = self.build_schedule(activation, &task, binding).await?;
                    let schedule_public_id = schedule.public_id.clone();
                    self.schedules.create_schedule(schedule).await?;
                    schedule_public_id
                }
            },
        };

        self.record_ownership(activation, binding, &task.public_id, &schedule_public_id)
            .await?;

        Ok(DotagentsTaskApplied::Updated {
            task_public_id: task.public_id,
            schedule_public_id,
        })
    }

    /// Return the owned recurring task, creating or updating it as needed.
    async fn ensure_task(
        &self,
        activation: &DotagentsTaskActivation,
        binding: &DotagentsAutomationBinding,
    ) -> SessionResult<Task> {
        let session = self
            .sessions
            .get_session(&binding.session_public_id)
            .await?;
        let session = session.ok_or_else(|| {
            SessionError::InvalidOperation(format!(
                "protocol automation session `{}` no longer exists",
                binding.session_public_id
            ))
        })?;

        if let Some(existing) = self
            .find_task_by_creation_key(&binding.session_public_id, &activation.task.creation_key)
            .await?
        {
            let updated = Task {
                expected_deliverable: Some(activation.task.prompt.clone()),
                updated_at: time::OffsetDateTime::now_utc(),
                ..existing
            };
            self.sessions.update_task(updated.clone()).await?;
            return Ok(updated);
        }

        let now = time::OffsetDateTime::now_utc();
        let task = Task {
            id: 0,
            public_id: uuid::Uuid::now_v7().to_string(),
            session_id: session.id,
            kind: TaskKind::Recurring,
            status: TaskStatus::Active,
            expected_deliverable: Some(activation.task.prompt.clone()),
            acceptance_criteria: None,
            revision: 1,
            creation_key: Some(activation.task.creation_key.clone()),
            completion_evidence: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
        };

        match self.sessions.create_task(task.clone()).await {
            Ok(created) => Ok(created),
            // A concurrent reconciliation won the race; adopt its record rather
            // than surfacing a uniqueness violation.
            Err(_) => match self
                .find_task_by_creation_key(
                    &binding.session_public_id,
                    &activation.task.creation_key,
                )
                .await?
            {
                Some(existing) => Ok(existing),
                None => Err(SessionError::InvalidOperation(format!(
                    "protocol task `{}` could not be created or found in session `{}`",
                    activation.task.creation_key, binding.session_public_id
                ))),
            },
        }
    }

    /// Find an existing protocol task by its creation key within a session.
    async fn find_task_by_creation_key(
        &self,
        session_public_id: &str,
        creation_key: &str,
    ) -> SessionResult<Option<Task>> {
        let tasks = self.sessions.list_tasks(session_public_id).await?;
        Ok(tasks
            .into_iter()
            .find(|task| task.creation_key.as_deref() == Some(creation_key)))
    }

    /// Build the interval schedule for a protocol task.
    async fn build_schedule(
        &self,
        activation: &DotagentsTaskActivation,
        task: &Task,
        binding: &DotagentsAutomationBinding,
    ) -> SessionResult<Schedule> {
        let mut schedule = Schedule::new(
            task.public_id.clone(),
            binding.session_public_id.clone(),
            ScheduleTrigger::Interval {
                seconds: activation.schedule.interval_seconds,
            },
        );
        schedule.task_id = task.id;
        schedule.session_id = task.session_id;
        Ok(schedule)
    }

    /// Rebuild an owned schedule from a new activation while keeping its identity.
    async fn updated_schedule(
        &self,
        activation: &DotagentsTaskActivation,
        task: &Task,
        binding: &DotagentsAutomationBinding,
        existing: Schedule,
    ) -> SessionResult<Schedule> {
        let rebuilt = self.build_schedule(activation, task, binding).await?;
        let now = time::OffsetDateTime::now_utc();
        Ok(Schedule {
            // Preserve the durable identity and observed run history.
            id: existing.id,
            public_id: existing.public_id,
            task_public_id: task.public_id.clone(),
            session_public_id: existing.session_public_id,
            task_id: task.id,
            session_id: task.session_id,
            trigger: rebuilt.trigger,
            // A protocol task that is trusted and enabled is armed; an update is
            // not a pause, so any prior Paused state is cleared.
            state: ScheduleState::Armed,
            next_run_at: rebuilt.next_run_at,
            last_run_at: existing.last_run_at,
            run_count: existing.run_count,
            consecutive_failures: existing.consecutive_failures,
            config: rebuilt.config,
            created_at: existing.created_at,
            updated_at: now,
        })
    }

    /// Persist the ownership row that ties the protocol source to its records.
    async fn record_ownership(
        &self,
        activation: &DotagentsTaskActivation,
        binding: &DotagentsAutomationBinding,
        task_public_id: &str,
        schedule_public_id: &str,
    ) -> SessionResult<()> {
        let mut ownership =
            DotagentsTaskOwnership::pending(&activation.identity, &activation.fingerprint);
        ownership.task_public_id = Some(task_public_id.to_string());
        ownership.schedule_public_id = Some(schedule_public_id.to_string());
        // Preserve the resolved profile for inspection; the automation session
        // itself carries the profile binding.
        let _ = binding;
        self.state.upsert_ownership(ownership).await
    }

    /// Pause the owned schedule without disturbing its task.
    async fn pause_owned_schedule(&self, ownership: &DotagentsTaskOwnership) -> SessionResult<()> {
        let Some(schedule_public_id) = &ownership.schedule_public_id else {
            return Ok(());
        };
        if self
            .schedules
            .get_schedule(schedule_public_id)
            .await?
            .is_none()
        {
            return Ok(());
        }
        // Pausing is best-effort under a concurrent state change: a schedule that
        // is already Paused or Exhausted is equally not-running.
        let _ = self
            .schedules
            .update_schedule_state(
                schedule_public_id,
                ScheduleState::Armed,
                ScheduleState::Paused,
            )
            .await?;
        let _ = self
            .schedules
            .update_schedule_state(
                schedule_public_id,
                ScheduleState::Running,
                ScheduleState::Paused,
            )
            .await?;
        Ok(())
    }
}

/// Extract the activation an action requires, or report a wiring error.
fn require_activation(
    activation: Option<&DotagentsTaskActivation>,
) -> SessionResult<&DotagentsTaskActivation> {
    activation.ok_or_else(|| {
        SessionError::InvalidOperation(
            "protocol task reconciliation requires an activation for this action".to_string(),
        )
    })
}

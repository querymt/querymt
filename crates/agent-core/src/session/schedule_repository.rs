//! The `ScheduleRepository` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use crate::session::domain_schedule::{Schedule, ScheduleState};
use crate::session::error::SessionResult;
use async_trait::async_trait;
use time::OffsetDateTime;

/// Data access layer for `Schedule` entities.
///
/// All public-facing parameters use public IDs (`schedule_public_id`,
/// `session_public_id`, `task_public_id`). Internal row IDs are resolved
/// at the repository boundary only.
#[async_trait]
pub trait ScheduleRepository: Send + Sync {
    /// Persist a new schedule. The `id` field is ignored; a new row ID is assigned.
    async fn create_schedule(&self, schedule: Schedule) -> SessionResult<Schedule>;

    /// Retrieve a schedule by its public ID.
    async fn get_schedule(&self, schedule_public_id: &str) -> SessionResult<Option<Schedule>>;

    /// List all schedules belonging to a session (by session public ID).
    async fn list_schedules(&self, session_public_id: &str) -> SessionResult<Vec<Schedule>>;

    /// List all schedules in `Armed` state (for scheduler startup recovery).
    async fn list_all_armed_schedules(&self) -> SessionResult<Vec<Schedule>>;

    /// Full update of a schedule row (caller must set all fields).
    async fn update_schedule(&self, schedule: Schedule) -> SessionResult<()>;

    /// Compare-and-swap state transition.
    ///
    /// `UPDATE schedules SET state = ? WHERE public_id = ? AND state = ?`
    ///
    /// Returns `true` if the transition was applied (exactly one row matched).
    async fn update_schedule_state(
        &self,
        schedule_public_id: &str,
        from_state: ScheduleState,
        to_state: ScheduleState,
    ) -> SessionResult<bool>;

    /// Hard-delete a schedule by public ID.
    async fn delete_schedule(&self, schedule_public_id: &str) -> SessionResult<()>;

    /// Find the schedule associated with a task (by task public ID).
    async fn get_schedule_for_task(&self, task_public_id: &str) -> SessionResult<Option<Schedule>>;

    // ── Reconciliation / operations helpers ──────────────────────────────

    /// List schedules stuck in `Running` state older than `cutoff`.
    async fn list_running_schedules_older_than(
        &self,
        cutoff: OffsetDateTime,
    ) -> SessionResult<Vec<Schedule>>;

    /// Attempt to acquire the single-row scheduler lease.
    ///
    /// Inserts or updates the lease row only if it is absent or expired.
    /// Returns `true` if the caller now owns the lease.
    async fn try_acquire_scheduler_lease(
        &self,
        owner_id: &str,
        ttl_seconds: u64,
    ) -> SessionResult<bool>;

    /// Renew an existing lease owned by `owner_id`.
    ///
    /// Returns `true` if the lease was renewed (the row still belongs to
    /// `owner_id` and hasn't expired).
    async fn renew_scheduler_lease(&self, owner_id: &str, ttl_seconds: u64) -> SessionResult<bool>;

    /// Release the scheduler lease owned by `owner_id`.
    ///
    /// Deletes the lease row only if it belongs to `owner_id`, preventing
    /// a node from releasing a lease it no longer owns.
    /// Returns `true` if the lease was released.
    async fn release_scheduler_lease(&self, owner_id: &str) -> SessionResult<bool>;
}

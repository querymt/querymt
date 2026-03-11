//! ScheduleRepository trait and SQLite implementation.
//!
//! Follows the same pattern as `repo_task.rs`: async trait with a
//! `run_blocking` helper that acquires the shared `Arc<Mutex<Connection>>`.

use crate::session::domain_schedule::{Schedule, ScheduleConfig, ScheduleState, ScheduleTrigger};
use crate::session::error::{SessionError, SessionResult};
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

// ─── Repository trait ────────────────────────────────────────────────────────

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
}

// ─── SQLite implementation ───────────────────────────────────────────────────

/// SQLite-backed `ScheduleRepository`.
#[derive(Clone)]
pub struct SqliteScheduleRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteScheduleRepository {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    async fn run_blocking<F, R>(&self, f: F) -> SessionResult<R>
    where
        F: FnOnce(&mut Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        let conn_arc = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn_arc.lock().unwrap();
            f(&mut conn)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Schedule task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }
}

/// Helper: format an `OffsetDateTime` as RFC 3339 for SQLite TEXT storage.
fn format_dt(dt: &OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Helper: parse an RFC 3339 string back into `OffsetDateTime`.
fn parse_dt(s: &str) -> Result<OffsetDateTime, rusqlite::Error> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map_err(|_| rusqlite::Error::InvalidQuery)
}

/// Helper: parse an optional RFC 3339 string.
fn parse_dt_opt(s: &Option<String>) -> Result<Option<OffsetDateTime>, rusqlite::Error> {
    match s {
        Some(v) => Ok(Some(parse_dt(v)?)),
        None => Ok(None),
    }
}

/// Read a `Schedule` from a rusqlite `Row`.
fn row_to_schedule(row: &rusqlite::Row<'_>) -> Result<Schedule, rusqlite::Error> {
    let trigger_json: String = row.get("trigger_json")?;
    let config_json: String = row.get("config_json")?;
    let state_str: String = row.get("state")?;
    let created_at_str: String = row.get("created_at")?;
    let updated_at_str: String = row.get("updated_at")?;
    let last_run_at_str: Option<String> = row.get("last_run_at")?;
    let next_run_at_str: Option<String> = row.get("next_run_at")?;

    let trigger: ScheduleTrigger =
        serde_json::from_str(&trigger_json).map_err(|_| rusqlite::Error::InvalidQuery)?;
    let config: ScheduleConfig =
        serde_json::from_str(&config_json).map_err(|_| rusqlite::Error::InvalidQuery)?;

    Ok(Schedule {
        id: row.get("id")?,
        public_id: row.get("public_id")?,
        task_id: row.get("task_id")?,
        task_public_id: row.get("task_public_id")?,
        session_id: row.get("session_id")?,
        session_public_id: row.get("session_public_id")?,
        trigger,
        state: ScheduleState::from_str(&state_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
        last_run_at: parse_dt_opt(&last_run_at_str)?,
        next_run_at: parse_dt_opt(&next_run_at_str)?,
        run_count: row.get("run_count")?,
        consecutive_failures: row.get("consecutive_failures")?,
        config,
        created_at: parse_dt(&created_at_str)?,
        updated_at: parse_dt(&updated_at_str)?,
    })
}

const SELECT_COLS: &str = "id, public_id, task_id, task_public_id, session_id, \
    session_public_id, trigger_json, state, last_run_at, next_run_at, \
    run_count, consecutive_failures, config_json, created_at, updated_at";

#[async_trait]
impl ScheduleRepository for SqliteScheduleRepository {
    async fn create_schedule(&self, mut schedule: Schedule) -> SessionResult<Schedule> {
        // Generate public_id if not provided
        if schedule.public_id.is_empty() {
            schedule.public_id = uuid::Uuid::now_v7().to_string();
        }

        let trigger_json = serde_json::to_string(&schedule.trigger)
            .map_err(|e| SessionError::SerializationError(e.to_string()))?;
        let config_json = serde_json::to_string(&schedule.config)
            .map_err(|e| SessionError::SerializationError(e.to_string()))?;

        let schedule = self
            .run_blocking(move |conn| {
                conn.execute(
                    "INSERT INTO schedules (
                        public_id, task_id, task_public_id, session_id, session_public_id,
                        trigger_json, state, last_run_at, next_run_at,
                        run_count, consecutive_failures, config_json,
                        created_at, updated_at
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        schedule.public_id,
                        schedule.task_id,
                        schedule.task_public_id,
                        schedule.session_id,
                        schedule.session_public_id,
                        trigger_json,
                        schedule.state.to_string(),
                        schedule.last_run_at.as_ref().map(format_dt),
                        schedule.next_run_at.as_ref().map(format_dt),
                        schedule.run_count,
                        schedule.consecutive_failures,
                        config_json,
                        format_dt(&schedule.created_at),
                        format_dt(&schedule.updated_at),
                    ],
                )?;
                let id = conn.last_insert_rowid();
                let mut s = schedule;
                s.id = id;
                Ok(s)
            })
            .await?;

        Ok(schedule)
    }

    async fn get_schedule(&self, schedule_public_id: &str) -> SessionResult<Option<Schedule>> {
        let pid = schedule_public_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                &format!("SELECT {SELECT_COLS} FROM schedules WHERE public_id = ?"),
                params![pid],
                row_to_schedule,
            )
            .optional()
        })
        .await
    }

    async fn list_schedules(&self, session_public_id: &str) -> SessionResult<Vec<Schedule>> {
        let spid = session_public_id.to_string();
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SELECT_COLS} FROM schedules WHERE session_public_id = ? ORDER BY created_at ASC"
            ))?;
            let rows = stmt.query_map(params![spid], row_to_schedule)?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn list_all_armed_schedules(&self) -> SessionResult<Vec<Schedule>> {
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SELECT_COLS} FROM schedules WHERE state = 'armed' ORDER BY next_run_at ASC"
            ))?;
            let rows = stmt.query_map([], row_to_schedule)?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn update_schedule(&self, schedule: Schedule) -> SessionResult<()> {
        let trigger_json = serde_json::to_string(&schedule.trigger)
            .map_err(|e| SessionError::SerializationError(e.to_string()))?;
        let config_json = serde_json::to_string(&schedule.config)
            .map_err(|e| SessionError::SerializationError(e.to_string()))?;
        let pid = schedule.public_id.clone();

        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE schedules SET
                    trigger_json = ?, state = ?, last_run_at = ?, next_run_at = ?,
                    run_count = ?, consecutive_failures = ?, config_json = ?,
                    updated_at = ?
                WHERE public_id = ?",
                params![
                    trigger_json,
                    schedule.state.to_string(),
                    schedule.last_run_at.as_ref().map(format_dt),
                    schedule.next_run_at.as_ref().map(format_dt),
                    schedule.run_count,
                    schedule.consecutive_failures,
                    config_json,
                    format_dt(&schedule.updated_at),
                    schedule.public_id,
                ],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            SessionError::DatabaseError(_) => {
                SessionError::Other(format!("Schedule not found: {}", pid))
            }
            _ => e,
        })
    }

    async fn update_schedule_state(
        &self,
        schedule_public_id: &str,
        from_state: ScheduleState,
        to_state: ScheduleState,
    ) -> SessionResult<bool> {
        let pid = schedule_public_id.to_string();
        let now = format_dt(&OffsetDateTime::now_utc());
        let from = from_state.to_string();
        let to = to_state.to_string();

        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE schedules SET state = ?, updated_at = ? WHERE public_id = ? AND state = ?",
                params![to, now, pid, from],
            )?;
            Ok(affected == 1)
        })
        .await
    }

    async fn delete_schedule(&self, schedule_public_id: &str) -> SessionResult<()> {
        let pid = schedule_public_id.to_string();
        let affected = self
            .run_blocking(move |conn| {
                conn.execute("DELETE FROM schedules WHERE public_id = ?", params![pid])
            })
            .await?;

        if affected == 0 {
            return Err(SessionError::Other(format!(
                "Schedule not found: {}",
                schedule_public_id
            )));
        }
        Ok(())
    }

    async fn get_schedule_for_task(&self, task_public_id: &str) -> SessionResult<Option<Schedule>> {
        let tpid = task_public_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                &format!("SELECT {SELECT_COLS} FROM schedules WHERE task_public_id = ?"),
                params![tpid],
                row_to_schedule,
            )
            .optional()
        })
        .await
    }

    async fn list_running_schedules_older_than(
        &self,
        cutoff: OffsetDateTime,
    ) -> SessionResult<Vec<Schedule>> {
        let cutoff_str = format_dt(&cutoff);
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SELECT_COLS} FROM schedules \
                 WHERE state = 'running' AND last_run_at < ? \
                 ORDER BY last_run_at ASC"
            ))?;
            let rows = stmt.query_map(params![cutoff_str], row_to_schedule)?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn try_acquire_scheduler_lease(
        &self,
        owner_id: &str,
        ttl_seconds: u64,
    ) -> SessionResult<bool> {
        let oid = owner_id.to_string();
        let now = OffsetDateTime::now_utc();
        let now_str = format_dt(&now);
        let expires_str = format_dt(&(now + time::Duration::seconds(ttl_seconds as i64)));

        self.run_blocking(move |conn| {
            // Try to insert if not exists, or update if expired
            let affected = conn.execute(
                "INSERT INTO scheduler_lease (id, owner_id, acquired_at, expires_at)
                 VALUES (1, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                     owner_id = excluded.owner_id,
                     acquired_at = excluded.acquired_at,
                     expires_at = excluded.expires_at
                 WHERE scheduler_lease.expires_at < ?",
                params![oid, now_str, expires_str, now_str],
            )?;
            Ok(affected == 1)
        })
        .await
    }

    async fn renew_scheduler_lease(&self, owner_id: &str, ttl_seconds: u64) -> SessionResult<bool> {
        let oid = owner_id.to_string();
        let now = OffsetDateTime::now_utc();
        let now_str = format_dt(&now);
        let expires_str = format_dt(&(now + time::Duration::seconds(ttl_seconds as i64)));

        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE scheduler_lease
                 SET acquired_at = ?, expires_at = ?
                 WHERE id = 1 AND owner_id = ? AND expires_at >= ?",
                params![now_str, expires_str, oid, now_str],
            )?;
            Ok(affected == 1)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::domain_schedule::*;
    use crate::session::schema;

    /// Create an in-memory SQLite DB with schema initialized.
    fn setup_db() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::init_schema(&mut conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn make_schedule(
        task_id: i64,
        session_id: i64,
        task_public_id: &str,
        session_public_id: &str,
    ) -> Schedule {
        Schedule {
            id: 0,
            public_id: String::new(), // will be generated
            task_id,
            task_public_id: task_public_id.to_string(),
            session_id,
            session_public_id: session_public_id.to_string(),
            trigger: ScheduleTrigger::Interval { seconds: 3600 },
            state: ScheduleState::Armed,
            last_run_at: None,
            next_run_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)),
            run_count: 0,
            consecutive_failures: 0,
            config: ScheduleConfig::default(),
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        }
    }

    /// Insert prerequisite session + task rows so FK constraints are satisfied.
    fn insert_prerequisites(
        conn: &Arc<Mutex<Connection>>,
        session_pub: &str,
        task_pub: &str,
    ) -> (i64, i64) {
        let c = conn.lock().unwrap();
        let now = format_dt(&OffsetDateTime::now_utc());
        c.execute(
            "INSERT INTO sessions (public_id, created_at, updated_at) VALUES (?, ?, ?)",
            params![session_pub, now, now],
        )
        .unwrap();
        let session_id = c.last_insert_rowid();

        c.execute(
            "INSERT INTO tasks (public_id, session_id, kind, status, created_at, updated_at) VALUES (?, ?, 'recurring', 'active', ?, ?)",
            params![task_pub, session_id, now, now],
        )
        .unwrap();
        let task_id = c.last_insert_rowid();
        (session_id, task_id)
    }

    #[tokio::test]
    async fn create_and_get_schedule() {
        let db = setup_db();
        let (session_id, task_id) = insert_prerequisites(&db, "sess-1", "task-1");
        let repo = SqliteScheduleRepository::new(db);

        let sched = make_schedule(task_id, session_id, "task-1", "sess-1");
        let created = repo.create_schedule(sched).await.unwrap();
        assert!(!created.public_id.is_empty());
        assert!(created.id > 0);

        let fetched = repo.get_schedule(&created.public_id).await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.public_id, created.public_id);
        assert_eq!(fetched.state, ScheduleState::Armed);
        assert_eq!(fetched.task_public_id, "task-1");
        assert_eq!(fetched.session_public_id, "sess-1");
    }

    #[tokio::test]
    async fn list_schedules_by_session() {
        let db = setup_db();
        let (s1, t1) = insert_prerequisites(&db, "sess-a", "task-a");
        let (s2, t2) = insert_prerequisites(&db, "sess-b", "task-b");
        let repo = SqliteScheduleRepository::new(db);

        repo.create_schedule(make_schedule(t1, s1, "task-a", "sess-a"))
            .await
            .unwrap();
        repo.create_schedule(make_schedule(t2, s2, "task-b", "sess-b"))
            .await
            .unwrap();

        let list_a = repo.list_schedules("sess-a").await.unwrap();
        assert_eq!(list_a.len(), 1);
        assert_eq!(list_a[0].session_public_id, "sess-a");

        let list_b = repo.list_schedules("sess-b").await.unwrap();
        assert_eq!(list_b.len(), 1);
    }

    #[tokio::test]
    async fn list_all_armed_schedules() {
        let db = setup_db();
        let (s, t) = insert_prerequisites(&db, "sess-c", "task-c");
        let repo = SqliteScheduleRepository::new(db);

        repo.create_schedule(make_schedule(t, s, "task-c", "sess-c"))
            .await
            .unwrap();

        let armed = repo.list_all_armed_schedules().await.unwrap();
        assert_eq!(armed.len(), 1);
    }

    #[tokio::test]
    async fn cas_update_schedule_state() {
        let db = setup_db();
        let (s, t) = insert_prerequisites(&db, "sess-cas", "task-cas");
        let repo = SqliteScheduleRepository::new(db);

        let created = repo
            .create_schedule(make_schedule(t, s, "task-cas", "sess-cas"))
            .await
            .unwrap();

        // Valid CAS: Armed -> Running
        let ok = repo
            .update_schedule_state(
                &created.public_id,
                ScheduleState::Armed,
                ScheduleState::Running,
            )
            .await
            .unwrap();
        assert!(ok);

        // Duplicate CAS should fail (state is now Running, not Armed)
        let dup = repo
            .update_schedule_state(
                &created.public_id,
                ScheduleState::Armed,
                ScheduleState::Running,
            )
            .await
            .unwrap();
        assert!(!dup);

        // Valid CAS: Running -> Armed
        let ok2 = repo
            .update_schedule_state(
                &created.public_id,
                ScheduleState::Running,
                ScheduleState::Armed,
            )
            .await
            .unwrap();
        assert!(ok2);
    }

    #[tokio::test]
    async fn delete_schedule() {
        let db = setup_db();
        let (s, t) = insert_prerequisites(&db, "sess-del", "task-del");
        let repo = SqliteScheduleRepository::new(db);

        let created = repo
            .create_schedule(make_schedule(t, s, "task-del", "sess-del"))
            .await
            .unwrap();

        repo.delete_schedule(&created.public_id).await.unwrap();

        let fetched = repo.get_schedule(&created.public_id).await.unwrap();
        assert!(fetched.is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_schedule_errors() {
        let db = setup_db();
        let repo = SqliteScheduleRepository::new(db);
        let result = repo.delete_schedule("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_schedule_for_task() {
        let db = setup_db();
        let (s, t) = insert_prerequisites(&db, "sess-ft", "task-ft");
        let repo = SqliteScheduleRepository::new(db);

        repo.create_schedule(make_schedule(t, s, "task-ft", "sess-ft"))
            .await
            .unwrap();

        let found = repo.get_schedule_for_task("task-ft").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().task_public_id, "task-ft");

        let not_found = repo.get_schedule_for_task("task-nope").await.unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn list_running_schedules_older_than() {
        let db = setup_db();
        let (s, t) = insert_prerequisites(&db, "sess-stale", "task-stale");
        let repo = SqliteScheduleRepository::new(db);

        let mut sched = make_schedule(t, s, "task-stale", "sess-stale");
        sched.state = ScheduleState::Armed;
        let created = repo.create_schedule(sched).await.unwrap();

        // Transition to running
        repo.update_schedule_state(
            &created.public_id,
            ScheduleState::Armed,
            ScheduleState::Running,
        )
        .await
        .unwrap();

        // Update last_run_at to a time in the past
        let past = OffsetDateTime::now_utc() - time::Duration::hours(2);
        let mut updated = repo
            .get_schedule(&created.public_id)
            .await
            .unwrap()
            .unwrap();
        updated.last_run_at = Some(past);
        updated.updated_at = OffsetDateTime::now_utc();
        repo.update_schedule(updated).await.unwrap();

        let cutoff = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let stale = repo
            .list_running_schedules_older_than(cutoff)
            .await
            .unwrap();
        assert_eq!(stale.len(), 1);
    }

    #[tokio::test]
    async fn update_schedule_full() {
        let db = setup_db();
        let (s, t) = insert_prerequisites(&db, "sess-upd", "task-upd");
        let repo = SqliteScheduleRepository::new(db);

        let created = repo
            .create_schedule(make_schedule(t, s, "task-upd", "sess-upd"))
            .await
            .unwrap();

        let mut updated = created.clone();
        updated.run_count = 5;
        updated.consecutive_failures = 2;
        updated.state = ScheduleState::Running;
        updated.updated_at = OffsetDateTime::now_utc();

        repo.update_schedule(updated).await.unwrap();

        let fetched = repo
            .get_schedule(&created.public_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.run_count, 5);
        assert_eq!(fetched.consecutive_failures, 2);
        assert_eq!(fetched.state, ScheduleState::Running);
    }

    #[tokio::test]
    async fn scheduler_lease_acquire_and_renew() {
        let db = setup_db();
        let repo = SqliteScheduleRepository::new(db);

        // First acquire should succeed
        let acquired = repo
            .try_acquire_scheduler_lease("owner-1", 60)
            .await
            .unwrap();
        assert!(acquired);

        // Second acquire by different owner should fail (lease not expired)
        let stolen = repo
            .try_acquire_scheduler_lease("owner-2", 60)
            .await
            .unwrap();
        assert!(!stolen);

        // Owner can renew
        let renewed = repo.renew_scheduler_lease("owner-1", 60).await.unwrap();
        assert!(renewed);

        // Non-owner cannot renew
        let bad_renew = repo.renew_scheduler_lease("owner-2", 60).await.unwrap();
        assert!(!bad_renew);
    }

    #[tokio::test]
    async fn trigger_serialization_roundtrip_in_db() {
        let db = setup_db();
        let (s, t) = insert_prerequisites(&db, "sess-trig", "task-trig");
        let repo = SqliteScheduleRepository::new(db);

        // Event-driven trigger
        let mut sched = make_schedule(t, s, "task-trig", "sess-trig");
        sched.trigger = ScheduleTrigger::EventDriven {
            event_filter: EventTriggerFilter {
                event_kinds: vec!["knowledge_ingested".to_string()],
                threshold: 5,
                session_public_id: Some("sess-trig".to_string()),
            },
            debounce_seconds: 30,
        };

        let created = repo.create_schedule(sched).await.unwrap();
        let fetched = repo
            .get_schedule(&created.public_id)
            .await
            .unwrap()
            .unwrap();

        if let ScheduleTrigger::EventDriven {
            event_filter,
            debounce_seconds,
        } = &fetched.trigger
        {
            assert_eq!(event_filter.threshold, 5);
            assert_eq!(*debounce_seconds, 30);
            assert_eq!(event_filter.session_public_id.as_deref(), Some("sess-trig"));
        } else {
            panic!("Expected EventDriven trigger");
        }
    }
}

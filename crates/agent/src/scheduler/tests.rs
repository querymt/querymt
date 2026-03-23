//! Integration tests for Phase 3 scheduler functionality.
//!
//! Tests lease-based ownership, event subscription, reconciliation, and management handlers.

use crate::session::domain_schedule::{
    EventTriggerFilter, Schedule, ScheduleConfig, ScheduleState, ScheduleTrigger,
};
use crate::session::repo_schedule::{ScheduleRepository, SqliteScheduleRepository};
use crate::session::schema;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// Create an in-memory SQLite DB with schema initialized.
fn setup_db() -> Arc<Mutex<Connection>> {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::init_schema(&mut conn).unwrap();
    Arc::new(Mutex::new(conn))
}

#[tokio::test]
async fn scheduler_lease_acquisition() {
    let db = setup_db();
    let repo = Arc::new(SqliteScheduleRepository::new(db.clone())) as Arc<dyn ScheduleRepository>;

    // First scheduler should acquire lease
    let acquired1 = repo
        .try_acquire_scheduler_lease("owner-1", 60)
        .await
        .unwrap();
    assert!(acquired1, "First scheduler should acquire lease");

    // Second scheduler should not acquire lease (same owner)
    let acquired2 = repo
        .try_acquire_scheduler_lease("owner-2", 60)
        .await
        .unwrap();
    assert!(
        !acquired2,
        "Second scheduler should not acquire lease while first holds it"
    );

    // Owner can renew
    let renewed = repo.renew_scheduler_lease("owner-1", 60).await.unwrap();
    assert!(renewed, "Owner should be able to renew lease");

    // Non-owner cannot renew
    let bad_renew = repo.renew_scheduler_lease("owner-2", 60).await.unwrap();
    assert!(!bad_renew, "Non-owner should not be able to renew lease");
}

#[tokio::test]
async fn schedule_state_transitions() {
    let db = setup_db();
    let repo = Arc::new(SqliteScheduleRepository::new(db.clone())) as Arc<dyn ScheduleRepository>;

    // Insert prerequisites — guard is dropped at end of block, before any await
    let (session_id, task_id) = {
        let c = db.lock().unwrap();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        c.execute(
            "INSERT INTO sessions (public_id, created_at, updated_at) VALUES (?, ?, ?)",
            rusqlite::params!["sess-1", now, now],
        )
        .unwrap();
        let session_id = c.last_insert_rowid();
        c.execute(
            "INSERT INTO tasks (public_id, session_id, kind, status, created_at, updated_at) VALUES (?, ?, 'recurring', 'active', ?, ?)",
            rusqlite::params!["task-1", session_id, now, now],
        )
        .unwrap();
        let task_id = c.last_insert_rowid();
        (session_id, task_id)
    };

    // Create a schedule
    let schedule = Schedule {
        id: 0,
        public_id: "test-sched".to_string(),
        task_id,
        task_public_id: "task-1".to_string(),
        session_id,
        session_public_id: "sess-1".to_string(),
        trigger: ScheduleTrigger::Interval { seconds: 3600 },
        state: ScheduleState::Armed,
        last_run_at: None,
        next_run_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)),
        run_count: 0,
        consecutive_failures: 0,
        config: ScheduleConfig::default(),
        created_at: OffsetDateTime::now_utc(),
        updated_at: OffsetDateTime::now_utc(),
    };

    let created = repo.create_schedule(schedule).await.unwrap();

    // Test CAS: Armed -> Running
    let ok = repo
        .update_schedule_state(
            &created.public_id,
            ScheduleState::Armed,
            ScheduleState::Running,
        )
        .await
        .unwrap();
    assert!(ok, "CAS Armed -> Running should succeed");

    // Duplicate CAS should fail
    let dup = repo
        .update_schedule_state(
            &created.public_id,
            ScheduleState::Armed,
            ScheduleState::Running,
        )
        .await
        .unwrap();
    assert!(!dup, "Duplicate CAS should fail");

    // CAS: Running -> Armed
    let ok2 = repo
        .update_schedule_state(
            &created.public_id,
            ScheduleState::Running,
            ScheduleState::Armed,
        )
        .await
        .unwrap();
    assert!(ok2, "CAS Running -> Armed should succeed");
}

#[tokio::test]
async fn event_driven_schedule_storage() {
    let db = setup_db();
    let repo = Arc::new(SqliteScheduleRepository::new(db.clone())) as Arc<dyn ScheduleRepository>;

    // Insert prerequisites — guard is dropped at end of block, before any await
    let (session_id, task_id) = {
        let c = db.lock().unwrap();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        c.execute(
            "INSERT INTO sessions (public_id, created_at, updated_at) VALUES (?, ?, ?)",
            rusqlite::params!["sess-evt", now, now],
        )
        .unwrap();
        let session_id = c.last_insert_rowid();
        c.execute(
            "INSERT INTO tasks (public_id, session_id, kind, status, created_at, updated_at) VALUES (?, ?, 'recurring', 'active', ?, ?)",
            rusqlite::params!["task-evt", session_id, now, now],
        )
        .unwrap();
        let task_id = c.last_insert_rowid();
        (session_id, task_id)
    };

    // Create an event-driven schedule
    let schedule = Schedule {
        id: 0,
        public_id: "evt-sched".to_string(),
        task_id,
        task_public_id: "task-evt".to_string(),
        session_id,
        session_public_id: "sess-evt".to_string(),
        trigger: ScheduleTrigger::EventDriven {
            event_filter: EventTriggerFilter {
                event_kinds: vec!["knowledge_ingested".to_string()],
                threshold: 3,
                session_public_id: Some("sess-evt".to_string()),
            },
            debounce_seconds: 10,
        },
        state: ScheduleState::Armed,
        last_run_at: None,
        next_run_at: None,
        run_count: 0,
        consecutive_failures: 0,
        config: ScheduleConfig::default(),
        created_at: OffsetDateTime::now_utc(),
        updated_at: OffsetDateTime::now_utc(),
    };

    let created = repo.create_schedule(schedule).await.unwrap();
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
        assert_eq!(event_filter.threshold, 3);
        assert_eq!(*debounce_seconds, 10);
        assert_eq!(
            event_filter.event_kinds,
            vec!["knowledge_ingested".to_string()]
        );
    } else {
        panic!("Expected EventDriven trigger");
    }
}

#[tokio::test]
async fn once_at_schedule_storage_and_max_runs() {
    let db = setup_db();
    let repo = Arc::new(SqliteScheduleRepository::new(db.clone())) as Arc<dyn ScheduleRepository>;

    let (session_id, task_id) = {
        let c = db.lock().unwrap();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        c.execute(
            "INSERT INTO sessions (public_id, created_at, updated_at) VALUES (?, ?, ?)",
            rusqlite::params!["sess-once", now, now],
        )
        .unwrap();
        let session_id = c.last_insert_rowid();
        c.execute(
            "INSERT INTO tasks (public_id, session_id, kind, status, created_at, updated_at) VALUES (?, ?, 'recurring', 'active', ?, ?)",
            rusqlite::params!["task-once", session_id, now, now],
        )
        .unwrap();
        let task_id = c.last_insert_rowid();
        (session_id, task_id)
    };

    let fire_at = OffsetDateTime::now_utc() + time::Duration::hours(3);
    let mut schedule = Schedule::new(
        "task-once".to_string(),
        "sess-once".to_string(),
        ScheduleTrigger::OnceAt { at: fire_at },
    );
    // Populate internal IDs required by the repository
    schedule.public_id = "once-sched".to_string();
    schedule.task_id = task_id;
    schedule.session_id = session_id;

    // Verify Schedule::new enforced max_runs = 1
    assert_eq!(schedule.config.max_runs, Some(1));
    assert_eq!(
        schedule.next_run_at.map(|t| t.unix_timestamp()),
        Some(fire_at.unix_timestamp())
    );

    let created = repo.create_schedule(schedule).await.unwrap();
    let fetched = repo
        .get_schedule(&created.public_id)
        .await
        .unwrap()
        .unwrap();

    if let ScheduleTrigger::OnceAt { at } = &fetched.trigger {
        assert_eq!(at.unix_timestamp(), fire_at.unix_timestamp());
    } else {
        panic!("Expected OnceAt trigger");
    }
    assert_eq!(fetched.config.max_runs, Some(1));
    assert_eq!(fetched.state, ScheduleState::Armed);
}

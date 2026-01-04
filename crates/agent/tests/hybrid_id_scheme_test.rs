use querymt_agent::session::domain::{ProgressEntry, ProgressKind, Task, TaskKind, TaskStatus};
use querymt_agent::session::sqlite_storage::SqliteStorage;
use querymt_agent::session::store::SessionStore;
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

async fn create_test_store() -> (TempDir, SqliteStorage) {
    let temp_dir = TempDir::new().expect("temp dir");
    let db_path = temp_dir.path().join("test.db");
    let store = SqliteStorage::connect(db_path).await.expect("store");
    (temp_dir, store)
}

#[tokio::test]
async fn test_hybrid_id_scheme() {
    let (_temp_dir, store) = create_test_store().await;

    let session = store
        .create_session(Some("test".to_string()))
        .await
        .expect("session");
    assert!(session.id > 0, "Should have INTEGER primary key");
    assert_eq!(session.public_id.len(), 36, "Should have UUID public_id");
    assert!(
        session.public_id.starts_with("019"),
        "UUID v7 starts with timestamp"
    );

    let retrieved = store
        .get_session(&session.public_id)
        .await
        .expect("session lookup")
        .expect("session exists");
    assert_eq!(retrieved.id, session.id, "Internal IDs should match");
    assert_eq!(
        retrieved.public_id, session.public_id,
        "Public IDs should match"
    );

    let task = Task {
        id: 0,
        public_id: Uuid::now_v7().to_string(),
        session_id: session.id,
        kind: TaskKind::Finite,
        status: TaskStatus::Active,
        expected_deliverable: None,
        acceptance_criteria: None,
        created_at: OffsetDateTime::now_utc(),
        updated_at: OffsetDateTime::now_utc(),
    };
    store.create_task(task.clone()).await.expect("create task");

    let tasks = store
        .list_tasks(&session.public_id)
        .await
        .expect("list tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].session_id, session.id, "FK should use internal ID");

    let progress = ProgressEntry {
        id: 0,
        session_id: session.id,
        task_id: Some(tasks[0].id),
        kind: ProgressKind::Note,
        content: "test".to_string(),
        metadata: None,
        created_at: OffsetDateTime::now_utc(),
    };
    store
        .append_progress_entry(progress)
        .await
        .expect("append progress");

    let entries = store
        .list_progress_entries(&session.public_id, None)
        .await
        .expect("list progress");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].session_id, session.id);
}

#[tokio::test]
async fn test_uuid_v7_ordering() {
    let id1 = Uuid::now_v7().to_string();
    sleep(Duration::from_millis(10)).await;
    let id2 = Uuid::now_v7().to_string();

    assert!(
        id1 < id2,
        "UUID v7 should be lexicographically ordered by time"
    );
}

#[tokio::test]
async fn test_foreign_key_constraints() {
    let (_temp_dir, store) = create_test_store().await;

    let session = store
        .create_session(Some("parent".to_string()))
        .await
        .expect("session");
    let task = Task {
        id: 0,
        public_id: Uuid::now_v7().to_string(),
        session_id: session.id,
        kind: TaskKind::Finite,
        status: TaskStatus::Active,
        expected_deliverable: None,
        acceptance_criteria: None,
        created_at: OffsetDateTime::now_utc(),
        updated_at: OffsetDateTime::now_utc(),
    };
    store.create_task(task.clone()).await.expect("task");

    store
        .delete_session(&session.public_id)
        .await
        .expect("delete session");

    let deleted_task = store.get_task(&task.public_id).await.expect("get task");
    assert!(deleted_task.is_none(), "Tasks should cascade delete");
}

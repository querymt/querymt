use std::collections::HashMap;

use querymt::LLMParams;
use rusqlite::Connection;

use crate::agent::core::AgentMode;
use crate::agent::session_control::{SessionControlState, SessionModelBinding};
use crate::events::{AgentEventKind, EventOrigin};
use crate::session::domain::ForkOrigin;
use crate::session::projection::{
    EventJournal, NewDurableEvent, RecentModelEntry, SessionScope, ViewStore,
};
use crate::session::store::SessionStore;

use super::SqliteStorage;
use super::migrations::{MIGRATIONS, apply_migrations};

#[test]
fn migration_0001_is_recorded() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let version: String = conn
        .query_row(
            "SELECT version FROM schema_migrations ORDER BY version LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("query migration version");
    assert_eq!(version, "0001_initial_reset");
}

#[test]
fn migration_0002_drops_legacy_events_table() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let events_table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='events'",
            [],
            |row| row.get(0),
        )
        .expect("check events table");
    assert_eq!(
        events_table_count, 0,
        "legacy events table should be dropped"
    );

    // event_journal table should still exist
    let journal_table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='event_journal'",
            [],
            |row| row.get(0),
        )
        .expect("check event_journal table");
    assert_eq!(journal_table_count, 1, "event_journal table should exist");
}

#[test]
fn migration_0003_adds_message_source_columns() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let source_provider_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'source_provider'",
            [],
            |row| row.get(0),
        )
        .expect("check source_provider column");
    assert_eq!(
        source_provider_count, 1,
        "source_provider column should exist"
    );

    let source_model_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'source_model'",
            [],
            |row| row.get(0),
        )
        .expect("check source_model column");
    assert_eq!(source_model_count, 1, "source_model column should exist");
}

#[test]
fn migration_0004_adds_session_kind_column() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let mut stmt = conn
        .prepare("PRAGMA table_info(sessions)")
        .expect("table info query");
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("load column names")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect columns");

    assert!(columns.into_iter().any(|name| name == "session_kind"));
}

#[test]
fn migration_0012_adds_task_and_intent_revision_columns() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = '0012_task_and_intent_revisions'",
            [],
            |row| row.get(0),
        )
        .expect("query migration");
    assert_eq!(recorded, 1);

    let task_columns: Vec<String> = conn
        .prepare("PRAGMA table_info(tasks)")
        .expect("task columns")
        .query_map([], |row| row.get(1))
        .expect("query task columns")
        .collect::<Result<_, _>>()
        .expect("collect task columns");
    for expected in [
        "revision",
        "creation_key",
        "completion_evidence",
        "completed_at",
    ] {
        assert!(task_columns.iter().any(|column| column == expected));
    }

    let intent_columns: Vec<String> = conn
        .prepare("PRAGMA table_info(intent_snapshots)")
        .expect("intent columns")
        .query_map([], |row| row.get(1))
        .expect("query intent columns")
        .collect::<Result<_, _>>()
        .expect("collect intent columns");
    for expected in ["revision", "source", "source_ref"] {
        assert!(intent_columns.iter().any(|column| column == expected));
    }
}

#[test]
fn migration_0014_adds_session_control_tables() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("apply migrations");

    for table in ["session_control_states", "session_mode_model_bindings"] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("query control table");
        assert_eq!(count, 1, "missing {table}");
    }
}

#[tokio::test]
async fn session_control_commit_is_revisioned_and_session_scoped() {
    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let first = storage
        .create_session(None, None, None, None)
        .await
        .expect("first session");
    let second = storage
        .create_session(None, None, None, None)
        .await
        .expect("second session");
    let config = storage
        .create_or_get_llm_config(&LLMParams::new().provider("mock").model("model-a"))
        .await
        .expect("LLM config");
    for session in [&first, &second] {
        storage
            .set_session_llm_config(&session.public_id, config.id)
            .await
            .expect("bind LLM config");
    }
    let binding = SessionModelBinding {
        model_id: "mock/model-a".to_string(),
        provider: "mock".to_string(),
        model: "model-a".to_string(),
        llm_config_id: config.id,
        provider_node_id: None,
    };
    let state = SessionControlState {
        revision: 1,
        active_mode: AgentMode::Build,
        reasoning_effort: None,
        effective_model: binding.clone(),
        mode_models: HashMap::from([
            ("build".to_string(), binding.clone()),
            ("plan".to_string(), binding),
        ]),
    };
    storage
        .commit_session_control(&first.public_id, 0, &state)
        .await
        .expect("commit first state");
    assert!(
        storage
            .get_session_control(&second.public_id)
            .await
            .unwrap()
            .is_none()
    );

    let stale = storage
        .commit_session_control(&first.public_id, 0, &state)
        .await
        .expect_err("stale revision must fail");
    assert!(stale.to_string().contains("revision conflict"));
}

#[tokio::test]
async fn set_session_llm_config_rejects_changes_after_control_initialization() {
    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let session = storage
        .create_session(None, None, None, None)
        .await
        .expect("session");
    let config_a = storage
        .create_or_get_llm_config(&LLMParams::new().provider("mock").model("model-a"))
        .await
        .expect("config A");
    let config_b = storage
        .create_or_get_llm_config(&LLMParams::new().provider("mock").model("model-b"))
        .await
        .expect("config B");
    storage
        .set_session_llm_config(&session.public_id, config_a.id)
        .await
        .expect("initialize config");
    let binding = SessionModelBinding {
        model_id: "mock/model-a".to_string(),
        provider: "mock".to_string(),
        model: "model-a".to_string(),
        llm_config_id: config_a.id,
        provider_node_id: None,
    };
    storage
        .commit_session_control(
            &session.public_id,
            0,
            &SessionControlState {
                revision: 1,
                active_mode: AgentMode::Build,
                reasoning_effort: None,
                effective_model: binding.clone(),
                mode_models: HashMap::from([("build".to_string(), binding)]),
            },
        )
        .await
        .expect("initialize control state");

    let error = storage
        .set_session_llm_config(&session.public_id, config_b.id)
        .await
        .expect_err("direct config changes must be rejected after control initialization");
    assert!(error.to_string().contains("session control"));
    assert_eq!(
        storage
            .get_session(&session.public_id)
            .await
            .unwrap()
            .unwrap()
            .llm_config_id,
        Some(config_a.id)
    );
}

#[tokio::test]
async fn fork_copies_session_control_bindings_independently() {
    use crate::model::{AgentMessage, MessagePart};
    use querymt::chat::ChatRole;

    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let source = storage
        .create_session(None, None, None, None)
        .await
        .expect("source session");
    let config = storage
        .create_or_get_llm_config(&LLMParams::new().provider("mock").model("source-model"))
        .await
        .expect("source config");
    storage
        .set_session_llm_config(&source.public_id, config.id)
        .await
        .expect("bind source config");
    let mut message = AgentMessage::new(source.public_id.clone(), ChatRole::User);
    message.parts.push(MessagePart::Text {
        content: "fork here".to_string(),
    });
    let message_id = message.id.clone();
    storage
        .add_message(&source.public_id, message)
        .await
        .expect("source message");

    let binding = SessionModelBinding {
        model_id: "mock/source-model".to_string(),
        provider: "mock".to_string(),
        model: "source-model".to_string(),
        llm_config_id: config.id,
        provider_node_id: None,
    };
    let source_state = SessionControlState {
        revision: 1,
        active_mode: AgentMode::Plan,
        reasoning_effort: None,
        effective_model: binding.clone(),
        mode_models: HashMap::from([("plan".to_string(), binding)]),
    };
    storage
        .commit_session_control(&source.public_id, 0, &source_state)
        .await
        .expect("source control state");

    let fork_id = storage
        .fork_session(&source.public_id, &message_id, ForkOrigin::User)
        .await
        .expect("fork session");
    let fork_state = storage
        .get_session_control(&fork_id)
        .await
        .expect("load fork control")
        .expect("fork control exists");
    assert_eq!(fork_state, source_state);

    let mut changed_fork = fork_state;
    changed_fork.revision += 1;
    changed_fork.active_mode = AgentMode::Build;
    storage
        .commit_session_control(&fork_id, 1, &changed_fork)
        .await
        .expect("change fork control");
    assert_eq!(
        storage
            .get_session_control(&source.public_id)
            .await
            .unwrap()
            .unwrap()
            .active_mode,
        AgentMode::Plan
    );
}

#[test]
fn migrations_are_idempotent() {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_migrations(&mut conn).expect("first migration run");
    let count_after_first: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("count migration rows");

    apply_migrations(&mut conn).expect("second migration run");
    let count_after_second: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("count migration rows");

    assert_eq!(count_after_first, MIGRATIONS.len() as i64);
    assert_eq!(count_after_first, count_after_second);
}

#[tokio::test]
async fn task_lifecycle_is_atomic_revisioned_and_explicit() {
    use crate::session::domain::{TaskKind, TaskStatus};
    use crate::session::store::TaskPatch;

    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let session = storage
        .create_session(None, None, None, None)
        .await
        .expect("create session");

    let created = storage
        .create_and_bind_current_task(
            &session.public_id,
            TaskKind::Finite,
            "deliver the fix".to_string(),
            Some("tests pass".to_string()),
            "call-1",
        )
        .await
        .expect("create task");
    assert_eq!(created.revision, 1);
    assert_eq!(
        storage
            .get_current_task(&session.public_id)
            .await
            .expect("read current")
            .expect("current task")
            .public_id,
        created.public_id
    );

    let duplicate = storage
        .create_and_bind_current_task(
            &session.public_id,
            TaskKind::Finite,
            "ignored duplicate".to_string(),
            None,
            "call-1",
        )
        .await
        .expect("idempotent create");
    assert_eq!(duplicate.public_id, created.public_id);

    let updated = storage
        .patch_task_for_session(
            &session.public_id,
            &created.public_id,
            1,
            TaskPatch {
                acceptance_criteria: Some("all relevant tests pass".to_string()),
                ..TaskPatch::default()
            },
            "clarify criteria",
        )
        .await
        .expect("update task");
    assert_eq!(updated.revision, 2);

    let unchanged = storage
        .patch_task_for_session(
            &session.public_id,
            &created.public_id,
            2,
            TaskPatch {
                expected_deliverable: updated.expected_deliverable.clone(),
                acceptance_criteria: updated.acceptance_criteria.clone(),
                status: Some(updated.status),
                ..TaskPatch::default()
            },
            "repeat current state",
        )
        .await
        .expect("no-op update");
    assert_eq!(unchanged.revision, 2);
    assert_eq!(unchanged.updated_at, updated.updated_at);

    assert!(
        storage
            .patch_task_for_session(
                &session.public_id,
                &created.public_id,
                1,
                TaskPatch::default(),
                "stale update",
            )
            .await
            .is_err()
    );

    let completed = storage
        .complete_task_for_session(
            &session.public_id,
            &created.public_id,
            2,
            "cargo test passed",
        )
        .await
        .expect("complete task");
    assert_eq!(completed.status, TaskStatus::Done);
    assert_eq!(
        completed.completion_evidence.as_deref(),
        Some("cargo test passed")
    );
    assert!(completed.completed_at.is_some());
    assert!(
        storage
            .get_current_task(&session.public_id)
            .await
            .expect("read cleared current")
            .is_none()
    );
}

#[tokio::test]
async fn intent_projection_insert_and_binding_are_atomic() {
    use crate::session::domain::IntentSnapshot;
    use time::OffsetDateTime;

    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    let session = storage
        .create_session(None, None, None, None)
        .await
        .expect("create session");
    let snapshot = IntentSnapshot {
        id: 0,
        session_id: session.id,
        task_id: None,
        summary: "implement the fix".to_string(),
        constraints: None,
        next_step_hint: None,
        revision: 1,
        source: "user_prompt".to_string(),
        source_ref: Some("message-1".to_string()),
        created_at: OffsetDateTime::now_utc(),
    };

    let stored = storage
        .create_and_set_current_intent_snapshot(&session.public_id, snapshot)
        .await
        .expect("persist intent projection");
    assert!(stored.id > 0);
    assert_eq!(
        storage
            .get_session(&session.public_id)
            .await
            .expect("read session")
            .expect("session")
            .current_intent_snapshot_id,
        Some(stored.id)
    );

    let missing = IntentSnapshot {
        id: 0,
        session_id: session.id,
        task_id: None,
        summary: "must roll back".to_string(),
        constraints: None,
        next_step_hint: None,
        revision: 2,
        source: "user_prompt".to_string(),
        source_ref: Some("message-2".to_string()),
        created_at: OffsetDateTime::now_utc(),
    };
    assert!(
        storage
            .create_and_set_current_intent_snapshot("missing-session", missing)
            .await
            .is_err()
    );
    assert_eq!(
        storage
            .list_intent_snapshots(&session.public_id)
            .await
            .expect("list snapshots")
            .len(),
        1
    );
}

#[tokio::test]
async fn connect_with_options_without_migration_keeps_db_unmodified() {
    let tmp = tempfile::NamedTempFile::new().expect("temp db file");
    let path = tmp.path().to_path_buf();

    let _storage = SqliteStorage::connect_with_options(path.clone(), false)
        .await
        .expect("connect without migrations");

    let conn = Connection::open(path).expect("reopen db");
    let has_migration_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |row| row.get(0),
        )
        .expect("check migration table existence");
    assert_eq!(has_migration_table, 0);
}

#[tokio::test]
async fn session_runtime_binding_round_trips_profile_and_delegate() {
    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");

    storage
        .set_session_runtime_binding(
            "delegate-session",
            "quorum",
            Some("coder"),
            Some("profile-sha"),
            Some("lock-sha"),
            Some("{}"),
        )
        .await
        .expect("persist runtime binding");

    let binding = storage
        .get_session_runtime_binding("delegate-session")
        .await
        .expect("load runtime binding")
        .expect("runtime binding");
    assert_eq!(binding.profile_id, "quorum");
    assert_eq!(binding.agent_id.as_deref(), Some("coder"));
    assert_eq!(
        storage
            .get_profile_binding("delegate-session")
            .await
            .expect("load compatibility profile binding")
            .as_deref(),
        Some("quorum")
    );

    assert_eq!(binding.profile_fingerprint.as_deref(), Some("profile-sha"));
    assert_eq!(binding.provider_lock_digest.as_deref(), Some("lock-sha"));
    assert_eq!(binding.provider_locks_json.as_deref(), Some("{}"));
}

#[tokio::test]
async fn session_runtime_binding_transaction_rolls_back_on_failure() {
    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");
    storage
        .set_session_runtime_binding(
            "session-1",
            "old-profile",
            None,
            Some("old-fingerprint"),
            Some("old-lock"),
            Some("{}"),
        )
        .await
        .expect("seed binding");
    storage
        .run_blocking(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER reject_provider_lock_update
                 BEFORE UPDATE ON profile_bindings
                 WHEN NEW.provider_locks_json = 'reject'
                 BEGIN
                    SELECT RAISE(ABORT, 'rejected provider lock');
                 END;",
            )
        })
        .await
        .expect("install failure trigger");

    storage
        .set_session_runtime_binding(
            "session-1",
            "new-profile",
            Some("coder"),
            Some("new-fingerprint"),
            Some("new-lock"),
            Some("reject"),
        )
        .await
        .expect_err("transaction must fail");

    let binding = storage
        .get_session_runtime_binding("session-1")
        .await
        .expect("load binding")
        .expect("original binding remains");
    assert_eq!(binding.profile_id, "old-profile");
    assert_eq!(binding.agent_id, None);
    assert_eq!(
        binding.profile_fingerprint.as_deref(),
        Some("old-fingerprint")
    );
    assert_eq!(binding.provider_lock_digest.as_deref(), Some("old-lock"));
    assert_eq!(binding.provider_locks_json.as_deref(), Some("{}"));
}

#[tokio::test]
async fn custom_model_crud_round_trip() {
    use crate::session::store::CustomModel;

    let storage = SqliteStorage::connect(":memory:".into())
        .await
        .expect("in-memory storage");

    let base = CustomModel {
        provider: "llama_cpp".to_string(),
        model_id: "hf:foo/bar:model.gguf".to_string(),
        display_name: "Model A".to_string(),
        config_json: serde_json::json!({"model": "hf:foo/bar:model.gguf"}),
        source_type: "hf".to_string(),
        source_ref: Some("foo/bar:model.gguf".to_string()),
        family: Some("Foo-Model".to_string()),
        quant: Some("Q8_0".to_string()),
        created_at: None,
        updated_at: None,
    };

    storage
        .upsert_custom_model(&base)
        .await
        .expect("insert custom model");

    let fetched = storage
        .get_custom_model("llama_cpp", "hf:foo/bar:model.gguf")
        .await
        .expect("get custom model")
        .expect("custom model exists");
    assert_eq!(fetched.display_name, "Model A");
    assert_eq!(fetched.source_type, "hf");

    let mut updated = fetched.clone();
    updated.display_name = "Model A Updated".to_string();
    storage
        .upsert_custom_model(&updated)
        .await
        .expect("update custom model");

    let listed = storage
        .list_custom_models("llama_cpp")
        .await
        .expect("list custom models");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].display_name, "Model A Updated");

    storage
        .delete_custom_model("llama_cpp", "hf:foo/bar:model.gguf")
        .await
        .expect("delete custom model");

    let after_delete = storage
        .get_custom_model("llama_cpp", "hf:foo/bar:model.gguf")
        .await
        .expect("get custom model after delete");
    assert!(after_delete.is_none());
}

// ══════════════════════════════════════════════════════════════════════
// EventJournal tests
// ══════════════════════════════════════════════════════════════════════

fn new_durable(session_id: &str, kind: AgentEventKind) -> NewDurableEvent {
    NewDurableEvent {
        session_id: session_id.to_string(),
        origin: EventOrigin::Local,
        source_node: None,
        kind,
    }
}

#[tokio::test]
async fn journal_append_durable_assigns_monotonic_seq() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let e1 = journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    let e2 = journal
        .append_durable(&new_durable("s1", AgentEventKind::Cancelled))
        .await
        .unwrap();

    assert!(
        e2.stream_seq > e1.stream_seq,
        "seq must be monotonically increasing"
    );
    assert_ne!(e1.event_id, e2.event_id, "event_ids must be unique");
}

#[tokio::test]
async fn journal_append_durable_returns_correct_fields() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let evt = journal
        .append_durable(&NewDurableEvent {
            session_id: "sess-x".to_string(),
            origin: EventOrigin::Remote,
            source_node: Some("node-a".to_string()),
            kind: AgentEventKind::Cancelled,
        })
        .await
        .unwrap();

    assert_eq!(evt.session_id, "sess-x");
    assert!(matches!(evt.origin, EventOrigin::Remote));
    assert_eq!(evt.source_node.as_deref(), Some("node-a"));
    assert!(matches!(evt.kind, AgentEventKind::Cancelled));
    assert!(evt.stream_seq >= 1);
    assert!(!evt.event_id.is_empty());
    assert!(evt.timestamp > 0);
}

#[tokio::test]
async fn journal_load_session_stream_returns_only_matching_session() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    journal
        .append_durable(&new_durable("s2", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    journal
        .append_durable(&new_durable("s1", AgentEventKind::Cancelled))
        .await
        .unwrap();

    let s1_events = journal.load_session_stream("s1", None, None).await.unwrap();
    assert_eq!(s1_events.len(), 2);
    assert!(s1_events.iter().all(|e| e.session_id == "s1"));

    let s2_events = journal.load_session_stream("s2", None, None).await.unwrap();
    assert_eq!(s2_events.len(), 1);
}

#[tokio::test]
async fn journal_load_session_stream_respects_after_seq_cursor() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let e1 = journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    let _e2 = journal
        .append_durable(&new_durable("s1", AgentEventKind::Cancelled))
        .await
        .unwrap();
    let _e3 = journal
        .append_durable(&new_durable(
            "s1",
            AgentEventKind::Error {
                message: "x".into(),
            },
        ))
        .await
        .unwrap();

    let after_first = journal
        .load_session_stream("s1", Some(e1.stream_seq), None)
        .await
        .unwrap();
    assert_eq!(after_first.len(), 2);
    assert!(after_first[0].stream_seq > e1.stream_seq);
}

#[tokio::test]
async fn journal_load_session_stream_respects_limit() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    for _ in 0..5 {
        journal
            .append_durable(&new_durable("s1", AgentEventKind::Cancelled))
            .await
            .unwrap();
    }

    let limited = journal
        .load_session_stream("s1", None, Some(2))
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);
}

#[tokio::test]
async fn journal_load_global_stream_returns_all_sessions() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    journal
        .append_durable(&new_durable("s2", AgentEventKind::SessionCreated))
        .await
        .unwrap();

    let global = journal.load_global_stream(None, None).await.unwrap();
    assert_eq!(global.len(), 2);
}

#[tokio::test]
async fn journal_load_global_stream_respects_cursor() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let e1 = journal
        .append_durable(&new_durable("s1", AgentEventKind::SessionCreated))
        .await
        .unwrap();
    journal
        .append_durable(&new_durable("s2", AgentEventKind::SessionCreated))
        .await
        .unwrap();

    let after = journal
        .load_global_stream(Some(e1.stream_seq), None)
        .await
        .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].session_id, "s2");
}

#[tokio::test]
async fn journal_durable_event_never_replayed_for_ephemeral_kind() {
    // Verify that classify_durability correctly identifies ephemeral events;
    // the EventSink will use this to route. The journal itself doesn't filter.
    assert_eq!(
        crate::events::classify_durability(&AgentEventKind::AssistantContentDelta {
            content: "x".into(),
            message_id: "m".into(),
        }),
        crate::events::Durability::Ephemeral
    );
}

#[tokio::test]
async fn journal_empty_session_returns_empty_vec() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    let events = journal
        .load_session_stream("nonexistent", None, None)
        .await
        .unwrap();
    assert!(events.is_empty());
}

#[tokio::test]
async fn journal_ordering_is_monotonic_per_stream() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    for _ in 0..10 {
        journal
            .append_durable(&new_durable("s1", AgentEventKind::Cancelled))
            .await
            .unwrap();
    }

    let events = journal.load_session_stream("s1", None, None).await.unwrap();
    for window in events.windows(2) {
        assert!(
            window[1].stream_seq > window[0].stream_seq,
            "stream_seq must be strictly increasing"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════
// ViewStore — scoped session browsing tests
// ══════════════════════════════════════════════════════════════════════

async fn seed_scoped_sessions(storage: &SqliteStorage) -> (String, String, String, String) {
    let root_a = storage
        .create_session(
            Some("root-alpha".to_string()),
            Some("/workspace".into()),
            None,
            None,
        )
        .await
        .unwrap();
    let root_b = storage
        .create_session(
            Some("root-beta".to_string()),
            Some("/workspace".into()),
            None,
            None,
        )
        .await
        .unwrap();
    let user_fork = storage
        .create_session(
            Some("user-fork".to_string()),
            Some("/workspace".into()),
            Some(root_a.public_id.clone()),
            Some(ForkOrigin::User),
        )
        .await
        .unwrap();
    let delegate = storage
        .create_session(
            Some("delegate-child".to_string()),
            Some("/workspace".into()),
            Some(root_a.public_id.clone()),
            Some(ForkOrigin::Delegation),
        )
        .await
        .unwrap();

    (
        root_a.public_id,
        root_b.public_id,
        user_fork.public_id,
        delegate.public_id,
    )
}

fn session_ids(groups: &[crate::session::projection::SessionGroup]) -> Vec<String> {
    groups
        .iter()
        .flat_map(|group| {
            group
                .sessions
                .iter()
                .map(|session| session.session_id.clone())
        })
        .collect()
}

#[tokio::test]
async fn browse_session_groups_filters_by_scope_and_counts_after_filtering() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let (root_a, root_b, user_fork, delegate) = seed_scoped_sessions(&storage).await;
    let view: &dyn ViewStore = &storage;

    let (groups, _, total) = view
        .browse_session_groups(None, 20, 10, SessionScope::All)
        .await
        .unwrap();
    assert_eq!(total, 4);
    assert_eq!(session_ids(&groups).len(), 4);

    let (groups, _, total) = view
        .browse_session_groups(None, 20, 10, SessionScope::Root)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(session_ids(&groups), vec![root_b.clone(), root_a.clone()]);
    assert_eq!(groups[0].total_count, Some(2));

    let (groups, _, total) = view
        .browse_session_groups(None, 20, 10, SessionScope::Forks)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(session_ids(&groups), vec![user_fork.clone()]);

    let (groups, _, total) = view
        .browse_session_groups(None, 20, 10, SessionScope::Delegates)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(session_ids(&groups), vec![delegate.clone()]);

    let (groups, _, total) = view
        .browse_session_groups(None, 20, 10, SessionScope::Children)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(session_ids(&groups), vec![delegate, user_fork]);
}

#[tokio::test]
async fn browse_session_groups_marks_only_user_forks_as_children() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let root_with_user_fork = storage
        .create_session(
            Some("root-with-user-fork".to_string()),
            Some("/workspace".into()),
            None,
            None,
        )
        .await
        .unwrap();
    storage
        .create_session(
            Some("user-fork".to_string()),
            Some("/workspace".into()),
            Some(root_with_user_fork.public_id.clone()),
            Some(ForkOrigin::User),
        )
        .await
        .unwrap();
    let root_with_delegate_only = storage
        .create_session(
            Some("root-with-delegate-only".to_string()),
            Some("/workspace".into()),
            None,
            None,
        )
        .await
        .unwrap();
    storage
        .create_session(
            Some("delegate-child".to_string()),
            Some("/workspace".into()),
            Some(root_with_delegate_only.public_id.clone()),
            Some(ForkOrigin::Delegation),
        )
        .await
        .unwrap();
    let view: &dyn ViewStore = &storage;

    let (groups, _, _) = view
        .browse_session_groups(None, 20, 20, SessionScope::Root)
        .await
        .unwrap();
    let sessions: Vec<_> = groups
        .iter()
        .flat_map(|group| group.sessions.iter())
        .collect();

    let root_with_user_fork_item = sessions
        .iter()
        .find(|session| session.session_id == root_with_user_fork.public_id)
        .unwrap();
    assert!(root_with_user_fork_item.has_children);
    assert_eq!(root_with_user_fork_item.fork_count, 1);

    let root_with_delegate_only_item = sessions
        .iter()
        .find(|session| session.session_id == root_with_delegate_only.public_id)
        .unwrap();
    assert!(!root_with_delegate_only_item.has_children);
    assert_eq!(root_with_delegate_only_item.fork_count, 0);
}

#[tokio::test]
async fn list_session_children_returns_user_forks_and_excludes_delegates() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let (root_a, _, user_fork, delegate) = seed_scoped_sessions(&storage).await;
    let view: &dyn ViewStore = &storage;

    let (group, total) = view.list_session_children(root_a, None, 20).await.unwrap();

    assert_eq!(total, 1);
    assert_eq!(group.total_count, Some(1));
    assert_eq!(group.sessions[0].fork_count, 0);
    assert_eq!(session_ids(&[group]), vec![user_fork.clone()]);
    assert_ne!(delegate, user_fork);
}

#[tokio::test]
async fn list_session_children_returns_empty_group_for_missing_parent() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let view: &dyn ViewStore = &storage;

    let (group, total) = view
        .list_session_children("missing-parent".to_string(), None, 20)
        .await
        .unwrap();

    assert_eq!(total, 0);
    assert_eq!(group.total_count, Some(0));
    assert!(group.sessions.is_empty());
    assert!(group.next_cursor.is_none());
}

#[tokio::test]
async fn group_sessions_scope_filtering_respects_cursors_and_counts() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let (root_a, root_b, _, _) = seed_scoped_sessions(&storage).await;
    let view: &dyn ViewStore = &storage;

    let (group, total) = view
        .list_group_sessions(Some("/workspace".to_string()), None, 1, SessionScope::Root)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(group.total_count, Some(2));
    assert_eq!(group.sessions.len(), 1);
    assert_eq!(group.sessions[0].session_id, root_b);
    assert_eq!(group.next_cursor.as_deref(), Some("1"));

    let (group, total) = view
        .list_group_sessions(
            Some("/workspace".to_string()),
            group.next_cursor,
            1,
            SessionScope::Root,
        )
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert_eq!(group.sessions[0].session_id, root_a);
    assert!(group.next_cursor.is_none());
}

#[tokio::test]
async fn search_sessions_filters_by_scope() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    seed_scoped_sessions(&storage).await;
    let view: &dyn ViewStore = &storage;

    let (groups, _, total) = view
        .search_sessions("root".to_string(), None, 20, SessionScope::Root)
        .await
        .unwrap();
    assert_eq!(total, 2);
    assert!(
        groups
            .iter()
            .flat_map(|group| group.sessions.iter())
            .all(|session| session.parent_session_id.is_none())
    );

    let (groups, _, total) = view
        .search_sessions("delegate".to_string(), None, 20, SessionScope::Delegates)
        .await
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(
        groups[0].sessions[0].fork_origin.as_deref(),
        Some("delegation")
    );
}

// ══════════════════════════════════════════════════════════════════════
// ViewStore — get_recent_models_view tests
// ══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn recent_models_view_reads_from_event_journal() {
    // This test verifies that get_recent_models_view reads from
    // event_journal (not the dropped legacy `events` table).
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();

    // Create a session so we can join on sessions.public_id
    let session = storage
        .create_session(
            None,
            Some(std::path::PathBuf::from("/home/user/project")),
            None,
            None,
        )
        .await
        .unwrap();
    let session_id = session.public_id;

    // Insert a ProviderChanged event into event_journal
    let journal: &dyn EventJournal = &storage;
    journal
        .append_durable(&NewDurableEvent {
            session_id: session_id.clone(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::ProviderChanged {
                provider: "anthropic".to_string(),
                model: "claude-3-opus".to_string(),
                config_id: 1,
                context_limit: Some(200_000),
                provider_node_id: None,
            },
        })
        .await
        .unwrap();

    // Query recent models — should find the one we just inserted
    let view: &dyn ViewStore = &storage;
    let result = view.get_recent_models_view(10).await.unwrap();

    // Flatten all workspace entries
    let all_entries: Vec<&RecentModelEntry> = result.by_workspace.values().flatten().collect();
    assert_eq!(
        all_entries.len(),
        1,
        "expected 1 recent model entry, got {}",
        all_entries.len()
    );
    assert_eq!(all_entries[0].provider, "anthropic");
    assert_eq!(all_entries[0].model, "claude-3-opus");
    assert_eq!(all_entries[0].use_count, 1);
}

#[tokio::test]
async fn recent_models_view_returns_empty_when_no_provider_changed_events() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();

    let view: &dyn ViewStore = &storage;
    let result = view.get_recent_models_view(10).await.unwrap();
    assert!(
        result.by_workspace.is_empty(),
        "expected empty recent models on fresh db"
    );
}

#[tokio::test]
async fn recent_models_view_respects_limit_per_workspace() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();

    let session = storage
        .create_session(
            None,
            Some(std::path::PathBuf::from("/workspace")),
            None,
            None,
        )
        .await
        .unwrap();
    let session_id = session.public_id;

    let journal: &dyn EventJournal = &storage;
    for (provider, model) in &[
        ("anthropic", "model-a"),
        ("openai", "model-b"),
        ("cohere", "model-c"),
    ] {
        journal
            .append_durable(&NewDurableEvent {
                session_id: session_id.clone(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::ProviderChanged {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    config_id: 1,
                    context_limit: None,
                    provider_node_id: None,
                },
            })
            .await
            .unwrap();
    }

    let view: &dyn ViewStore = &storage;
    let result = view.get_recent_models_view(2).await.unwrap();

    // Each workspace should have at most 2 entries
    for entries in result.by_workspace.values() {
        assert!(
            entries.len() <= 2,
            "expected at most 2 entries per workspace, got {}",
            entries.len()
        );
    }
}

#[tokio::test]
async fn journal_preserves_remote_origin_and_source_node() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal: &dyn EventJournal = &storage;

    journal
        .append_durable(&NewDurableEvent {
            session_id: "s1".to_string(),
            origin: EventOrigin::Remote,
            source_node: Some("peer-42".to_string()),
            kind: AgentEventKind::SessionCreated,
        })
        .await
        .unwrap();

    let events = journal.load_session_stream("s1", None, None).await.unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0].origin, EventOrigin::Remote));
    assert_eq!(events[0].source_node.as_deref(), Some("peer-42"));
}

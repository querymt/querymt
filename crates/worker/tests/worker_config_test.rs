//! Integration tests for `build_worker_config`.
//!
//! These tests run against the real `querymt-worker` library crate and verify
//! the contract between the worker's `AgentConfig` and the shared SQLite DB.

use querymt_agent::events::AgentEventKind;
use querymt_agent::session::backend::StorageBackend;
use querymt_agent::session::projection::EventJournal;
use querymt_agent::session::sqlite_storage::SqliteStorage;
use querymt_worker::config::build_worker_config;
use tempfile::NamedTempFile;

/// `build_worker_config` must use an in-memory EventJournal, NOT the shared
/// DB's journal.
///
/// Rationale: the `SessionActor` inside the sandbox calls `emit_event`
/// (e.g. on `SetMode` and `SetSessionModel`). That write goes through the
/// `EventSink`'s journal. If the journal is backed by the shared DB file,
/// SQLite needs to create a `-journal` sidecar in `~/.qmt/` — but the
/// sandbox restricts directory writes there, causing "unable to open database
/// file" errors.
///
/// Using an in-memory journal keeps event writes fully sandbox-safe: the
/// in-memory DB has no filesystem sidecar requirements.
///
/// RED: currently `build_worker_config` passes `storage.event_journal()` —
/// the shared DB's journal. The test fails because writing an event to the
/// config's sink persists it back to the shared DB file.
#[tokio::test]
async fn worker_config_uses_in_memory_event_journal_not_shared_db() {
    // Create a temp file to act as the shared DB.
    let tmp = NamedTempFile::new().expect("temp file");
    let db_path = tmp.path().to_path_buf();

    // Bootstrap the shared DB (run migrations so the schema exists).
    let shared_storage = SqliteStorage::connect(db_path.clone())
        .await
        .expect("open shared DB");

    // Build the worker config pointing at the shared DB file.
    let config = build_worker_config(&db_path)
        .await
        .expect("build_worker_config");

    // Emit a durable event through the worker config's event sink.
    config
        .event_sink
        .emit_durable("test-session", AgentEventKind::SessionCreated)
        .await
        .expect("emit_durable must succeed (in-memory journal never fails on writes)");

    // Query the SHARED DB's event journal directly.
    // The event must NOT be there — it was absorbed by the in-memory journal.
    let shared_journal = shared_storage.event_journal();
    let shared_events = shared_journal
        .load_session_stream("test-session", None, None)
        .await
        .expect("load_session_stream from shared DB");

    assert!(
        shared_events.is_empty(),
        "worker events must NOT be written to the shared DB file; \
         found {} event(s) in the shared DB. \
         Fix: pass an in-memory SqliteStorage::connect(\":memory:\") journal \
         instead of storage.event_journal() in build_worker_config().",
        shared_events.len()
    );
}

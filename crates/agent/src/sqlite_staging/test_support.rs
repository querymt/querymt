//! SQLite fixtures (`#[cfg(test)]`; becomes the `test-support` module of
//! `querymt-agent-sqlite`).

use crate::session::backend::StorageBackend;
use crate::session::schema;
use crate::session::sqlite_storage::SqliteStorage;
use parking_lot::Mutex;
use rusqlite::Connection;
use std::sync::Arc;
use tempfile::TempDir;

/// Creates an in-memory SQLite connection with the full agent schema initialized.
pub fn sqlite_conn_with_schema() -> Arc<Mutex<Connection>> {
    let mut conn = Connection::open_in_memory().expect("open in-memory sqlite");
    schema::init_schema(&mut conn).expect("initialize schema");
    Arc::new(Mutex::new(conn))
}

/// In-memory SQLite storage with no agent.
pub struct TestStorage {
    pub storage: Arc<SqliteStorage>,
    pub _tempdir: TempDir,
}

impl TestStorage {
    pub async fn new() -> Self {
        let tempdir = TempDir::new().expect("create temp dir");
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create in-memory sqlite"),
        );
        Self {
            storage,
            _tempdir: tempdir,
        }
    }

    pub fn session_store(&self) -> Arc<dyn crate::session::store::SessionStore> {
        self.storage.session_store()
    }

    pub fn event_journal(&self) -> Arc<dyn crate::session::projection::EventJournal> {
        self.storage.event_journal()
    }
}

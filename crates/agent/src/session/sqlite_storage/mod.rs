//! SQLite-backed storage for agent sessions.
//!
//! This module is split into focused submodules:
//!
//! | File               | Responsibility                                      |
//! |--------------------|-----------------------------------------------------|
//! | `mod.rs`           | `SqliteStorage` struct, connection helpers          |
//! | `session_store.rs` | `impl SessionStore` — messages, LLM cfg, undo, ...  |
//! | `view_store.rs`    | `impl ViewStore` — projections, session list        |
//! | `event_journal.rs` | `impl EventJournal` — durable event persistence    |
//! | `migrations.rs`    | Schema migrations (0001 … 0007)                     |
//! | `row_parsers.rs`   | Shared SQLite row → domain-type helpers             |
//! | `tests.rs`         | Integration tests                                   |

mod event_journal;
mod migrations;
mod row_parsers;
mod session_store;
mod view_store;

#[cfg(test)]
mod tests;

use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::session::error::{SessionError, SessionResult};

use migrations::apply_migrations;

/// A unified SQLite storage implementation.
///
/// This implementation provides all storage functionality in a single struct:
/// - Session and message persistence (SessionStore)
/// - Durable event persistence and querying (EventJournal)
/// - View generation for observability (ViewStore)
/// - Storage backend interface (StorageBackend)
///
/// ## Session Isolation Guarantees
///
/// This implementation ensures strict session isolation through:
/// 1. **Unique Session IDs**: Each session has a UUID-based unique identifier
/// 2. **Query Scoping**: All database queries are scoped by session_id
/// 3. **Thread-Safe Access**: Uses `Arc<Mutex<Connection>>` for thread-safe database access
/// 4. **Non-Blocking Operations**: All database operations use `spawn_blocking` to avoid
///    blocking the async runtime, allowing parallel session operations
///
/// ## Concurrency Model
///
/// - The `Mutex<Connection>` serializes database access but only for the duration of each query
/// - Each operation quickly acquires the lock, executes the query, and releases the lock
/// - Different sessions can execute operations in parallel (interleaved database access)
/// - Within a session, operations are executed in the order they arrive
///
/// This design balances simplicity (single connection) with reasonable concurrency
/// for most use cases. For higher throughput, consider using a connection pool.
#[derive(Clone)]
pub struct SqliteStorage {
    pub(super) conn: Arc<Mutex<Connection>>,
}

impl SqliteStorage {
    /// Expose the raw connection for sharing with other SQLite-backed components
    /// (e.g. `SqliteScheduleRepository`, `SqliteKnowledgeStore`) that operate
    /// on the same database.
    pub fn conn(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    /// Expose the raw connection for test assertions (e.g. querying legacy tables).
    #[cfg(test)]
    pub fn conn_for_test(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    pub async fn connect(path: PathBuf) -> SessionResult<Self> {
        Self::connect_with_options(path, true).await
    }

    /// Connect to the database with control over migration behavior.
    ///
    /// When `migrate` is `false`, the database is opened as-is without running
    /// migrations.  This is useful for read-only tooling (e.g. session replay,
    /// export) that must not alter the existing data.
    pub async fn connect_with_options(path: PathBuf, migrate: bool) -> SessionResult<Self> {
        let db_path = path.clone();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, rusqlite::Error> {
            let mut conn = Connection::open(&db_path)?;
            conn.execute("PRAGMA foreign_keys = ON;", [])?;
            if migrate {
                apply_migrations(&mut conn)?;
            }
            Ok(conn)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Failed to spawn blocking task: {}", e)))?
        .map_err(SessionError::from)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub(super) async fn run_blocking<F, R>(&self, f: F) -> SessionResult<R>
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
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    /// Helper: Resolve session public_id → internal i64
    pub(super) async fn resolve_session_internal_id(
        &self,
        session_public_id: &str,
    ) -> SessionResult<i64> {
        let session_public_id_owned = session_public_id.to_string();
        let error_value = session_public_id_owned.clone();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id FROM sessions WHERE public_id = ?",
                rusqlite::params![session_public_id_owned],
                |row| row.get(0),
            )
        })
        .await
        .map_err(|err| match err {
            SessionError::DatabaseError(msg) if msg.contains("Query returned no rows") => {
                SessionError::SessionNotFound(error_value.clone())
            }
            other => other,
        })
    }

    /// Helper: Resolve message public_id → internal i64
    pub(super) async fn resolve_message_internal_id(
        &self,
        message_public_id: &str,
    ) -> SessionResult<i64> {
        let message_public_id_owned = message_public_id.to_string();
        let error_value = message_public_id_owned.clone();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id FROM messages WHERE public_id = ?",
                rusqlite::params![message_public_id_owned],
                |row| row.get(0),
            )
        })
        .await
        .map_err(|err| match err {
            SessionError::DatabaseError(msg) if msg.contains("Query returned no rows") => {
                SessionError::InvalidOperation(format!("Message not found: {}", error_value))
            }
            other => other,
        })
    }
}

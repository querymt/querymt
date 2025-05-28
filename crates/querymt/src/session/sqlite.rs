use super::{Session, SessionEntry, SessionId, SessionStore, SessionStoreError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use log::debug;
use serde_json;
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};

/// A SQLite implementation of the `SessionStore` trait, supporting full-text search.
pub struct SqliteSessionStore {
    pool: SqlitePool,
}

impl SqliteSessionStore {
    /// Creates a new `SqliteSessionStore` and initializes the database schema.
    ///
    /// `database_url` typically refers to a file path (e.g., "sqlite:sessions.db").
    pub async fn new(database_url: &str) -> Result<Self, SessionStoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5) // Configure connection pool size as needed
            .connect(database_url)
            .await
            .map_err(|e| {
                SessionStoreError::DbError(format!("Failed to connect to SQLite: {}", e))
            })?;

        Self::migrate(&pool).await?;

        Ok(Self { pool })
    }

    /// Runs database migrations to create necessary tables and indexes.
    async fn migrate(pool: &SqlitePool) -> Result<(), SessionStoreError> {
        // Create sessions table
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )
        .execute(pool)
        .await
        .map_err(|e| {
            SessionStoreError::DbError(format!("Failed to create sessions table: {}", e))
        })?;

        // Create session_entries table (for individual entries)
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS session_entries (
                session_id TEXT NOT NULL,
                entry_id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_timestamp TEXT NOT NULL,
                entry_data_json TEXT NOT NULL,
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );",
        )
        .execute(pool)
        .await
        .map_err(|e| {
            SessionStoreError::DbError(format!("Failed to create session_entries table: {}", e))
        })?;

        // Create FTS5 virtual table for searchable content
        sqlx::query(
            "CREATE VIRTUAL TABLE IF NOT EXISTS session_entries_fts USING fts5(searchable_content);",
        )
        .execute(pool)
        .await
        .map_err(|e| SessionStoreError::DbError(format!("Failed to create session_entries_fts table: {}", e)))?;

        // Add index for session_id on session_entries for faster lookup and foreign key efficiency
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_session_entries_session_id ON session_entries(session_id);",
        )
        .execute(pool)
        .await
        .map_err(|e| SessionStoreError::DbError(format!("Failed to create index on session_entries: {}", e)))?;

        debug!("SQLite database migrations completed successfully.");
        Ok(())
    }

    /// Helper function to extract searchable text from a `SessionEntry`.
    /// This content is then stored in the FTS5 table.
    fn extract_searchable_content(entry: &SessionEntry) -> String {
        match entry {
            SessionEntry::Message(msg) => msg.content.clone(),
            SessionEntry::ToolCallAttempt(tool_call) => {
                // Combine tool name and stringified arguments for searchability
                format!(
                    "tool_call: {} args: {}",
                    tool_call.function.name, tool_call.function.arguments
                )
            }
            SessionEntry::LLMFailure(op_type, error_msg) => {
                // Combine operation type and error message
                format!("LLM_FAILURE: {} {}", op_type, error_msg)
            }
        }
    }
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn create_session(&self, session: Session) -> Result<(), SessionStoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SessionStoreError::DbError(e.to_string()))?;

        // Check if session already exists
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?)")
            .bind(session.id.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| SessionStoreError::DbError(e.to_string()))?;

        if exists {
            tx.rollback()
                .await
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
            return Err(SessionStoreError::AlreadyExists(session.id));
        }

        // Insert session metadata
        sqlx::query("INSERT INTO sessions (id, created_at, updated_at) VALUES (?, ?, ?)")
            .bind(session.id.as_str())
            .bind(session.created_at.to_rfc3339())
            .bind(session.updated_at.to_rfc3339())
            .execute(&mut *tx)
            .await
            .map_err(|e| SessionStoreError::DbError(format!("Failed to insert session: {}", e)))?;

        // Insert initial entries and their searchable content
        for (timestamp, entry) in session.entries {
            let entry_json = serde_json::to_string(&entry).map_err(|e| {
                SessionStoreError::CodecError(format!("Failed to serialize SessionEntry: {}", e))
            })?;
            let searchable_content = Self::extract_searchable_content(&entry);

            let row = sqlx::query(
                "INSERT INTO session_entries (session_id, entry_timestamp, entry_data_json) VALUES (?, ?, ?)",
            )
            .bind(session.id.as_str())
            .bind(timestamp.to_rfc3339())
            .bind(entry_json)
            .execute(&mut *tx)
            .await
            .map_err(|e| SessionStoreError::DbError(format!("Failed to insert session entry: {}", e)))?;

            let entry_id = row.last_insert_rowid(); // Get the ID of the newly inserted entry

            // Insert into FTS table, linking with the entry_id
            sqlx::query("INSERT INTO session_entries_fts(rowid, searchable_content) VALUES (?, ?)")
                .bind(entry_id)
                .bind(searchable_content)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    SessionStoreError::DbError(format!("Failed to insert session FTS entry: {}", e))
                })?;
        }

        tx.commit()
            .await
            .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
        Ok(())
    }

    async fn get_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Session>, SessionStoreError> {
        let session_row =
            sqlx::query("SELECT id, created_at, updated_at FROM sessions WHERE id = ?")
                .bind(session_id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| {
                    SessionStoreError::DbError(format!("Failed to fetch session: {}", e))
                })?;

        if let Some(s_row) = session_row {
            let id: String = s_row
                .try_get("id")
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
            let created_at_str: String = s_row
                .try_get("created_at")
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
            let updated_at_str: String = s_row
                .try_get("updated_at")
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;

            let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                .map_err(|e| {
                    SessionStoreError::CodecError(format!(
                        "Failed to parse created_at timestamp: {}",
                        e
                    ))
                })?
                .with_timezone(&Utc);
            let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
                .map_err(|e| {
                    SessionStoreError::CodecError(format!(
                        "Failed to parse updated_at timestamp: {}",
                        e
                    ))
                })?
                .with_timezone(&Utc);

            // Fetch all entries for this session, ordered by their insertion ID (chronological)
            let entry_rows = sqlx::query(
                "SELECT entry_timestamp, entry_data_json FROM session_entries WHERE session_id = ? ORDER BY entry_id ASC",
            )
            .bind(session_id.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| SessionStoreError::DbError(format!("Failed to fetch session entries: {}", e)))?;

            let mut entries = Vec::new();
            for e_row in entry_rows {
                let timestamp_str: String = e_row
                    .try_get("entry_timestamp")
                    .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
                let entry_json: String = e_row
                    .try_get("entry_data_json")
                    .map_err(|e| SessionStoreError::DbError(e.to_string()))?;

                let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                    .map_err(|e| {
                        SessionStoreError::CodecError(format!(
                            "Failed to parse entry timestamp: {}",
                            e
                        ))
                    })?
                    .with_timezone(&Utc);
                let entry: SessionEntry = serde_json::from_str(&entry_json).map_err(|e| {
                    SessionStoreError::CodecError(format!(
                        "Failed to deserialize SessionEntry: {}",
                        e
                    ))
                })?;
                entries.push((timestamp, entry));
            }

            Ok(Some(Session {
                id: SessionId::from_str(&id),
                created_at,
                updated_at,
                entries,
            }))
        } else {
            Ok(None)
        }
    }

    async fn add_session_entry(
        &self,
        session_id: &SessionId,
        entry: SessionEntry,
    ) -> Result<(), SessionStoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SessionStoreError::DbError(e.to_string()))?;

        // First, check if the session exists (to return NotFound error)
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?)")
            .bind(session_id.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| SessionStoreError::DbError(e.to_string()))?;

        if !exists {
            tx.rollback()
                .await
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
            return Err(SessionStoreError::NotFound(session_id.clone()));
        }

        let now = Utc::now();
        let entry_json = serde_json::to_string(&entry).map_err(|e| {
            SessionStoreError::CodecError(format!("Failed to serialize SessionEntry: {}", e))
        })?;
        let searchable_content = Self::extract_searchable_content(&entry);

        // Insert new entry
        let row = sqlx::query(
            "INSERT INTO session_entries (session_id, entry_timestamp, entry_data_json) VALUES (?, ?, ?)",
        )
        .bind(session_id.as_str())
        .bind(now.to_rfc3339())
        .bind(entry_json)
        .execute(&mut *tx)
        .await
        .map_err(|e| SessionStoreError::DbError(format!("Failed to insert session entry: {}", e)))?;

        let entry_id = row.last_insert_rowid();

        // Insert into FTS table
        sqlx::query("INSERT INTO session_entries_fts(rowid, searchable_content) VALUES (?, ?)")
            .bind(entry_id)
            .bind(searchable_content)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                SessionStoreError::DbError(format!("Failed to insert session FTS entry: {}", e))
            })?;

        // Update session's updated_at timestamp
        sqlx::query("UPDATE sessions SET updated_at = ? WHERE id = ?")
            .bind(now.to_rfc3339())
            .bind(session_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                SessionStoreError::DbError(format!("Failed to update session timestamp: {}", e))
            })?;

        tx.commit()
            .await
            .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
        Ok(())
    }

    async fn update_session(&self, session: &Session) -> Result<(), SessionStoreError> {
        // This method updates session metadata only. Entries are handled by `add_session_entry`.
        // The `entries` field of the input `Session` is ignored here.
        let result = sqlx::query("UPDATE sessions SET created_at = ?, updated_at = ? WHERE id = ?")
            .bind(session.created_at.to_rfc3339())
            .bind(session.updated_at.to_rfc3339())
            .bind(session.id.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| SessionStoreError::DbError(format!("Failed to update session: {}", e)))?;

        if result.rows_affected() == 0 {
            Err(SessionStoreError::NotFound(session.id.clone()))
        } else {
            Ok(())
        }
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), SessionStoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SessionStoreError::DbError(e.to_string()))?;

        // Get entry_ids associated with this session for FTS deletion
        let entry_ids: Vec<i64> =
            sqlx::query_scalar("SELECT entry_id FROM session_entries WHERE session_id = ?")
                .bind(session_id.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;

        // Delete associated entries from the FTS table using their rowids
        for entry_id in entry_ids {
            sqlx::query("DELETE FROM session_entries_fts WHERE rowid = ?")
                .bind(entry_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
        }

        // Delete the session itself. This will cascade delete related entries from `session_entries`
        // due to `ON DELETE CASCADE` foreign key constraint.
        let result = sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(session_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| SessionStoreError::DbError(format!("Failed to delete session: {}", e)))?;

        if result.rows_affected() == 0 {
            tx.rollback()
                .await
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
            Err(SessionStoreError::NotFound(session_id.clone()))
        } else {
            tx.commit()
                .await
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
            Ok(())
        }
    }

    async fn search_session_entries(
        &self,
        session_id: &SessionId,
        query: &str,
    ) -> Result<Vec<(DateTime<Utc>, SessionEntry)>, SessionStoreError> {
        let rows = sqlx::query(
            // Join FTS table with entries table to get original data
            "SELECT T2.entry_timestamp, T2.entry_data_json
             FROM session_entries_fts AS T1
             JOIN session_entries AS T2 ON T1.rowid = T2.entry_id
             WHERE T2.session_id = ? AND T1.searchable_content MATCH ?
             ORDER BY T2.entry_timestamp ASC", // Order by original timestamp for chronological results
        )
        .bind(session_id.as_str())
        .bind(query)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            SessionStoreError::DbError(format!("Failed to search session entries: {}", e))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let timestamp_str: String = row
                .try_get("entry_timestamp")
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
            let entry_json: String = row
                .try_get("entry_data_json")
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;

            let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                .map_err(|e| {
                    SessionStoreError::CodecError(format!("Failed to parse entry timestamp: {}", e))
                })?
                .with_timezone(&Utc);
            let entry: SessionEntry = serde_json::from_str(&entry_json).map_err(|e| {
                SessionStoreError::CodecError(format!("Failed to deserialize SessionEntry: {}", e))
            })?;
            results.push((timestamp, entry));
        }
        Ok(results)
    }

    async fn search_all_session_entries(
        &self,
        query: &str,
    ) -> Result<Vec<(SessionId, DateTime<Utc>, SessionEntry)>, SessionStoreError> {
        let rows = sqlx::query(
            // Join FTS table with entries table to get session_id and original data
            "SELECT T2.session_id, T2.entry_timestamp, T2.entry_data_json
             FROM session_entries_fts AS T1
             JOIN session_entries AS T2 ON T1.rowid = T2.entry_id
             WHERE T1.searchable_content MATCH ?
             ORDER BY T2.entry_timestamp ASC", // Order by original timestamp for chronological results
        )
        .bind(query)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            SessionStoreError::DbError(format!("Failed to search all session entries: {}", e))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let session_id_str: String = row
                .try_get("session_id")
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
            let timestamp_str: String = row
                .try_get("entry_timestamp")
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;
            let entry_json: String = row
                .try_get("entry_data_json")
                .map_err(|e| SessionStoreError::DbError(e.to_string()))?;

            let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                .map_err(|e| {
                    SessionStoreError::CodecError(format!("Failed to parse entry timestamp: {}", e))
                })?
                .with_timezone(&Utc);
            let entry: SessionEntry = serde_json::from_str(&entry_json).map_err(|e| {
                SessionStoreError::CodecError(format!("Failed to deserialize SessionEntry: {}", e))
            })?;
            results.push((SessionId::from_str(&session_id_str), timestamp, entry));
        }
        Ok(results)
    }
}

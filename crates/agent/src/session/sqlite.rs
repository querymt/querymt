use crate::model::{AgentMessage, MessagePart};
use crate::session::store::{Session, SessionStore};
use async_trait::async_trait;
use querymt::{chat::ChatRole, error::LLMError};
use rusqlite::{Connection, OptionalExtension, params};
use rusqlite_migration::{M, Migrations};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;
use uuid::Uuid;

/// An implementation of `SessionStore` using `rusqlite`.
#[derive(Clone)]
pub struct SqliteSessionStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteSessionStore {
    pub async fn connect(path: PathBuf) -> Result<Self, LLMError> {
        let db_path = path.clone();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, rusqlite::Error> {
            let mut conn = Connection::open(&db_path)?;
            conn.execute("PRAGMA foreign_keys = ON;", [])?;
            apply_migrations(&mut conn)?;
            Ok(conn)
        })
        .await
        .map_err(|e| LLMError::ProviderError(format!("Failed to spawn blocking task: {}", e)))?
        .map_err(|e| LLMError::ProviderError(format!("Database connection failed: {}", e)))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn run_blocking<F, R>(&self, f: F) -> Result<R, LLMError>
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
        .map_err(|e| LLMError::ProviderError(format!("Task execution failed: {}", e)))?
        .map_err(|e| LLMError::ProviderError(format!("Database operation failed: {}", e)))
    }
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn create_session(&self, name: Option<String>) -> Result<Session, LLMError> {
        let session = Session {
            id: Uuid::new_v4().to_string(),
            name,
            created_at: Some(OffsetDateTime::now_utc()),
            updated_at: Some(OffsetDateTime::now_utc()),
        };

        let session_to_insert = session.clone();
        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO sessions (id, name, created_at, updated_at) VALUES (?, ?, ?, ?)",
                params![
                    session_to_insert.id,
                    session_to_insert.name,
                    session_to_insert.created_at,
                    session_to_insert.updated_at
                ],
            )?;
            Ok(())
        })
        .await?;

        Ok(session)
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<Session>, LLMError> {
        let session_id = session_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id, name, created_at, updated_at FROM sessions WHERE id = ?",
                params![session_id],
                |row| {
                    Ok(Session {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        created_at: Some(row.get(2)?),
                        updated_at: Some(row.get(3)?),
                    })
                },
            )
            .optional()
        })
        .await
    }

    async fn list_sessions(&self) -> Result<Vec<Session>, LLMError> {
        self.run_blocking(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, created_at, updated_at FROM sessions ORDER BY updated_at DESC",
            )?;
            let sessions_iter = stmt.query_map([], |row| {
                Ok(Session {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    created_at: Some(row.get(2)?),
                    updated_at: Some(row.get(3)?),
                })
            })?;

            sessions_iter.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), LLMError> {
        let session_id = session_id.to_string();
        let session_id_clone = session_id.clone();
        let affected = self
            .run_blocking(move |conn| {
                conn.execute(
                    "DELETE FROM sessions WHERE id = ?",
                    params![session_id_clone],
                )
            })
            .await?;

        if affected == 0 {
            return Err(LLMError::InvalidRequest(format!(
                "Session with id '{}' not found",
                session_id
            )));
        }
        Ok(())
    }

    async fn get_history(&self, session_id: &str) -> Result<Vec<AgentMessage>, LLMError> {
        let session_id = session_id.to_string();
        self.run_blocking(move |conn| {
            // 1. Fetch Messages
            let mut stmt = conn.prepare(
                "SELECT id, role, created_at, parent_message_id FROM messages WHERE session_id = ? ORDER BY created_at ASC"
            )?;

            let messages_iter = stmt.query_map(params![session_id], |row| {
                let role_str: String = row.get(1)?;
                let role = match role_str.as_str() {
                    "User" => ChatRole::User,
                    "Assistant" => ChatRole::Assistant,
                    _ => ChatRole::User, // Default fallback
                };

                Ok(AgentMessage {
                    id: row.get(0)?,
                    session_id: session_id.clone(),
                    role,
                    parts: Vec::new(), // Will populate next
                    created_at: row.get::<_, i64>(2)?, // Stored as unix timestamp integer
                    parent_message_id: row.get(3)?,
                })
            })?;

            let mut messages = messages_iter.collect::<Result<Vec<_>, _>>()?;

            // 2. Fetch Parts for all messages in this session
            // Optimization: Fetch all parts for the session and group them
            let mut part_stmt = conn.prepare(
                "SELECT message_id, content_json FROM message_parts WHERE message_id IN (SELECT id FROM messages WHERE session_id = ?) ORDER BY sort_order ASC"
            )?;

            let parts_iter = part_stmt.query_map(params![session_id], |row| {
                let mid: String = row.get(0)?;
                let content: String = row.get(1)?;
                let part: MessagePart = serde_json::from_str(&content).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
                })?;
                Ok((mid, part))
            })?;

            // Group parts by message_id
            let mut parts_map: std::collections::HashMap<String, Vec<MessagePart>> = std::collections::HashMap::new();
            for res in parts_iter {
                let (mid, part) = res?;
                parts_map.entry(mid).or_default().push(part);
            }

            // Attach parts to messages
            for msg in &mut messages {
                if let Some(parts) = parts_map.remove(&msg.id) {
                    msg.parts = parts;
                }
            }

            Ok(messages)
        })
        .await
    }

    async fn add_message(&self, session_id: &str, message: AgentMessage) -> Result<(), LLMError> {
        let session_id = session_id.to_string();
        let msg = message.clone();

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;

            let role_str = match msg.role {
                ChatRole::User => "User",
                ChatRole::Assistant => "Assistant",
            };

            tx.execute(
                "INSERT INTO messages (id, session_id, role, created_at, parent_message_id) VALUES (?, ?, ?, ?, ?)",
                params![msg.id, session_id, role_str, msg.created_at, msg.parent_message_id],
            )?;

            for (idx, part) in msg.parts.iter().enumerate() {
                let content_json = serde_json::to_string(part).map_err(|e| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
                })?;

                tx.execute(
                    "INSERT INTO message_parts (id, message_id, part_type, content_json, sort_order) VALUES (?, ?, ?, ?, ?)",
                    params![Uuid::new_v4().to_string(), msg.id, part.type_name(), content_json, idx as i32],
                )?;
            }

            tx.execute(
                "UPDATE sessions SET updated_at = ? WHERE id = ?",
                params![OffsetDateTime::now_utc(), session_id],
            )?;

            tx.commit()?;
            Ok(())
        })
        .await
    }

    async fn fork_session(
        &self,
        source_session_id: &str,
        target_message_id: &str,
    ) -> Result<String, LLMError> {
        let source_session_id = source_session_id.to_string();
        let target_message_id = target_message_id.to_string();

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;

            // 1. Create New Session
            let new_session_id = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO sessions (id, name, created_at, updated_at) VALUES (?, ?, ?, ?)",
                params![
                    new_session_id,
                    format!("Fork of {}", source_session_id), // Temporary name
                    OffsetDateTime::now_utc(),
                    OffsetDateTime::now_utc()
                ],
            )?;

            // 2. Identify messages to copy (up to target_message_id)
            let messages_to_copy = {
                let mut stmt = tx.prepare(
                    "SELECT id, role, created_at, parent_message_id FROM messages WHERE session_id = ? ORDER BY created_at ASC"
                )?;

                let messages: Vec<(String, String, i64, Option<String>)> = stmt.query_map(params![source_session_id], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?
                    ))
                })?.collect::<Result<Vec<_>, _>>()?;

                let mut to_copy = Vec::new();
                for m in messages {
                    let mid = m.0.clone();
                    to_copy.push(m);
                    if mid == target_message_id {
                        break;
                    }
                }
                to_copy
            };

            // 3. Copy messages and their parts
            for (old_id, role, created_at, _parent) in messages_to_copy {
                let new_msg_id = Uuid::new_v4().to_string();

                // Insert Message
                tx.execute(
                    "INSERT INTO messages (id, session_id, role, created_at, parent_message_id) VALUES (?, ?, ?, ?, ?)",
                    params![new_msg_id, new_session_id, role, created_at, Option::<String>::None],
                )?;

                // Copy Parts
                {
                    let mut part_stmt = tx.prepare(
                        "SELECT part_type, content_json, sort_order FROM message_parts WHERE message_id = ?"
                    )?;

                    let parts: Vec<(String, String, i32)> = part_stmt.query_map(params![old_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?.collect::<Result<Vec<_>, _>>()?;

                    for (ptype, content, order) in parts {
                        tx.execute(
                            "INSERT INTO message_parts (id, message_id, part_type, content_json, sort_order) VALUES (?, ?, ?, ?, ?)",
                            params![Uuid::new_v4().to_string(), new_msg_id, ptype, content, order],
                        )?;
                    }
                }
            }

            tx.commit()?;
            Ok(new_session_id)
        })
        .await
    }
}

fn apply_migrations(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    let migrations = Migrations::new(vec![M::up(
        r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                name TEXT,
                created_at TIMESTAMP NOT NULL,
                updated_at TIMESTAMP NOT NULL
            );

            -- Recreate messages table for new schema
            -- We drop legacy tables if they exist to ensure clean slate for this refactor
            DROP TABLE IF EXISTS message_tool_calls;
            DROP TABLE IF EXISTS message_binaries;
            DROP TABLE IF EXISTS message_usage;
            DROP TABLE IF EXISTS messages_fts;
            DROP TABLE IF EXISTS messages;

            CREATE TABLE messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                created_at INTEGER NOT NULL, -- Unix timestamp
                parent_message_id TEXT,
                FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );

            CREATE TABLE message_parts (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                part_type TEXT NOT NULL,
                content_json TEXT NOT NULL,
                sort_order INTEGER NOT NULL,
                FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE
            );
        "#,
    )]);

    migrations.to_latest(conn).map_err(|e| {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISUSE),
            Some(format!("Migration failed: {}", e)),
        )
    })?;

    Ok(())
}

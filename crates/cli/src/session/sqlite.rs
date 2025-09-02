use crate::session::store::{SearchResult, Session, SessionStore, StoredMessage};
use async_trait::async_trait;
use querymt::{
    chat::{ChatMessage, ChatResponse, ChatRole, ImageMime, MessageType},
    error::LLMError,
    FunctionCall, ToolCall, Usage,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use rusqlite_migration::{Migrations, M};
use std::sync::{Arc, Mutex};
use std::{path::PathBuf, str::FromStr};
use time::OffsetDateTime;
use uuid::Uuid;

/// An implementation of `SessionStore` using `rusqlite`.
///
/// The connection is managed within an `Arc<Mutex<>>` to allow for safe,
/// concurrent access from multiple async tasks.
#[derive(Clone)]
pub struct SqliteSessionStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteSessionStore {
    /// Opens a connection to the SQLite database at the given path.
    ///
    /// If the database does not exist, it will be created. The necessary schema,
    /// including tables, FTS5 virtual tables, and triggers, will be applied automatically.
    /// This operation is performed in a blocking thread to avoid stalling the async runtime.
    pub async fn connect(path: PathBuf) -> Result<Self, LLMError> {
        let db_path = path.clone();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, rusqlite::Error> {
            let mut conn = Connection::open(&db_path)?;
            // Enable foreign key support, which is off by default.
            conn.execute("PRAGMA foreign_keys = ON;", [])?;

            // Apply migrations instead of executing SQL file directly
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

    pub async fn run_migrations(&self) -> Result<(), LLMError> {
        let conn_arc = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<(), rusqlite::Error> {
            let mut conn = conn_arc.lock().unwrap();
            apply_migrations(&mut conn)
        })
        .await
        .map_err(|e| LLMError::ProviderError(format!("Failed to spawn blocking task: {}", e)))?
        .map_err(|e| LLMError::ProviderError(format!("Migration failed: {}", e)))?;

        Ok(())
    }

    /// Executes a database operation within a blocking thread.
    ///
    /// This helper function abstracts the boilerplate of cloning the connection Arc,
    /// spawning a blocking task, and handling potential errors from both the task
    /// execution and the database operation itself.
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
        let session_id_owned = session_id.to_string();
        let affected_rows = self
            .run_blocking(move |conn| {
                conn.execute(
                    "DELETE FROM sessions WHERE id = ?",
                    params![session_id_owned],
                )
            })
            .await?;

        if affected_rows == 0 {
            return Err(LLMError::InvalidRequest(format!(
                "Session with id '{}' not found",
                session_id
            )));
        }

        Ok(())
    }

    async fn log_exchange(
        &self,
        session_id: &str,
        user_messages: &[ChatMessage],
        assistant_response: &dyn ChatResponse,
    ) -> Result<(), LLMError> {
        let session_id = session_id.to_string();
        let user_msgs_owned = user_messages.to_vec().clone();
        let assistant_msg: ChatMessage = assistant_response.into();
        let usage = assistant_response.usage();

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;

            user_msgs_owned
                .iter()
                .try_for_each(|msg| log_message(&tx, &session_id, msg).map(|_| ()))?;

            let assistant_msg_id = Uuid::new_v4().to_string();
            log_message_with_id(&tx, &session_id, &assistant_msg, &assistant_msg_id)?;

            if let Some(u) = usage {
                tx.execute(
                    "INSERT INTO message_usage (message_id, input_tokens, output_tokens) VALUES (?, ?, ?)",
                    params![&assistant_msg_id, u.input_tokens, u.output_tokens],
                )?;
            }

            tx.execute(
                "UPDATE sessions SET updated_at = ? WHERE id = ?",
                params![OffsetDateTime::now_utc(), &session_id],
            )?;

            tx.commit()
        })
        .await
    }

    async fn get_history(&self, session_id: &str) -> Result<Vec<ChatMessage>, LLMError> {
        let session_id = session_id.to_string();
        self.run_blocking(move |conn| {
            struct DbMessage {
                id: String,
                role: String,
                content: String,
                message_type: String,
                binary_mime_type: Option<String>,
                binary_data: Option<Vec<u8>>,
            }

            let mut msg_stmt = conn.prepare(
                r#"
                SELECT m.id, m.role, m.content, m.message_type, b.mime_type, b.data
                FROM messages m
                LEFT JOIN message_binaries b ON m.id = b.message_id
                WHERE m.session_id = ?
                ORDER BY m.timestamp ASC
                "#,
            )?;

            let db_messages = msg_stmt.query_map(params![session_id], |row| Ok(DbMessage {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                message_type: row.get(3)?,
                binary_mime_type: row.get(4)?,
                binary_data: row.get(5)?,
            }))?.collect::<Result<Vec<_>, _>>()?;

            let mut tool_call_stmt = conn.prepare("SELECT id, call_type, function_name, arguments FROM message_tool_calls WHERE message_id = ?")?;
            let mut history = Vec::with_capacity(db_messages.len());

            for msg in db_messages {
                let role = match msg.role.as_str() {
                    "User" => ChatRole::User,
                    "Assistant" => ChatRole::Assistant,
                    _ => continue, // Should not happen due to DB constraint
                };

                let message_type = match msg.message_type.as_str() {
                    "Text" => MessageType::Text,
                    "ToolUse" | "ToolResult" => {
                        let tool_calls = tool_call_stmt.query_map(params![&msg.id], |row| {
                            Ok(ToolCall {
                                id: row.get(0)?,
                                call_type: row.get(1)?,
                                function: FunctionCall { name: row.get(2)?, arguments: row.get(3)? },
                            })
                        })?.collect::<Result<Vec<_>,_>>()?;

                        if msg.message_type == "ToolUse" { MessageType::ToolUse(tool_calls) } else { MessageType::ToolResult(tool_calls) }
                    }
                    "Image" => {
                        match (msg.binary_mime_type, msg.binary_data) {
                            (Some(mime_str), Some(data)) => {
                                let mime = ImageMime::from_str(&mime_str).unwrap_or(ImageMime::PNG); // Default on parse failure
                                MessageType::Image((mime, data))
                            },
                            _ => MessageType::Text, // Data inconsistency, fallback to text
                        }
                    }
                    "Pdf" => {
                        if let Some(data) = msg.binary_data {
                            MessageType::Pdf(data)
                        } else {
                            MessageType::Text // Data inconsistency, fallback to text
                        }
                    }
                    _ => MessageType::Text, // Fallback for unknown types
                };

                history.push(ChatMessage { role, content: msg.content, message_type });
            }
            Ok(history)
        }).await
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchResult>, LLMError> {
        let fts_query = query
            .split_whitespace()
            .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");

        self.run_blocking(move |conn| {
            let mut fts_stmt = conn.prepare(r#"
                SELECT
                    s.id as session_id, s.name as session_name, s.created_at as session_created_at, s.updated_at as session_updated_at,
                    m.id as message_id, m.timestamp as message_timestamp, m.role as message_role, m.content as message_content,
                    u.input_tokens, u.output_tokens,
                    rank as score
                FROM messages_fts f
                JOIN messages m ON f.rowid = m.rowid
                JOIN sessions s ON m.session_id = s.id
                LEFT JOIN message_usage u ON m.id = u.message_id
                WHERE messages_fts MATCH ? ORDER BY rank
            "#)?;

            struct SearchRow {
                session: Session,
                message: StoredMessage,
                score: f32,
            }

            let search_rows_iter = fts_stmt.query_map(params![fts_query], |row| {
                let session = Session {
                    id: row.get("session_id")?,
                    name: row.get("session_name")?,
                    created_at: Some(row.get("session_created_at")?),
                    updated_at: Some(row.get("session_updated_at")?),
                };

                let usage = match (row.get::<_, Option<u32>>("input_tokens")?, row.get::<_, Option<u32>>("output_tokens")?) {
                    (Some(input_tokens), Some(output_tokens)) => Some(Usage { input_tokens, output_tokens }),
                    _ => None,
                };

                let message = StoredMessage {
                    id: row.get("message_id")?,
                    session_id: session.id.clone(),
                    timestamp: row.get("message_timestamp")?,
                    role: match row.get::<_, String>("message_role")?.as_str() {
                        "User" => ChatRole::User,
                        "Assistant" => ChatRole::Assistant,
                        other => return Err(rusqlite::Error::InvalidColumnType(0, format!("Invalid role '{}'", other), rusqlite::types::Type::Text)),
                    },
                    content: row.get("message_content")?,
                    tool_calls: None,
                    usage,
                };

                Ok(SearchRow { session, message, score: row.get("score")? })
            })?;

            let mut tool_call_stmt = conn.prepare("SELECT id, call_type, function_name, arguments FROM message_tool_calls WHERE message_id = ?")?;
            let mut results = Vec::new();
            for search_row_result in search_rows_iter {
                let mut row = search_row_result?;
                let tool_calls: Vec<ToolCall> = tool_call_stmt.query_map(params![&row.message.id], |tc_row| {
                    Ok(ToolCall { id: tc_row.get(0)?, call_type: tc_row.get(1)?, function: FunctionCall { name: tc_row.get(2)?, arguments: tc_row.get(3)? }})
                })?.collect::<Result<Vec<_>, _>>()?;

                if !tool_calls.is_empty() {
                    row.message.tool_calls = Some(tool_calls);
                }

                results.push(SearchResult { session: row.session, message: row.message, score: row.score });
            }

            Ok(results)
        }).await
    }
}

/// Helper function to log a `ChatMessage` to the database within a transaction.
/// Generates a new UUID for the message id.
fn log_message(
    tx: &Transaction,
    session_id: &str,
    message: &ChatMessage,
) -> Result<String, rusqlite::Error> {
    let msg_id = Uuid::new_v4().to_string();
    log_message_with_id(tx, session_id, message, &msg_id)?;
    Ok(msg_id)
}

/// Helper function to log a `ChatMessage` with a specified message id.
fn log_message_with_id(
    tx: &Transaction,
    session_id: &str,
    message: &ChatMessage,
    msg_id: &str,
) -> Result<(), rusqlite::Error> {
    let msg_type_str = match &message.message_type {
        MessageType::Text => "Text",
        MessageType::ToolUse(_) => "ToolUse",
        MessageType::ToolResult(_) => "ToolResult",
        MessageType::Image(_) => "Image",
        MessageType::Pdf(_) => "Pdf",
        MessageType::ImageURL(_) => todo!(),
    };
    let role_str = match message.role {
        ChatRole::User => "User",
        ChatRole::Assistant => "Assistant",
    };

    tx.execute(
        "INSERT INTO messages (id, session_id, role, content, message_type) VALUES (?, ?, ?, ?, ?)",
        params![msg_id, session_id, role_str, &message.content, msg_type_str],
    )?;

    match &message.message_type {
        MessageType::ToolUse(calls) | MessageType::ToolResult(calls) => {
            for tc in calls {
                tx.execute(
                    "INSERT INTO message_tool_calls (id, message_id, call_type, function_name, arguments) VALUES (?, ?, ?, ?, ?)",
                    params![&tc.id, msg_id, &tc.call_type, &tc.function.name, &tc.function.arguments],
                )?;
            }
        }
        MessageType::Image((mime, data)) => {
            tx.execute(
                "INSERT INTO message_binaries (message_id, mime_type, data) VALUES (?, ?, ?)",
                params![msg_id, mime.as_str(), data],
            )?;
        }
        MessageType::Pdf(data) => {
            tx.execute(
                "INSERT INTO message_binaries (message_id, mime_type, data) VALUES (?, ?, ?)",
                params![msg_id, "application/pdf", data],
            )?;
        }
        MessageType::Text => { /* No associated data to store */ }
        MessageType::ImageURL(_) => todo!(),
    }
    Ok(())
}

fn apply_migrations(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    let migrations = Migrations::new(vec![M::up(
        r#"
            -- Create sessions table
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                name TEXT,
                created_at TIMESTAMP NOT NULL,
                updated_at TIMESTAMP NOT NULL
            );

            -- Create messages table
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                timestamp TIMESTAMP NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now')),
                role TEXT NOT NULL CHECK(role IN ('User', 'Assistant')),
                content TEXT NOT NULL,
                -- This field will help reconstruct the MessageType enum without complex joins.
                message_type TEXT NOT NULL CHECK(message_type IN ('Text', 'ToolUse', 'ToolResult', 'Image', 'Pdf')),
                FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );

            -- Create message_binaries table for storing blob data
            CREATE TABLE IF NOT EXISTS message_binaries (
                message_id TEXT PRIMARY KEY,
                mime_type TEXT NOT NULL,
                data BLOB NOT NULL,
                FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE
            );

            -- Create message_usage table
            CREATE TABLE IF NOT EXISTS message_usage (
                message_id TEXT PRIMARY KEY,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE
            );

            -- Create message_tool_calls table
            CREATE TABLE IF NOT EXISTS message_tool_calls (
                id TEXT NOT NULL, -- from ToolCall.id, not unique
                message_id TEXT NOT NULL,
                call_type TEXT NOT NULL,
                function_name TEXT NOT NULL,
                arguments TEXT NOT NULL,
                PRIMARY KEY (id, message_id),
                FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE
            );

            -- FTS5 table for powerful full-text search
            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                content,
                content='messages',
                content_rowid='rowid',
                tokenize='porter unicode61'
            );

            -- Triggers to keep the FTS table synchronized with the messages table
            DROP TRIGGER IF EXISTS messages_after_insert;
            DROP TRIGGER IF EXISTS messages_after_delete;
            DROP TRIGGER IF EXISTS messages_after_update;

            CREATE TRIGGER messages_after_insert AFTER INSERT ON messages BEGIN
              INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
            END;

            CREATE TRIGGER messages_after_delete AFTER DELETE ON messages BEGIN
              INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
            END;

            CREATE TRIGGER messages_after_update AFTER UPDATE ON messages BEGIN
              INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
              INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
            END;
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

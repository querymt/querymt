use crate::events::{AgentEvent, AgentEventKind, DurableEvent, EventOrigin};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::{
    Alternative, AlternativeStatus, Artifact, Decision, DecisionStatus, Delegation,
    DelegationStatus, ForkInfo, ForkOrigin, ForkPointType, IntentSnapshot, ProgressEntry,
    ProgressKind, Task, TaskStatus,
};
use crate::session::error::{SessionError, SessionResult};
use crate::session::projection::{
    AuditView, DefaultRedactor, EventJournal, FilterExpr, NewDurableEvent, PredicateOp,
    RecentModelEntry, RecentModelsView, RedactedArtifact, RedactedProgress, RedactedTask,
    RedactedView, RedactionPolicy, Redactor, SessionGroup, SessionListFilter, SessionListItem,
    SessionListView, SummaryView, ViewStore,
};
use crate::session::repo_artifact::SqliteArtifactRepository;
use crate::session::repo_decision::SqliteDecisionRepository;
use crate::session::repo_delegation::SqliteDelegationRepository;
use crate::session::repo_intent::SqliteIntentRepository;
use crate::session::repo_progress::SqliteProgressRepository;
use crate::session::repo_session::SqliteSessionRepository;
use crate::session::repo_task::SqliteTaskRepository;
use crate::session::repository::{
    ArtifactRepository, DecisionRepository, DelegationRepository, IntentRepository,
    ProgressRepository, SessionRepository, TaskRepository,
};
use crate::session::schema;
use crate::session::store::{
    CustomModel, LLMConfig, Session, SessionExecutionConfig, SessionStore,
    extract_llm_config_values,
};
use async_trait::async_trait;
use querymt::LLMParams;
use querymt::chat::ChatRole;

use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;
use uuid::Uuid;

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
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStorage {
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
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    /// Helper: Resolve session public_id → internal i64
    async fn resolve_session_internal_id(&self, session_public_id: &str) -> SessionResult<i64> {
        let session_public_id_owned = session_public_id.to_string();
        let error_value = session_public_id_owned.clone();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id FROM sessions WHERE public_id = ?",
                params![session_public_id_owned],
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
    async fn resolve_message_internal_id(&self, message_public_id: &str) -> SessionResult<i64> {
        let message_public_id_owned = message_public_id.to_string();
        let error_value = message_public_id_owned.clone();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id FROM messages WHERE public_id = ?",
                params![message_public_id_owned],
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

#[async_trait]
impl SessionStore for SqliteStorage {
    async fn create_session(
        &self,
        name: Option<String>,
        cwd: Option<std::path::PathBuf>,
        parent_session_id: Option<String>,
        fork_origin: Option<ForkOrigin>,
    ) -> SessionResult<Session> {
        let repo = SqliteSessionRepository::new(self.conn.clone());
        repo.create_session(name, cwd, parent_session_id, fork_origin)
            .await
    }

    async fn get_session(&self, session_id: &str) -> SessionResult<Option<Session>> {
        let repo = SqliteSessionRepository::new(self.conn.clone());
        repo.get_session(session_id).await
    }

    async fn list_sessions(&self) -> SessionResult<Vec<Session>> {
        let repo = SqliteSessionRepository::new(self.conn.clone());
        repo.list_sessions().await
    }

    async fn delete_session(&self, session_id: &str) -> SessionResult<()> {
        let repo = SqliteSessionRepository::new(self.conn.clone());
        repo.delete_session(session_id).await
    }

    async fn get_history(&self, session_id: &str) -> SessionResult<Vec<AgentMessage>> {
        // Resolve session public_id → internal i64
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let session_id_str = session_id.to_string();

        self.run_blocking(move |conn| {
            // 1. Fetch Messages with public_id and internal parent_message_id
            let mut stmt = conn.prepare(
                "SELECT id, public_id, role, created_at, parent_message_id FROM messages WHERE session_id = ? ORDER BY created_at ASC"
            )?;

            // Build a map: internal_id → public_id for parent resolution
            let mut id_map: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
            let messages_data: Vec<(i64, String, String, i64, Option<i64>)> = stmt
                .query_map(params![session_internal_id], |row| {
                    let internal_id: i64 = row.get(0)?;
                    let public_id: String = row.get(1)?;
                    let role_str: String = row.get(2)?;
                    let created_at: i64 = row.get(3)?;
                    let parent_internal_id: Option<i64> = row.get(4)?;
                    Ok((internal_id, public_id, role_str, created_at, parent_internal_id))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            // Build id_map
            for (internal_id, public_id, _, _, _) in &messages_data {
                id_map.insert(*internal_id, public_id.clone());
            }

            // Convert to AgentMessage, resolving parent_message_id
            let mut messages: Vec<AgentMessage> = messages_data
                .into_iter()
                .map(|(_internal_id, public_id, role_str, created_at, parent_internal_id)| {
                    let role = match role_str.as_str() {
                        "User" => ChatRole::User,
                        "Assistant" => ChatRole::Assistant,
                        _ => ChatRole::User, // Default fallback
                    };

                    let parent_message_id = parent_internal_id.and_then(|pid| id_map.get(&pid).cloned());

                    AgentMessage {
                        id: public_id.clone(),
                        session_id: session_id_str.clone(),
                        role,
                        parts: Vec::new(), // Will populate next
                        created_at,
                        parent_message_id,
                    }
                })
                .collect();

            // 2. Fetch Parts for all messages in this session (by internal message_id)
            let mut part_stmt = conn.prepare(
                "SELECT message_id, content_json FROM message_parts WHERE message_id IN (SELECT id FROM messages WHERE session_id = ?) ORDER BY sort_order ASC"
            )?;

            let parts_iter = part_stmt.query_map(params![session_internal_id], |row| {
                let message_internal_id: i64 = row.get(0)?;
                let content: String = row.get(1)?;
                let part: MessagePart = serde_json::from_str(&content).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
                })?;
                Ok((message_internal_id, part))
            })?;

            // Group parts by message internal_id, then convert to public_id
            let mut parts_map: std::collections::HashMap<String, Vec<MessagePart>> = std::collections::HashMap::new();
            for res in parts_iter {
                let (message_internal_id, part) = res?;
                if let Some(public_id) = id_map.get(&message_internal_id) {
                    parts_map.entry(public_id.clone()).or_default().push(part);
                }
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

    async fn add_message(&self, session_id: &str, message: AgentMessage) -> SessionResult<()> {
        // Resolve session public_id → internal i64
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;

        // Resolve parent message public_id → internal i64 if present
        let parent_internal_id = if let Some(ref parent_public_id) = message.parent_message_id {
            Some(self.resolve_message_internal_id(parent_public_id).await?)
        } else {
            None
        };

        let msg = message.clone();

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;

            let role_str = match msg.role {
                ChatRole::User => "User",
                ChatRole::Assistant => "Assistant",
            };

            // Insert message with public_id and internal session_id/parent_message_id
            tx.execute(
                "INSERT INTO messages (public_id, session_id, role, created_at, parent_message_id) VALUES (?, ?, ?, ?, ?)",
                params![msg.id, session_internal_id, role_str, msg.created_at, parent_internal_id],
            )?;

            // Get the internal message_id that was just inserted
            let message_internal_id: i64 = tx.last_insert_rowid();

            for (idx, part) in msg.parts.iter().enumerate() {
                let content_json = serde_json::to_string(part).map_err(|e| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
                })?;

                // Use internal message_id for FK
                tx.execute(
                    "INSERT INTO message_parts (message_id, part_type, content_json, sort_order) VALUES (?, ?, ?, ?)",
                    params![message_internal_id, part.type_name(), content_json, idx as i32],
                )?;
            }

            // Update session with internal ID
            tx.execute(
                "UPDATE sessions SET updated_at = ? WHERE id = ?",
                params![OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default(), session_internal_id],
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
        fork_origin: ForkOrigin,
    ) -> SessionResult<String> {
        // Resolve source session public_id → internal i64
        let source_session_internal_id =
            self.resolve_session_internal_id(source_session_id).await?;
        let source_session_public_id = source_session_id.to_string();

        // Resolve target message public_id → internal i64
        let target_message_internal_id =
            self.resolve_message_internal_id(target_message_id).await?;

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;

            // 1. Create New Session with UUID v7 public_id
            let new_session_public_id = Uuid::now_v7().to_string();
            let now = OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default();

            // Inherit parent config and workspace for the forked session.
            let (parent_llm_config_id, parent_cwd): (Option<i64>, Option<String>) = tx
                .query_row(
                    "SELECT llm_config_id, cwd FROM sessions WHERE id = ?",
                    params![source_session_internal_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .unwrap_or((None, None));

            tx.execute(
                "INSERT INTO sessions (public_id, name, cwd, created_at, updated_at, llm_config_id, parent_session_id, fork_origin, fork_point_type, fork_point_ref) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    new_session_public_id.clone(),
                    format!("Fork of session"), // Temporary name
                    parent_cwd,
                    now.clone(),
                    now,
                    parent_llm_config_id,
                    source_session_internal_id,
                    fork_origin.to_string(),
                    ForkPointType::MessageIndex.to_string(),
                    target_message_internal_id,
                ],
            )?;

            // Get new session internal ID
            let new_session_internal_id: i64 = tx.last_insert_rowid();

            // 2. Identify messages to copy (up to target_message_id internal ID).
            // Use deterministic internal-id ordering so same-second timestamps don't truncate
            // assistant turns unpredictably.
            let messages_to_copy = {
                let mut stmt = tx.prepare(
                    "SELECT id, public_id, role, created_at FROM messages WHERE session_id = ? ORDER BY id ASC"
                )?;

                let messages: Vec<(i64, String, String, i64)> = stmt.query_map(params![source_session_internal_id], |row| {
                    Ok((
                        row.get(0)?, // internal id
                        row.get(1)?, // public_id
                        row.get(2)?, // role
                        row.get(3)?  // created_at
                    ))
                })?.collect::<Result<Vec<_>, _>>()?;

                let mut to_copy = Vec::new();
                for m in messages {
                    let msg_internal_id = m.0;
                    to_copy.push(m);
                    if msg_internal_id == target_message_internal_id {
                        break;
                    }
                }
                to_copy
            };

            let copied_message_ids: std::collections::HashSet<String> = messages_to_copy
                .iter()
                .map(|(_, public_id, _, _)| public_id.clone())
                .collect();

            let conversational_events_to_copy: Vec<(String, i64, String, Option<String>, String, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT event_id, timestamp, origin, source_node, kind, payload_json \
                     FROM event_journal \
                     WHERE session_id = ? \
                     ORDER BY stream_seq ASC",
                )?;

                let rows: Vec<(String, i64, String, Option<String>, String, String)> = stmt
                    .query_map(params![source_session_public_id], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                rows.into_iter()
                    .filter(|(_, _, _, _, kind, payload_json)| {
                        if kind == "prompt_received" {
                            let parsed: serde_json::Value = match serde_json::from_str(payload_json) {
                                Ok(value) => value,
                                Err(_) => return false,
                            };
                            let message_id = parsed.get("message_id").and_then(|id| id.as_str());
                            return message_id
                                .map(|id| copied_message_ids.contains(id))
                                .unwrap_or(false);
                        }

                        if kind == "assistant_message_stored" {
                            let parsed: serde_json::Value = match serde_json::from_str(payload_json) {
                                Ok(value) => value,
                                Err(_) => return false,
                            };
                            let message_id = parsed.get("message_id").and_then(|id| id.as_str());
                            return message_id
                                .map(|id| copied_message_ids.contains(id))
                                .unwrap_or(false);
                        }

                        false
                    })
                    .collect()
            };

            // 3. Copy messages and their parts with new UUID v7 public_ids
            for (old_internal_id, _old_public_id, role, created_at) in messages_to_copy {
                let new_msg_public_id = Uuid::now_v7().to_string();

                // Insert Message with new public_id and internal session_id
                tx.execute(
                    "INSERT INTO messages (public_id, session_id, role, created_at, parent_message_id) VALUES (?, ?, ?, ?, ?)",
                    params![new_msg_public_id, new_session_internal_id, role, created_at, Option::<i64>::None],
                )?;

                // Get new message internal ID
                let new_msg_internal_id: i64 = tx.last_insert_rowid();

                // Copy Parts using internal message_id
                {
                    let mut part_stmt = tx.prepare(
                        "SELECT part_type, content_json, sort_order FROM message_parts WHERE message_id = ?"
                    )?;

                    let parts: Vec<(String, String, i32)> = part_stmt.query_map(params![old_internal_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?.collect::<Result<Vec<_>, _>>()?;

                    for (ptype, content, order) in parts {
                        tx.execute(
                            "INSERT INTO message_parts (message_id, part_type, content_json, sort_order) VALUES (?, ?, ?, ?)",
                            params![new_msg_internal_id, ptype, content, order],
                        )?;
                    }
                }
            }

            // Copy only conversational durable events into the fork timeline.
            // Risk/tradeoff: operational events (tool calls, llm lifecycle, provider/task/progress/
            // delegation telemetry) are intentionally excluded because replaying them in a fork can
            // create contradictory state and duplicate execution traces. The downside is inherited
            // UI telemetry before the fork point is intentionally partial.
            for (_event_id, timestamp, origin, source_node, kind, payload_json) in conversational_events_to_copy {
                let new_event_id = Uuid::now_v7().to_string();
                let stream_seq: u64 = tx.query_row(
                    "UPDATE event_journal_seq SET next_seq = next_seq + 1 WHERE id = 1 RETURNING next_seq - 1",
                    [],
                    |row| row.get(0),
                )?;

                tx.execute(
                    "INSERT INTO event_journal (event_id, stream_seq, session_id, timestamp, origin, source_node, kind, payload_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        new_event_id,
                        stream_seq,
                        new_session_public_id,
                        timestamp,
                        origin,
                        source_node,
                        kind,
                        payload_json,
                    ],
                )?;
            }

            tx.commit()?;
            Ok(new_session_public_id)
        })
        .await
    }

    async fn create_or_get_llm_config(&self, input: &LLMParams) -> SessionResult<LLMConfig> {
        let (provider, model, params) = extract_llm_config_values(input)?;
        let name = input.name.clone();

        let params_str = if let Some(ref p) = params {
            serde_json::to_string(p)?
        } else {
            serde_json::to_string(&serde_json::Value::Object(serde_json::Map::new()))?
        };

        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, provider, model, params, created_at, updated_at FROM llm_configs WHERE provider = ? AND model = ? AND params = ?",
            )?;
            if let Some(config) = stmt
                .query_row(params![provider, model, params_str], parse_llm_config_row)
                .optional()?
            {
                return Ok(config);
            }

            let now = OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();

            // Insert without explicit id to let INTEGER PRIMARY KEY autoincrement
            conn.execute(
                "INSERT INTO llm_configs (name, provider, model, params, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    name,
                    provider,
                    model,
                    params_str,
                    now.clone(),
                    now.clone(),
                ],
            )?;

            // Get the autoincremented id
            let id: i64 = conn.last_insert_rowid();

            Ok(LLMConfig {
                id,
                name,
                provider,
                model,
                params: parse_llm_params(&params_str)?,
                created_at: OffsetDateTime::parse(
                    &now,
                    &time::format_description::well_known::Rfc3339,
                )
                .ok(),
                updated_at: OffsetDateTime::parse(
                    &now,
                    &time::format_description::well_known::Rfc3339,
                )
                .ok(),
                provider_node_id: None,
            })
        })
        .await
    }

    async fn get_llm_config(&self, id: i64) -> SessionResult<Option<LLMConfig>> {
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id, name, provider, model, params, created_at, updated_at FROM llm_configs WHERE id = ?",
                params![id],
                parse_llm_config_row,
            )
            .optional()
        })
        .await
    }

    async fn get_session_llm_config(&self, session_id: &str) -> SessionResult<Option<LLMConfig>> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT c.id, c.name, c.provider, c.model, c.params, c.created_at, c.updated_at FROM llm_configs c INNER JOIN sessions s ON s.llm_config_id = c.id WHERE s.id = ?",
                params![session_internal_id],
                parse_llm_config_row,
            )
            .optional()
        })
        .await
    }

    async fn set_session_llm_config(&self, session_id: &str, config_id: i64) -> SessionResult<()> {
        // Resolve session public_id → internal i64
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;

        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE sessions SET llm_config_id = ?, updated_at = ? WHERE id = ?",
                params![
                    config_id,
                    OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                    session_internal_id
                ],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            SessionError::DatabaseError(_) => SessionError::SessionNotFound(session_id.to_string()),
            _ => e,
        })
    }

    async fn set_session_provider_node_id(
        &self,
        session_id: &str,
        provider_node_id: Option<&str>,
    ) -> SessionResult<()> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let provider_node_id_owned = provider_node_id.map(|s| s.to_string());
        self.run_blocking(move |conn| {
            conn.execute(
                "UPDATE sessions SET provider_node_id = ?, updated_at = ? WHERE id = ?",
                params![
                    provider_node_id_owned,
                    OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                    session_internal_id
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_session_provider_node_id(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<String>> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        self.run_blocking(move |conn| {
            let result: rusqlite::Result<Option<String>> = conn.query_row(
                "SELECT provider_node_id FROM sessions WHERE id = ?",
                params![session_internal_id],
                |row| row.get(0),
            );
            match result {
                Ok(val) => Ok(val),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .await
    }

    async fn set_session_execution_config(
        &self,
        session_id: &str,
        config: &SessionExecutionConfig,
    ) -> SessionResult<()> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let config_json = serde_json::to_string(config).map_err(|e| {
            SessionError::InvalidOperation(format!(
                "Failed to serialize session execution config: {}",
                e
            ))
        })?;
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();

        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO session_execution_configs (session_id, config_json, created_at, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET config_json = excluded.config_json, updated_at = excluded.updated_at",
                params![session_internal_id, config_json, now, now],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_session_execution_config(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<SessionExecutionConfig>> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        self.run_blocking(move |conn| {
            let config_json: Option<String> = conn
                .query_row(
                    "SELECT config_json FROM session_execution_configs WHERE session_id = ?",
                    params![session_internal_id],
                    |row| row.get(0),
                )
                .optional()?;

            match config_json {
                Some(raw) => {
                    let config: SessionExecutionConfig =
                        serde_json::from_str(&raw).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?;
                    Ok(Some(config))
                }
                None => Ok(None),
            }
        })
        .await
    }

    async fn list_custom_models(&self, provider: &str) -> SessionResult<Vec<CustomModel>> {
        let provider = provider.to_string();
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT provider, model_id, display_name, config_json, source_type, source_ref, family, quant, created_at, updated_at FROM custom_models WHERE provider = ? ORDER BY updated_at DESC",
            )?;
            let rows = stmt.query_map(params![provider], |row| {
                let config_json: String = row.get(3)?;
                let parsed_json = serde_json::from_str(&config_json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                let created_at: Option<OffsetDateTime> = row
                    .get::<_, String>(8)
                    .ok()
                    .and_then(|s| OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok());
                let updated_at: Option<OffsetDateTime> = row
                    .get::<_, String>(9)
                    .ok()
                    .and_then(|s| OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok());
                Ok(CustomModel {
                    provider: row.get(0)?,
                    model_id: row.get(1)?,
                    display_name: row.get(2)?,
                    config_json: parsed_json,
                    source_type: row.get(4)?,
                    source_ref: row.get(5)?,
                    family: row.get(6)?,
                    quant: row.get(7)?,
                    created_at,
                    updated_at,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn get_custom_model(
        &self,
        provider: &str,
        model_id: &str,
    ) -> SessionResult<Option<CustomModel>> {
        let provider = provider.to_string();
        let model_id = model_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT provider, model_id, display_name, config_json, source_type, source_ref, family, quant, created_at, updated_at FROM custom_models WHERE provider = ? AND model_id = ?",
                params![provider, model_id],
                |row| {
                    let config_json: String = row.get(3)?;
                    let parsed_json = serde_json::from_str(&config_json).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    let created_at: Option<OffsetDateTime> = row
                        .get::<_, String>(8)
                        .ok()
                        .and_then(|s| OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok());
                    let updated_at: Option<OffsetDateTime> = row
                        .get::<_, String>(9)
                        .ok()
                        .and_then(|s| OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok());
                    Ok(CustomModel {
                        provider: row.get(0)?,
                        model_id: row.get(1)?,
                        display_name: row.get(2)?,
                        config_json: parsed_json,
                        source_type: row.get(4)?,
                        source_ref: row.get(5)?,
                        family: row.get(6)?,
                        quant: row.get(7)?,
                        created_at,
                        updated_at,
                    })
                },
            )
            .optional()
        })
        .await
    }

    async fn upsert_custom_model(&self, model: &CustomModel) -> SessionResult<()> {
        let model = model.clone();
        self.run_blocking(move |conn| {
            let now = OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();
            let created_at = model
                .created_at
                .and_then(|ts| ts.format(&time::format_description::well_known::Rfc3339).ok())
                .unwrap_or_else(|| now.clone());
            let config_json = serde_json::to_string(&model.config_json)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            conn.execute(
                "INSERT INTO custom_models (provider, model_id, display_name, config_json, source_type, source_ref, family, quant, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(provider, model_id) DO UPDATE SET display_name = excluded.display_name, config_json = excluded.config_json, source_type = excluded.source_type, source_ref = excluded.source_ref, family = excluded.family, quant = excluded.quant, updated_at = excluded.updated_at",
                params![
                    model.provider,
                    model.model_id,
                    model.display_name,
                    config_json,
                    model.source_type,
                    model.source_ref,
                    model.family,
                    model.quant,
                    created_at,
                    now,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_custom_model(&self, provider: &str, model_id: &str) -> SessionResult<()> {
        let provider = provider.to_string();
        let model_id = model_id.to_string();
        self.run_blocking(move |conn| {
            conn.execute(
                "DELETE FROM custom_models WHERE provider = ? AND model_id = ?",
                params![provider, model_id],
            )?;
            Ok(())
        })
        .await
    }

    // Phase 3: Delegate to repository implementations
    async fn set_current_intent_snapshot(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
    ) -> SessionResult<()> {
        let repo = SqliteSessionRepository::new(self.conn.clone());
        repo.set_current_intent_snapshot(session_id, snapshot_id)
            .await
    }

    async fn set_active_task(&self, session_id: &str, task_id: Option<&str>) -> SessionResult<()> {
        let repo = SqliteSessionRepository::new(self.conn.clone());
        repo.set_active_task(session_id, task_id).await
    }

    async fn get_session_fork_info(&self, session_id: &str) -> SessionResult<Option<ForkInfo>> {
        let repo = SqliteSessionRepository::new(self.conn.clone());
        repo.get_session_fork_info(session_id).await
    }

    async fn list_child_sessions(&self, parent_id: &str) -> SessionResult<Vec<String>> {
        let repo = SqliteSessionRepository::new(self.conn.clone());
        repo.list_child_sessions(parent_id).await
    }

    // Task repository methods
    async fn create_task(&self, task: Task) -> SessionResult<Task> {
        let repo = SqliteTaskRepository::new(self.conn.clone());
        repo.create_task(task).await
    }

    async fn get_task(&self, task_id: &str) -> SessionResult<Option<Task>> {
        let repo = SqliteTaskRepository::new(self.conn.clone());
        repo.get_task(task_id).await
    }

    async fn list_tasks(&self, session_id: &str) -> SessionResult<Vec<Task>> {
        let repo = SqliteTaskRepository::new(self.conn.clone());
        repo.list_tasks(session_id).await
    }

    async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> SessionResult<()> {
        let repo = SqliteTaskRepository::new(self.conn.clone());
        repo.update_task_status(task_id, status).await
    }

    async fn update_task(&self, task: Task) -> SessionResult<()> {
        let repo = SqliteTaskRepository::new(self.conn.clone());
        repo.update_task(task).await
    }

    async fn delete_task(&self, task_id: &str) -> SessionResult<()> {
        let repo = SqliteTaskRepository::new(self.conn.clone());
        repo.delete_task(task_id).await
    }

    // Intent repository methods
    async fn create_intent_snapshot(&self, snapshot: IntentSnapshot) -> SessionResult<()> {
        let repo = SqliteIntentRepository::new(self.conn.clone());
        repo.create_intent_snapshot(snapshot).await
    }

    async fn get_intent_snapshot(
        &self,
        snapshot_id: &str,
    ) -> SessionResult<Option<IntentSnapshot>> {
        let repo = SqliteIntentRepository::new(self.conn.clone());
        repo.get_intent_snapshot(snapshot_id).await
    }

    async fn list_intent_snapshots(&self, session_id: &str) -> SessionResult<Vec<IntentSnapshot>> {
        let repo = SqliteIntentRepository::new(self.conn.clone());
        repo.list_intent_snapshots(session_id).await
    }

    async fn get_current_intent_snapshot(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<IntentSnapshot>> {
        let repo = SqliteIntentRepository::new(self.conn.clone());
        repo.get_current_intent_snapshot(session_id).await
    }

    // Decision repository methods
    async fn record_decision(&self, decision: Decision) -> SessionResult<()> {
        let repo = SqliteDecisionRepository::new(self.conn.clone());
        repo.record_decision(decision).await
    }

    async fn record_alternative(&self, alternative: Alternative) -> SessionResult<()> {
        let repo = SqliteDecisionRepository::new(self.conn.clone());
        repo.record_alternative(alternative).await
    }

    async fn get_decision(&self, decision_id: &str) -> SessionResult<Option<Decision>> {
        let repo = SqliteDecisionRepository::new(self.conn.clone());
        repo.get_decision(decision_id).await
    }

    async fn list_decisions(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Decision>> {
        let repo = SqliteDecisionRepository::new(self.conn.clone());
        repo.list_decisions(session_id, task_id).await
    }

    async fn list_alternatives(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Alternative>> {
        let repo = SqliteDecisionRepository::new(self.conn.clone());
        repo.list_alternatives(session_id, task_id).await
    }

    async fn update_decision_status(
        &self,
        decision_id: &str,
        status: DecisionStatus,
    ) -> SessionResult<()> {
        let repo = SqliteDecisionRepository::new(self.conn.clone());
        repo.update_decision_status(decision_id, status).await
    }

    async fn update_alternative_status(
        &self,
        alternative_id: &str,
        status: AlternativeStatus,
    ) -> SessionResult<()> {
        let repo = SqliteDecisionRepository::new(self.conn.clone());
        repo.update_alternative_status(alternative_id, status).await
    }

    // Progress repository methods
    async fn append_progress_entry(&self, entry: ProgressEntry) -> SessionResult<()> {
        let repo = SqliteProgressRepository::new(self.conn.clone());
        repo.append_progress_entry(entry).await
    }

    async fn get_progress_entry(&self, entry_id: &str) -> SessionResult<Option<ProgressEntry>> {
        let repo = SqliteProgressRepository::new(self.conn.clone());
        repo.get_progress_entry(entry_id).await
    }

    async fn list_progress_entries(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<ProgressEntry>> {
        let repo = SqliteProgressRepository::new(self.conn.clone());
        repo.list_progress_entries(session_id, task_id).await
    }

    async fn list_progress_by_kind(
        &self,
        session_id: &str,
        kind: ProgressKind,
    ) -> SessionResult<Vec<ProgressEntry>> {
        let repo = SqliteProgressRepository::new(self.conn.clone());
        repo.list_progress_by_kind(session_id, kind).await
    }

    // Artifact repository methods
    async fn record_artifact(&self, artifact: Artifact) -> SessionResult<()> {
        let repo = SqliteArtifactRepository::new(self.conn.clone());
        repo.record_artifact(artifact).await
    }

    async fn get_artifact(&self, artifact_id: &str) -> SessionResult<Option<Artifact>> {
        let repo = SqliteArtifactRepository::new(self.conn.clone());
        repo.get_artifact(artifact_id).await
    }

    async fn list_artifacts(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Artifact>> {
        let repo = SqliteArtifactRepository::new(self.conn.clone());
        repo.list_artifacts(session_id, task_id).await
    }

    async fn list_artifacts_by_kind(
        &self,
        session_id: &str,
        kind: &str,
    ) -> SessionResult<Vec<Artifact>> {
        let repo = SqliteArtifactRepository::new(self.conn.clone());
        repo.list_artifacts_by_kind(session_id, kind).await
    }

    // Delegation repository methods
    async fn create_delegation(&self, delegation: Delegation) -> SessionResult<Delegation> {
        let repo = SqliteDelegationRepository::new(self.conn.clone());
        repo.create_delegation(delegation).await
    }

    async fn get_delegation(&self, delegation_id: &str) -> SessionResult<Option<Delegation>> {
        let repo = SqliteDelegationRepository::new(self.conn.clone());
        repo.get_delegation(delegation_id).await
    }

    async fn list_delegations(&self, session_id: &str) -> SessionResult<Vec<Delegation>> {
        let repo = SqliteDelegationRepository::new(self.conn.clone());
        repo.list_delegations(session_id).await
    }

    async fn update_delegation_status(
        &self,
        delegation_id: &str,
        status: DelegationStatus,
    ) -> SessionResult<()> {
        let repo = SqliteDelegationRepository::new(self.conn.clone());
        repo.update_delegation_status(delegation_id, status).await
    }

    async fn update_delegation(&self, delegation: Delegation) -> SessionResult<()> {
        let repo = SqliteDelegationRepository::new(self.conn.clone());
        repo.update_delegation(delegation).await
    }

    async fn peek_revert_state(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<crate::session::domain::RevertState>> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let session_id_str = session_id.to_string();

        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT public_id, message_id, snapshot_id, backend_id, created_at FROM revert_states WHERE session_id = ? ORDER BY id DESC LIMIT 1",
                params![session_internal_id],
                |row| {
                    let public_id: String = row.get(0)?;
                    let message_id: String = row.get(1)?;
                    let snapshot_id: String = row.get(2)?;
                    let backend_id: String = row.get(3)?;
                    let created_at_str: String = row.get(4)?;
                    let created_at = OffsetDateTime::parse(
                        &created_at_str,
                        &time::format_description::well_known::Rfc3339,
                    )
                    .unwrap_or_else(|_| OffsetDateTime::now_utc());

                    Ok(crate::session::domain::RevertState {
                        public_id,
                        session_id: session_id_str.clone(),
                        message_id,
                        snapshot_id,
                        backend_id,
                        created_at,
                    })
                },
            )
            .optional()
        })
        .await
    }

    async fn push_revert_state(
        &self,
        session_id: &str,
        state: crate::session::domain::RevertState,
    ) -> SessionResult<()> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;

        self.run_blocking(move |conn| {
            let now = OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();
            conn.execute(
                "INSERT INTO revert_states (public_id, session_id, message_id, snapshot_id, backend_id, created_at) VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    state.public_id,
                    session_internal_id,
                    state.message_id,
                    state.snapshot_id,
                    state.backend_id,
                    now,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn pop_revert_state(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<crate::session::domain::RevertState>> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let session_id_str = session_id.to_string();

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;
            let state = tx
                .query_row(
                    "SELECT id, public_id, message_id, snapshot_id, backend_id, created_at FROM revert_states WHERE session_id = ? ORDER BY id DESC LIMIT 1",
                    params![session_internal_id],
                    |row| {
                        let id: i64 = row.get(0)?;
                        let public_id: String = row.get(1)?;
                        let message_id: String = row.get(2)?;
                        let snapshot_id: String = row.get(3)?;
                        let backend_id: String = row.get(4)?;
                        let created_at_str: String = row.get(5)?;
                        let created_at = OffsetDateTime::parse(
                            &created_at_str,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .unwrap_or_else(|_| OffsetDateTime::now_utc());

                        Ok((
                            id,
                            crate::session::domain::RevertState {
                                public_id,
                                session_id: session_id_str.clone(),
                                message_id,
                                snapshot_id,
                                backend_id,
                                created_at,
                            },
                        ))
                    },
                )
                .optional()?;

            if let Some((id, revert_state)) = state {
                tx.execute("DELETE FROM revert_states WHERE id = ?", params![id])?;
                tx.commit()?;
                Ok(Some(revert_state))
            } else {
                tx.commit()?;
                Ok(None)
            }
        })
        .await
    }

    async fn list_revert_states(
        &self,
        session_id: &str,
    ) -> SessionResult<Vec<crate::session::domain::RevertState>> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let session_id_str = session_id.to_string();

        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT public_id, message_id, snapshot_id, backend_id, created_at FROM revert_states WHERE session_id = ? ORDER BY id ASC",
            )?;
            let rows = stmt.query_map(params![session_internal_id], |row| {
                let public_id: String = row.get(0)?;
                let message_id: String = row.get(1)?;
                let snapshot_id: String = row.get(2)?;
                let backend_id: String = row.get(3)?;
                let created_at_str: String = row.get(4)?;
                let created_at = OffsetDateTime::parse(
                    &created_at_str,
                    &time::format_description::well_known::Rfc3339,
                )
                .unwrap_or_else(|_| OffsetDateTime::now_utc());

                Ok(crate::session::domain::RevertState {
                    public_id,
                    session_id: session_id_str.clone(),
                    message_id,
                    snapshot_id,
                    backend_id,
                    created_at,
                })
            })?;

            rows.collect::<Result<Vec<_>, rusqlite::Error>>()
        })
        .await
    }

    async fn clear_revert_states(&self, session_id: &str) -> SessionResult<()> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;

        self.run_blocking(move |conn| {
            conn.execute(
                "DELETE FROM revert_states WHERE session_id = ?",
                params![session_internal_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_messages_after(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> SessionResult<usize> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let message_internal_id = self.resolve_message_internal_id(message_id).await?;

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;

            // Get the created_at timestamp of the target message
            let target_created_at: i64 = tx.query_row(
                "SELECT created_at FROM messages WHERE id = ?",
                params![message_internal_id],
                |row| row.get(0),
            )?;

            // Delete message_parts for messages after the target
            tx.execute(
                "DELETE FROM message_parts WHERE message_id IN (
                    SELECT id FROM messages WHERE session_id = ? AND created_at > ?
                )",
                params![session_internal_id, target_created_at],
            )?;

            // Delete messages after the target
            let deleted: usize = tx.execute(
                "DELETE FROM messages WHERE session_id = ? AND created_at > ?",
                params![session_internal_id, target_created_at],
            )?;

            tx.commit()?;
            Ok(deleted)
        })
        .await
    }

    async fn mark_tool_results_compacted(
        &self,
        session_id: &str,
        call_ids: &[String],
    ) -> SessionResult<usize> {
        if call_ids.is_empty() {
            return Ok(0);
        }

        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let call_ids_owned: Vec<String> = call_ids.to_vec();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;
            let mut total_updated = 0;

            // Get all message_parts for this session that are ToolResult type
            // Collect into Vec first to avoid borrowing issues with stmt/tx
            let parts_to_update: Vec<(i64, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT mp.id, mp.content_json 
                     FROM message_parts mp 
                     INNER JOIN messages m ON mp.message_id = m.id 
                     WHERE m.session_id = ? AND mp.part_type = 'tool_result'",
                )?;

                stmt.query_map(params![session_internal_id], |row| {
                    let part_id: i64 = row.get(0)?;
                    let content_json: String = row.get(1)?;
                    Ok((part_id, content_json))
                })?
                .collect::<Result<Vec<_>, _>>()?
            };

            // Update each matching part
            for (part_id, content_json) in parts_to_update {
                // Parse the JSON to check if this is a matching call_id
                let mut part: serde_json::Value = serde_json::from_str(&content_json)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

                // Check if the call_id matches one we want to compact
                let call_id = part
                    .get("data")
                    .and_then(|d| d.get("call_id"))
                    .and_then(|v| v.as_str());

                if let Some(cid) = call_id
                    && call_ids_owned.contains(&cid.to_string())
                {
                    // Check if not already compacted
                    let already_compacted = part
                        .get("data")
                        .and_then(|d| d.get("compacted_at"))
                        .map(|v| !v.is_null())
                        .unwrap_or(false);

                    if !already_compacted {
                        // Update the compacted_at field
                        if let Some(data) = part.get_mut("data")
                            && let Some(obj) = data.as_object_mut()
                        {
                            obj.insert("compacted_at".to_string(), serde_json::json!(now));
                        }

                        let updated_json = serde_json::to_string(&part)
                            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

                        tx.execute(
                            "UPDATE message_parts SET content_json = ? WHERE id = ?",
                            params![updated_json, part_id],
                        )?;
                        total_updated += 1;
                    }
                }
            }

            tx.commit()?;
            Ok(total_updated)
        })
        .await
    }
}

// ============================================================================
// ViewStore implementation
// ============================================================================

#[async_trait]
impl ViewStore for SqliteStorage {
    async fn get_audit_view(
        &self,
        session_id: &str,
        include_children: bool,
    ) -> SessionResult<AuditView> {
        let mut events: Vec<AgentEvent> = self
            .load_session_stream(session_id, None, None)
            .await?
            .into_iter()
            .map(AgentEvent::from)
            .collect();

        // Include child session events (delegations) if requested
        if include_children {
            let session_repo = SqliteSessionRepository::new(self.conn.clone());
            let child_session_ids = session_repo.list_child_sessions(session_id).await?;
            for child_id in &child_session_ids {
                let child_events: Vec<AgentEvent> = self
                    .load_session_stream(child_id, None, None)
                    .await?
                    .into_iter()
                    .map(AgentEvent::from)
                    .collect();
                events.extend(child_events);
            }
        }

        // Sort by sequence number for correct chronological order
        events.sort_by_key(|e| e.seq);

        let task_repo = SqliteTaskRepository::new(self.conn.clone());
        let intent_repo = SqliteIntentRepository::new(self.conn.clone());
        let decision_repo = SqliteDecisionRepository::new(self.conn.clone());
        let progress_repo = SqliteProgressRepository::new(self.conn.clone());
        let artifact_repo = SqliteArtifactRepository::new(self.conn.clone());
        let delegation_repo = SqliteDelegationRepository::new(self.conn.clone());

        let tasks = task_repo.list_tasks(session_id).await?;
        let intent_snapshots = intent_repo.list_intent_snapshots(session_id).await?;
        let decisions = decision_repo.list_decisions(session_id, None).await?;
        let progress_entries = progress_repo
            .list_progress_entries(session_id, None)
            .await?;
        let artifacts = artifact_repo.list_artifacts(session_id, None).await?;
        let delegations = delegation_repo.list_delegations(session_id).await?;

        Ok(AuditView {
            session_id: session_id.to_string(),
            events,
            tasks,
            intent_snapshots,
            decisions,
            progress_entries,
            artifacts,
            delegations,
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    async fn get_redacted_view(
        &self,
        session_id: &str,
        policy: RedactionPolicy,
    ) -> SessionResult<RedactedView> {
        let redactor = DefaultRedactor;

        let intent_repo = SqliteIntentRepository::new(self.conn.clone());
        let task_repo = SqliteTaskRepository::new(self.conn.clone());
        let progress_repo = SqliteProgressRepository::new(self.conn.clone());
        let artifact_repo = SqliteArtifactRepository::new(self.conn.clone());

        // Get current intent
        let intent_snapshots = intent_repo.list_intent_snapshots(session_id).await?;
        let current_intent = intent_snapshots
            .last()
            .map(|s| redactor.redact(&s.summary, policy));

        // Get active task
        let tasks = task_repo.list_tasks(session_id).await?;
        let active_task = tasks
            .iter()
            .find(|t| matches!(t.status, TaskStatus::Active))
            .map(|t| RedactedTask {
                id: t.public_id.clone(),
                status: format!("{:?}", t.status),
                expected_deliverable: t
                    .expected_deliverable
                    .as_ref()
                    .map(|d| redactor.redact(d, policy)),
            });

        // Get recent progress (last 10 entries)
        let all_progress = progress_repo
            .list_progress_entries(session_id, None)
            .await?;
        let recent_progress: Vec<RedactedProgress> = all_progress
            .iter()
            .rev()
            .take(10)
            .map(|p| RedactedProgress {
                kind: format!("{:?}", p.kind),
                summary: redactor.redact(&p.content, policy),
                created_at: p.created_at,
            })
            .collect();

        // Get artifacts
        let all_artifacts = artifact_repo.list_artifacts(session_id, None).await?;
        let artifacts: Vec<RedactedArtifact> = all_artifacts
            .iter()
            .map(|a| RedactedArtifact {
                kind: a.kind.clone(),
                summary: a.summary.as_ref().map(|s| redactor.redact(s, policy)),
                created_at: a.created_at,
            })
            .collect();

        Ok(RedactedView {
            session_id: session_id.to_string(),
            current_intent,
            active_task,
            recent_progress,
            artifacts,
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    async fn get_summary_view(&self, session_id: &str) -> SessionResult<SummaryView> {
        let intent_repo = SqliteIntentRepository::new(self.conn.clone());
        let task_repo = SqliteTaskRepository::new(self.conn.clone());
        let decision_repo = SqliteDecisionRepository::new(self.conn.clone());
        let progress_repo = SqliteProgressRepository::new(self.conn.clone());
        let artifact_repo = SqliteArtifactRepository::new(self.conn.clone());

        // Get current intent
        let intent_snapshots = intent_repo.list_intent_snapshots(session_id).await?;
        let current_intent = intent_snapshots.last().map(|s| s.summary.clone());

        // Get active task status
        let tasks = task_repo.list_tasks(session_id).await?;
        let active_task_status = tasks
            .iter()
            .find(|t| matches!(t.status, TaskStatus::Active))
            .map(|t| format!("{:?}", t.status));

        // Count entities
        let decisions = decision_repo.list_decisions(session_id, None).await?;
        let progress_entries = progress_repo
            .list_progress_entries(session_id, None)
            .await?;
        let artifacts = artifact_repo.list_artifacts(session_id, None).await?;

        // Get last activity
        let last_activity = progress_entries.last().map(|p| {
            format!(
                "{:?}: {}",
                p.kind,
                p.content.chars().take(50).collect::<String>()
            )
        });

        Ok(SummaryView {
            session_id: session_id.to_string(),
            current_intent,
            active_task_status,
            progress_count: progress_entries.len(),
            artifact_count: artifacts.len(),
            decision_count: decisions.len(),
            last_activity,
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    #[tracing::instrument(
        name = "session.get_session_list_view",
        skip(self, filter),
        fields(
            session_count = tracing::field::Empty,
            filtered_out_count = tracing::field::Empty,
            total_count = tracing::field::Empty,
            group_count = tracing::field::Empty,
            title_lookup_count = tracing::field::Empty,
            total_ms = tracing::field::Empty,
            list_sessions_ms = tracing::field::Empty,
            filter_ms = tracing::field::Empty,
            title_lookup_ms = tracing::field::Empty,
            hierarchy_build_ms = tracing::field::Empty,
            group_build_ms = tracing::field::Empty
        )
    )]
    async fn get_session_list_view(
        &self,
        filter: Option<SessionListFilter>,
    ) -> SessionResult<SessionListView> {
        use std::collections::{HashMap, HashSet};
        use std::time::Instant;

        let started = Instant::now();
        let session_repo = SqliteSessionRepository::new(self.conn.clone());
        let intent_repo = SqliteIntentRepository::new(self.conn.clone());

        // Get all sessions (list_sessions already returns sorted by updated_at DESC)
        let list_sessions_started = Instant::now();
        let mut sessions = session_repo.list_sessions().await?;
        let list_sessions_ms = list_sessions_started.elapsed().as_millis() as u64;
        let session_count_before_filter = sessions.len();

        let filter_started = Instant::now();
        // Apply filters if provided
        if let Some(filter_spec) = filter {
            if let Some(filter_expr) = filter_spec.filter {
                sessions.retain(|s| evaluate_session_filter(s, &filter_expr));
            }

            // Apply limit if specified
            if let Some(limit) = filter_spec.limit {
                sessions.truncate(limit);
            }
        }
        let filter_ms = filter_started.elapsed().as_millis() as u64;

        let total_count = sessions.len();

        // Build a map of internal ID -> public ID for parent resolution
        let mut id_to_public_id: HashMap<i64, String> = HashMap::new();
        for session in &sessions {
            id_to_public_id.insert(session.id, session.public_id.clone());
        }

        // Build a set of sessions that have children
        let mut sessions_with_children: HashSet<String> = HashSet::new();
        for session in &sessions {
            if let Some(parent_id) = session.parent_session_id
                && let Some(parent_public_id) = id_to_public_id.get(&parent_id)
            {
                sessions_with_children.insert(parent_public_id.clone());
            }
        }

        // Build session list items with titles and hierarchy info
        let title_lookup_started = Instant::now();
        let mut items = Vec::with_capacity(sessions.len());

        for session in sessions {
            // Get latest intent snapshot for title
            // Intent snapshots now contain clean user text (no attachments)
            let title = intent_repo
                .get_current_intent_snapshot(&session.public_id)
                .await
                .ok()
                .flatten()
                .map(|intent| {
                    // Truncate to reasonable display length (80 chars)
                    if intent.summary.len() > 80 {
                        format!("{}...", &intent.summary[..77])
                    } else {
                        intent.summary
                    }
                });

            // Resolve parent_session_id from internal ID to public ID
            let parent_session_id = session
                .parent_session_id
                .and_then(|parent_id| id_to_public_id.get(&parent_id).cloned());

            // Check if this session has children
            let has_children = sessions_with_children.contains(&session.public_id);

            // Convert fork_origin to string
            let fork_origin = session.fork_origin.map(|fo| fo.to_string());

            items.push(SessionListItem {
                session_id: session.public_id,
                name: session.name,
                cwd: session.cwd.map(|p| p.display().to_string()),
                title,
                created_at: session.created_at,
                updated_at: session.updated_at,
                parent_session_id,
                fork_origin,
                has_children,
            });
        }
        let title_lookup_ms = title_lookup_started.elapsed().as_millis() as u64;

        // Build a parent-child map to organize sessions hierarchically
        let hierarchy_started = Instant::now();
        let mut parent_children_map: HashMap<String, Vec<SessionListItem>> = HashMap::new();
        let mut root_sessions: Vec<SessionListItem> = Vec::new();

        for item in items {
            if let Some(ref parent_id) = item.parent_session_id {
                parent_children_map
                    .entry(parent_id.clone())
                    .or_default()
                    .push(item);
            } else {
                root_sessions.push(item);
            }
        }

        // Recursively attach children to their parents
        fn attach_children(
            session: &mut SessionListItem,
            children_map: &HashMap<String, Vec<SessionListItem>>,
        ) -> Vec<SessionListItem> {
            let mut all_sessions = vec![session.clone()];

            if let Some(children) = children_map.get(&session.session_id) {
                for mut child in children.clone() {
                    let child_descendants = attach_children(&mut child, children_map);
                    all_sessions.extend(child_descendants);
                }
            }

            all_sessions
        }

        // Flatten hierarchy while maintaining parent-child order
        // Filter out delegated child sessions to prevent empty groups
        let mut flat_items = Vec::new();
        for mut root in root_sessions {
            let sessions_with_descendants = attach_children(&mut root, &parent_children_map);
            for session in sessions_with_descendants {
                // Only include parent sessions or non-delegated children
                let is_delegated_child = session.parent_session_id.is_some()
                    && session.fork_origin.as_deref() == Some("delegation");
                if !is_delegated_child {
                    flat_items.push(session);
                }
            }
        }

        let hierarchy_build_ms = hierarchy_started.elapsed().as_millis() as u64;

        // Group by CWD
        let group_build_started = Instant::now();
        let mut groups_map: HashMap<Option<String>, Vec<SessionListItem>> = HashMap::new();
        for item in flat_items {
            groups_map.entry(item.cwd.clone()).or_default().push(item);
        }

        // Convert to SessionGroup vec and sort
        let mut groups: Vec<SessionGroup> = groups_map
            .into_iter()
            .map(|(cwd, sessions)| {
                let latest_activity = sessions.iter().filter_map(|s| s.updated_at).max();
                SessionGroup {
                    cwd,
                    sessions,
                    latest_activity,
                }
            })
            .collect();

        // Sort groups: No-CWD first, then by latest_activity desc
        groups.sort_by(|a, b| {
            match (&a.cwd, &b.cwd) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(_), Some(_)) => {
                    // Both have CWD, sort by latest activity (most recent first)
                    b.latest_activity.cmp(&a.latest_activity)
                }
            }
        });

        let group_count = groups.len();
        let group_build_ms = group_build_started.elapsed().as_millis() as u64;
        let total_ms = started.elapsed().as_millis() as u64;
        let span = tracing::Span::current();
        span.record("session_count", session_count_before_filter);
        span.record(
            "filtered_out_count",
            session_count_before_filter.saturating_sub(total_count),
        );
        span.record("total_count", total_count);
        span.record("group_count", group_count);
        span.record("title_lookup_count", total_count);
        span.record("total_ms", total_ms);
        span.record("list_sessions_ms", list_sessions_ms);
        span.record("filter_ms", filter_ms);
        span.record("title_lookup_ms", title_lookup_ms);
        span.record("hierarchy_build_ms", hierarchy_build_ms);
        span.record("group_build_ms", group_build_ms);

        Ok(SessionListView {
            groups,
            total_count,
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    async fn get_atif(
        &self,
        session_id: &str,
        options: &crate::export::AtifExportOptions,
    ) -> SessionResult<crate::export::ATIF> {
        use crate::export::ATIFBuilder;

        // Get the full audit view which contains all events and domain data
        // Include child sessions for complete trajectory export
        let audit_view = self.get_audit_view(session_id, true).await?;

        // Build the ATIF trajectory from the audit view
        // Tool definitions will be extracted from ToolsAvailable events
        let builder = ATIFBuilder::from_audit_view(&audit_view, options);
        let trajectory = builder.build();

        Ok(trajectory)
    }

    async fn get_recent_models_view(
        &self,
        limit_per_workspace: usize,
    ) -> SessionResult<RecentModelsView> {
        use std::collections::HashMap;

        let conn_arc = self.conn.clone();

        let results = tokio::task::spawn_blocking(
            move || -> Result<Vec<(Option<String>, String, String, i64, u32)>, rusqlite::Error> {
                let conn = conn_arc.lock().unwrap();

                // Query all ProviderChanged events with workspace info.
                // Uses event_journal (the legacy `events` table was dropped
                // by migration 0002).
                let mut stmt = conn.prepare(
                    r#"
                SELECT 
                    s.cwd,
                    json_extract(e.payload_json, '$.provider') as provider,
                    json_extract(e.payload_json, '$.model') as model,
                    MAX(e.timestamp) as last_used_ts,
                    COUNT(*) as use_count
                FROM event_journal e
                JOIN sessions s ON s.public_id = e.session_id
                WHERE e.kind = 'provider_changed'
                  AND provider IS NOT NULL
                  AND model IS NOT NULL
                GROUP BY s.cwd, provider, model
                ORDER BY last_used_ts DESC
                "#,
                )?;

                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, u32>(4)?,
                    ))
                })?;

                let mut results = Vec::new();
                for row in rows {
                    results.push(row?);
                }
                Ok(results)
            },
        )
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)?;

        // Group by workspace and limit per workspace
        let mut by_workspace: HashMap<Option<String>, Vec<RecentModelEntry>> = HashMap::new();

        for (cwd, provider, model, last_used_ts, use_count) in results {
            let entry = RecentModelEntry {
                provider,
                model,
                last_used: OffsetDateTime::from_unix_timestamp(last_used_ts / 1000)
                    .unwrap_or_else(|_| OffsetDateTime::now_utc()),
                use_count,
            };

            let workspace_entries = by_workspace.entry(cwd).or_default();
            if workspace_entries.len() < limit_per_workspace {
                workspace_entries.push(entry);
            }
        }

        Ok(RecentModelsView {
            by_workspace,
            generated_at: OffsetDateTime::now_utc(),
        })
    }
}

// ============================================================================
// Helper methods for SqliteStorage
// ============================================================================
// (No fallback needed - intent snapshots now contain clean user text)

/// Evaluate a filter expression against a session
fn evaluate_session_filter(session: &Session, expr: &FilterExpr) -> bool {
    match expr {
        FilterExpr::Predicate(pred) => evaluate_predicate(session, pred),
        FilterExpr::And(exprs) => exprs.iter().all(|e| evaluate_session_filter(session, e)),
        FilterExpr::Or(exprs) => exprs.iter().any(|e| evaluate_session_filter(session, e)),
        FilterExpr::Not(expr) => !evaluate_session_filter(session, expr),
    }
}

/// Evaluate a single predicate against a session
fn evaluate_predicate(
    session: &Session,
    pred: &crate::session::projection::FieldPredicate,
) -> bool {
    use serde_json::json;

    let field_value = match pred.field.as_str() {
        "session_id" | "public_id" => Some(json!(session.public_id)),
        "name" => session.name.as_ref().map(|n| json!(n)),
        "cwd" => session.cwd.as_ref().map(|p| json!(p.display().to_string())),
        "created_at" => session.created_at.map(|t| {
            json!(
                t.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            )
        }),
        "updated_at" => session.updated_at.map(|t| {
            json!(
                t.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            )
        }),
        _ => None,
    };

    match &pred.op {
        PredicateOp::IsNull => field_value.is_none(),
        PredicateOp::IsNotNull => field_value.is_some(),
        PredicateOp::Eq(val) => field_value.as_ref() == Some(val),
        PredicateOp::Ne(val) => field_value.as_ref() != Some(val),
        PredicateOp::Gt(val) => {
            // For string timestamps, compare lexicographically
            match (field_value.as_ref().and_then(|v| v.as_str()), val.as_str()) {
                (Some(fv), Some(v)) => fv > v,
                _ => false,
            }
        }
        PredicateOp::Gte(val) => {
            match (field_value.as_ref().and_then(|v| v.as_str()), val.as_str()) {
                (Some(fv), Some(v)) => fv >= v,
                _ => false,
            }
        }
        PredicateOp::Lt(val) => {
            match (field_value.as_ref().and_then(|v| v.as_str()), val.as_str()) {
                (Some(fv), Some(v)) => fv < v,
                _ => false,
            }
        }
        PredicateOp::Lte(val) => {
            match (field_value.as_ref().and_then(|v| v.as_str()), val.as_str()) {
                (Some(fv), Some(v)) => fv <= v,
                _ => false,
            }
        }
        PredicateOp::Contains(s) => {
            if let Some(fv) = field_value.as_ref().and_then(|v| v.as_str()) {
                fv.contains(s.as_str())
            } else {
                false
            }
        }
        PredicateOp::StartsWith(s) => {
            if let Some(fv) = field_value.as_ref().and_then(|v| v.as_str()) {
                fv.starts_with(s.as_str())
            } else {
                false
            }
        }
        PredicateOp::In(vals) => field_value.as_ref().is_some_and(|fv| vals.contains(fv)),
    }
}

// ============================================================================
// EventJournal — durable event persistence (new pipeline)
// ============================================================================

#[async_trait]
impl EventJournal for SqliteStorage {
    async fn append_durable(&self, event: &NewDurableEvent) -> SessionResult<DurableEvent> {
        let event_clone = event.clone();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<DurableEvent, rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();

            let kind_tag = serde_json::to_value(&event_clone.kind)
                .ok()
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_else(|| "unknown".to_string());

            let payload_json = serde_json::to_string(&event_clone.kind)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

            let origin_str = match &event_clone.origin {
                EventOrigin::Local => "local",
                EventOrigin::Remote => "remote",
                EventOrigin::Unknown(s) => s.as_str(),
            };

            let event_id = Uuid::now_v7().to_string();
            let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();

            // Atomically allocate the next stream_seq and insert the event.
            let stream_seq: u64 = conn.query_row(
                "UPDATE event_journal_seq SET next_seq = next_seq + 1 WHERE id = 1 RETURNING next_seq - 1",
                [],
                |row| row.get(0),
            )?;

            conn.execute(
                "INSERT INTO event_journal (event_id, stream_seq, session_id, timestamp, origin, source_node, kind, payload_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    event_id,
                    stream_seq,
                    event_clone.session_id,
                    timestamp,
                    origin_str,
                    event_clone.source_node,
                    kind_tag,
                    payload_json,
                ],
            )?;

            Ok(DurableEvent {
                event_id,
                stream_seq,
                session_id: event_clone.session_id,
                timestamp,
                origin: event_clone.origin,
                source_node: event_clone.source_node,
                kind: event_clone.kind,
            })
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    async fn load_session_stream(
        &self,
        session_id: &str,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> SessionResult<Vec<DurableEvent>> {
        let session_id = session_id.to_string();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<DurableEvent>, rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();
            let after = after_seq.unwrap_or(0);
            let lim = limit.unwrap_or(10_000) as i64;

            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, payload_json \
                 FROM event_journal \
                 WHERE session_id = ? AND stream_seq > ? \
                 ORDER BY stream_seq ASC \
                 LIMIT ?",
            )?;

            let events = stmt
                .query_map(params![session_id, after, lim], parse_journal_row)?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(events)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    async fn load_global_stream(
        &self,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> SessionResult<Vec<DurableEvent>> {
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<DurableEvent>, rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();
            let after = after_seq.unwrap_or(0);
            let lim = limit.unwrap_or(10_000) as i64;

            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, payload_json \
                 FROM event_journal \
                 WHERE stream_seq > ? \
                 ORDER BY stream_seq ASC \
                 LIMIT ?",
            )?;

            let events = stmt
                .query_map(params![after, lim], parse_journal_row)?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(events)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }
}

fn parse_journal_row(row: &rusqlite::Row) -> Result<DurableEvent, rusqlite::Error> {
    let event_id: String = row.get(0)?;
    let stream_seq: u64 = row.get(1)?;
    let session_id: String = row.get(2)?;
    let timestamp: i64 = row.get(3)?;
    let origin_str: String = row.get(4)?;
    let source_node: Option<String> = row.get(5)?;
    let payload_json: String = row.get(6)?;

    let origin = match origin_str.as_str() {
        "local" => EventOrigin::Local,
        "remote" => EventOrigin::Remote,
        other => EventOrigin::Unknown(other.to_string()),
    };

    let kind: AgentEventKind =
        serde_json::from_str(&payload_json).map_err(|_| rusqlite::Error::InvalidQuery)?;

    Ok(DurableEvent {
        event_id,
        stream_seq,
        session_id,
        timestamp,
        origin,
        source_node,
        kind,
    })
}

// ============================================================================
// Helper functions
// ============================================================================

fn parse_llm_params(params: &str) -> Result<Option<serde_json::Value>, rusqlite::Error> {
    if params.trim().is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_str(params).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    if value.as_object().is_none_or(|obj| obj.is_empty()) {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn parse_llm_config_row(row: &rusqlite::Row<'_>) -> Result<LLMConfig, rusqlite::Error> {
    let params_str: String = row.get(4)?;
    Ok(LLMConfig {
        id: row.get(0)?,
        name: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        params: parse_llm_params(&params_str)?,
        created_at: row.get::<_, Option<String>>(5)?.and_then(|s| {
            OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok()
        }),
        updated_at: row.get::<_, Option<String>>(6)?.and_then(|s| {
            OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok()
        }),
        provider_node_id: None,
    })
}

type MigrationFn = fn(&mut Connection) -> Result<(), rusqlite::Error>;

struct Migration {
    version: &'static str,
    apply: MigrationFn,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: "0001_initial_reset",
        apply: migration_0001_initial_reset,
    },
    Migration {
        version: "0002_drop_legacy_events",
        apply: migration_0002_drop_legacy_events,
    },
];

fn apply_migrations(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version TEXT PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );
        "#,
    )?;

    let applied = load_applied_migrations(conn)?;

    for migration in MIGRATIONS {
        if applied.contains(migration.version) {
            continue;
        }

        (migration.apply)(conn)?;
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![
                migration.version,
                OffsetDateTime::now_utc().unix_timestamp()
            ],
        )?;
    }

    Ok(())
}

fn load_applied_migrations(conn: &Connection) -> Result<HashSet<String>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

fn migration_0001_initial_reset(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    // Migration 0001 intentionally resets all known tables so this release becomes
    // the new baseline for forward-only schema evolution.
    conn.execute_batch(
        r#"
            DROP TABLE IF EXISTS message_tool_calls;
            DROP TABLE IF EXISTS message_binaries;
            DROP TABLE IF EXISTS message_usage;
            DROP TABLE IF EXISTS messages_fts;
            DROP TABLE IF EXISTS message_parts;
            DROP TABLE IF EXISTS messages;
            DROP TABLE IF EXISTS events;
            DROP TABLE IF EXISTS revert_states;
            DROP TABLE IF EXISTS delegations;
            DROP TABLE IF EXISTS artifacts;
            DROP TABLE IF EXISTS progress_entries;
            DROP TABLE IF EXISTS alternatives;
            DROP TABLE IF EXISTS decisions;
            DROP TABLE IF EXISTS intent_snapshots;
            DROP TABLE IF EXISTS tasks;
            DROP TABLE IF EXISTS sessions;
            DROP TABLE IF EXISTS session_execution_configs;
            DROP TABLE IF EXISTS llm_configs;
        "#,
    )?;

    schema::init_schema(conn)?;
    Ok(())
}

fn migration_0002_drop_legacy_events(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    // The legacy `events` table is no longer used. All event reads and writes
    // go through the `event_journal` table exclusively.
    conn.execute_batch(
        r#"
            DROP TABLE IF EXISTS events;
        "#,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn custom_model_crud_round_trip() {
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
}

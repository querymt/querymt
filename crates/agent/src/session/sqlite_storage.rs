use crate::events::{AgentEvent, AgentEventKind, EventObserver};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::{
    Alternative, AlternativeStatus, Artifact, Decision, DecisionStatus, Delegation,
    DelegationStatus, ForkInfo, ForkOrigin, ForkPointType, IntentSnapshot, ProgressEntry,
    ProgressKind, Task, TaskStatus,
};
use crate::session::error::{SessionError, SessionResult};
use crate::session::projection::{
    AuditView, DefaultRedactor, EventStore, FilterExpr, PredicateOp, RedactedArtifact,
    RedactedProgress, RedactedTask, RedactedView, RedactionPolicy, Redactor, SessionGroup,
    SessionListFilter, SessionListItem, SessionListView, SummaryView, ViewStore,
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
use crate::session::store::{LLMConfig, Session, SessionStore, extract_llm_config_values};
use async_trait::async_trait;
use querymt::LLMParams;
use querymt::chat::ChatRole;
use querymt::error::LLMError;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;
use uuid::Uuid;

/// A unified SQLite storage implementation.
///
/// This implementation provides all storage functionality in a single struct:
/// - Session and message persistence (SessionStore)
/// - Event persistence and querying (EventStore)
/// - View generation for observability (ViewStore)
/// - Event observation for the event bus (EventObserver)
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

        // Resolve target message public_id → internal i64
        let target_message_internal_id =
            self.resolve_message_internal_id(target_message_id).await?;

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;

            // 1. Create New Session with UUID v7 public_id
            let new_session_public_id = Uuid::now_v7().to_string();
            let now = OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default();

            // Get parent session llm_config_id (internal i64)
            let parent_llm_config_id: Option<i64> = tx
                .query_row(
                    "SELECT llm_config_id FROM sessions WHERE id = ?",
                    params![source_session_internal_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();

            tx.execute(
                "INSERT INTO sessions (public_id, name, created_at, updated_at, llm_config_id, parent_session_id, fork_origin, fork_point_type, fork_point_ref) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    new_session_public_id.clone(),
                    format!("Fork of session"), // Temporary name
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

            // 2. Identify messages to copy (up to target_message_id internal ID)
            let messages_to_copy = {
                let mut stmt = tx.prepare(
                    "SELECT id, public_id, role, created_at FROM messages WHERE session_id = ? ORDER BY created_at ASC"
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

    async fn get_revert_state(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<crate::session::domain::RevertState>> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let session_id_str = session_id.to_string();

        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT public_id, message_id, snapshot_id, backend_id, created_at FROM revert_states WHERE session_id = ?",
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

    async fn set_revert_state(
        &self,
        session_id: &str,
        state: Option<crate::session::domain::RevertState>,
    ) -> SessionResult<()> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;

        self.run_blocking(move |conn| {
            // Always delete existing revert state for this session
            conn.execute(
                "DELETE FROM revert_states WHERE session_id = ?",
                params![session_internal_id],
            )?;

            // Insert new state if provided
            if let Some(state) = state {
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
            }
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
// EventStore implementation
// ============================================================================

#[async_trait]
impl EventStore for SqliteStorage {
    async fn append_event(&self, event: &AgentEvent) -> SessionResult<()> {
        let conn_arc = self.conn.clone();
        let event_clone = event.clone();

        tokio::task::spawn_blocking(move || -> Result<(), rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();
            let kind_json = serde_json::to_string(&event_clone.kind)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

            conn.execute(
                "INSERT INTO events (seq, timestamp, session_id, kind) VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    event_clone.seq,
                    event_clone.timestamp,
                    &event_clone.session_id,
                    kind_json
                ],
            )?;

            Ok(())
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    async fn get_session_events(&self, session_id: &str) -> SessionResult<Vec<AgentEvent>> {
        let session_id_str = session_id.to_string();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<AgentEvent>, rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT seq, timestamp, session_id, kind FROM events WHERE session_id = ? ORDER BY seq ASC")?;

            let events = stmt
                .query_map([session_id_str], |row| {
                    let kind_json: String = row.get(3)?;
                    let kind: AgentEventKind = serde_json::from_str(&kind_json)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;

                    Ok(AgentEvent {
                        seq: row.get(0)?,
                        timestamp: row.get(1)?,
                        session_id: row.get(2)?,
                        kind,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(events)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    async fn get_events_since(
        &self,
        session_id: &str,
        after_seq: u64,
    ) -> SessionResult<Vec<AgentEvent>> {
        let session_id_str = session_id.to_string();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<AgentEvent>, rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT seq, timestamp, session_id, kind FROM events WHERE session_id = ? AND seq > ? ORDER BY seq ASC")?;

            let events = stmt
                .query_map(rusqlite::params![session_id_str, after_seq], |row| {
                    let kind_json: String = row.get(3)?;
                    let kind: AgentEventKind = serde_json::from_str(&kind_json)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;

                    Ok(AgentEvent {
                        seq: row.get(0)?,
                        timestamp: row.get(1)?,
                        session_id: row.get(2)?,
                        kind,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(events)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
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
        let mut events = self.get_session_events(session_id).await?;

        // Include child session events (delegations) if requested
        if include_children {
            let session_repo = SqliteSessionRepository::new(self.conn.clone());
            let child_session_ids = session_repo.list_child_sessions(session_id).await?;
            for child_id in &child_session_ids {
                let child_events = self.get_session_events(child_id).await?;
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

    async fn get_session_list_view(
        &self,
        filter: Option<SessionListFilter>,
    ) -> SessionResult<SessionListView> {
        use std::collections::{HashMap, HashSet};

        let session_repo = SqliteSessionRepository::new(self.conn.clone());
        let intent_repo = SqliteIntentRepository::new(self.conn.clone());

        // Get all sessions (list_sessions already returns sorted by updated_at DESC)
        let mut sessions = session_repo.list_sessions().await?;

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

        // Build a parent-child map to organize sessions hierarchically
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

        // Group by CWD
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
// EventObserver implementation
// ============================================================================

#[async_trait]
impl EventObserver for SqliteStorage {
    async fn on_event(&self, event: &AgentEvent) -> Result<(), LLMError> {
        self.append_event(event)
            .await
            .map_err(|e| LLMError::ProviderError(format!("Event storage failed: {}", e)))?;
        Ok(())
    }
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
    })
}

fn apply_migrations(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    // Drop every table that might have been created by older schema versions so we can start fresh.
    conn.execute_batch(
        r#"
            DROP TABLE IF EXISTS message_tool_calls;
            DROP TABLE IF EXISTS message_binaries;
            DROP TABLE IF EXISTS message_usage;
            DROP TABLE IF EXISTS messages_fts;
            DROP TABLE IF EXISTS message_parts;
            DROP TABLE IF EXISTS messages;
            DROP TABLE IF EXISTS events;
            DROP TABLE IF EXISTS delegations;
            DROP TABLE IF EXISTS artifacts;
            DROP TABLE IF EXISTS progress_entries;
            DROP TABLE IF EXISTS alternatives;
            DROP TABLE IF EXISTS decisions;
            DROP TABLE IF EXISTS intent_snapshots;
            DROP TABLE IF EXISTS tasks;
            DROP TABLE IF EXISTS sessions;
            DROP TABLE IF EXISTS llm_configs;
        "#,
    )?;

    schema::init_schema(conn)?;
    Ok(())
}

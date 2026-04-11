use async_trait::async_trait;
use querymt::LLMParams;
use querymt::chat::ChatRole;
use rusqlite::{OptionalExtension, params};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::{
    Alternative, AlternativeStatus, Artifact, Decision, DecisionStatus, Delegation,
    DelegationStatus, ForkInfo, ForkOrigin, ForkPointType, IntentSnapshot, ProgressEntry,
    ProgressKind, RevertState, Task, TaskStatus,
};
use crate::session::error::SessionResult;
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
use crate::session::store::{
    CustomModel, LLMConfig, RemoteSessionBookmark, Session, SessionExecutionConfig, SessionStore,
    extract_llm_config_values,
};

use super::SqliteStorage;
use super::row_parsers::{parse_llm_config_row, parse_llm_params};

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
                "SELECT id, public_id, role, created_at, parent_message_id, source_provider, source_model FROM messages WHERE session_id = ? ORDER BY created_at ASC"
            )?;

            // Build a map: internal_id → public_id for parent resolution
            let mut id_map: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
            let messages_data: Vec<(
                i64,
                String,
                String,
                i64,
                Option<i64>,
                Option<String>,
                Option<String>,
            )> = stmt
                .query_map(params![session_internal_id], |row| {
                    let internal_id: i64 = row.get(0)?;
                    let public_id: String = row.get(1)?;
                    let role_str: String = row.get(2)?;
                    let created_at: i64 = row.get(3)?;
                    let parent_internal_id: Option<i64> = row.get(4)?;
                    let source_provider: Option<String> = row.get(5)?;
                    let source_model: Option<String> = row.get(6)?;
                    Ok((
                        internal_id,
                        public_id,
                        role_str,
                        created_at,
                        parent_internal_id,
                        source_provider,
                        source_model,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            // Build id_map
            for (internal_id, public_id, _, _, _, _, _) in &messages_data {
                id_map.insert(*internal_id, public_id.clone());
            }

            // Convert to AgentMessage, resolving parent_message_id
            let mut messages: Vec<AgentMessage> = messages_data
                .into_iter()
                .map(
                    |(
                        _internal_id,
                        public_id,
                        role_str,
                        created_at,
                        parent_internal_id,
                        source_provider,
                        source_model,
                    )| {
                        let role = match role_str.as_str() {
                            "User" => ChatRole::User,
                            "Assistant" => ChatRole::Assistant,
                            _ => ChatRole::User, // Default fallback
                        };

                        let parent_message_id =
                            parent_internal_id.and_then(|pid| id_map.get(&pid).cloned());

                        AgentMessage {
                            id: public_id.clone(),
                            session_id: session_id_str.clone(),
                            role,
                            parts: Vec::new(), // Will populate next
                            created_at,
                            parent_message_id,
                            source_provider,
                            source_model,
                        }
                    },
                )
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
                "INSERT INTO messages (public_id, session_id, role, created_at, parent_message_id, source_provider, source_model) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    msg.id,
                    session_internal_id,
                    role_str,
                    msg.created_at,
                    parent_internal_id,
                    msg.source_provider,
                    msg.source_model
                ],
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
                    "SELECT id, public_id, role, created_at, source_provider, source_model FROM messages WHERE session_id = ? ORDER BY id ASC"
                )?;

                let messages: Vec<(i64, String, String, i64, Option<String>, Option<String>)> = stmt.query_map(params![source_session_internal_id], |row| {
                    Ok((
                        row.get(0)?, // internal id
                        row.get(1)?, // public_id
                        row.get(2)?, // role
                        row.get(3)?, // created_at
                        row.get(4)?, // source_provider
                        row.get(5)?  // source_model
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
                .map(|(_, public_id, _, _, _, _)| public_id.clone())
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
                            // payload_json uses adjacently tagged format: fields are under "data"
                            let message_id = parsed
                                .get("data")
                                .and_then(|d| d.get("message_id"))
                                .and_then(|id| id.as_str());
                            return message_id
                                .map(|id| copied_message_ids.contains(id))
                                .unwrap_or(false);
                        }

                        if kind == "assistant_message_stored" {
                            let parsed: serde_json::Value = match serde_json::from_str(payload_json) {
                                Ok(value) => value,
                                Err(_) => return false,
                            };
                            // payload_json uses adjacently tagged format: fields are under "data"
                            let message_id = parsed
                                .get("data")
                                .and_then(|d| d.get("message_id"))
                                .and_then(|id| id.as_str());
                            return message_id
                                .map(|id| copied_message_ids.contains(id))
                                .unwrap_or(false);
                        }

                        false
                    })
                    .collect()
            };

            // 3. Copy messages and their parts with new UUID v7 public_ids
            for (
                old_internal_id,
                _old_public_id,
                role,
                created_at,
                source_provider,
                source_model,
            ) in messages_to_copy
            {
                let new_msg_public_id = Uuid::now_v7().to_string();

                // Insert Message with new public_id and internal session_id
                tx.execute(
                    "INSERT INTO messages (public_id, session_id, role, created_at, parent_message_id, source_provider, source_model) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![
                        new_msg_public_id,
                        new_session_internal_id,
                        role,
                        created_at,
                        Option::<i64>::None,
                        source_provider,
                        source_model
                    ],
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
                let stream_seq: i64 = tx.query_row(
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
            crate::session::error::SessionError::DatabaseError(_) => {
                crate::session::error::SessionError::SessionNotFound(session_id.to_string())
            }
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
            crate::session::error::SessionError::InvalidOperation(format!(
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

    async fn get_initial_intent_snapshot(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<IntentSnapshot>> {
        let repo = SqliteIntentRepository::new(self.conn.clone());
        repo.get_initial_intent_snapshot(session_id).await
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

    async fn peek_revert_state(&self, session_id: &str) -> SessionResult<Option<RevertState>> {
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

                    Ok(RevertState {
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

    async fn push_revert_state(&self, session_id: &str, state: RevertState) -> SessionResult<()> {
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

    async fn pop_revert_state(&self, session_id: &str) -> SessionResult<Option<RevertState>> {
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
                            RevertState {
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

    async fn list_revert_states(&self, session_id: &str) -> SessionResult<Vec<RevertState>> {
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

                Ok(RevertState {
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

            // Delete message_parts for the target message and everything after it.
            // We use the internal auto-increment id (>= target) rather than
            // created_at timestamps which can collide at second resolution.
            tx.execute(
                "DELETE FROM message_parts WHERE message_id IN (
                    SELECT id FROM messages WHERE session_id = ? AND id >= ?
                )",
                params![session_internal_id, message_internal_id],
            )?;

            // Delete the target message and all messages after it
            let deleted: usize = tx.execute(
                "DELETE FROM messages WHERE session_id = ? AND id >= ?",
                params![session_internal_id, message_internal_id],
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

    // ── Remote session bookmarks ─────────────────────────────────────────

    async fn save_remote_session_bookmark(
        &self,
        bookmark: &RemoteSessionBookmark,
    ) -> SessionResult<()> {
        let b = bookmark.clone();
        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO remote_session_bookmarks
                     (session_id, node_id, peer_label, cwd, created_at, title)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    b.session_id,
                    b.node_id,
                    b.peer_label,
                    b.cwd,
                    b.created_at,
                    b.title,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn list_remote_session_bookmarks(&self) -> SessionResult<Vec<RemoteSessionBookmark>> {
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT session_id, node_id, peer_label, cwd, created_at, title
                 FROM remote_session_bookmarks
                 ORDER BY created_at DESC",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(RemoteSessionBookmark {
                        session_id: row.get(0)?,
                        node_id: row.get(1)?,
                        peer_label: row.get(2)?,
                        cwd: row.get(3)?,
                        created_at: row.get(4)?,
                        title: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    async fn remove_remote_session_bookmark(&self, session_id: &str) -> SessionResult<()> {
        let sid = session_id.to_string();
        self.run_blocking(move |conn| {
            conn.execute(
                "DELETE FROM remote_session_bookmarks WHERE session_id = ?1",
                params![sid],
            )?;
            Ok(())
        })
        .await
    }
}

//! SQLite implementation of `KnowledgeStore`.
//!
//! Follows the same pattern as `repo_schedule.rs`: async trait with a
//! `run_blocking` helper that acquires the shared `Arc<Mutex<Connection>>`.

use crate::knowledge::{
    ConsolidateRequest, Consolidation, IngestRequest, KnowledgeEntry, KnowledgeError,
    KnowledgeFilter, KnowledgeQueryResult, KnowledgeStats, KnowledgeStore, QueryOpts,
    RetentionPolicy, RetentionResult, RetrievalMode,
};
use async_trait::async_trait;
use rusqlite::{Connection, params, types::Value};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// SQLite-backed `KnowledgeStore`.
#[derive(Clone)]
pub struct SqliteKnowledgeStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteKnowledgeStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    async fn run_blocking<F, R>(&self, f: F) -> Result<R, KnowledgeError>
    where
        F: FnOnce(&mut Connection) -> Result<R, KnowledgeError> + Send + 'static,
        R: Send + 'static,
    {
        let conn_arc = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn_arc.lock().unwrap();
            f(&mut conn)
        })
        .await
        .map_err(|e| KnowledgeError::Other(format!("Knowledge task execution failed: {}", e)))?
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Format an `OffsetDateTime` as RFC 3339 for SQLite TEXT storage.
fn format_dt(dt: &OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Parse an RFC 3339 string back into `OffsetDateTime`.
fn parse_dt(s: &str) -> Result<OffsetDateTime, KnowledgeError> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).map_err(|e| {
        KnowledgeError::DatabaseError(format!("Failed to parse datetime '{}': {}", s, e))
    })
}

/// Parse an optional RFC 3339 string.
fn parse_dt_opt(s: &Option<String>) -> Result<Option<OffsetDateTime>, KnowledgeError> {
    match s {
        Some(v) => Ok(Some(parse_dt(v)?)),
        None => Ok(None),
    }
}

/// Read a `KnowledgeEntry` from a rusqlite `Row`.
fn row_to_entry(row: &rusqlite::Row<'_>) -> Result<KnowledgeEntry, rusqlite::Error> {
    let entities_json: String = row.get("entities_json")?;
    let topics_json: String = row.get("topics_json")?;
    let connections_json: String = row.get("connections_json")?;
    let created_at_str: String = row.get("created_at")?;
    let consolidated_at_str: Option<String> = row.get("consolidated_at")?;

    let entities: Vec<String> = serde_json::from_str(&entities_json).unwrap_or_default();
    let topics: Vec<String> = serde_json::from_str(&topics_json).unwrap_or_default();
    let connections: Vec<String> = serde_json::from_str(&connections_json).unwrap_or_default();

    let created_at = OffsetDateTime::parse(
        &created_at_str,
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|_| rusqlite::Error::InvalidQuery)?;
    let consolidated_at = consolidated_at_str
        .as_deref()
        .map(|s| {
            OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .map_err(|_| rusqlite::Error::InvalidQuery)
        })
        .transpose()?;

    Ok(KnowledgeEntry {
        id: row.get("id")?,
        public_id: row.get("public_id")?,
        scope: row.get("scope")?,
        source: row.get("source")?,
        raw_text: row.get("raw_text")?,
        summary: row.get("summary")?,
        entities,
        topics,
        connections,
        importance: row.get("importance")?,
        consolidated_at,
        created_at,
    })
}

/// Read a `Consolidation` from a rusqlite `Row`.
fn row_to_consolidation(row: &rusqlite::Row<'_>) -> Result<Consolidation, rusqlite::Error> {
    let source_ids_json: String = row.get("source_entry_public_ids_json")?;
    let connections_json: String = row.get("connections_json")?;
    let created_at_str: String = row.get("created_at")?;

    let source_entry_public_ids: Vec<String> =
        serde_json::from_str(&source_ids_json).unwrap_or_default();
    let connections: Vec<String> = serde_json::from_str(&connections_json).unwrap_or_default();

    let created_at = OffsetDateTime::parse(
        &created_at_str,
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|_| rusqlite::Error::InvalidQuery)?;

    Ok(Consolidation {
        id: row.get("id")?,
        public_id: row.get("public_id")?,
        scope: row.get("scope")?,
        source_entry_public_ids,
        summary: row.get("summary")?,
        insight: row.get("insight")?,
        connections,
        created_at,
    })
}

const ENTRY_COLS: &str = "id, public_id, scope, source, raw_text, summary, \
    entities_json, topics_json, connections_json, importance, \
    consolidated_at, created_at";

const CONSOLIDATION_COLS: &str = "id, public_id, scope, source_entry_public_ids_json, \
    summary, insight, connections_json, created_at";

// ─── KnowledgeStore implementation ───────────────────────────────────────────

#[async_trait]
impl KnowledgeStore for SqliteKnowledgeStore {
    async fn ingest(
        &self,
        scope: &str,
        entry: IngestRequest,
    ) -> Result<KnowledgeEntry, KnowledgeError> {
        let scope = scope.to_string();
        let public_id = uuid::Uuid::now_v7().to_string();
        let now = OffsetDateTime::now_utc();
        let now_str = format_dt(&now);

        let entities_json = serde_json::to_string(&entry.entities)?;
        let topics_json = serde_json::to_string(&entry.topics)?;
        let connections_json = serde_json::to_string(&entry.connections)?;

        let pid = public_id.clone();
        let scope_clone = scope.clone();
        let source = entry.source.clone();
        let raw_text = entry.raw_text.clone();
        let summary = entry.summary.clone();
        let importance = entry.importance;
        let entities = entry.entities.clone();
        let topics = entry.topics.clone();
        let connections = entry.connections.clone();

        let id = self
            .run_blocking(move |conn| {
                conn.execute(
                    "INSERT INTO knowledge_entries (
                        public_id, scope, source, raw_text, summary,
                        entities_json, topics_json, connections_json,
                        importance, created_at
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        pid,
                        scope_clone,
                        entry.source,
                        entry.raw_text,
                        entry.summary,
                        entities_json,
                        topics_json,
                        connections_json,
                        entry.importance,
                        now_str,
                    ],
                )
                .map_err(KnowledgeError::from)?;
                Ok(conn.last_insert_rowid())
            })
            .await?;

        Ok(KnowledgeEntry {
            id,
            public_id,
            scope,
            source,
            raw_text: Some(raw_text),
            summary,
            entities,
            topics,
            connections,
            importance,
            consolidated_at: None,
            created_at: now,
        })
    }

    async fn list_unconsolidated(
        &self,
        scope: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeEntry>, KnowledgeError> {
        let scope = scope.to_string();
        self.run_blocking(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {ENTRY_COLS} FROM knowledge_entries \
                     WHERE scope = ? AND consolidated_at IS NULL \
                     ORDER BY created_at ASC LIMIT ?"
                ))
                .map_err(KnowledgeError::from)?;
            let rows = stmt
                .query_map(params![scope, limit as i64], row_to_entry)
                .map_err(KnowledgeError::from)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(KnowledgeError::from)
        })
        .await
    }

    async fn list(
        &self,
        scope: &str,
        filter: KnowledgeFilter,
    ) -> Result<Vec<KnowledgeEntry>, KnowledgeError> {
        let scope = scope.to_string();
        self.run_blocking(move |conn| {
            let mut conditions = vec!["scope = ?1".to_string()];
            let mut param_idx = 2usize;

            // We use explicit named index binding via stmt.raw_bind_parameter
            // to avoid issues with dynamic param slices.
            let mut bindings: Vec<(usize, Value)> = vec![(1, Value::Text(scope))];

            if let Some(ref since) = filter.since {
                conditions.push(format!("created_at >= ?{}", param_idx));
                bindings.push((param_idx, Value::Text(format_dt(since))));
                param_idx += 1;
            }

            match filter.consolidated {
                Some(true) => conditions.push("consolidated_at IS NOT NULL".to_string()),
                Some(false) => conditions.push("consolidated_at IS NULL".to_string()),
                None => {}
            }

            // Topic/entity filtering uses JSON LIKE matching
            if let Some(ref topics) = filter.topics {
                let topic_conditions: Vec<String> = topics
                    .iter()
                    .map(|t| {
                        let cond = format!("topics_json LIKE ?{}", param_idx);
                        bindings.push((
                            param_idx,
                            Value::Text(format!("%\"{}\"%", t.replace('"', ""))),
                        ));
                        param_idx += 1;
                        cond
                    })
                    .collect();
                if !topic_conditions.is_empty() {
                    conditions.push(format!("({})", topic_conditions.join(" OR ")));
                }
            }

            if let Some(ref entities) = filter.entities {
                let entity_conditions: Vec<String> = entities
                    .iter()
                    .map(|e| {
                        let cond = format!("entities_json LIKE ?{}", param_idx);
                        bindings.push((
                            param_idx,
                            Value::Text(format!("%\"{}\"%", e.replace('"', ""))),
                        ));
                        param_idx += 1;
                        cond
                    })
                    .collect();
                if !entity_conditions.is_empty() {
                    conditions.push(format!("({})", entity_conditions.join(" OR ")));
                }
            }

            let where_clause = conditions.join(" AND ");
            let limit_idx = param_idx;
            let sql = format!(
                "SELECT {ENTRY_COLS} FROM knowledge_entries \
                 WHERE {where_clause} \
                 ORDER BY created_at DESC LIMIT ?{limit_idx}"
            );
            bindings.push((limit_idx, Value::Integer(filter.limit as i64)));

            let mut stmt = conn.prepare(&sql).map_err(KnowledgeError::from)?;
            for (idx, val) in &bindings {
                stmt.raw_bind_parameter(*idx, val)
                    .map_err(KnowledgeError::from)?;
            }
            let mut rows = stmt.raw_query();
            let mut results = Vec::new();
            while let Some(row) = rows.next().map_err(KnowledgeError::from)? {
                results.push(row_to_entry(row).map_err(KnowledgeError::from)?);
            }
            Ok(results)
        })
        .await
    }

    async fn consolidate(
        &self,
        scope: &str,
        request: ConsolidateRequest,
    ) -> Result<Consolidation, KnowledgeError> {
        let scope = scope.to_string();
        let public_id = uuid::Uuid::now_v7().to_string();
        let now = OffsetDateTime::now_utc();
        let now_str = format_dt(&now);

        let source_ids_json = serde_json::to_string(&request.source_entry_public_ids)?;
        let connections_json = serde_json::to_string(&request.connections)?;

        let pid = public_id.clone();
        let scope_clone = scope.clone();
        let summary = request.summary.clone();
        let insight = request.insight.clone();
        let connections = request.connections.clone();
        let source_entry_public_ids = request.source_entry_public_ids.clone();

        let id = self
            .run_blocking(move |conn| {
                let tx = conn.transaction().map_err(KnowledgeError::from)?;

                // Insert the consolidation
                tx.execute(
                    "INSERT INTO knowledge_consolidations (
                        public_id, scope, source_entry_public_ids_json,
                        summary, insight, connections_json, created_at
                    ) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![
                        pid,
                        scope_clone,
                        source_ids_json,
                        request.summary,
                        request.insight,
                        connections_json,
                        now_str,
                    ],
                )
                .map_err(KnowledgeError::from)?;
                let consolidation_id = tx.last_insert_rowid();

                // Mark source entries as consolidated
                for entry_public_id in &request.source_entry_public_ids {
                    tx.execute(
                        "UPDATE knowledge_entries SET consolidated_at = ? \
                         WHERE public_id = ? AND scope = ? AND consolidated_at IS NULL",
                        params![now_str, entry_public_id, scope_clone],
                    )
                    .map_err(KnowledgeError::from)?;
                }

                tx.commit().map_err(KnowledgeError::from)?;
                Ok(consolidation_id)
            })
            .await?;

        Ok(Consolidation {
            id,
            public_id,
            scope,
            source_entry_public_ids,
            summary,
            insight,
            connections,
            created_at: now,
        })
    }

    async fn query(
        &self,
        scope: &str,
        question: &str,
        opts: QueryOpts,
    ) -> Result<KnowledgeQueryResult, KnowledgeError> {
        let scope = scope.to_string();
        let question = question.to_string();

        self.run_blocking(move |conn| {
            // Extract keywords from the question for matching
            let keywords = extract_keywords(&question);

            let entries = query_entries(conn, &scope, &keywords, &opts)?;
            let consolidations = if opts.include_consolidations {
                query_consolidations(conn, &scope, &keywords, opts.limit)?
            } else {
                vec![]
            };

            Ok(KnowledgeQueryResult {
                entries,
                consolidations,
            })
        })
        .await
    }

    async fn stats(&self, scope: &str) -> Result<KnowledgeStats, KnowledgeError> {
        let scope = scope.to_string();
        self.run_blocking(move |conn| {
            let total_entries: u64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM knowledge_entries WHERE scope = ?",
                    params![scope],
                    |row| row.get(0),
                )
                .map_err(KnowledgeError::from)?;

            let unconsolidated_entries: u64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM knowledge_entries \
                     WHERE scope = ? AND consolidated_at IS NULL",
                    params![scope],
                    |row| row.get(0),
                )
                .map_err(KnowledgeError::from)?;

            let total_consolidations: u64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM knowledge_consolidations WHERE scope = ?",
                    params![scope],
                    |row| row.get(0),
                )
                .map_err(KnowledgeError::from)?;

            let latest_entry_at: Option<String> = conn
                .query_row(
                    "SELECT MAX(created_at) FROM knowledge_entries WHERE scope = ?",
                    params![scope],
                    |row| row.get(0),
                )
                .map_err(KnowledgeError::from)?;

            let latest_consolidation_at: Option<String> = conn
                .query_row(
                    "SELECT MAX(created_at) FROM knowledge_consolidations WHERE scope = ?",
                    params![scope],
                    |row| row.get(0),
                )
                .map_err(KnowledgeError::from)?;

            Ok(KnowledgeStats {
                total_entries,
                unconsolidated_entries,
                total_consolidations,
                latest_entry_at: parse_dt_opt(&latest_entry_at)?,
                latest_consolidation_at: parse_dt_opt(&latest_consolidation_at)?,
            })
        })
        .await
    }

    async fn apply_retention(
        &self,
        scope: &str,
        policy: &RetentionPolicy,
    ) -> Result<RetentionResult, KnowledgeError> {
        let scope = scope.to_string();
        let policy = policy.clone();

        self.run_blocking(move |conn| {
            let tx = conn.transaction().map_err(KnowledgeError::from)?;
            let mut archived: u64 = 0;
            let mut deleted: u64 = 0;

            // Age-based retention
            if let Some(max_age_days) = policy.max_age_days {
                let cutoff = OffsetDateTime::now_utc() - time::Duration::days(max_age_days as i64);
                let cutoff_str = format_dt(&cutoff);

                if policy.archive_raw_text {
                    // Archive: set raw_text to NULL but keep summaries
                    let count = tx
                        .execute(
                            "UPDATE knowledge_entries SET raw_text = NULL \
                             WHERE scope = ? AND created_at < ? AND raw_text IS NOT NULL",
                            params![scope, cutoff_str],
                        )
                        .map_err(KnowledgeError::from)?;
                    archived += count as u64;
                } else {
                    // Hard delete old entries
                    let count = tx
                        .execute(
                            "DELETE FROM knowledge_entries \
                             WHERE scope = ? AND created_at < ?",
                            params![scope, cutoff_str],
                        )
                        .map_err(KnowledgeError::from)?;
                    deleted += count as u64;
                }
            }

            // Count-based retention
            if let Some(max_entries) = policy.max_entries {
                let current_count: u64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM knowledge_entries WHERE scope = ?",
                        params![scope],
                        |row| row.get(0),
                    )
                    .map_err(KnowledgeError::from)?;

                if current_count > max_entries {
                    let excess = current_count - max_entries;

                    if policy.archive_raw_text {
                        // Archive the oldest entries by setting raw_text to NULL
                        let count = tx
                            .execute(
                                "UPDATE knowledge_entries SET raw_text = NULL \
                                 WHERE scope = ? AND raw_text IS NOT NULL \
                                 AND id IN (
                                     SELECT id FROM knowledge_entries \
                                     WHERE scope = ? \
                                     ORDER BY created_at ASC LIMIT ?
                                 )",
                                params![scope, scope, excess as i64],
                            )
                            .map_err(KnowledgeError::from)?;
                        archived += count as u64;
                    } else {
                        // Hard delete the oldest entries
                        let count = tx
                            .execute(
                                "DELETE FROM knowledge_entries \
                                 WHERE scope = ? AND id IN (
                                     SELECT id FROM knowledge_entries \
                                     WHERE scope = ? \
                                     ORDER BY created_at ASC LIMIT ?
                                 )",
                                params![scope, scope, excess as i64],
                            )
                            .map_err(KnowledgeError::from)?;
                        deleted += count as u64;
                    }
                }
            }

            tx.commit().map_err(KnowledgeError::from)?;
            Ok(RetentionResult { archived, deleted })
        })
        .await
    }

    async fn is_source_ingested(
        &self,
        scope: &str,
        source_key: &str,
    ) -> Result<bool, KnowledgeError> {
        let scope = scope.to_string();
        let source_key = source_key.to_string();
        self.run_blocking(move |conn| {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM knowledge_ingestion_log \
                     WHERE scope = ? AND source_key = ?)",
                    params![scope, source_key],
                    |row| row.get(0),
                )
                .map_err(KnowledgeError::from)?;
            Ok(exists)
        })
        .await
    }

    async fn mark_source_ingested(
        &self,
        scope: &str,
        source_key: &str,
    ) -> Result<(), KnowledgeError> {
        let scope = scope.to_string();
        let source_key = source_key.to_string();
        let now_str = format_dt(&OffsetDateTime::now_utc());

        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO knowledge_ingestion_log \
                 (scope, source_key, processed_at) VALUES (?, ?, ?)",
                params![scope, source_key, now_str],
            )
            .map_err(KnowledgeError::from)?;
            Ok(())
        })
        .await
    }
}

// ─── Query Helpers ───────────────────────────────────────────────────────────

/// Extract keywords from a question string for keyword-based search.
/// Strips common stop words and returns lowercase tokens.
fn extract_keywords(question: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "the", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
        "do", "does", "did", "will", "would", "could", "should", "may", "might", "shall", "can",
        "need", "dare", "ought", "used", "to", "of", "in", "for", "on", "with", "at", "by", "from",
        "as", "into", "through", "during", "before", "after", "above", "below", "between", "out",
        "off", "over", "under", "again", "further", "then", "once", "here", "there", "when",
        "where", "why", "how", "all", "both", "each", "few", "more", "most", "other", "some",
        "such", "no", "nor", "not", "only", "own", "same", "so", "than", "too", "very", "just",
        "don", "now", "and", "but", "or", "if", "it", "its", "i", "me", "my", "we", "our", "you",
        "your", "he", "she", "they", "them", "his", "her", "what", "which", "who", "whom", "this",
        "that", "these", "those", "am", "about", "up",
    ];

    question
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|w| w.len() >= 2 && !STOP_WORDS.contains(w))
        .map(String::from)
        .collect()
}

/// Query entries using keyword matching with optional hybrid scoring.
///
/// Uses `?N` positional params (1-indexed) with `raw_bind_parameter` so the
/// same keyword pattern can be referenced in both the WHERE clause and the
/// CASE WHEN score expression without duplicating values.
fn query_entries(
    conn: &Connection,
    scope: &str,
    keywords: &[String],
    opts: &QueryOpts,
) -> Result<Vec<KnowledgeEntry>, KnowledgeError> {
    if keywords.is_empty() {
        // No keywords: return most recent entries
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ENTRY_COLS} FROM knowledge_entries \
                 WHERE scope = ?1 ORDER BY created_at DESC LIMIT ?2"
            ))
            .map_err(KnowledgeError::from)?;
        stmt.raw_bind_parameter(1, scope)
            .map_err(KnowledgeError::from)?;
        stmt.raw_bind_parameter(2, opts.limit as i64)
            .map_err(KnowledgeError::from)?;
        let mut rows = stmt.raw_query();
        let mut results = Vec::new();
        while let Some(row) = rows.next().map_err(KnowledgeError::from)? {
            results.push(row_to_entry(row).map_err(KnowledgeError::from)?);
        }
        return Ok(results);
    }

    // Build LIKE conditions for each keyword across summary, raw_text, source
    // and optionally entities/topics for hybrid mode.
    //
    // Uses `?N` params so the same pattern param can be reused in both WHERE
    // and CASE WHEN score expression.
    let mut like_parts = Vec::new();
    let mut bindings: Vec<(usize, Value)> = vec![(1, Value::Text(scope.to_string()))];
    let mut param_idx = 2usize;

    // Build a score expression for ranking
    let mut score_parts = Vec::new();

    for keyword in keywords {
        let pattern = format!("%{}%", keyword);

        // One param for the keyword pattern, reused across WHERE + score
        let kw_idx = param_idx;
        bindings.push((kw_idx, Value::Text(pattern.clone())));
        param_idx += 1;

        // WHERE: summary OR raw_text OR source
        like_parts.push(format!(
            "(summary LIKE ?{kw_idx} OR raw_text LIKE ?{kw_idx} OR source LIKE ?{kw_idx})"
        ));
        // Score: same columns with weights
        score_parts.push(format!(
            "(CASE WHEN summary LIKE ?{kw_idx} THEN 3 ELSE 0 END + \
             CASE WHEN raw_text LIKE ?{kw_idx} THEN 1 ELSE 0 END + \
             CASE WHEN source LIKE ?{kw_idx} THEN 1 ELSE 0 END)"
        ));

        if opts.retrieval_mode == RetrievalMode::Hybrid {
            // Separate param for entity/topic matching (same pattern)
            let ht_idx = param_idx;
            bindings.push((ht_idx, Value::Text(pattern)));
            param_idx += 1;

            like_parts.push(format!(
                "(entities_json LIKE ?{ht_idx} OR topics_json LIKE ?{ht_idx})"
            ));
            score_parts.push(format!(
                "(CASE WHEN entities_json LIKE ?{ht_idx} THEN 2 ELSE 0 END + \
                 CASE WHEN topics_json LIKE ?{ht_idx} THEN 2 ELSE 0 END)"
            ));
        }
    }

    let where_clause = format!("scope = ?1 AND ({})", like_parts.join(" OR "));

    let score_expr = if score_parts.is_empty() {
        "0".to_string()
    } else {
        let base_score = score_parts.join(" + ");
        if opts.retrieval_mode == RetrievalMode::Hybrid {
            // In hybrid mode, boost by importance
            format!("({base_score}) + (importance * 5)")
        } else {
            base_score
        }
    };

    // Limit param
    let limit_idx = param_idx;
    bindings.push((limit_idx, Value::Integer(opts.limit as i64)));

    let sql = format!(
        "SELECT {ENTRY_COLS}, ({score_expr}) as _score \
         FROM knowledge_entries \
         WHERE {where_clause} \
         ORDER BY _score DESC, created_at DESC, id DESC \
         LIMIT ?{limit_idx}"
    );

    let mut stmt = conn.prepare(&sql).map_err(KnowledgeError::from)?;
    for (idx, val) in &bindings {
        stmt.raw_bind_parameter(*idx, val)
            .map_err(KnowledgeError::from)?;
    }
    let mut rows = stmt.raw_query();
    let mut results = Vec::new();
    while let Some(row) = rows.next().map_err(KnowledgeError::from)? {
        results.push(row_to_entry(row).map_err(KnowledgeError::from)?);
    }
    Ok(results)
}

/// Query consolidations using keyword matching.
fn query_consolidations(
    conn: &Connection,
    scope: &str,
    keywords: &[String],
    limit: usize,
) -> Result<Vec<Consolidation>, KnowledgeError> {
    if keywords.is_empty() {
        // No keywords: return most recent consolidations
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {CONSOLIDATION_COLS} FROM knowledge_consolidations \
                 WHERE scope = ?1 ORDER BY created_at DESC LIMIT ?2"
            ))
            .map_err(KnowledgeError::from)?;
        stmt.raw_bind_parameter(1, scope)
            .map_err(KnowledgeError::from)?;
        stmt.raw_bind_parameter(2, limit as i64)
            .map_err(KnowledgeError::from)?;
        let mut rows = stmt.raw_query();
        let mut results = Vec::new();
        while let Some(row) = rows.next().map_err(KnowledgeError::from)? {
            results.push(row_to_consolidation(row).map_err(KnowledgeError::from)?);
        }
        return Ok(results);
    }

    // Build LIKE conditions for each keyword
    let mut like_parts = Vec::new();
    let mut bindings: Vec<(usize, Value)> = vec![(1, Value::Text(scope.to_string()))];
    let mut param_idx = 2usize;
    let mut score_parts = Vec::new();

    for keyword in keywords {
        let pattern = format!("%{}%", keyword);

        let kw_idx = param_idx;
        bindings.push((kw_idx, Value::Text(pattern)));
        param_idx += 1;

        // WHERE: summary OR insight (reuse same param)
        like_parts.push(format!(
            "(summary LIKE ?{kw_idx} OR insight LIKE ?{kw_idx})"
        ));
        // Score: same columns with weights
        score_parts.push(format!(
            "(CASE WHEN summary LIKE ?{kw_idx} THEN 3 ELSE 0 END + \
             CASE WHEN insight LIKE ?{kw_idx} THEN 2 ELSE 0 END)"
        ));
    }

    let where_clause = format!("scope = ?1 AND ({})", like_parts.join(" OR "));

    let score_expr = score_parts.join(" + ");

    let limit_idx = param_idx;
    bindings.push((limit_idx, Value::Integer(limit as i64)));

    let sql = format!(
        "SELECT {CONSOLIDATION_COLS}, ({score_expr}) as _score \
         FROM knowledge_consolidations \
         WHERE {where_clause} \
         ORDER BY _score DESC, created_at DESC, id DESC \
         LIMIT ?{limit_idx}"
    );

    let mut stmt = conn.prepare(&sql).map_err(KnowledgeError::from)?;
    for (idx, val) in &bindings {
        stmt.raw_bind_parameter(*idx, val)
            .map_err(KnowledgeError::from)?;
    }
    let mut rows = stmt.raw_query();
    let mut results = Vec::new();
    while let Some(row) = rows.next().map_err(KnowledgeError::from)? {
        results.push(row_to_consolidation(row).map_err(KnowledgeError::from)?);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::schema;

    /// Create an in-memory SQLite DB with schema initialized.
    fn setup_db() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::init_schema(&mut conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn make_ingest(source: &str, summary: &str) -> IngestRequest {
        IngestRequest {
            source: source.to_string(),
            raw_text: format!("Raw text for: {}", summary),
            summary: summary.to_string(),
            entities: vec!["entity1".to_string()],
            topics: vec!["topic1".to_string()],
            connections: vec![],
            importance: 0.5,
        }
    }

    // ── Ingest + basic retrieval ─────────────────────────────────────────

    #[tokio::test]
    async fn ingest_creates_entry_with_public_id() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        let entry = store
            .ingest("test-scope", make_ingest("user", "Test summary"))
            .await
            .unwrap();

        assert!(!entry.public_id.is_empty());
        assert_eq!(entry.scope, "test-scope");
        assert_eq!(entry.source, "user");
        assert_eq!(entry.summary, "Test summary");
        assert_eq!(
            entry.raw_text.as_deref(),
            Some("Raw text for: Test summary")
        );
        assert!(entry.consolidated_at.is_none());
    }

    #[tokio::test]
    async fn list_unconsolidated_returns_only_unconsolidated() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        store
            .ingest("scope1", make_ingest("src", "Entry 1"))
            .await
            .unwrap();
        store
            .ingest("scope1", make_ingest("src", "Entry 2"))
            .await
            .unwrap();
        let entry3 = store
            .ingest("scope1", make_ingest("src", "Entry 3"))
            .await
            .unwrap();

        // Consolidate entry3
        store
            .consolidate(
                "scope1",
                ConsolidateRequest {
                    source_entry_public_ids: vec![entry3.public_id.clone()],
                    summary: "Consolidated".to_string(),
                    insight: "Insight".to_string(),
                    connections: vec![],
                },
            )
            .await
            .unwrap();

        let unconsolidated = store.list_unconsolidated("scope1", 10).await.unwrap();
        assert_eq!(unconsolidated.len(), 2);
        // Should not contain entry3
        assert!(
            unconsolidated
                .iter()
                .all(|e| e.public_id != entry3.public_id)
        );
    }

    #[tokio::test]
    async fn list_unconsolidated_respects_scope() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        store
            .ingest("scope-a", make_ingest("src", "A"))
            .await
            .unwrap();
        store
            .ingest("scope-b", make_ingest("src", "B"))
            .await
            .unwrap();

        let list_a = store.list_unconsolidated("scope-a", 10).await.unwrap();
        assert_eq!(list_a.len(), 1);
        assert_eq!(list_a[0].scope, "scope-a");

        let list_b = store.list_unconsolidated("scope-b", 10).await.unwrap();
        assert_eq!(list_b.len(), 1);
    }

    // ── List with filters ───────────────────────────────────────────────

    #[tokio::test]
    async fn list_with_topic_filter() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        store
            .ingest(
                "scope",
                IngestRequest {
                    source: "src".to_string(),
                    raw_text: "text".to_string(),
                    summary: "Has rust topic".to_string(),
                    entities: vec![],
                    topics: vec!["rust".to_string(), "programming".to_string()],
                    connections: vec![],
                    importance: 0.5,
                },
            )
            .await
            .unwrap();
        store
            .ingest(
                "scope",
                IngestRequest {
                    source: "src".to_string(),
                    raw_text: "text".to_string(),
                    summary: "Has python topic".to_string(),
                    entities: vec![],
                    topics: vec!["python".to_string()],
                    connections: vec![],
                    importance: 0.5,
                },
            )
            .await
            .unwrap();

        let results = store
            .list(
                "scope",
                KnowledgeFilter {
                    topics: Some(vec!["rust".to_string()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].summary, "Has rust topic");
    }

    // ── Consolidation ───────────────────────────────────────────────────

    #[tokio::test]
    async fn consolidate_marks_entries_and_creates_consolidation() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        let e1 = store
            .ingest("scope", make_ingest("src", "Entry 1"))
            .await
            .unwrap();
        let e2 = store
            .ingest("scope", make_ingest("src", "Entry 2"))
            .await
            .unwrap();

        let consolidation = store
            .consolidate(
                "scope",
                ConsolidateRequest {
                    source_entry_public_ids: vec![e1.public_id.clone(), e2.public_id.clone()],
                    summary: "Combined summary".to_string(),
                    insight: "Key insight from both entries".to_string(),
                    connections: vec!["connection1".to_string()],
                },
            )
            .await
            .unwrap();

        assert!(!consolidation.public_id.is_empty());
        assert_eq!(consolidation.scope, "scope");
        assert_eq!(consolidation.source_entry_public_ids.len(), 2);
        assert_eq!(consolidation.summary, "Combined summary");
        assert_eq!(consolidation.insight, "Key insight from both entries");

        // Entries should now be marked as consolidated
        let unconsolidated = store.list_unconsolidated("scope", 10).await.unwrap();
        assert_eq!(unconsolidated.len(), 0);
    }

    // ── Query semantics ─────────────────────────────────────────────────

    #[tokio::test]
    async fn query_keyword_matches_summary() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        store
            .ingest(
                "scope",
                make_ingest("src", "Rust memory management patterns"),
            )
            .await
            .unwrap();
        store
            .ingest("scope", make_ingest("src", "Python web frameworks"))
            .await
            .unwrap();

        let result = store
            .query("scope", "rust memory", QueryOpts::default())
            .await
            .unwrap();
        assert!(!result.entries.is_empty());
        assert_eq!(result.entries[0].summary, "Rust memory management patterns");
    }

    #[tokio::test]
    async fn query_returns_consolidations_when_enabled() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        let e1 = store
            .ingest("scope", make_ingest("src", "Rust ownership model"))
            .await
            .unwrap();

        store
            .consolidate(
                "scope",
                ConsolidateRequest {
                    source_entry_public_ids: vec![e1.public_id.clone()],
                    summary: "Ownership patterns in Rust".to_string(),
                    insight: "Rust uses ownership for memory safety".to_string(),
                    connections: vec![],
                },
            )
            .await
            .unwrap();

        let result = store
            .query(
                "scope",
                "rust ownership",
                QueryOpts {
                    include_consolidations: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!result.consolidations.is_empty());
    }

    #[tokio::test]
    async fn query_hybrid_mode_boosts_entities() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        // Entry with matching entity
        store
            .ingest(
                "scope",
                IngestRequest {
                    source: "src".to_string(),
                    raw_text: "Some text about coding".to_string(),
                    summary: "General coding notes".to_string(),
                    entities: vec!["tokio".to_string()],
                    topics: vec!["async".to_string()],
                    connections: vec![],
                    importance: 0.9,
                },
            )
            .await
            .unwrap();

        // Entry without matching entity
        store
            .ingest(
                "scope",
                IngestRequest {
                    source: "src".to_string(),
                    raw_text: "tokio runtime details".to_string(),
                    summary: "Other notes".to_string(),
                    entities: vec![],
                    topics: vec![],
                    connections: vec![],
                    importance: 0.1,
                },
            )
            .await
            .unwrap();

        let result = store
            .query(
                "scope",
                "tokio async",
                QueryOpts {
                    retrieval_mode: RetrievalMode::Hybrid,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!result.entries.is_empty());
        // The entry with matching entity+topic and high importance should rank first
        assert_eq!(result.entries[0].summary, "General coding notes");
    }

    #[tokio::test]
    async fn query_empty_question_returns_recent() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        store
            .ingest("scope", make_ingest("src", "Old entry"))
            .await
            .unwrap();
        store
            .ingest("scope", make_ingest("src", "New entry"))
            .await
            .unwrap();

        // Empty question with no keywords
        let result = store
            .query("scope", "", QueryOpts::default())
            .await
            .unwrap();
        assert_eq!(result.entries.len(), 2);
        // Most recent first
        assert_eq!(result.entries[0].summary, "New entry");
    }

    // ── Stats ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stats_returns_correct_counts() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        let e1 = store
            .ingest("scope", make_ingest("src", "Entry 1"))
            .await
            .unwrap();
        store
            .ingest("scope", make_ingest("src", "Entry 2"))
            .await
            .unwrap();
        store
            .ingest("scope", make_ingest("src", "Entry 3"))
            .await
            .unwrap();

        store
            .consolidate(
                "scope",
                ConsolidateRequest {
                    source_entry_public_ids: vec![e1.public_id.clone()],
                    summary: "Consolidated".to_string(),
                    insight: "Insight".to_string(),
                    connections: vec![],
                },
            )
            .await
            .unwrap();

        let stats = store.stats("scope").await.unwrap();
        assert_eq!(stats.total_entries, 3);
        assert_eq!(stats.unconsolidated_entries, 2);
        assert_eq!(stats.total_consolidations, 1);
        assert!(stats.latest_entry_at.is_some());
        assert!(stats.latest_consolidation_at.is_some());
    }

    #[tokio::test]
    async fn stats_empty_scope() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        let stats = store.stats("empty-scope").await.unwrap();
        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.unconsolidated_entries, 0);
        assert_eq!(stats.total_consolidations, 0);
        assert!(stats.latest_entry_at.is_none());
    }

    // ── Retention ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn retention_archive_sets_raw_text_null() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        store
            .ingest("scope", make_ingest("src", "Old entry"))
            .await
            .unwrap();

        let result = store
            .apply_retention(
                "scope",
                &RetentionPolicy {
                    max_age_days: Some(0), // Everything is "old"
                    max_entries: None,
                    archive_raw_text: true,
                },
            )
            .await
            .unwrap();

        assert_eq!(result.archived, 1);
        assert_eq!(result.deleted, 0);

        // Verify raw_text is NULL but summary survives
        let entries = store
            .list(
                "scope",
                KnowledgeFilter {
                    limit: 10,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].raw_text.is_none());
        assert_eq!(entries[0].summary, "Old entry");
    }

    #[tokio::test]
    async fn retention_delete_removes_entries() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        store
            .ingest("scope", make_ingest("src", "Entry to delete"))
            .await
            .unwrap();

        let result = store
            .apply_retention(
                "scope",
                &RetentionPolicy {
                    max_age_days: Some(0),
                    max_entries: None,
                    archive_raw_text: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(result.deleted, 1);

        let stats = store.stats("scope").await.unwrap();
        assert_eq!(stats.total_entries, 0);
    }

    #[tokio::test]
    async fn retention_max_entries_caps_count() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        for i in 0..5 {
            store
                .ingest("scope", make_ingest("src", &format!("Entry {}", i)))
                .await
                .unwrap();
        }

        let result = store
            .apply_retention(
                "scope",
                &RetentionPolicy {
                    max_age_days: None,
                    max_entries: Some(3),
                    archive_raw_text: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(result.deleted, 2);

        let stats = store.stats("scope").await.unwrap();
        assert_eq!(stats.total_entries, 3);
    }

    // ── Ingestion dedup ─────────────────────────────────────────────────

    #[tokio::test]
    async fn ingestion_dedup_prevents_reprocessing() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        assert!(!store.is_source_ingested("scope", "doc:123").await.unwrap());

        store
            .mark_source_ingested("scope", "doc:123")
            .await
            .unwrap();

        assert!(store.is_source_ingested("scope", "doc:123").await.unwrap());

        // Different scope should not collide
        assert!(
            !store
                .is_source_ingested("other-scope", "doc:123")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn mark_source_ingested_is_idempotent() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        store.mark_source_ingested("scope", "key1").await.unwrap();
        // Second call should not error (INSERT OR IGNORE)
        store.mark_source_ingested("scope", "key1").await.unwrap();

        assert!(store.is_source_ingested("scope", "key1").await.unwrap());
    }

    // ── Keyword extraction ──────────────────────────────────────────────

    #[test]
    fn extract_keywords_filters_stop_words() {
        let kw = extract_keywords("what are the patterns I have been working on?");
        assert!(kw.contains(&"patterns".to_string()));
        assert!(kw.contains(&"working".to_string()));
        assert!(!kw.contains(&"the".to_string()));
        assert!(!kw.contains(&"are".to_string()));
        assert!(!kw.contains(&"i".to_string()));
    }

    #[test]
    fn extract_keywords_returns_empty_for_only_stop_words() {
        let kw = extract_keywords("is it a the");
        assert!(kw.is_empty());
    }

    #[test]
    fn extract_keywords_handles_punctuation() {
        let kw = extract_keywords("rust's memory-safe, concurrent!");
        assert!(kw.contains(&"rust".to_string()));
        assert!(kw.contains(&"memory-safe".to_string()));
        assert!(kw.contains(&"concurrent".to_string()));
    }

    // ── Citation contract ───────────────────────────────────────────────

    #[tokio::test]
    async fn query_results_use_public_ids_not_row_ids() {
        let db = setup_db();
        let store = SqliteKnowledgeStore::new(db);

        let entry = store
            .ingest("scope", make_ingest("src", "Test citation"))
            .await
            .unwrap();

        let result = store
            .query("scope", "test citation", QueryOpts::default())
            .await
            .unwrap();

        assert!(!result.entries.is_empty());
        // public_id should be set and match the ingested entry
        assert_eq!(result.entries[0].public_id, entry.public_id);
        // internal id should be 0 (serde skip)
        let json = serde_json::to_string(&result.entries[0]).unwrap();
        assert!(!json.contains("\"id\""));
    }
}

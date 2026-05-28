//! SQLite-backed implementation of [`WorkPacketStore`].

use crate::work_packet::{
    CreateWorkPacket, UpdateWorkPacket, WorkPacket, WorkPacketError, WorkPacketFilter,
    WorkPacketKind, WorkPacketStatus, WorkPacketStore,
};
use async_trait::async_trait;
use rusqlite::{Connection, params};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

pub struct SqliteWorkPacketStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteWorkPacketStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Run a synchronous closure on the connection via tokio::task::spawn_blocking.
    async fn run_blocking<F, R>(&self, f: F) -> Result<R, WorkPacketError>
    where
        F: FnOnce(&mut Connection) -> Result<R, WorkPacketError> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|e| {
                WorkPacketError::DatabaseError(format!("Connection lock poisoned: {}", e))
            })?;
            f(&mut guard)
        })
        .await
        .map_err(|e| WorkPacketError::DatabaseError(format!("Blocking task failed: {}", e)))?
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_dt(dt: &OffsetDateTime) -> String {
    dt.format(&time::format_description::well_known::Iso8601::DEFAULT)
        .unwrap_or_default()
}

fn parse_dt(s: &str) -> Result<OffsetDateTime, WorkPacketError> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Iso8601::DEFAULT)
        .map_err(|e| WorkPacketError::DatabaseError(format!("Invalid datetime: {}", e)))
}

const PACKET_COLS: &str = "\
    id, public_id, scope, kind, status, title, summary, body_markdown, \
    metadata_json, origin_session_id, parent_packet_id, \
    source_delegation_id, target_delegation_id, created_at, updated_at";

fn row_to_packet(row: &rusqlite::Row<'_>) -> Result<WorkPacket, rusqlite::Error> {
    let kind_str: String = row.get(3)?;
    let status_str: String = row.get(4)?;
    let metadata_str: Option<String> = row.get(8)?;

    Ok(WorkPacket {
        id: row.get(0)?,
        public_id: row.get(1)?,
        scope: row.get(2)?,
        kind: WorkPacketKind::from_str(&kind_str).unwrap_or(WorkPacketKind::Plan),
        status: WorkPacketStatus::from_str(&status_str).unwrap_or(WorkPacketStatus::Draft),
        title: row.get(5)?,
        summary: row.get(6)?,
        body_markdown: row.get(7)?,
        metadata_json: metadata_str
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok()),
        origin_session_id: row.get(9)?,
        parent_packet_id: row.get(10)?,
        source_delegation_id: row.get(11)?,
        target_delegation_id: row.get(12)?,
        created_at: parse_dt(&row.get::<_, String>(13)?).unwrap_or(OffsetDateTime::UNIX_EPOCH),
        updated_at: parse_dt(&row.get::<_, String>(14)?).unwrap_or(OffsetDateTime::UNIX_EPOCH),
    })
}

/// Generate a new public id with the `pkt_` prefix.
fn new_public_id() -> String {
    format!("pkt_{}", uuid::Uuid::now_v7().simple())
}

// ---------------------------------------------------------------------------
// Trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl WorkPacketStore for SqliteWorkPacketStore {
    async fn create(&self, input: CreateWorkPacket) -> Result<WorkPacket, WorkPacketError> {
        self.run_blocking(move |conn| {
            let public_id = new_public_id();
            let now = OffsetDateTime::now_utc();
            let now_str = format_dt(&now);
            let metadata_str = input
                .metadata_json
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());

            conn.execute(
                "INSERT INTO work_packets \
                     (public_id, scope, kind, status, title, summary, body_markdown, \
                      metadata_json, origin_session_id, parent_packet_id, \
                      source_delegation_id, target_delegation_id, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    public_id,
                    input.scope,
                    input.kind.as_str(),
                    WorkPacketStatus::Draft.as_str(), // new packets start as draft
                    input.title,
                    input.summary,
                    input.body_markdown,
                    metadata_str,
                    input.origin_session_id,
                    input.parent_packet_id,
                    input.source_delegation_id,
                    input.target_delegation_id,
                    now_str,
                    now_str,
                ],
            )?;

            let row_id = conn.last_insert_rowid();

            let packet = conn.query_row(
                &format!("SELECT {PACKET_COLS} FROM work_packets WHERE id = ?1"),
                params![row_id],
                row_to_packet,
            )?;

            Ok(packet)
        })
        .await
    }

    async fn load(&self, public_id: &str) -> Result<WorkPacket, WorkPacketError> {
        let pid = public_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                &format!("SELECT {PACKET_COLS} FROM work_packets WHERE public_id = ?1"),
                params![pid],
                row_to_packet,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => WorkPacketError::NotFound(pid.clone()),
                other => WorkPacketError::from(other),
            })
        })
        .await
    }

    async fn search(
        &self,
        query: &str,
        filter: &WorkPacketFilter,
    ) -> Result<Vec<WorkPacket>, WorkPacketError> {
        let query = query.to_string();
        let filter = filter.clone();
        self.run_blocking(move |conn| {
            if query.trim().is_empty() {
                // No full-text query — fall back to filtered list
                return fetch_filtered(conn, &filter);
            }

            // Build FTS query joining back to the main table
            let mut sql = format!(
                "SELECT {PACKET_COLS_QUALIFIED} \
                 FROM work_packets p \
                 JOIN work_packets_fts f ON f.rowid = p.id \
                 WHERE work_packets_fts MATCH ?1"
            );
            let mut param_idx = 2u32;
            let mut where_clauses = Vec::new();

            if filter.scope.is_some() {
                where_clauses.push(format!("p.scope = ?{param_idx}"));
                param_idx += 1;
            }
            if filter.kind.is_some() {
                where_clauses.push(format!("p.kind = ?{param_idx}"));
                param_idx += 1;
            }
            if filter.status.is_some() {
                where_clauses.push(format!("p.status = ?{param_idx}"));
                param_idx += 1;
            }
            if filter.parent_packet_id.is_some() {
                where_clauses.push(format!("p.parent_packet_id = ?{param_idx}"));
                param_idx += 1;
            }

            if !where_clauses.is_empty() {
                sql.push_str(" AND ");
                sql.push_str(&where_clauses.join(" AND "));
            }

            sql.push_str(&format!(
                " ORDER BY rank LIMIT ?{param_idx} OFFSET ?{}",
                param_idx + 1
            ));

            let mut stmt = conn.prepare(&sql)?;

            // Bind params dynamically
            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(query)];
            if let Some(ref scope) = filter.scope {
                param_values.push(Box::new(scope.clone()));
            }
            if let Some(ref kind) = filter.kind {
                param_values.push(Box::new(kind.as_str().to_string()));
            }
            if let Some(ref status) = filter.status {
                param_values.push(Box::new(status.as_str().to_string()));
            }
            if let Some(ref parent) = filter.parent_packet_id {
                param_values.push(Box::new(parent.clone()));
            }
            param_values.push(Box::new(filter.limit as i64));
            param_values.push(Box::new(filter.offset as i64));

            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                param_values.iter().map(|p| p.as_ref()).collect();

            let packets = stmt
                .query_map(param_refs.as_slice(), row_to_packet)?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(packets)
        })
        .await
    }

    async fn list(&self, filter: &WorkPacketFilter) -> Result<Vec<WorkPacket>, WorkPacketError> {
        let filter = filter.clone();
        self.run_blocking(move |conn| fetch_filtered(conn, &filter))
            .await
    }

    async fn update(
        &self,
        public_id: &str,
        update: UpdateWorkPacket,
    ) -> Result<WorkPacket, WorkPacketError> {
        let pid = public_id.to_string();
        self.run_blocking(move |conn| {
            let now_str = format_dt(&OffsetDateTime::now_utc());

            let mut set_clauses = Vec::new();
            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            let mut idx = 1u32;

            if let Some(ref kind) = update.kind {
                set_clauses.push(format!("kind = ?{idx}"));
                param_values.push(Box::new(kind.as_str().to_string()));
                idx += 1;
            }
            if let Some(ref status) = update.status {
                set_clauses.push(format!("status = ?{idx}"));
                param_values.push(Box::new(status.as_str().to_string()));
                idx += 1;
            }
            if let Some(ref title) = update.title {
                set_clauses.push(format!("title = ?{idx}"));
                param_values.push(Box::new(title.clone()));
                idx += 1;
            }
            if let Some(ref summary) = update.summary {
                set_clauses.push(format!("summary = ?{idx}"));
                param_values.push(Box::new(summary.clone()));
                idx += 1;
            }
            if let Some(ref body) = update.body_markdown {
                set_clauses.push(format!("body_markdown = ?{idx}"));
                param_values.push(Box::new(body.clone()));
                idx += 1;
            }
            if let Some(ref metadata) = update.metadata_json {
                set_clauses.push(format!("metadata_json = ?{idx}"));
                param_values.push(Box::new(
                    serde_json::to_string(metadata).unwrap_or_default(),
                ));
                idx += 1;
            }
            if let Some(ref parent) = update.parent_packet_id {
                set_clauses.push(format!("parent_packet_id = ?{idx}"));
                param_values.push(Box::new(parent.clone()));
                idx += 1;
            }
            if let Some(ref source) = update.source_delegation_id {
                set_clauses.push(format!("source_delegation_id = ?{idx}"));
                param_values.push(Box::new(source.clone()));
                idx += 1;
            }
            if let Some(ref target) = update.target_delegation_id {
                set_clauses.push(format!("target_delegation_id = ?{idx}"));
                param_values.push(Box::new(target.clone()));
                idx += 1;
            }

            if set_clauses.is_empty() {
                // Nothing to update — just return current packet
                return conn
                    .query_row(
                        &format!("SELECT {PACKET_COLS} FROM work_packets WHERE public_id = ?1"),
                        params![pid],
                        row_to_packet,
                    )
                    .map_err(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => {
                            WorkPacketError::NotFound(pid.clone())
                        }
                        other => WorkPacketError::from(other),
                    });
            }

            // Always update updated_at
            set_clauses.push(format!("updated_at = ?{idx}"));
            param_values.push(Box::new(now_str));

            let sql = format!(
                "UPDATE work_packets SET {} WHERE public_id = ?{}",
                set_clauses.join(", "),
                idx + 1
            );
            param_values.push(Box::new(pid.clone()));

            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                param_values.iter().map(|p| p.as_ref()).collect();

            let rows_affected = conn.execute(&sql, param_refs.as_slice())?;
            if rows_affected == 0 {
                return Err(WorkPacketError::NotFound(pid.clone()));
            }

            // Return updated packet
            conn.query_row(
                &format!("SELECT {PACKET_COLS} FROM work_packets WHERE public_id = ?1"),
                params![pid],
                row_to_packet,
            )
            .map_err(WorkPacketError::from)
        })
        .await
    }

    async fn link(
        &self,
        child_public_id: &str,
        parent_public_id: &str,
    ) -> Result<(), WorkPacketError> {
        let child = child_public_id.to_string();
        let parent = parent_public_id.to_string();
        self.run_blocking(move |conn| {
            let now_str = format_dt(&OffsetDateTime::now_utc());
            let rows = conn.execute(
                "UPDATE work_packets SET parent_packet_id = ?1, updated_at = ?2 WHERE public_id = ?3",
                params![parent, now_str, child],
            )?;
            if rows == 0 {
                return Err(WorkPacketError::NotFound(child.clone()));
            }
            Ok(())
        })
        .await
    }

    async fn delete(&self, public_id: &str) -> Result<(), WorkPacketError> {
        let pid = public_id.to_string();
        self.run_blocking(move |conn| {
            let rows = conn.execute(
                "DELETE FROM work_packets WHERE public_id = ?1",
                params![pid],
            )?;
            if rows == 0 {
                return Err(WorkPacketError::NotFound(pid.clone()));
            }
            Ok(())
        })
        .await
    }

    async fn set_active_packet(
        &self,
        session_public_id: &str,
        packet_public_id: Option<&str>,
    ) -> Result<(), WorkPacketError> {
        let session_id = session_public_id.to_string();
        let pkt_id = packet_public_id.map(|s| s.to_string());
        self.run_blocking(move |conn| {
            let now_str = format_dt(&OffsetDateTime::now_utc());

            // Upsert: delete existing then insert
            conn.execute(
                "DELETE FROM active_work_packets WHERE session_public_id = ?1",
                params![session_id],
            )?;

            if let Some(ref pid) = pkt_id {
                conn.execute(
                    "INSERT INTO active_work_packets (session_public_id, packet_public_id, set_at) \
                     VALUES (?1, ?2, ?3)",
                    params![session_id, pid, now_str],
                )?;
            }

            Ok(())
        })
        .await
    }

    async fn get_active_packet(
        &self,
        session_public_id: &str,
    ) -> Result<Option<String>, WorkPacketError> {
        let session_id = session_public_id.to_string();
        self.run_blocking(move |conn| {
            let result = conn.query_row(
                "SELECT packet_public_id FROM active_work_packets WHERE session_public_id = ?1",
                params![session_id],
                |row| row.get::<_, String>(0),
            );

            match result {
                Ok(pid) => Ok(Some(pid)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(WorkPacketError::from(e)),
            }
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Filtered query helper (no FTS)
// ---------------------------------------------------------------------------

/// Column list qualified with table alias `p.`
const PACKET_COLS_QUALIFIED: &str = "\
    p.id, p.public_id, p.scope, p.kind, p.status, p.title, p.summary, p.body_markdown, \
    p.metadata_json, p.origin_session_id, p.parent_packet_id, \
    p.source_delegation_id, p.target_delegation_id, p.created_at, p.updated_at";

fn fetch_filtered(
    conn: &mut Connection,
    filter: &WorkPacketFilter,
) -> Result<Vec<WorkPacket>, WorkPacketError> {
    let mut sql = format!("SELECT {PACKET_COLS_QUALIFIED} FROM work_packets p WHERE 1=1");
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1u32;

    if let Some(ref scope) = filter.scope {
        sql.push_str(&format!(" AND p.scope = ?{idx}"));
        param_values.push(Box::new(scope.clone()));
        idx += 1;
    }
    if let Some(ref kind) = filter.kind {
        sql.push_str(&format!(" AND p.kind = ?{idx}"));
        param_values.push(Box::new(kind.as_str().to_string()));
        idx += 1;
    }
    if let Some(ref status) = filter.status {
        sql.push_str(&format!(" AND p.status = ?{idx}"));
        param_values.push(Box::new(status.as_str().to_string()));
        idx += 1;
    }
    if let Some(ref parent) = filter.parent_packet_id {
        sql.push_str(&format!(" AND p.parent_packet_id = ?{idx}"));
        param_values.push(Box::new(parent.clone()));
        idx += 1;
    }
    if let Some(ref session) = filter.origin_session_id {
        sql.push_str(&format!(" AND p.origin_session_id = ?{idx}"));
        param_values.push(Box::new(session.clone()));
        idx += 1;
    }

    sql.push_str(&format!(
        " ORDER BY p.updated_at DESC LIMIT ?{idx} OFFSET ?",
    ));
    param_values.push(Box::new(filter.limit as i64));
    param_values.push(Box::new(filter.offset as i64));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();

    let packets = stmt
        .query_map(param_refs.as_slice(), row_to_packet)?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(packets)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::schema;
    use std::sync::{Arc, Mutex};

    fn setup_db() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().expect("open in-memory sqlite");
        schema::init_schema(&mut conn).expect("initialize schema");
        Arc::new(Mutex::new(conn))
    }

    fn make_create(title: &str, kind: WorkPacketKind) -> CreateWorkPacket {
        CreateWorkPacket {
            scope: "test_scope".to_string(),
            kind,
            title: title.to_string(),
            summary: format!("Summary for {}", title),
            body_markdown: format!("## {}\n\nBody content for {}.", title, title),
            metadata_json: None,
            origin_session_id: Some("sess_abc".to_string()),
            parent_packet_id: None,
            source_delegation_id: None,
            target_delegation_id: None,
        }
    }

    #[tokio::test]
    async fn create_and_load() {
        let store = SqliteWorkPacketStore::new(setup_db());
        let created = store
            .create(make_create("Test Plan", WorkPacketKind::Plan))
            .await
            .unwrap();
        assert!(created.public_id.starts_with("pkt_"));
        assert_eq!(created.kind, WorkPacketKind::Plan);
        assert_eq!(created.status, WorkPacketStatus::Draft);
        assert_eq!(created.title, "Test Plan");

        let loaded = store.load(&created.public_id).await.unwrap();
        assert_eq!(loaded.title, "Test Plan");
        assert_eq!(loaded.body_markdown, loaded.body_markdown);
    }

    #[tokio::test]
    async fn load_nonexistent() {
        let store = SqliteWorkPacketStore::new(setup_db());
        let result = store.load("pkt_nonexistent").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), WorkPacketError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_status_and_title() {
        let store = SqliteWorkPacketStore::new(setup_db());
        let created = store
            .create(make_create("Original", WorkPacketKind::Plan))
            .await
            .unwrap();

        let updated = store
            .update(
                &created.public_id,
                UpdateWorkPacket {
                    status: Some(WorkPacketStatus::Ready),
                    title: Some("Updated Title".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.status, WorkPacketStatus::Ready);
        assert_eq!(updated.title, "Updated Title");
        assert!(updated.updated_at >= created.updated_at);
    }

    #[tokio::test]
    async fn update_nonexistent() {
        let store = SqliteWorkPacketStore::new(setup_db());
        let result = store
            .update(
                "pkt_nonexistent",
                UpdateWorkPacket {
                    title: Some("x".to_string()),
                    ..Default::default()
                },
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn link_packets() {
        let store = SqliteWorkPacketStore::new(setup_db());
        let parent = store
            .create(make_create("Parent Plan", WorkPacketKind::Plan))
            .await
            .unwrap();
        let child = store
            .create(make_create("Checkpoint 1", WorkPacketKind::Checkpoint))
            .await
            .unwrap();

        store
            .link(&child.public_id, &parent.public_id)
            .await
            .unwrap();

        let loaded = store.load(&child.public_id).await.unwrap();
        assert_eq!(
            loaded.parent_packet_id.as_deref(),
            Some(parent.public_id.as_str())
        );
    }

    #[tokio::test]
    async fn delete_packet() {
        let store = SqliteWorkPacketStore::new(setup_db());
        let created = store
            .create(make_create("ToDelete", WorkPacketKind::Plan))
            .await
            .unwrap();
        store.delete(&created.public_id).await.unwrap();
        assert!(store.load(&created.public_id).await.is_err());
    }

    #[tokio::test]
    async fn list_with_filter() {
        let store = SqliteWorkPacketStore::new(setup_db());
        store
            .create(make_create("Plan A", WorkPacketKind::Plan))
            .await
            .unwrap();
        store
            .create(make_create("Handoff B", WorkPacketKind::Handoff))
            .await
            .unwrap();
        store
            .create(make_create("Plan C", WorkPacketKind::Plan))
            .await
            .unwrap();

        let plans = store
            .list(&WorkPacketFilter {
                kind: Some(WorkPacketKind::Plan),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(plans.len(), 2);

        let handoffs = store
            .list(&WorkPacketFilter {
                kind: Some(WorkPacketKind::Handoff),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(handoffs.len(), 1);
    }

    #[tokio::test]
    async fn search_by_text() {
        let store = SqliteWorkPacketStore::new(setup_db());
        store
            .create(CreateWorkPacket {
                scope: "test".to_string(),
                kind: WorkPacketKind::Plan,
                title: "Structured plan storage".to_string(),
                summary: "A plan for adding work packet storage".to_string(),
                body_markdown: "## Implementation\n\nAdd SQLite table and FTS index.".to_string(),
                metadata_json: None,
                origin_session_id: None,
                parent_packet_id: None,
                source_delegation_id: None,
                target_delegation_id: None,
            })
            .await
            .unwrap();
        store
            .create(CreateWorkPacket {
                scope: "test".to_string(),
                kind: WorkPacketKind::Plan,
                title: "Unrelated plan".to_string(),
                summary: "Something completely different".to_string(),
                body_markdown: "No relevant keywords here.".to_string(),
                metadata_json: None,
                origin_session_id: None,
                parent_packet_id: None,
                source_delegation_id: None,
                target_delegation_id: None,
            })
            .await
            .unwrap();

        let results = store
            .search("work packet storage", &WorkPacketFilter::default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Structured plan storage");
    }

    #[tokio::test]
    async fn active_packet_roundtrip() {
        let store = SqliteWorkPacketStore::new(setup_db());
        let pkt = store
            .create(make_create("Active", WorkPacketKind::Plan))
            .await
            .unwrap();

        // No active packet initially
        let active = store.get_active_packet("sess_123").await.unwrap();
        assert!(active.is_none());

        // Set active
        store
            .set_active_packet("sess_123", Some(&pkt.public_id))
            .await
            .unwrap();
        let active = store.get_active_packet("sess_123").await.unwrap();
        assert_eq!(active.as_deref(), Some(pkt.public_id.as_str()));

        // Clear active
        store.set_active_packet("sess_123", None).await.unwrap();
        let active = store.get_active_packet("sess_123").await.unwrap();
        assert!(active.is_none());
    }

    #[tokio::test]
    async fn metadata_json_roundtrip() {
        let store = SqliteWorkPacketStore::new(setup_db());
        let meta = serde_json::json!({
            "phases": ["phase1", "phase2"],
            "files": ["src/main.rs"],
            "open_questions": 3
        });

        let created = store
            .create(CreateWorkPacket {
                scope: "test".to_string(),
                kind: WorkPacketKind::Plan,
                title: "With metadata".to_string(),
                summary: "Plan with JSON metadata".to_string(),
                body_markdown: "Body".to_string(),
                metadata_json: Some(meta.clone()),
                origin_session_id: None,
                parent_packet_id: None,
                source_delegation_id: None,
                target_delegation_id: None,
            })
            .await
            .unwrap();

        let loaded = store.load(&created.public_id).await.unwrap();
        assert_eq!(loaded.metadata_json, Some(meta));
    }
}

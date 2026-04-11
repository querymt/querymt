use async_trait::async_trait;
use rusqlite::params;
use uuid::Uuid;

use crate::events::{AgentEventKind, DurableEvent, EventOrigin};
use crate::session::error::{SessionError, SessionResult};
use crate::session::projection::{EventJournal, NewDurableEvent};

use super::SqliteStorage;

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
            let stream_seq: i64 = conn.query_row(
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
        after_seq: Option<i64>,
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
        after_seq: Option<i64>,
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

    async fn delete_session_events_from(
        &self,
        session_id: &str,
        from_seq: i64,
    ) -> SessionResult<usize> {
        let session_id = session_id.to_string();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<usize, rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();
            let deleted = conn.execute(
                "DELETE FROM event_journal WHERE session_id = ? AND stream_seq >= ?",
                params![session_id, from_seq],
            )?;
            Ok(deleted)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }
}

fn parse_journal_row(row: &rusqlite::Row) -> Result<DurableEvent, rusqlite::Error> {
    let event_id: String = row.get(0)?;
    let stream_seq: i64 = row.get(1)?;
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

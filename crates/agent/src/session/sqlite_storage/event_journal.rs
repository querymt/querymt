use async_trait::async_trait;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::events::{AgentEventKind, DurableEvent, EventOrigin};
use crate::session::error::{SessionError, SessionResult};
use crate::session::projection::{
    EventJournal, NewDurableEvent, RunLifecycle, last_unfinished_run_in,
};

use super::SqliteStorage;

#[async_trait]
impl EventJournal for SqliteStorage {
    async fn append_durable(&self, event: &NewDurableEvent) -> SessionResult<DurableEvent> {
        let event_clone = event.clone();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> SessionResult<DurableEvent> {
            let conn = conn_arc.lock().unwrap();

            insert_journal_event(
                &conn,
                &event_clone,
                event_clone.source_node_id.as_deref(),
                event_clone.source_seq,
            )?
            .ok_or_else(|| {
                SessionError::DatabaseError(
                    "duplicate source-identity event rejected by the journal unique index"
                        .to_string(),
                )
            })
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
    }

    async fn append_durable_batch(
        &self,
        events: &[NewDurableEvent],
    ) -> Vec<SessionResult<DurableEvent>> {
        if events.is_empty() {
            return Vec::new();
        }
        let batch = events.to_vec();
        let count = batch.len();
        let conn_arc = self.conn.clone();

        let joined = tokio::task::spawn_blocking(move || {
            let mut conn = conn_arc.lock().unwrap();
            insert_batch(&mut conn, &batch)
        })
        .await;
        match joined {
            Ok(results) => results,
            Err(e) => (0..count)
                .map(|_| Err(SessionError::Other(format!("Task execution failed: {}", e))))
                .collect(),
        }
    }

    fn max_session_seq_blocking(&self, session_id: &str) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            // Local numbering only: relayed rows carry the host's numbers.
            // The `session_seq IS NOT NULL` term lets SQLite answer MAX from
            // the partial unique index instead of scanning the session's rows.
            "SELECT COALESCE(MAX(session_seq), 0) FROM event_journal \
             WHERE session_id = ?1 AND source_node_id IS NULL AND session_seq IS NOT NULL",
            params![session_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|err| {
            log::warn!("failed to read max session_seq for {session_id}: {err}");
            0
        })
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
            // SQLite's negative LIMIT means unlimited; explicit limits never wrap.
            let lim = limit.map_or(-1, |limit| i64::try_from(limit).unwrap_or(i64::MAX));

            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, payload_json, session_seq \
                 FROM event_journal \
                 WHERE session_id = ? AND stream_seq > ? \
                 ORDER BY stream_seq ASC \
                 LIMIT ?",
            )?;

            // Read in index order (no temp sort), then restore production
            // order only if the journal holds out-of-order sequenced rows.
            let mut events = stmt
                .query_map(params![session_id, after, lim], parse_journal_row)?
                .collect::<Result<Vec<_>, _>>()?;
            restore_production_order(&mut events);

            Ok(events)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    async fn load_delegation_terminals(
        &self,
        session_id: &str,
        delegation_ids: &[String],
    ) -> SessionResult<Vec<DurableEvent>> {
        if delegation_ids.is_empty() {
            return Ok(Vec::new());
        }
        let session_id = session_id.to_string();
        let ids = serde_json::to_string(delegation_ids)
            .map_err(|err| SessionError::Other(err.to_string()))?;
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<DurableEvent>, rusqlite::Error> {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, payload_json, session_seq \
                 FROM event_journal WHERE session_id = ?1 \
                 AND kind IN ('delegation_completed', 'delegation_failed', 'delegation_cancelled') \
                 AND json_extract(payload_json, '$.data.delegation_id') IN (SELECT value FROM json_each(?2)) \
                 ORDER BY stream_seq ASC"
            )?;
            stmt.query_map(params![session_id, ids], parse_journal_row)?.collect()
        }).await.map_err(|err| SessionError::Other(err.to_string()))?.map_err(SessionError::from)
    }

    async fn last_unfinished_run(&self, session_id: &str) -> SessionResult<Option<String>> {
        let session_id = session_id.to_string();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<Option<String>, rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();
            // Only run lifecycle rows are decoded; the partial run lifecycle
            // index bounds the scan to this session. Only this node's own
            // events count: mirrored events of a remote host describe runs
            // that host owns and closes, so they must never look orphaned
            // here (even after the session's bookmark is gone).
            let mut stmt = conn.prepare(
                "SELECT kind, json_extract(payload_json, '$.data.run_id') \
                 FROM event_journal \
                 WHERE session_id = ? AND kind IN ('run_started', 'run_completed') \
                 AND origin = 'local' \
                 ORDER BY stream_seq ASC",
            )?;
            let rows = stmt
                .query_map(params![session_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(last_unfinished_run_in(rows.into_iter().filter_map(
                |(kind, run_id)| {
                    let run_id = run_id?;
                    match kind.as_str() {
                        "run_started" => Some((RunLifecycle::Started, run_id)),
                        "run_completed" => Some((RunLifecycle::Completed, run_id)),
                        _ => None,
                    }
                },
            )))
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    async fn load_session_page(
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
            let lim = limit.map_or(-1, |limit| i64::try_from(limit).unwrap_or(i64::MAX));
            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, payload_json, session_seq \
                 FROM event_journal \
                 WHERE session_id = ? AND stream_seq > ? \
                 ORDER BY stream_seq ASC \
                 LIMIT ?",
            )?;
            stmt.query_map(params![session_id, after, lim], parse_journal_row)?
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    async fn load_session_range(
        &self,
        session_id: &str,
        after_session_seq: i64,
        up_to_session_seq: Option<i64>,
    ) -> SessionResult<Vec<DurableEvent>> {
        let session_id = session_id.to_string();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<DurableEvent>, rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();
            let upper = up_to_session_seq.unwrap_or(i64::MAX);
            // Served by idx_event_journal_session_order, already in order.
            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, payload_json, session_seq \
                 FROM event_journal \
                 WHERE session_id = ? AND session_seq > ? AND session_seq <= ? \
                 ORDER BY session_seq ASC",
            )?;
            stmt.query_map(
                params![session_id, after_session_seq, upper],
                parse_journal_row,
            )?
            .collect::<Result<Vec<_>, _>>()
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
            // SQLite's negative LIMIT means unlimited; explicit limits never wrap.
            let lim = limit.map_or(-1, |limit| i64::try_from(limit).unwrap_or(i64::MAX));

            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, payload_json, session_seq \
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
            // Conversation-tail deletion must not erase incurred spend or the
            // evidence that generation happened without an accounting basis:
            // * attempt start/settlement records are the accounting ledger;
            // * a request-end without `accounting` is a legacy generation
            //   record whose cost cannot be reconstructed, so it is kept to
            //   keep historical coverage honestly incomplete.
            // Retained rows keep their numbers; the resulting gaps are valid.
            let deleted = conn.execute(
                "DELETE FROM event_journal \
                 WHERE session_id = ? AND stream_seq >= ? \
                   AND kind NOT IN ('llm_attempt_started', 'llm_attempt_settled') \
                   AND NOT (kind = 'llm_request_end' \
                            AND json_extract(payload_json, '$.data.accounting') IS NULL)",
                params![session_id, from_seq],
            )?;
            Ok(deleted)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    async fn append_durable_from_source(
        &self,
        event: &NewDurableEvent,
    ) -> SessionResult<Option<DurableEvent>> {
        let (source_node_id, source_seq) = match (&event.source_node_id, event.source_seq) {
            (Some(node_id), Some(seq)) => (node_id.clone(), seq),
            _ => {
                return Err(SessionError::Other(
                    "append_durable_from_source requires source_node_id and source_seq; \
                     use append_durable for events without source identity"
                        .to_string(),
                ));
            }
        };
        let event_clone = event.clone();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> SessionResult<Option<DurableEvent>> {
            // Duplicate replay is a no-op (plan §15/§16): no insert and no
            // stream_seq allocation. The source-existence check, sequence
            // allocation, and insert all run inside one Immediate transaction
            // so concurrent writers serialize at the database level (not just
            // in-process via the connection mutex); the partial unique index
            // (idx_event_journal_source_identity) remains the durable backstop.
            let mut conn = conn_arc.lock().unwrap();
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

            let existing: Option<i64> = tx
                .query_row(
                    "SELECT stream_seq FROM event_journal \
                     WHERE session_id = ?1 AND source_node_id = ?2 AND source_seq = ?3",
                    params![event_clone.session_id, source_node_id, source_seq],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                return Ok(None);
            }

            let inserted =
                insert_journal_event(&tx, &event_clone, Some(&source_node_id), Some(source_seq))?;
            if inserted.is_none() {
                // Lost the race against a concurrent duplicate writer: dropping
                // the transaction rolls back (releasing the allocated sequence)
                // and the replay reports no-op.
                return Ok(None);
            }
            tx.commit()?;
            Ok(inserted)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
    }

    async fn latest_source_seq(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Option<i64>> {
        let session_id = session_id.to_string();
        let source_node_id = source_node_id.to_string();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<Option<i64>, rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();
            conn.query_row(
                "SELECT MAX(source_seq) FROM event_journal \
                 WHERE session_id = ?1 AND source_node_id = ?2",
                params![session_id, source_node_id],
                |row| row.get(0),
            )
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }

    async fn remote_sync_cursor(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Option<i64>> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            conn.lock().unwrap().query_row(
                "SELECT source_seq FROM remote_session_sync WHERE session_id = ?1 AND source_node_id = ?2",
                params![session_id, source_node_id],
                |row| row.get(0),
            ).optional().map_err(SessionError::from)
        }).await.map_err(|e| SessionError::Other(e.to_string()))?
    }

    async fn advance_remote_sync_cursor(
        &self,
        session_id: &str,
        source_node_id: &str,
        source_seq: i64,
        complete: bool,
    ) -> SessionResult<()> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            conn.lock().unwrap().execute(
                "INSERT INTO remote_session_sync (session_id, source_node_id, source_seq, complete)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (session_id, source_node_id) DO UPDATE SET
                 source_seq = MAX(source_seq, excluded.source_seq),
                 complete = MAX(complete, excluded.complete)",
                params![session_id, source_node_id, source_seq, complete],
            ).map(|_| ()).map_err(SessionError::from)
        })
        .await
        .map_err(|e| SessionError::Other(e.to_string()))?
    }

    async fn load_remote_session_stream(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Vec<DurableEvent>> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            // Legacy rows remain intact. Once an authoritative snapshot is
            // complete, prefer it over cursorless copies of the same history.
            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, payload_json, session_seq
                 FROM event_journal WHERE session_id = ?1 AND (
                     source_node_id = ?2 OR (source_node_id IS NULL AND NOT EXISTS (
                         SELECT 1 FROM remote_session_sync
                         WHERE session_id = ?1 AND source_node_id = ?2 AND complete = 1
                     ))
                 ) ORDER BY CASE WHEN source_seq IS NULL THEN 0 ELSE 1 END, source_seq, stream_seq",
            )?;
            stmt.query_map(params![session_id, source_node_id], parse_journal_row)?
                .collect::<Result<Vec<_>, _>>().map_err(SessionError::from)
        }).await.map_err(|e| SessionError::Other(e.to_string()))?
    }

    async fn max_stream_seq(&self, session_id: &str) -> SessionResult<i64> {
        let session_id = session_id.to_string();
        let conn_arc = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<i64, rusqlite::Error> {
            let conn = conn_arc.lock().unwrap();
            conn.query_row(
                "SELECT COALESCE(MAX(stream_seq), 0) FROM event_journal WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }
}

/// Shared event-journal insertion: derives the storage columns from the event,
/// allocates the next stream sequence, and inserts the row. Returns `None`
/// when the partial source-identity unique index
/// (`idx_event_journal_source_identity`) suppressed the insert as a duplicate
/// replay of `(session_id, source_node_id, source_seq)`; unrelated constraint
/// failures still propagate.
fn insert_journal_event(
    conn: &rusqlite::Connection,
    event: &NewDurableEvent,
    source_node_id: Option<&str>,
    source_seq: Option<i64>,
) -> Result<Option<DurableEvent>, rusqlite::Error> {
    // Unset producer sequence is stored as NULL, never 0, so the unique
    // (session_id, session_seq) index only constrains real sequence numbers.
    let session_seq = event.session_seq.filter(|seq| *seq > 0);
    let kind_tag = serde_json::to_value(&event.kind)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
        .unwrap_or_else(|| "unknown".to_string());

    let payload_json = serde_json::to_string(&event.kind)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

    let origin_str = match &event.origin {
        EventOrigin::Local => "local",
        EventOrigin::Remote => "remote",
        EventOrigin::Unknown(s) => s.as_str(),
    };

    let event_id = Uuid::now_v7().to_string();
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();

    // Atomically allocate the next stream_seq and insert the event. The
    // conflict target matches idx_event_journal_source_identity so duplicate
    // replays are skipped here instead of surfacing as constraint errors.
    let stream_seq: i64 = conn.query_row(
        "UPDATE event_journal_seq SET next_seq = next_seq + 1 WHERE id = 1 RETURNING next_seq - 1",
        [],
        |row| row.get(0),
    )?;

    let inserted = conn.execute(
        "INSERT INTO event_journal \
         (event_id, stream_seq, session_id, timestamp, origin, source_node, source_node_id, source_seq, kind, payload_json, session_seq) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (session_id, source_node_id, source_seq) \
         WHERE source_node_id IS NOT NULL AND source_seq IS NOT NULL DO NOTHING",
        params![
            event_id,
            stream_seq,
            event.session_id,
            timestamp,
            origin_str,
            event.source_node,
            source_node_id,
            source_seq,
            kind_tag,
            payload_json,
            session_seq,
        ],
    )?;
    if inserted == 0 {
        return Ok(None);
    }

    Ok(Some(DurableEvent {
        event_id,
        stream_seq,
        session_seq: session_seq.unwrap_or(0),
        session_id: event.session_id.clone(),
        timestamp,
        origin: event.origin.clone(),
        source_node: event.source_node.clone(),
        kind: event.kind.clone(),
    }))
}

/// Reorder events read in `stream_seq` (insert) order into producer
/// `session_seq` order.
///
/// Rows with `session_seq == 0` (legacy or unsequenced annotations) keep their
/// insert-order slot; the sequenced rows are sorted among the slots they
/// occupy. Already ordered input (the common case) is left untouched without
/// allocating.
fn restore_production_order(events: &mut [DurableEvent]) {
    let is_ordered = events
        .iter()
        .filter(|event| event.session_seq > 0)
        .map(|event| event.session_seq)
        .is_sorted();
    if is_ordered {
        return;
    }
    let slots: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| (event.session_seq > 0).then_some(index))
        .collect();
    let mut sequenced: Vec<DurableEvent> = slots.iter().map(|&i| events[i].clone()).collect();
    sequenced.sort_by_key(|event| (event.session_seq, event.stream_seq));
    for (slot, event) in slots.into_iter().zip(sequenced) {
        events[slot] = event;
    }
}

/// Persist a batch in one transaction. If any row fails, the transaction is
/// rolled back and each event is retried on its own so one bad event cannot
/// fail its neighbours.
fn insert_batch(
    conn: &mut rusqlite::Connection,
    events: &[NewDurableEvent],
) -> Vec<SessionResult<DurableEvent>> {
    let batched = (|| -> Result<Vec<DurableEvent>, rusqlite::Error> {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut persisted = Vec::with_capacity(events.len());
        for event in events {
            let inserted = insert_journal_event(
                &tx,
                event,
                event.source_node_id.as_deref(),
                event.source_seq,
            )?;
            persisted.push(inserted.ok_or(rusqlite::Error::StatementChangedRows(0))?);
        }
        tx.commit()?;
        Ok(persisted)
    })();
    match batched {
        Ok(persisted) => persisted.into_iter().map(Ok).collect(),
        Err(batch_error) if events.len() == 1 => vec![Err(SessionError::from(batch_error))],
        Err(batch_error) => {
            log::debug!("journal batch failed, retrying events individually: {batch_error}");
            events
                .iter()
                .map(|event| {
                    insert_journal_event(
                        conn,
                        event,
                        event.source_node_id.as_deref(),
                        event.source_seq,
                    )
                    .map_err(SessionError::from)
                    .and_then(|inserted| {
                        inserted.ok_or_else(|| {
                            SessionError::DatabaseError(
                                "duplicate source-identity event rejected by the journal unique index"
                                    .to_string(),
                            )
                        })
                    })
                })
                .collect()
        }
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
    let session_seq: Option<i64> = row.get(7)?;

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
        session_seq: session_seq.unwrap_or(0),
        session_id,
        timestamp,
        origin,
        source_node,
        kind,
    })
}

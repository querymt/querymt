use async_trait::async_trait;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use crate::events::{AgentEventKind, DurableEvent, EventOrigin};
use crate::session::error::{SessionError, SessionResult};
use crate::session::projection::{
    EventJournal, HistoryPageDirection, NewDurableEvent, RunLifecycle, last_unfinished_run_in,
    slice_history_page,
};

use super::{SqliteStorage, blocking};

#[async_trait]
impl EventJournal for SqliteStorage {
    async fn append_durable(&self, event: &NewDurableEvent) -> SessionResult<DurableEvent> {
        let event_clone = event.clone();

        blocking::with_conn(&self.conn, move |conn| {
            insert_journal_event(
                conn,
                &event_clone,
                event_clone.source_node_id.as_deref(),
                event_clone.source_seq,
            )?
            .ok_or_else(|| {
                SessionError::Other(
                    "duplicate source-identity event rejected by the journal unique index"
                        .to_string(),
                )
            })
        })
        .await
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

        let joined =
            blocking::with_conn(&self.conn, move |conn| Ok(insert_batch(conn, &batch))).await;
        match joined {
            Ok(results) => results,
            Err(e) => {
                // The whole batch failed together. `SessionError` is not
                // `Clone`, so the first slot keeps the typed error and the
                // others carry its rendered chain.
                let message = e.to_string();
                let mut first = Some(e);
                (0..count)
                    .map(|_| {
                        Err(first
                            .take()
                            .unwrap_or_else(|| SessionError::Other(message.clone())))
                    })
                    .collect()
            }
        }
    }

    async fn max_session_seq(&self, session_id: &str) -> i64 {
        let key = session_id.to_string();
        blocking::with_conn_sqlite(&self.conn, move |conn| {
            conn.query_row(
                // Local numbering only: relayed rows carry the host's numbers.
                // The `session_seq IS NOT NULL` term lets SQLite answer MAX from
                // the partial unique index instead of scanning the session's rows.
                "SELECT COALESCE(MAX(session_seq), 0) FROM event_journal \
                 WHERE session_id = ?1 AND source_node_id IS NULL AND session_seq IS NOT NULL",
                params![key],
                |row| row.get::<_, i64>(0),
            )
        })
        .await
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

        blocking::with_conn_sqlite(&self.conn, move |conn| {
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
        blocking::with_conn_sqlite(&self.conn, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, payload_json, session_seq \
                 FROM event_journal WHERE session_id = ?1 \
                 AND kind IN (?3, ?4, ?5) \
                 AND json_extract(payload_json, '$.data.delegation_id') IN (SELECT value FROM json_each(?2)) \
                 ORDER BY stream_seq ASC"
            )?;
            stmt.query_map(
                params![
                    session_id,
                    ids,
                    AgentEventKind::DELEGATION_COMPLETED_TAG,
                    AgentEventKind::DELEGATION_FAILED_TAG,
                    AgentEventKind::DELEGATION_CANCELLED_TAG,
                ],
                parse_journal_row,
            )?
            .collect()
        })
        .await
    }

    async fn last_unfinished_run(&self, session_id: &str) -> SessionResult<Option<String>> {
        let session_id = session_id.to_string();

        blocking::with_conn_sqlite(&self.conn, move |conn| {
            // Only run lifecycle rows are decoded; the partial run lifecycle
            // index bounds the scan to this session. Only this node's own
            // events count: mirrored events of a remote host describe runs
            // that host owns and closes, so they must never look orphaned
            // here (even after the session's bookmark is gone).
            let mut stmt = conn.prepare(
                "SELECT kind, json_extract(payload_json, '$.data.run_id') \
                 FROM event_journal \
                 WHERE session_id = ?1 AND kind IN (?2, ?3) \
                 AND origin = 'local' \
                 ORDER BY stream_seq ASC",
            )?;
            let rows = stmt
                .query_map(
                    params![
                        session_id,
                        AgentEventKind::RUN_STARTED_TAG,
                        AgentEventKind::RUN_COMPLETED_TAG
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(last_unfinished_run_in(rows.into_iter().filter_map(
                |(kind, run_id)| {
                    let run_id = run_id?;
                    match kind.as_str() {
                        AgentEventKind::RUN_STARTED_TAG => Some((RunLifecycle::Started, run_id)),
                        AgentEventKind::RUN_COMPLETED_TAG => {
                            Some((RunLifecycle::Completed, run_id))
                        }
                        _ => None,
                    }
                },
            )))
        })
        .await
    }

    async fn load_session_page(
        &self,
        session_id: &str,
        after_seq: Option<i64>,
        limit: Option<usize>,
    ) -> SessionResult<Vec<DurableEvent>> {
        let session_id = session_id.to_string();

        blocking::with_conn_sqlite(&self.conn, move |conn| {
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
    }

    async fn load_session_page_before(
        &self,
        session_id: &str,
        before_seq: Option<i64>,
        limit: usize,
    ) -> SessionResult<Vec<DurableEvent>> {
        let session_id = session_id.to_string();

        blocking::with_conn_sqlite(&self.conn, move |conn| {
            let before = before_seq.unwrap_or(i64::MAX);
            let lim = i64::try_from(limit).unwrap_or(i64::MAX);
            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, payload_json, session_seq \
                 FROM event_journal \
                 WHERE session_id = ? AND stream_seq < ? \
                 ORDER BY stream_seq DESC \
                 LIMIT ?",
            )?;
            let mut events = stmt
                .query_map(params![session_id, before, lim], parse_journal_row)?
                .collect::<Result<Vec<_>, _>>()?;
            events.reverse();
            Ok(events)
        })
        .await
    }

    async fn load_history_page(
        &self,
        session_id: &str,
        source_node_id: Option<&str>,
        direction: HistoryPageDirection,
        limit: usize,
    ) -> SessionResult<Vec<(i64, DurableEvent)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(node_id) = source_node_id else {
            let events = match direction {
                HistoryPageDirection::Forward { after } => {
                    self.load_session_page(session_id, Some(after), Some(limit))
                        .await?
                }
                HistoryPageDirection::Backward { before } => {
                    self.load_session_page_before(session_id, before, limit)
                        .await?
                }
            };
            return Ok(events
                .into_iter()
                .map(|event| (event.stream_seq, event))
                .collect());
        };
        let session = session_id.to_owned();
        let node = node_id.to_owned();
        let paged = blocking::with_conn(
            &self.conn,
            move |conn| -> SessionResult<Option<Vec<(i64, DurableEvent)>>> {
                let (cursor, complete) = read_sync_row(conn, &session, &node)?;
                let mut covered = read_coverage(conn, &session, &node)?;
                if cursor > 0 {
                    covered.insert(1, cursor);
                }
                if !complete && covered.is_empty() {
                    // Cursorless legacy copies are visible: take the slow path.
                    return Ok(None);
                }
                const COLUMNS: &str = "SELECT event_id, stream_seq, session_id, timestamp, origin, \
                     source_node, payload_json, session_seq, source_seq FROM event_journal \
                     WHERE session_id = ?1 AND source_node_id = ?2 AND source_seq BETWEEN ?3 AND ?4";
                let read = |lo: i64, hi: i64, desc: bool, take: usize| -> SessionResult<Vec<(i64, DurableEvent)>> {
                    let sql = format!(
                        "{COLUMNS} ORDER BY source_seq {} LIMIT ?5",
                        if desc { "DESC" } else { "ASC" }
                    );
                    let mut stmt = conn.prepare_cached(&sql)?;
                    let take = i64::try_from(take).unwrap_or(i64::MAX);
                    let rows = stmt.query_map(params![session, node, lo, hi, take], |row| {
                        Ok((parse_journal_row(row)?, row.get::<_, i64>(8)?))
                    })?;
                    let mut out = Vec::new();
                    for row in rows {
                        let (event, position) = row?;
                        out.push((position, event));
                    }
                    Ok(out)
                };
                let mut out: Vec<(i64, DurableEvent)> = Vec::new();
                match direction {
                    HistoryPageDirection::Forward { after } => {
                        for &(a, b) in covered.ranges() {
                            if b <= after {
                                continue;
                            }
                            let lo = a.max(after.saturating_add(1));
                            out.extend(read(lo, b, false, limit - out.len())?);
                            if out.len() >= limit {
                                break;
                            }
                        }
                    }
                    HistoryPageDirection::Backward { before } => {
                        for &(a, b) in covered.ranges().iter().rev() {
                            let hi = before.map_or(b, |before| b.min(before.saturating_sub(1)));
                            if hi < a {
                                continue;
                            }
                            out.extend(read(a, hi, true, limit - out.len())?);
                            if out.len() >= limit {
                                break;
                            }
                        }
                        out.reverse();
                    }
                }
                Ok(Some(out))
            },
        )
        .await?;
        match paged {
            Some(page) => Ok(page),
            None => {
                let history = self.load_remote_history(session_id, node_id).await?;
                Ok(slice_history_page(history, direction, limit))
            }
        }
    }

    async fn load_session_range(
        &self,
        session_id: &str,
        after_session_seq: i64,
        up_to_session_seq: Option<i64>,
    ) -> SessionResult<Vec<DurableEvent>> {
        let session_id = session_id.to_string();

        blocking::with_conn_sqlite(&self.conn, move |conn| {
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
    }

    async fn load_global_stream(
        &self,
        after_seq: Option<i64>,
        limit: Option<usize>,
    ) -> SessionResult<Vec<DurableEvent>> {
        blocking::with_conn_sqlite(&self.conn, move |conn| {
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
    }

    async fn delete_session_events_from(
        &self,
        session_id: &str,
        from_seq: i64,
    ) -> SessionResult<usize> {
        let session_id = session_id.to_string();

        blocking::with_conn_sqlite(&self.conn, move |conn| {
            // Conversation-tail deletion must not erase incurred spend or the
            // evidence that generation happened without an accounting basis:
            // * attempt start/settlement records are the accounting ledger;
            // * a request-end without `accounting` is a legacy generation
            //   record whose cost cannot be reconstructed, so it is kept to
            //   keep historical coverage honestly incomplete.
            // Retained rows keep their numbers; the resulting gaps are valid.
            let tx = conn.unchecked_transaction()?;
            let deleted = tx.execute(
                "DELETE FROM event_journal \
                 WHERE session_id = ?1 AND stream_seq >= ?2 \
                    AND kind NOT IN (?3, ?4) \
                   AND NOT (kind = ?5 \
                            AND json_extract(payload_json, '$.data.accounting') IS NULL)",
                params![
                    session_id,
                    from_seq,
                    AgentEventKind::LLM_ATTEMPT_STARTED_TAG,
                    AgentEventKind::LLM_ATTEMPT_SETTLED_TAG,
                    AgentEventKind::LLM_REQUEST_END_TAG,
                ],
            )?;
            if deleted > 0 {
                tx.execute(
                    "INSERT INTO event_journal_history_epoch (session_id, epoch) VALUES (?1, 1) \
                     ON CONFLICT(session_id) DO UPDATE SET epoch = epoch + 1",
                    params![session_id],
                )?;
            }
            tx.commit()?;
            Ok(deleted)
        })
        .await
    }

    async fn history_epoch(&self, session_id: &str) -> SessionResult<u64> {
        let session_id = session_id.to_string();

        blocking::with_conn_sqlite(&self.conn, move |conn| {
            let epoch: Option<i64> = conn
                .query_row(
                    "SELECT epoch FROM event_journal_history_epoch WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(epoch.unwrap_or(0).max(0) as u64)
        })
        .await
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

        blocking::with_conn(&self.conn, move |conn| {
            // Duplicate replay is a no-op (plan §15/§16): no insert and no
            // stream_seq allocation. The source-existence check, sequence
            // allocation, and insert all run inside one Immediate transaction
            // so concurrent writers serialize at the database level (not just
            // in-process via the connection mutex); the partial unique index
            // (idx_event_journal_source_identity) remains the durable backstop.
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
    }

    async fn latest_source_seq(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Option<i64>> {
        let session_id = session_id.to_string();
        let source_node_id = source_node_id.to_string();

        blocking::with_conn_sqlite(&self.conn, move |conn| {
            conn.query_row(
                "SELECT MAX(source_seq) FROM event_journal \
                 WHERE session_id = ?1 AND source_node_id = ?2",
                params![session_id, source_node_id],
                |row| row.get(0),
            )
        })
        .await
    }

    async fn remote_sync_cursor(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Option<i64>> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        blocking::with_conn_sqlite(&self.conn, move |conn| {
            conn.query_row(
                "SELECT source_seq FROM remote_session_sync WHERE session_id = ?1 AND source_node_id = ?2",
                params![session_id, source_node_id],
                |row| row.get(0),
            ).optional()
        }).await
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
        blocking::with_conn_sqlite(&self.conn, move |conn| {
            conn.execute(
                "INSERT INTO remote_session_sync (session_id, source_node_id, source_seq, complete)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (session_id, source_node_id) DO UPDATE SET
                 source_seq = MAX(source_seq, excluded.source_seq),
                 complete = MAX(complete, excluded.complete)",
                params![session_id, source_node_id, source_seq, complete],
            )
            .map(|_| ())
        })
        .await
    }

    async fn load_remote_session_stream(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Vec<DurableEvent>> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        blocking::with_conn_sqlite(&self.conn, move |conn| {
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
                .collect::<Result<Vec<_>, _>>()
        }).await
    }

    async fn load_remote_history(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Vec<(i64, DurableEvent)>> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        blocking::with_conn(&self.conn, move |conn| {
            let (cursor, complete) = read_sync_row(conn, &session_id, &source_node_id)?;
            let mut covered = read_coverage(conn, &session_id, &source_node_id)?;
            if cursor > 0 {
                covered.insert(1, cursor);
            }
            // Cursorless legacy copies stay visible only until any
            // source-identified history exists for this session.
            let include_legacy = !complete && covered.is_empty();
            let mut stmt = conn.prepare(
                "SELECT event_id, stream_seq, session_id, timestamp, origin, source_node, \
                        payload_json, session_seq, source_seq \
                 FROM event_journal WHERE session_id = ?1 AND ( \
                     source_node_id = ?2 OR (source_node_id IS NULL AND ?3)) \
                 ORDER BY CASE WHEN source_seq IS NULL THEN 0 ELSE 1 END, source_seq, stream_seq",
            )?;
            let rows = stmt
                .query_map(params![session_id, source_node_id, include_legacy], |row| {
                    Ok((parse_journal_row(row)?, row.get::<_, Option<i64>>(8)?))
                })?;
            let mut history = Vec::new();
            for row in rows {
                let (event, source_seq) = row?;
                match source_seq {
                    Some(seq) if covered.contains(seq) => history.push((seq, event)),
                    Some(_) => {}
                    None => history.push((event.stream_seq, event)),
                }
            }
            Ok(history)
        })
        .await
    }

    async fn remote_coverage(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Vec<(i64, i64)>> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        blocking::with_conn(&self.conn, move |conn| {
            Ok(read_coverage(conn, &session_id, &source_node_id)?
                .ranges()
                .to_vec())
        })
        .await
    }

    async fn add_remote_coverage(
        &self,
        session_id: &str,
        source_node_id: &str,
        start: i64,
        end: i64,
    ) -> SessionResult<()> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        blocking::with_conn(&self.conn, move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut set = read_coverage(&tx, &session_id, &source_node_id)?;
            set.insert(start, end);
            tx.execute(
                "DELETE FROM remote_session_coverage WHERE session_id = ?1 AND source_node_id = ?2",
                params![session_id, source_node_id],
            )?;
            for (a, b) in set.ranges() {
                tx.execute(
                    "INSERT INTO remote_session_coverage (session_id, source_node_id, start_seq, end_seq) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![session_id, source_node_id, a, b],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    async fn remote_history_epoch(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Option<u64>> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        blocking::with_conn(&self.conn, move |conn| {
            let epoch: Option<i64> = conn
                .query_row(
                    "SELECT epoch FROM remote_session_epoch WHERE session_id = ?1 AND source_node_id = ?2",
                    params![session_id, source_node_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(epoch.map(|epoch| epoch.max(0) as u64))
        })
        .await
    }

    async fn set_remote_history_epoch(
        &self,
        session_id: &str,
        source_node_id: &str,
        epoch: u64,
    ) -> SessionResult<()> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        blocking::with_conn(&self.conn, move |conn| {
            conn.execute(
                "INSERT INTO remote_session_epoch (session_id, source_node_id, epoch) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (session_id, source_node_id) DO UPDATE SET epoch = excluded.epoch",
                params![session_id, source_node_id, i64::try_from(epoch).unwrap_or(i64::MAX)],
            )?;
            Ok(())
        })
        .await
    }

    async fn reset_remote_source(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<usize> {
        let session_id = session_id.to_owned();
        let source_node_id = source_node_id.to_owned();
        blocking::with_conn(&self.conn, move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let deleted = tx.execute(
                "DELETE FROM event_journal WHERE session_id = ?1 AND source_node_id = ?2",
                params![session_id, source_node_id],
            )?;
            tx.execute(
                "DELETE FROM remote_session_coverage WHERE session_id = ?1 AND source_node_id = ?2",
                params![session_id, source_node_id],
            )?;
            tx.execute(
                "DELETE FROM remote_session_epoch WHERE session_id = ?1 AND source_node_id = ?2",
                params![session_id, source_node_id],
            )?;
            // Cursor back to 0 but marked authoritative, so cursorless legacy
            // copies of the rewritten history never reappear.
            tx.execute(
                "INSERT INTO remote_session_sync (session_id, source_node_id, source_seq, complete) \
                 VALUES (?1, ?2, 0, 1) \
                 ON CONFLICT (session_id, source_node_id) DO UPDATE SET source_seq = 0, complete = 1",
                params![session_id, source_node_id],
            )?;
            tx.commit()?;
            Ok(deleted)
        })
        .await
    }

    async fn max_stream_seq(&self, session_id: &str) -> SessionResult<i64> {
        let session_id = session_id.to_string();

        blocking::with_conn_sqlite(&self.conn, move |conn| {
            conn.query_row(
                "SELECT COALESCE(MAX(stream_seq), 0) FROM event_journal WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
        })
        .await
    }
}

/// Remote sync cursor and completeness flag (`(0, false)` when absent).
fn read_sync_row(
    conn: &rusqlite::Connection,
    session_id: &str,
    source_node_id: &str,
) -> Result<(i64, bool), rusqlite::Error> {
    Ok(conn
        .query_row(
            "SELECT source_seq, complete FROM remote_session_sync \
             WHERE session_id = ?1 AND source_node_id = ?2",
            params![session_id, source_node_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? != 0)),
        )
        .optional()?
        .unwrap_or((0, false)))
}

/// Stored transferred `source_seq` ranges of one remote source.
fn read_coverage(
    conn: &rusqlite::Connection,
    session_id: &str,
    source_node_id: &str,
) -> Result<crate::session::ranges::RangeSet, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT start_seq, end_seq FROM remote_session_coverage \
         WHERE session_id = ?1 AND source_node_id = ?2 ORDER BY start_seq",
    )?;
    let ranges = stmt
        .query_map(params![session_id, source_node_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(crate::session::ranges::RangeSet::from_ranges(ranges))
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
    let kind_tag = event.kind.tag();

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
                            SessionError::Other(
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

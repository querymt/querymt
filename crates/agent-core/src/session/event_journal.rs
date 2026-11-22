//! The `EventJournal` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use super::projection::*;
use crate::events::{DurableEvent, EventOrigin};
use crate::session::error::SessionResult;
use async_trait::async_trait;

/// Durable event journal — persistence and replay of a session's events.
///
/// Ordering authority is split in two:
/// - Producers set `session_seq` (the per-session production order) before
///   appending; session replay orders by it.
/// - The journal still assigns `stream_seq` atomically on append; it is the
///   insert-order cursor for global-stream consumers and the host cursor for
///   remote dedup and backfill.
#[async_trait]
pub trait EventJournal: Send + Sync {
    /// Persist a durable event and return it with assigned `event_id` and `stream_seq`.
    async fn append_durable(&self, event: &NewDurableEvent) -> SessionResult<DurableEvent>;

    /// Persist consecutive durable events in order, returning one result per
    /// input. Implementations should write the batch in a single transaction;
    /// the default appends one by one.
    async fn append_durable_batch(
        &self,
        events: &[NewDurableEvent],
    ) -> Vec<SessionResult<DurableEvent>> {
        let mut results = Vec::with_capacity(events.len());
        for event in events {
            results.push(self.append_durable(event).await);
        }
        results
    }

    /// Highest producer `session_seq` persisted for a session (0 when none).
    /// Read once per session per process, when its event stream opens.
    async fn max_session_seq(&self, _session_id: &str) -> i64 {
        0
    }

    /// Load the durable event stream for a session, optionally starting after a cursor.
    /// `None` loads all matching events; `Some(limit)` returns at most that many.
    async fn load_session_stream(
        &self,
        session_id: &str,
        after_seq: Option<i64>,
        limit: Option<usize>,
    ) -> SessionResult<Vec<DurableEvent>>;

    /// Recover correlated terminal delegation signals after a lost live wakeup.
    async fn load_delegation_terminals(
        &self,
        session_id: &str,
        delegation_ids: &[String],
    ) -> SessionResult<Vec<DurableEvent>> {
        if delegation_ids.is_empty() {
            return Ok(Vec::new());
        }
        let events = self.load_session_stream(session_id, None, None).await?;
        Ok(events
            .into_iter()
            .filter(|event| match &event.kind {
                crate::events::AgentEventKind::DelegationCompleted { delegation_id, .. }
                | crate::events::AgentEventKind::DelegationFailed { delegation_id, .. }
                | crate::events::AgentEventKind::DelegationCancelled { delegation_id } => {
                    delegation_ids.contains(delegation_id)
                }
                _ => false,
            })
            .collect())
    }

    /// Load one page of a session's durable events strictly in `stream_seq`
    /// order, with no production-order restoration.
    ///
    /// Pages served to other nodes (backfill, gap recovery) must have a
    /// strictly increasing `stream_seq`, because the receiver uses it as its
    /// cursor and rejects pages that go backwards. The default falls back to
    /// [`Self::load_session_stream`] and re-sorts.
    async fn load_session_page(
        &self,
        session_id: &str,
        after_seq: Option<i64>,
        limit: Option<usize>,
    ) -> SessionResult<Vec<DurableEvent>> {
        let mut events = self
            .load_session_stream(session_id, after_seq, limit)
            .await?;
        events.sort_by_key(|event| event.stream_seq);
        Ok(events)
    }

    /// Load up to `limit` events with `stream_seq < before_seq` (`None` = up to
    /// the tip), the newest ones first, returned in ascending `stream_seq`
    /// order. Used for tail-first transfer and older-history paging.
    ///
    /// The default filters the full stream; storage backends should override
    /// it with an indexed `ORDER BY stream_seq DESC LIMIT` read.
    async fn load_session_page_before(
        &self,
        session_id: &str,
        before_seq: Option<i64>,
        limit: usize,
    ) -> SessionResult<Vec<DurableEvent>> {
        let mut events = self.load_session_stream(session_id, None, None).await?;
        events.sort_by_key(|event| event.stream_seq);
        events.retain(|event| before_seq.is_none_or(|before| event.stream_seq < before));
        let skip = events.len().saturating_sub(limit);
        Ok(events.split_off(skip))
    }

    /// Load one page of a session's history by **history position**, never
    /// materialising the whole stream.
    ///
    /// The position is `stream_seq` for local sessions
    /// (`source_node_id == None`) and the owner's `source_seq` for the remote
    /// source `source_node_id`, restricted to fully transferred ranges (the
    /// same rows [`Self::load_remote_history`] returns). Pages are returned in
    /// ascending position order whatever the direction, so paging forward or
    /// backward visits every event exactly once.
    ///
    /// The default derives pages from the existing reads; storage backends
    /// should override it with indexed, limited queries.
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
        match source_node_id {
            None => {
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
                Ok(events
                    .into_iter()
                    .map(|event| (event.stream_seq, event))
                    .collect())
            }
            Some(node_id) => {
                let history = self.load_remote_history(session_id, node_id).await?;
                Ok(slice_history_page(history, direction, limit))
            }
        }
    }

    /// Run id of the most recent `run_started` that has no `run_completed`,
    /// considering only events produced by this node (`EventOrigin::Local`).
    /// Mirrored events of a remote host are ignored: that host owns and closes
    /// its runs.
    ///
    /// The default scans the whole session stream; storage backends should
    /// override it with an indexed query over run lifecycle rows only.
    async fn last_unfinished_run(&self, session_id: &str) -> SessionResult<Option<String>> {
        let events = self.load_session_stream(session_id, None, None).await?;
        Ok(last_unfinished_run_in(events.into_iter().filter_map(
            |event| match event.kind {
                _ if event.origin != EventOrigin::Local => None,
                crate::events::AgentEventKind::RunStarted { run_id, .. } => {
                    Some((RunLifecycle::Started, run_id))
                }
                crate::events::AgentEventKind::RunCompleted { run_id, .. } => {
                    Some((RunLifecycle::Completed, run_id))
                }
                _ => None,
            },
        )))
    }

    /// Load a session's durable events with `session_seq` in
    /// `(after_session_seq, up_to_session_seq]` (`None` = no upper bound), in
    /// production order. Used by live consumers to recover events they missed
    /// (broadcast lag or a sequence gap). Unsequenced rows (`session_seq` 0)
    /// are never returned.
    ///
    /// The default filters the full stream; storage backends should override
    /// it with an indexed range read.
    async fn load_session_range(
        &self,
        session_id: &str,
        after_session_seq: i64,
        up_to_session_seq: Option<i64>,
    ) -> SessionResult<Vec<DurableEvent>> {
        let mut events = self.load_session_stream(session_id, None, None).await?;
        events.retain(|event| {
            event.session_seq > after_session_seq
                && up_to_session_seq.is_none_or(|upper| event.session_seq <= upper)
        });
        events.sort_by_key(|event| event.session_seq);
        Ok(events)
    }

    /// Load events across all sessions (global stream), after a given sequence.
    /// `None` loads all matching events; `Some(limit)` returns at most that many.
    async fn load_global_stream(
        &self,
        after_seq: Option<i64>,
        limit: Option<usize>,
    ) -> SessionResult<Vec<DurableEvent>>;

    /// Delete the event at `from_seq` and all subsequent events for a session.
    /// Equivalent to [`Self::delete_session_events_range`] without an upper
    /// bound; see there for what is exempt from deletion.
    async fn delete_session_events_from(
        &self,
        session_id: &str,
        from_seq: i64,
    ) -> SessionResult<usize> {
        self.delete_session_events_range(session_id, from_seq, None)
            .await
    }

    /// Delete the events of a session with `stream_seq >= from_seq` and, when
    /// `before_seq` is `Some`, `stream_seq < before_seq`. Used by undo cleanup
    /// to prune the event journal when the user commits a new prompt after an
    /// undo so that the reverted turns do not reappear on page reload. The
    /// upper bound keeps events the new run has already written (its
    /// `run_started`, queued-input markers) out of the deletion.
    ///
    /// Exempt, never deleted:
    /// * accounting: `llm_attempt_started` / `llm_attempt_settled` records, and
    ///   legacy `llm_request_end` records that carry no `accounting` detail
    ///   (evidence of generation with no accounting basis), so rewinding cannot
    ///   erase incurred spend;
    /// * run lifecycle: `run_started` / `run_completed`, so every run keeps
    ///   both ends and no run is left looking open (or closed without a start)
    ///   by a delete that begins in the middle of it.
    ///
    /// Retained records are not renumbered. Deleting the whole session is
    /// separate.
    ///
    /// Returns the number of deleted rows.
    async fn delete_session_events_range(
        &self,
        session_id: &str,
        from_seq: i64,
        before_seq: Option<i64>,
    ) -> SessionResult<usize>;

    /// Owner-side history epoch: a counter bumped whenever
    /// [`Self::delete_session_events_from`] actually deletes rows (undo
    /// cleanup). Appends never change it. Remote peers compare it across
    /// chunk replies to detect rewritten history. `0` until the first rewrite;
    /// backends without persistence report `0`.
    async fn history_epoch(&self, _session_id: &str) -> SessionResult<u64> {
        Ok(0)
    }

    /// Idempotent append for durable events that carry remote source identity
    /// (plan §15/§16).
    ///
    /// Returns `Ok(None)` when an event with the same
    /// `(session_id, source_node_id, source_seq)` is already persisted — the
    /// duplicate replay is a full no-op (no insert, no new `stream_seq`
    /// allocation). Callers must not republish `None` results to live
    /// subscribers.
    ///
    /// Requires `source_node_id` and `source_seq` on the event; events without
    /// source identity must go through `append_durable` instead.
    async fn append_durable_from_source(
        &self,
        event: &NewDurableEvent,
    ) -> SessionResult<Option<DurableEvent>>;

    /// Latest received source-side sequence for a session/node pair.
    /// This is NOT a synchronization checkpoint: live delivery may skip gaps.
    /// Returns `None` when no row carries source identity. Use
    /// `remote_sync_cursor` to resume backfill without skipping missing events.
    async fn latest_source_seq(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Option<i64>>;

    /// Last successfully backfilled source sequence, independent of live delivery.
    async fn remote_sync_cursor(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Option<i64>>;

    /// Advance only after every event in a historical page has persisted.
    /// `complete` makes the source-identified snapshot authoritative over legacy rows.
    async fn advance_remote_sync_cursor(
        &self,
        session_id: &str,
        source_node_id: &str,
        source_seq: i64,
        complete: bool,
    ) -> SessionResult<()>;

    /// Cached remote snapshot in host order, without duplicating legacy rows once synced.
    async fn load_remote_session_stream(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Vec<DurableEvent>>;

    /// Stream tip for a session: the highest `stream_seq` currently persisted
    /// (0 when empty). Serves as the completion marker for cursor-based
    /// backfill paging (plan §16.5).
    async fn max_stream_seq(&self, session_id: &str) -> SessionResult<i64>;

    /// Cached remote history with each event's **history position**: the
    /// owner's `source_seq` for source-identified rows (legacy cursorless
    /// rows report their local `stream_seq`). Ordered by position, and only
    /// ranges that were fully transferred are returned (see
    /// [`Self::remote_coverage`]).
    ///
    /// The default derives positions from [`Self::load_remote_session_stream`].
    async fn load_remote_history(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> SessionResult<Vec<(i64, DurableEvent)>> {
        Ok(self
            .load_remote_session_stream(session_id, source_node_id)
            .await?
            .into_iter()
            .map(|event| (event.stream_seq, event))
            .collect())
    }

    /// `source_seq` ranges (inclusive, merged) of a remote session's history
    /// that were fully transferred. Does not include the legacy contiguous
    /// cursor range, which [`Self::remote_sync_cursor`] reports. Backends
    /// without persistence report no ranges.
    async fn remote_coverage(
        &self,
        _session_id: &str,
        _source_node_id: &str,
    ) -> SessionResult<Vec<(i64, i64)>> {
        Ok(Vec::new())
    }

    /// Record `[start, end]` as fully transferred, merging with stored ranges.
    async fn add_remote_coverage(
        &self,
        _session_id: &str,
        _source_node_id: &str,
        _start: i64,
        _end: i64,
    ) -> SessionResult<()> {
        Ok(())
    }

    /// The owner's history epoch seen at the last sync (`None` = never
    /// recorded).
    async fn remote_history_epoch(
        &self,
        _session_id: &str,
        _source_node_id: &str,
    ) -> SessionResult<Option<u64>> {
        Ok(None)
    }

    async fn set_remote_history_epoch(
        &self,
        _session_id: &str,
        _source_node_id: &str,
        _epoch: u64,
    ) -> SessionResult<()> {
        Ok(())
    }

    /// Drop everything synced from one source: its journal rows, coverage,
    /// sync cursor and recorded epoch. Used when the owner rewrote its
    /// history. Returns the number of deleted journal rows.
    async fn reset_remote_source(
        &self,
        _session_id: &str,
        _source_node_id: &str,
    ) -> SessionResult<usize> {
        Ok(0)
    }
}

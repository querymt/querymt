//! Historical synchronization of a remote session into the local journal.
//!
//! History moves in owner-served chunks (`GetEventStreamChunk`). Chunks are
//! persisted without publishing (history is not a live update) and recorded in
//! the session's [`RemoteSyncState`] as covered `source_seq` ranges; the sync
//! cursor only ever describes the contiguous prefix. A first windowed open
//! fetches the recent tail first and fills older history afterwards; on-demand
//! requests fetch uncovered ranges backwards. Ranges are reserved before they
//! are requested, so no event crosses the mesh twice concurrently. A changed
//! owner history epoch discards the source's local copy and resyncs.
//!
//! Live relay and gap repair (`recover_source_gap`) still publish.

use std::sync::Arc;
use std::time::Duration;

use tracing::Instrument;

use crate::agent::messages::{
    EventStreamChunk, GetEventStreamChunk, MAX_EVENT_STREAM_CHUNK_BYTES,
    MAX_EVENT_STREAM_CHUNK_EVENTS,
};
use crate::event_sink::EventSink;
use crate::events::{AgentEventKind, EventOrigin};
use crate::session::remote_sync::{Claim, EpochCheck, RemoteSyncState, SyncGuard};

use super::actor_ref::SessionActorRef;

/// Page size for backfill requests. Bounded so a large gap cannot produce a
/// single huge message; the loop pages until the host tip is reached. Pinned
/// to the host-side `MAX_EVENT_STREAM_PAGE_SIZE` cap so every page is served
/// in full.
const BACKFILL_PAGE_SIZE: usize = crate::agent::messages::MAX_EVENT_STREAM_PAGE_SIZE;

/// Fetch durable events the live relay missed (a gap in the host's
/// `session_seq`) from the host and persist/publish them in host order.
///
/// Pages after `after_source_seq` up to the host tip. Unlike
/// [`RemoteSync::backfill`] this never touches the remote sync checkpoint:
/// it repairs a hole in the live stream and says nothing about whether the
/// historical snapshot is complete. Persistence is idempotent per source
/// identity, so overlap with the live event that revealed the gap is a no-op.
///
/// With `before_session_seq` set, recovery stops before the first event whose
/// host `session_seq` is at or beyond it, so repair never overtakes the event
/// the caller is about to deliver. `None` recovers up to the host tip.
///
/// Returns the highest source sequence now persisted.
pub(crate) async fn recover_source_gap(
    event_sink: &EventSink,
    session_ref: &SessionActorRef,
    session_id: &str,
    source_node_id: &str,
    peer_label: &str,
    after_source_seq: i64,
    before_session_seq: Option<i64>,
) -> Result<i64, crate::error::AgentError> {
    let mut after = after_source_seq;
    loop {
        let page = session_ref
            .get_event_stream_since(Some(after), BACKFILL_PAGE_SIZE)
            .await?;
        let mut previous = after;
        for event in page.events {
            if event.seq <= previous || event.session_id != session_id {
                return Err(crate::error::AgentError::Internal(
                    "invalid gap recovery page".into(),
                ));
            }
            if before_session_seq
                .is_some_and(|bound| event.session_seq > 0 && event.session_seq >= bound)
            {
                return Ok(previous);
            }
            previous = event.seq;
            event_sink
                .emit_durable_from_source(
                    session_id,
                    event.kind,
                    Some(peer_label.to_string()),
                    source_node_id.to_string(),
                    event.seq,
                    (event.session_seq > 0).then_some(event.session_seq),
                )
                .await
                .map_err(|error| crate::error::AgentError::Internal(error.to_string()))?;
        }
        if previous == after || previous >= page.latest_source_seq {
            return Ok(previous);
        }
        after = previous;
    }
}

/// Events requested by the first windowed open's tail chunk.
pub(crate) const WINDOW_CHUNK_EVENTS: usize = MAX_EVENT_STREAM_CHUNK_EVENTS;

/// Longest wait for another fetcher that holds a range before re-checking.
const BUSY_RECHECK: Duration = Duration::from_secs(5);

fn invalid_chunk() -> crate::error::AgentError {
    crate::error::AgentError::Internal("invalid history chunk".into())
}

fn chunk_request(
    after: Option<i64>,
    before: Option<i64>,
    tail: bool,
    max_events: usize,
) -> GetEventStreamChunk {
    GetEventStreamChunk {
        after_source_seq: after,
        before_source_seq: before,
        tail,
        max_events,
        max_bytes: MAX_EVENT_STREAM_CHUNK_BYTES,
    }
}

/// The `source_seq` range a served chunk proves is now held locally.
/// Empty (`start > end`) when it proves nothing.
fn covered_range(req: &GetEventStreamChunk, chunk: &EventStreamChunk) -> (i64, i64) {
    let tip = chunk.latest_source_seq;
    let first = chunk.events.first().map(|e| e.seq);
    let last = chunk.events.last().map(|e| e.seq);
    if req.tail {
        let end = req.before_source_seq.map_or(tip, |b| b - 1);
        let start = if chunk.has_more {
            first.unwrap_or(end + 1)
        } else {
            req.after_source_seq.map_or(0, |s| s.max(0)) + 1
        };
        (start, end)
    } else {
        let start = req.after_source_seq.map_or(0, |s| s.max(0)) + 1;
        let end = if chunk.has_more {
            last.unwrap_or(start - 1)
        } else {
            req.before_source_seq.map_or(tip, |b| (b - 1).min(tip))
        };
        (start, end)
    }
}

struct Applied {
    has_more: bool,
    first_seq: Option<i64>,
    last_seq: Option<i64>,
    inserted: usize,
}

/// Chunk-based sync of one remote session from one owner node.
#[derive(Clone)]
pub(crate) struct RemoteSync {
    sink: Arc<EventSink>,
    session_ref: SessionActorRef,
    session_id: String,
    node_id: String,
    peer_label: String,
    state: Arc<RemoteSyncState>,
    window_events: usize,
}

impl RemoteSync {
    pub(crate) fn new(
        sink: Arc<EventSink>,
        session_ref: SessionActorRef,
        session_id: &str,
        node_id: &str,
        peer_label: &str,
    ) -> Self {
        let state = sink.remote_sync(session_id, node_id);
        Self {
            sink,
            session_ref,
            session_id: session_id.to_string(),
            node_id: node_id.to_string(),
            peer_label: peer_label.to_string(),
            state,
            window_events: WINDOW_CHUNK_EVENTS,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_window_events(mut self, events: usize) -> Self {
        self.window_events = events;
        self
    }

    pub(crate) fn state(&self) -> &Arc<RemoteSyncState> {
        &self.state
    }

    /// Request one chunk, then (under the apply lock) check the epoch, persist
    /// without publishing, and record the covered range.
    ///
    /// `None` means the owner rewrote its history: local state was reset and
    /// the caller must restart its sync.
    async fn request_apply(
        &self,
        req: GetEventStreamChunk,
    ) -> Result<Option<Applied>, crate::error::AgentError> {
        for _ in 0..3 {
            let chunk = self.session_ref.get_event_stream_chunk(req.clone()).await?;
            self.state.record_request(chunk.events.len());
            let _apply = self.state.lock_apply().await;
            match self.state.check_epoch(chunk.history_epoch).await? {
                EpochCheck::Stale => continue,
                EpochCheck::Reset => return Ok(None),
                EpochCheck::Current => {}
            }
            let mut previous = 0;
            for event in &chunk.events {
                if event.session_id != self.session_id || event.seq <= previous {
                    return Err(invalid_chunk());
                }
                previous = event.seq;
            }
            let (start, end) = covered_range(&req, &chunk);
            let tip = chunk.latest_source_seq;
            let has_more = chunk.has_more;
            let first_seq = chunk.events.first().map(|e| e.seq);
            let last_seq = chunk.events.last().map(|e| e.seq);
            let mut inserted = 0;
            for event in chunk.events {
                let persisted = self
                    .sink
                    .persist_durable_from_source(
                        &self.session_id,
                        event.kind,
                        Some(self.peer_label.clone()),
                        self.node_id.clone(),
                        event.seq,
                        (event.session_seq > 0).then_some(event.session_seq),
                    )
                    .await?;
                inserted += usize::from(persisted.is_some());
            }
            self.state.mark_covered(start, end, tip).await?;
            self.state.record_inserted(inserted);
            return Ok(Some(Applied {
                has_more,
                first_seq,
                last_seq,
                inserted,
            }));
        }
        Err(crate::error::AgentError::Internal(
            "owner reported a stale history epoch".into(),
        ))
    }

    /// Forward chunks after `after` until the owner's tip.
    async fn forward_until_tip(
        &self,
        mut after: i64,
    ) -> Result<Option<usize>, crate::error::AgentError> {
        let mut total = 0;
        loop {
            let req = chunk_request(Some(after), None, false, MAX_EVENT_STREAM_CHUNK_EVENTS);
            let Some(applied) = self.request_apply(req).await? else {
                return Ok(None);
            };
            total += applied.inserted;
            if !applied.has_more {
                return Ok(Some(total));
            }
            match applied.last_seq {
                Some(seq) if seq > after => after = seq,
                _ => return Err(invalid_chunk()),
            }
        }
    }

    /// Forward chunks covering exactly `[start, end]`.
    async fn forward_part(
        &self,
        start: i64,
        end: i64,
    ) -> Result<Option<usize>, crate::error::AgentError> {
        let mut after = start - 1;
        let mut total = 0;
        loop {
            let req = chunk_request(
                Some(after),
                Some(end.saturating_add(1)),
                false,
                MAX_EVENT_STREAM_CHUNK_EVENTS,
            );
            let Some(applied) = self.request_apply(req).await? else {
                return Ok(None);
            };
            total += applied.inserted;
            if !applied.has_more {
                return Ok(Some(total));
            }
            match applied.last_seq {
                Some(seq) if seq > after => after = seq,
                _ => return Err(invalid_chunk()),
            }
        }
    }

    /// Backward chunks covering exactly `[start, end]`, newest first.
    async fn backward_part(
        &self,
        start: i64,
        end: i64,
    ) -> Result<Option<usize>, crate::error::AgentError> {
        let mut before = end.saturating_add(1);
        let mut total = 0;
        loop {
            let req = chunk_request(
                Some(start - 1),
                Some(before),
                true,
                MAX_EVENT_STREAM_CHUNK_EVENTS,
            );
            let Some(applied) = self.request_apply(req).await? else {
                return Ok(None);
            };
            total += applied.inserted;
            if !applied.has_more {
                return Ok(Some(total));
            }
            match applied.first_seq {
                Some(seq) if seq < before => before = seq,
                _ => return Err(invalid_chunk()),
            }
        }
    }

    /// Bring the recent history local: the tail chunk on a first windowed
    /// open, otherwise everything newer than what is already held. Returns
    /// the number of newly persisted events. The load barrier
    /// ([`RemoteSyncState::wait_synced`]) is held for the duration.
    pub(crate) async fn open(&self, windowed: bool) -> Result<usize, crate::error::AgentError> {
        self.state.hydrate().await?;
        let _sync = self.state.begin_sync();
        let _open = self.state.lock_open().await;
        for _ in 0..3 {
            let result = if windowed && !self.state.has_data() {
                let req = chunk_request(None, None, true, self.window_events);
                self.request_apply(req).await?.map(|a| a.inserted)
            } else {
                self.forward_until_tip(self.state.max_covered()).await?
            };
            if let Some(inserted) = result {
                self.state.set_live_anchored(true);
                return Ok(inserted);
            }
        }
        Err(crate::error::AgentError::Internal(
            "remote history kept changing during sync".into(),
        ))
    }

    /// Fill every uncovered range below the owner's tip, oldest first,
    /// yielding to on-demand requests. A second concurrent fill is a no-op.
    /// `None` means the owner rewrote its history mid-fill.
    pub(crate) async fn fill(&self) -> Result<Option<usize>, crate::error::AgentError> {
        self.state.hydrate().await?;
        if !self.state.try_start_fill() {
            return Ok(Some(0));
        }
        let result = self.fill_inner().await;
        self.state.finish_fill();
        result
    }

    async fn fill_inner(&self) -> Result<Option<usize>, crate::error::AgentError> {
        let mut total = 0;
        loop {
            let mut rx = self.state.subscribe();
            match self.state.claim_fill(1, self.state.tip()) {
                Claim::Covered => return Ok(Some(total)),
                Claim::Busy => {
                    let _ = tokio::time::timeout(BUSY_RECHECK, rx.changed()).await;
                }
                Claim::Part(start, end) => {
                    let result = self.forward_part(start, end).await;
                    self.state.release(start, end);
                    match result? {
                        Some(inserted) => total += inserted,
                        None => return Ok(None),
                    }
                }
            }
        }
    }

    /// Open, then fill. Returns the number of newly persisted events.
    /// `pre_guard` (a sync registered by the caller before spawning) is
    /// released once the recent window is local, before the fill starts.
    pub(crate) async fn sync_all(
        &self,
        windowed: bool,
        pre_guard: Option<SyncGuard>,
    ) -> Result<usize, crate::error::AgentError> {
        let mut total = 0;
        let mut pre_guard = pre_guard;
        for _ in 0..3 {
            let opened = self.open(windowed).await;
            drop(pre_guard.take());
            total += opened?;
            if let Some(filled) = self.fill().await? {
                return Ok(total + filled);
            }
        }
        Err(crate::error::AgentError::Internal(
            "remote history kept changing during sync".into(),
        ))
    }

    /// Make sure `[start, end]` is local, fetching uncovered parts backwards.
    /// Concurrent callers and the background fill never fetch the same range.
    pub(crate) async fn ensure_covered(
        &self,
        start: i64,
        end: i64,
    ) -> Result<(), crate::error::AgentError> {
        self.state.hydrate().await?;
        let _demand = self.state.demand_guard();
        loop {
            let mut rx = self.state.subscribe();
            match self.state.claim(start, end) {
                Claim::Covered => return Ok(()),
                Claim::Busy => {
                    let _ = tokio::time::timeout(BUSY_RECHECK, rx.changed()).await;
                }
                Claim::Part(a, b) => {
                    let result = self.backward_part(a, b).await;
                    self.state.release(a, b);
                    if result?.is_none() {
                        return Err(crate::error::AgentError::Internal(
                            "remote history changed".into(),
                        ));
                    }
                }
            }
        }
    }
}

impl RemoteSync {
    /// Backfill durable events for a freshly (re)attached remote session.
    ///
    /// Spawn-and-forget: must never block connection establishment nor fail
    /// it. Events are persisted without publishing. `tail_first` fetches the
    /// recent window first (first windowed open); the older history is filled
    /// afterwards.
    pub(crate) async fn backfill(
        self,
        attachment_id: u64,
        cursor: Option<i64>,
        tail_first: bool,
        pre_guard: Option<SyncGuard>,
    ) -> Result<(), crate::error::AgentError> {
        let span = tracing::info_span!(
            "remote.session.sync",
            session_id = %self.session_id,
            node_id = %self.node_id,
            attachment_id,
        );
        async move {
            let session_id = self.session_id.as_str();
            let boundary = cursor.is_none();
            let backfilled = match self.sync_all(tail_first, pre_guard).await {
                Ok(count) => count,
                Err(error) => {
                    tracing::warn!(session_id, %error, "backfill unavailable; history may be stale");
                    return Err(error);
                }
            };
            tracing::info!(session_id, backfilled, "backfill completed");
            self.sink.emit_ephemeral_with_origin(
                session_id,
                AgentEventKind::RemoteSessionSyncCompleted {
                    backfilled: backfilled as _,
                    boundary,
                    node_id: Some(self.node_id.clone()),
                },
                EventOrigin::Local,
                None,
            );
            Ok(())
        }
        .instrument(span)
        .await
    }
}

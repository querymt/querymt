//! Owner-side chunked event transfer (`GetEventStreamChunk`) and the
//! requester-side fallback to `GetEventStreamSince` paging for owners that
//! predate it.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Mutex;

use crate::agent::messages::{
    EventStreamChunk, EventStreamPage, GetEventStreamChunk, MAX_EVENT_STREAM_CHUNK_BYTES,
    MAX_EVENT_STREAM_CHUNK_EVENTS,
};
use crate::error::AgentError;
use crate::events::AgentEvent;
use crate::session::projection::EventJournal;
use crate::session::turns::first_turn_start;

/// Journal read size while filling a chunk.
const READ_PAGE: usize = 500;

/// Clamp requested limits to the protocol maxima (never below one).
pub(crate) fn clamp_limits(msg: &GetEventStreamChunk) -> (usize, usize) {
    (
        msg.max_events.clamp(1, MAX_EVENT_STREAM_CHUNK_EVENTS),
        msg.max_bytes.clamp(1, MAX_EVENT_STREAM_CHUNK_BYTES),
    )
}

fn encoded_size(event: &AgentEvent) -> usize {
    rmp_serde::to_vec_named(event).map(|b| b.len()).unwrap_or(0)
}

/// Accumulates events under the event and byte limits. A chunk always accepts
/// its first event so an oversized event still makes progress.
struct Budget {
    max_events: usize,
    max_bytes: usize,
    count: usize,
    bytes: usize,
}

impl Budget {
    fn new((max_events, max_bytes): (usize, usize)) -> Self {
        Self {
            max_events,
            max_bytes,
            count: 0,
            bytes: 0,
        }
    }

    fn try_take(&mut self, event: &AgentEvent) -> bool {
        let size = encoded_size(event);
        if self.count > 0 && (self.count >= self.max_events || self.bytes + size > self.max_bytes) {
            return false;
        }
        self.count += 1;
        self.bytes += size;
        true
    }

    fn remaining_events(&self) -> usize {
        self.max_events.saturating_sub(self.count)
    }
}

/// Build the reply to a [`GetEventStreamChunk`] from the owner's journal.
pub(crate) async fn build_event_stream_chunk(
    journal: &dyn EventJournal,
    session_id: &str,
    msg: &GetEventStreamChunk,
) -> Result<EventStreamChunk, AgentError> {
    let internal = |e: crate::session::error::SessionError| AgentError::Internal(e.to_string());
    let mut budget = Budget::new(clamp_limits(msg));
    let history_epoch = journal.history_epoch(session_id).await.map_err(internal)?;
    let tip = journal.max_stream_seq(session_id).await.map_err(internal)?;

    let mut events: Vec<AgentEvent> = Vec::new();

    let has_more = if !msg.tail {
        let mut after = msg.after_source_seq.map_or(0, |s| s.max(0));
        // Forward chunks may be bounded above (exclusive), so a fill of a
        // known gap never re-transfers the range beyond it.
        let bound = msg.before_source_seq;
        let mut full = false;
        let mut bounded = false;
        'read: loop {
            let want = budget.remaining_events().clamp(1, READ_PAGE);
            let page = journal
                .load_session_page(session_id, Some(after), Some(want))
                .await
                .map_err(internal)?;
            let exhausted = page.len() < want;
            for durable in page {
                let event = AgentEvent::from(durable);
                if bound.is_some_and(|b| event.seq >= b) {
                    bounded = true;
                    break 'read;
                }
                if !budget.try_take(&event) {
                    full = true;
                    break 'read;
                }
                after = event.seq;
                events.push(event);
            }
            if exhausted {
                break;
            }
        }
        full || (!bounded && events.last().is_some_and(|e| e.seq < tip))
    } else {
        let mut before = msg.before_source_seq;
        // `after_source_seq` is an exclusive lower bound for backward chunks,
        // so filling a gap never re-reads covered history below it.
        let lower = msg.after_source_seq.map_or(0, |s| s.max(0));
        let mut exhausted = false;
        let mut full = false;
        'read: loop {
            let want = budget.remaining_events().clamp(1, READ_PAGE);
            let page = journal
                .load_session_page_before(session_id, before, want)
                .await
                .map_err(internal)?;
            let short = page.len() < want;
            for durable in page.into_iter().rev() {
                let event = AgentEvent::from(durable);
                if event.seq <= lower {
                    exhausted = true;
                    break 'read;
                }
                if !budget.try_take(&event) {
                    full = true;
                    break 'read;
                }
                before = Some(event.seq);
                events.push(event);
            }
            if short {
                exhausted = true;
                break;
            }
        }
        events.reverse();

        // Start at a turn boundary when older events remain and one fits.
        let mut trimmed = false;
        if !exhausted
            && let Some(start) = first_turn_start(events.iter().map(|e| &e.kind))
            && start > 0
        {
            events.drain(..start);
            trimmed = true;
        }
        trimmed || full || !exhausted
    };

    Ok(EventStreamChunk {
        events,
        latest_source_seq: tip,
        history_epoch,
        has_more,
    })
}

// ── Requester-side fallback ──────────────────────────────────────────────

/// Owners (by stable node key) known not to support `GetEventStreamChunk`.
/// Lives for the process lifetime.
#[derive(Default)]
pub(crate) struct ChunkSupportCache {
    unsupported: Mutex<HashSet<String>>,
    supported: Mutex<HashSet<String>>,
}

impl ChunkSupportCache {
    pub(crate) fn lacks_support(&self, owner: &str) -> bool {
        self.unsupported.lock().unwrap().contains(owner)
    }

    /// True once a chunk request to `owner` has succeeded.
    pub(crate) fn has_support(&self, owner: &str) -> bool {
        self.supported.lock().unwrap().contains(owner)
    }

    pub(crate) fn mark_unsupported(&self, owner: &str) {
        self.unsupported.lock().unwrap().insert(owner.to_string());
    }

    pub(crate) fn mark_supported(&self, owner: &str) {
        self.supported.lock().unwrap().insert(owner.to_string());
    }
}

pub(crate) static CHUNK_SUPPORT: std::sync::LazyLock<ChunkSupportCache> =
    std::sync::LazyLock::new(ChunkSupportCache::default);

/// True for the failure an owner returns for a message type it has not
/// registered (`RemoteSendError::UnknownMessage` classifies as
/// `ProtocolMismatch`; see `querymt_remote::classify_remote_send_error`).
pub(crate) fn is_unsupported_message(error: &AgentError) -> bool {
    error.transport_failure().is_some_and(|failure| {
        failure.kind == querymt_remote::RemoteTransportFailureKind::ProtocolMismatch
    })
}

/// Fetch a chunk, falling back to `GetEventStreamSince` paging when the owner
/// does not support chunks. The result is remembered per owner in `cache`.
///
/// Fallback replies carry `history_epoch` 0, since old owners have none.
pub(crate) async fn chunk_with_fallback<C, CF, P, PF>(
    owner: &str,
    cache: &ChunkSupportCache,
    msg: GetEventStreamChunk,
    chunk: C,
    pages: P,
) -> Result<EventStreamChunk, AgentError>
where
    C: FnOnce(GetEventStreamChunk) -> CF,
    CF: Future<Output = Result<EventStreamChunk, AgentError>>,
    P: FnMut(i64) -> PF,
    PF: Future<Output = Result<EventStreamPage, AgentError>>,
{
    if !cache.lacks_support(owner) {
        match chunk(msg.clone()).await {
            Err(error) if is_unsupported_message(&error) => cache.mark_unsupported(owner),
            other => {
                if other.is_ok() {
                    cache.mark_supported(owner);
                }
                return other;
            }
        }
    }
    chunk_from_pages(&msg, pages).await
}

/// Emulate a chunk with `GetEventStreamSince` pages.
async fn chunk_from_pages<P, PF>(
    msg: &GetEventStreamChunk,
    mut pages: P,
) -> Result<EventStreamChunk, AgentError>
where
    P: FnMut(i64) -> PF,
    PF: Future<Output = Result<EventStreamPage, AgentError>>,
{
    let limits = clamp_limits(msg);
    let mut after = msg.after_source_seq.map_or(0, |s| s.max(0));
    let upper = msg.before_source_seq.unwrap_or(i64::MAX);
    let mut bounded = false;
    let mut collected: Vec<AgentEvent> = Vec::new();
    let mut tip = 0;
    let mut stopped_early = false;
    let mut budget = Budget::new(limits);
    'outer: loop {
        let page = pages(after).await?;
        tip = tip.max(page.latest_source_seq);
        let last = page.events.last().map(|e| e.seq);
        for event in page.events {
            if event.seq >= upper {
                bounded = true;
                break 'outer;
            }
            if !msg.tail && !budget.try_take(&event) {
                stopped_early = true;
                break 'outer;
            }
            collected.push(event);
        }
        match last {
            Some(seq) if seq < page.latest_source_seq => after = seq,
            _ => break,
        }
    }

    let mut has_more =
        !msg.tail && (stopped_early || (!bounded && collected.last().is_some_and(|e| e.seq < tip)));
    if msg.tail {
        let mut budget = Budget::new(limits);
        let mut keep = 0;
        for event in collected.iter().rev() {
            if !budget.try_take(event) {
                break;
            }
            keep += 1;
        }
        let skip = collected.len() - keep;
        collected.drain(..skip);
        has_more = skip > 0;
        if has_more
            && let Some(start) = first_turn_start(collected.iter().map(|e| &e.kind))
            && start > 0
        {
            collected.drain(..start);
        }
    }

    Ok(EventStreamChunk {
        events: collected,
        latest_source_seq: tip,
        history_epoch: 0,
        has_more,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{AgentEventKind, EventOrigin};
    use crate::session::projection::NewDurableEvent;
    use crate::session::sqlite_storage::SqliteStorage;
    use querymt_remote::{DeliveryCertainty, RemoteTransportFailure, RemoteTransportFailureKind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SID: &str = "s1";

    async fn storage() -> SqliteStorage {
        SqliteStorage::connect(":memory:".into()).await.unwrap()
    }

    async fn append(storage: &SqliteStorage, kind: AgentEventKind) -> i64 {
        storage
            .append_durable(&NewDurableEvent {
                session_id: SID.into(),
                origin: EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                session_seq: None,
                kind,
            })
            .await
            .unwrap()
            .stream_seq
    }

    fn text(content: impl Into<String>) -> AgentEventKind {
        AgentEventKind::PromptReceived {
            content: content.into(),
            message_id: None,
        }
    }

    fn run(id: &str) -> AgentEventKind {
        AgentEventKind::RunStarted {
            run_id: id.into(),
            origin: "interactive".into(),
        }
    }

    fn fwd(after: Option<i64>, max_events: usize, max_bytes: usize) -> GetEventStreamChunk {
        GetEventStreamChunk {
            after_source_seq: after,
            before_source_seq: None,
            tail: false,
            max_events,
            max_bytes,
        }
    }

    fn back(before: Option<i64>, max_events: usize, max_bytes: usize) -> GetEventStreamChunk {
        GetEventStreamChunk {
            after_source_seq: None,
            before_source_seq: before,
            tail: true,
            max_events,
            max_bytes,
        }
    }

    fn seqs(chunk: &EventStreamChunk) -> Vec<i64> {
        chunk.events.iter().map(|e| e.seq).collect()
    }

    #[test]
    fn limits_are_clamped_to_protocol_maxima_and_floor_of_one() {
        assert_eq!(
            clamp_limits(&fwd(None, usize::MAX, usize::MAX)),
            (MAX_EVENT_STREAM_CHUNK_EVENTS, MAX_EVENT_STREAM_CHUNK_BYTES)
        );
        assert_eq!(clamp_limits(&fwd(None, 0, 0)), (1, 1));
    }

    #[tokio::test]
    async fn typical_session_is_one_complete_forward_chunk() {
        let s = storage().await;
        for i in 0..30 {
            append(&s, text(format!("e{i}"))).await;
        }
        let chunk = build_event_stream_chunk(&s, SID, &fwd(None, 5_000, 4 << 20))
            .await
            .unwrap();
        assert_eq!(chunk.events.len(), 30);
        assert!(!chunk.has_more);
        assert_eq!(chunk.latest_source_seq, 30);
        assert_eq!(chunk.history_epoch, 0);
    }

    #[tokio::test]
    async fn event_limit_splits_forward_transfer_without_gaps() {
        let s = storage().await;
        for i in 0..25 {
            append(&s, text(format!("e{i}"))).await;
        }
        let first = build_event_stream_chunk(&s, SID, &fwd(None, 10, 4 << 20))
            .await
            .unwrap();
        assert_eq!(first.events.len(), 10);
        assert!(first.has_more);
        let second =
            build_event_stream_chunk(&s, SID, &fwd(Some(first.events[9].seq), 10, 4 << 20))
                .await
                .unwrap();
        let third =
            build_event_stream_chunk(&s, SID, &fwd(Some(second.events[9].seq), 10, 4 << 20))
                .await
                .unwrap();
        assert_eq!(third.events.len(), 5);
        assert!(!third.has_more);
        let all: Vec<i64> = [first, second, third].iter().flat_map(seqs).collect();
        assert_eq!(all, (1..=25).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn byte_limit_splits_transfer() {
        let s = storage().await;
        for _ in 0..6 {
            append(&s, text("x".repeat(10_000))).await;
        }
        let chunk = build_event_stream_chunk(&s, SID, &fwd(None, 5_000, 25_000))
            .await
            .unwrap();
        assert_eq!(chunk.events.len(), 2, "10 KB events: two fit under 25 KB");
        assert!(chunk.has_more);
    }

    #[tokio::test]
    async fn single_oversized_event_still_advances() {
        let s = storage().await;
        append(&s, text("y".repeat(50_000))).await;
        append(&s, text("small")).await;
        let chunk = build_event_stream_chunk(&s, SID, &fwd(None, 5_000, 1_000))
            .await
            .unwrap();
        assert_eq!(seqs(&chunk), vec![1]);
        assert!(chunk.has_more);
        let backward = build_event_stream_chunk(&s, SID, &back(Some(2), 5_000, 1_000))
            .await
            .unwrap();
        assert_eq!(seqs(&backward), vec![1]);
    }

    #[tokio::test]
    async fn backward_chunk_is_ascending_and_ends_before_the_bound() {
        let s = storage().await;
        for i in 0..10 {
            append(&s, text(format!("e{i}"))).await;
        }
        let tail = build_event_stream_chunk(&s, SID, &back(None, 4, 4 << 20))
            .await
            .unwrap();
        assert_eq!(seqs(&tail), vec![7, 8, 9, 10]);
        assert!(tail.has_more);
        let older = build_event_stream_chunk(&s, SID, &back(Some(7), 4, 4 << 20))
            .await
            .unwrap();
        assert_eq!(seqs(&older), vec![3, 4, 5, 6]);
        let oldest = build_event_stream_chunk(&s, SID, &back(Some(3), 4, 4 << 20))
            .await
            .unwrap();
        assert_eq!(seqs(&oldest), vec![1, 2]);
        assert!(!oldest.has_more);
    }

    #[tokio::test]
    async fn backward_chunk_starts_at_a_run_boundary() {
        let s = storage().await;
        // seq: 1 run r1, 2 prompt, 3 other, 4 run r2, 5 prompt, 6 other
        append(&s, run("r1")).await;
        append(&s, text("p1")).await;
        append(&s, AgentEventKind::Cancelled).await;
        append(&s, run("r2")).await;
        append(&s, text("p2")).await;
        append(&s, AgentEventKind::Cancelled).await;
        // Limit of 4 would start at seq 3 (mid-run); the boundary is seq 4.
        let chunk = build_event_stream_chunk(&s, SID, &back(None, 4, 4 << 20))
            .await
            .unwrap();
        assert_eq!(seqs(&chunk), vec![4, 5, 6]);
        assert!(chunk.has_more);
        // The next backward request picks up exactly where this one stopped.
        let older = build_event_stream_chunk(&s, SID, &back(Some(4), 4, 4 << 20))
            .await
            .unwrap();
        assert_eq!(seqs(&older), vec![1, 2, 3]);
        assert!(!older.has_more);
    }

    // ── Fallback ─────────────────────────────────────────────────────────

    fn unsupported() -> AgentError {
        AgentError::from_transport_failure(RemoteTransportFailure::new(
            RemoteTransportFailureKind::ProtocolMismatch,
            DeliveryCertainty::NotDelivered,
            "unknown message",
        ))
    }

    #[test]
    fn only_protocol_mismatch_means_unsupported() {
        assert!(is_unsupported_message(&unsupported()));
        let timeout = AgentError::from_transport_failure(RemoteTransportFailure::new(
            RemoteTransportFailureKind::ReplyTimeout,
            DeliveryCertainty::Unknown,
            "t",
        ));
        assert!(!is_unsupported_message(&timeout));
        assert!(!is_unsupported_message(&AgentError::Internal("x".into())));
    }

    #[test]
    fn unknown_message_from_kameo_classifies_as_unsupported() {
        use kameo::error::RemoteSendError;
        let failure =
            querymt_remote::classify_remote_send_error::<String>(RemoteSendError::UnknownMessage {
                actor_remote_id: std::borrow::Cow::Borrowed("querymt::SessionActor"),
                message_remote_id: std::borrow::Cow::Borrowed("querymt::GetEventStreamChunk"),
            })
            .expect("transport failure");
        assert!(is_unsupported_message(&AgentError::from_transport_failure(
            failure
        )));
    }

    /// Owner stub serving `GetEventStreamSince` pages from fixed events.
    fn paged_owner(
        owner: SqliteStorage,
        page_size: usize,
        page_calls: &'static AtomicUsize,
    ) -> impl FnMut(i64) -> std::pin::Pin<Box<dyn Future<Output = Result<EventStreamPage, AgentError>>>>
    {
        let owner = std::sync::Arc::new(owner);
        move |after| {
            let owner = owner.clone();
            Box::pin(async move {
                page_calls.fetch_add(1, Ordering::SeqCst);
                let events = owner
                    .load_session_page(SID, Some(after), Some(page_size))
                    .await
                    .unwrap()
                    .into_iter()
                    .map(AgentEvent::from)
                    .collect();
                let latest_source_seq = owner.max_stream_seq(SID).await.unwrap();
                Ok(EventStreamPage {
                    events,
                    latest_source_seq,
                })
            })
        }
    }

    #[tokio::test]
    async fn old_owner_falls_back_with_identical_events_and_is_remembered() {
        static PAGES: AtomicUsize = AtomicUsize::new(0);
        let owner = storage().await;
        for i in 0..450 {
            append(&owner, text(format!("e{i}"))).await;
        }
        let expected = build_event_stream_chunk(&owner, SID, &fwd(None, 5_000, 4 << 20))
            .await
            .unwrap();

        let cache = ChunkSupportCache::default();
        let chunk_requests = AtomicUsize::new(0);
        let attempt = |msg: GetEventStreamChunk| {
            chunk_requests.fetch_add(1, Ordering::SeqCst);
            async move {
                let _ = msg;
                Err::<EventStreamChunk, _>(unsupported())
            }
        };
        let first = chunk_with_fallback(
            "owner-1",
            &cache,
            fwd(None, 5_000, 4 << 20),
            attempt,
            paged_owner(storage_clone(&owner).await, 200, &PAGES),
        )
        .await
        .unwrap();
        assert_eq!(seqs(&first), seqs(&expected));
        assert!(!first.has_more);
        assert_eq!(chunk_requests.load(Ordering::SeqCst), 1);

        // A second open from the same owner never sends a chunk request.
        let second = chunk_with_fallback(
            "owner-1",
            &cache,
            fwd(None, 5_000, 4 << 20),
            attempt,
            paged_owner(storage_clone(&owner).await, 200, &PAGES),
        )
        .await
        .unwrap();
        assert_eq!(seqs(&second), seqs(&expected));
        assert_eq!(chunk_requests.load(Ordering::SeqCst), 1);
        // Another owner is still probed.
        assert!(!cache.lacks_support("owner-2"));
    }

    /// Copy all events into a fresh storage so the paged stub owns its data.
    async fn storage_clone(src: &SqliteStorage) -> SqliteStorage {
        let dst = storage().await;
        for e in src.load_session_page(SID, None, None).await.unwrap() {
            append(&dst, e.kind).await;
        }
        dst
    }

    #[tokio::test]
    async fn successful_chunk_marks_owner_supported() {
        static PAGES: AtomicUsize = AtomicUsize::new(0);
        let cache = ChunkSupportCache::default();
        let owner = storage().await;
        append(&owner, text("e")).await;
        let chunk = build_event_stream_chunk(&owner, SID, &fwd(None, 10, 4 << 20))
            .await
            .unwrap();
        chunk_with_fallback(
            "owner-new",
            &cache,
            fwd(None, 10, 4 << 20),
            |_| async move { Ok(chunk) },
            paged_owner(storage().await, 200, &PAGES),
        )
        .await
        .unwrap();
        assert!(cache.has_support("owner-new"));
        assert!(!cache.lacks_support("owner-new"));
        assert!(!cache.has_support("owner-old"));
    }

    #[tokio::test]
    async fn other_chunk_errors_do_not_trigger_fallback() {
        static PAGES: AtomicUsize = AtomicUsize::new(0);
        let cache = ChunkSupportCache::default();
        let result = chunk_with_fallback(
            "owner-x",
            &cache,
            fwd(None, 10, 100),
            |_| async { Err::<EventStreamChunk, _>(AgentError::Internal("boom".into())) },
            paged_owner(storage().await, 200, &PAGES),
        )
        .await;
        assert!(result.is_err());
        assert!(!cache.lacks_support("owner-x"));
        assert_eq!(PAGES.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fallback_backward_chunk_matches_owner_chunk() {
        static PAGES: AtomicUsize = AtomicUsize::new(0);
        let owner = storage().await;
        append(&owner, run("r1")).await;
        append(&owner, text("p1")).await;
        append(&owner, AgentEventKind::Cancelled).await;
        append(&owner, run("r2")).await;
        append(&owner, text("p2")).await;
        append(&owner, AgentEventKind::Cancelled).await;
        let expected = build_event_stream_chunk(&owner, SID, &back(None, 4, 4 << 20))
            .await
            .unwrap();
        let got = chunk_from_pages(
            &back(None, 4, 4 << 20),
            paged_owner(storage_clone(&owner).await, 2, &PAGES),
        )
        .await
        .unwrap();
        assert_eq!(seqs(&got), seqs(&expected));
        assert_eq!(got.has_more, expected.has_more);
    }
}

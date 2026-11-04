//! Per-session sync state for a remote session's history.
//!
//! The attaching agent keeps, per `(session, owner node)`, the set of
//! `source_seq` ranges it holds locally (persisted beside the remote sync
//! cursor), the ranges currently being fetched, and the owner's history epoch
//! seen at the last sync. A `watch` barrier signals every change; it lives in
//! the [`RemoteSyncRegistry`] owned by the `EventSink`, so it outlives any
//! attachment or backfill task.
//!
//! The state is deliberately transport-free: drivers (see
//! `agent::remote::event_backfill`) fetch chunks and report them here.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{Mutex as AsyncMutex, OnceCell, watch};

use crate::session::error::SessionResult;
use crate::session::projection::EventJournal;
use crate::session::ranges::RangeSet;

/// Result of reserving part of a requested range for fetching.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// The whole requested range is already local.
    Covered,
    /// Missing parts exist, but every one is being fetched by someone else.
    Busy,
    /// The caller now owns this missing part and must [`RemoteSyncState::release`] it.
    Part(i64, i64),
}

/// Outcome of comparing a chunk's history epoch with the stored one.
#[derive(Debug, PartialEq, Eq)]
pub enum EpochCheck {
    /// Same epoch (or first observation): apply the chunk.
    Current,
    /// The chunk predates the stored epoch: drop it and re-request.
    Stale,
    /// The owner rewrote its history: local rows and coverage were deleted.
    /// The chunk was not applied; the caller must resync.
    Reset,
}

#[derive(Default)]
struct Inner {
    coverage: RangeSet,
    reserved: RangeSet,
    epoch: Option<u64>,
    tip: i64,
    /// True once a sync observed the owner's tip after the live relay was
    /// subscribed, so relayed events beyond the covered range can extend it.
    live_anchored: bool,
    fill_running: bool,
    /// On-demand requests currently waiting for or fetching a range. The
    /// background fill yields to them.
    demand: usize,
    /// Syncs (attach backfill, reopen increments) currently running.
    syncing: usize,
    version: u64,
}

pub struct RemoteSyncState {
    session_id: String,
    node_id: String,
    journal: Arc<dyn EventJournal>,
    inner: Mutex<Inner>,
    hydrated: OnceCell<()>,
    /// Serialises epoch checks, persistence and coverage updates of chunks.
    apply: AsyncMutex<()>,
    /// Serialises opens (first sync / incremental sync) so a load racing the
    /// attach backfill waits for it instead of transferring the same range.
    open: AsyncMutex<()>,
    changed: watch::Sender<u64>,
    requests: std::sync::atomic::AtomicU64,
    received: std::sync::atomic::AtomicU64,
    inserted: std::sync::atomic::AtomicU64,
}

impl RemoteSyncState {
    fn new(session_id: &str, node_id: &str, journal: Arc<dyn EventJournal>) -> Self {
        Self {
            session_id: session_id.to_string(),
            node_id: node_id.to_string(),
            journal,
            inner: Mutex::new(Inner::default()),
            hydrated: OnceCell::new(),
            apply: AsyncMutex::new(()),
            open: AsyncMutex::new(()),
            changed: watch::channel(0).0,
            requests: Default::default(),
            received: Default::default(),
            inserted: Default::default(),
        }
    }

    /// Count one chunk request that returned `received` events.
    pub fn record_request(&self, received: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        self.requests.fetch_add(1, Relaxed);
        self.received.fetch_add(received as u64, Relaxed);
    }

    /// Count events newly persisted (not duplicates).
    pub fn record_inserted(&self, inserted: usize) {
        self.inserted
            .fetch_add(inserted as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// `(chunk requests, events received, events newly persisted)` so far.
    pub fn transfers(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.requests.load(Relaxed),
            self.received.load(Relaxed),
            self.inserted.load(Relaxed),
        )
    }

    /// Load persisted coverage, cursor and epoch (once).
    pub async fn hydrate(&self) -> SessionResult<()> {
        self.hydrated
            .get_or_try_init(|| async {
                let cursor = self
                    .journal
                    .remote_sync_cursor(&self.session_id, &self.node_id)
                    .await?
                    .unwrap_or(0);
                let ranges = self
                    .journal
                    .remote_coverage(&self.session_id, &self.node_id)
                    .await?;
                let epoch = self
                    .journal
                    .remote_history_epoch(&self.session_id, &self.node_id)
                    .await?;
                let mut inner = self.inner.lock();
                inner.coverage = RangeSet::from_ranges(ranges);
                if cursor > 0 {
                    inner.coverage.insert(1, cursor);
                }
                inner.tip = inner.tip.max(inner.coverage.max_end().unwrap_or(0));
                inner.epoch = epoch;
                Ok(())
            })
            .await
            .map(|_| ())
    }

    fn bump(&self, inner: &mut Inner) {
        inner.version += 1;
        self.changed.send_replace(inner.version);
    }

    /// Subscribe before checking a condition, then `changed().await`, so a
    /// change between the check and the wait is never missed.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn coverage(&self) -> RangeSet {
        self.inner.lock().coverage.clone()
    }

    pub fn is_covered(&self, start: i64, end: i64) -> bool {
        self.inner.lock().coverage.covers(start, end)
    }

    pub fn has_data(&self) -> bool {
        !self.inner.lock().coverage.is_empty()
    }

    /// Highest covered `source_seq` (0 when nothing is covered).
    pub fn max_covered(&self) -> i64 {
        self.inner.lock().coverage.max_end().unwrap_or(0)
    }

    /// Highest owner tip observed so far.
    pub fn tip(&self) -> i64 {
        self.inner.lock().tip
    }

    pub fn epoch(&self) -> Option<u64> {
        self.inner.lock().epoch
    }

    /// Reserve the first missing, unreserved part of `[start, end]`.
    pub fn claim(&self, start: i64, end: i64) -> Claim {
        let mut inner = self.inner.lock();
        let gaps = inner.coverage.gaps(start, end);
        if gaps.is_empty() {
            return Claim::Covered;
        }
        let free = gaps
            .iter()
            .find_map(|&(a, b)| inner.reserved.gaps(a, b).into_iter().next());
        match free {
            Some((a, b)) => {
                inner.reserved.insert(a, b);
                Claim::Part(a, b)
            }
            None => Claim::Busy,
        }
    }

    /// Like [`Self::claim`] for the background fill: yields (`Busy`) while an
    /// on-demand request is pending, so a user waiting for older history is
    /// never starved by the fill re-claiming every released range.
    pub fn claim_fill(&self, start: i64, end: i64) -> Claim {
        if self.inner.lock().demand > 0 {
            return Claim::Busy;
        }
        self.claim(start, end)
    }

    /// Mark a sync as running until the guard drops.
    pub fn begin_sync(self: &Arc<Self>) -> SyncGuard {
        self.inner.lock().syncing += 1;
        SyncGuard(self.clone())
    }

    /// Wait until no sync is running. This is the load barrier: a load that
    /// triggered or joined a sync waits here, then reads the local journal.
    pub async fn wait_synced(&self) {
        loop {
            let mut rx = self.changed.subscribe();
            if self.inner.lock().syncing == 0 {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Mark an on-demand request as pending until the guard drops.
    pub fn demand_guard(self: &Arc<Self>) -> DemandGuard {
        self.inner.lock().demand += 1;
        DemandGuard(self.clone())
    }

    pub fn release(&self, start: i64, end: i64) {
        let mut inner = self.inner.lock();
        inner.reserved.remove(start, end);
        self.bump(&mut inner);
    }

    /// Exclusive access for running an open.
    pub async fn lock_open(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.open.lock().await
    }

    /// Exclusive access for applying one chunk.
    pub async fn lock_apply(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.apply.lock().await
    }

    /// Compare a chunk's epoch with the stored one. The caller holds the
    /// [`Self::lock_apply`] guard.
    pub async fn check_epoch(&self, chunk_epoch: u64) -> SessionResult<EpochCheck> {
        let (stored, has_data) = {
            let inner = self.inner.lock();
            (inner.epoch, !inner.coverage.is_empty())
        };
        match stored {
            Some(epoch) if epoch == chunk_epoch => Ok(EpochCheck::Current),
            Some(epoch) if chunk_epoch < epoch => Ok(EpochCheck::Stale),
            // First observation: remember it. Existing data of unknown
            // epoch is only trusted while the owner never rewrote history.
            None if !has_data || chunk_epoch == 0 => {
                self.journal
                    .set_remote_history_epoch(&self.session_id, &self.node_id, chunk_epoch)
                    .await?;
                let mut inner = self.inner.lock();
                inner.epoch = Some(chunk_epoch);
                Ok(EpochCheck::Current)
            }
            _ => {
                self.reset_local_copy_locked(chunk_epoch).await?;
                Ok(EpochCheck::Reset)
            }
        }
    }

    /// Discard the local copy of this source (rows, coverage, cursor) so the
    /// next sync transfers it afresh. Used when the owner cannot report
    /// rewrites (no chunk support), where each sync replaces the copy.
    pub async fn reset_local_copy(&self) -> SessionResult<()> {
        let _apply = self.lock_apply().await;
        self.reset_local_copy_locked(0).await
    }

    /// As [`Self::reset_local_copy`]; the caller holds the apply guard.
    pub async fn reset_local_copy_locked(&self, epoch: u64) -> SessionResult<()> {
        self.journal
            .reset_remote_source(&self.session_id, &self.node_id)
            .await?;
        self.journal
            .set_remote_history_epoch(&self.session_id, &self.node_id, epoch)
            .await?;
        let mut inner = self.inner.lock();
        inner.coverage.clear();
        inner.epoch = Some(epoch);
        inner.tip = 0;
        inner.live_anchored = false;
        self.bump(&mut inner);
        Ok(())
    }

    /// Record `[start, end]` as fully held and advance the contiguous sync
    /// cursor. `tip` is the owner's stream tip from the same chunk. The
    /// caller holds the [`Self::lock_apply`] guard and has persisted the
    /// chunk's events.
    pub async fn mark_covered(&self, start: i64, end: i64, tip: i64) -> SessionResult<()> {
        if start <= end {
            self.journal
                .add_remote_coverage(&self.session_id, &self.node_id, start, end)
                .await?;
        }
        let (contiguous, tip) = {
            let mut inner = self.inner.lock();
            inner.coverage.insert(start, end);
            inner.tip = inner.tip.max(tip).max(end);
            (inner.coverage.contiguous_end(), inner.tip)
        };
        // The cursor only ever describes the contiguous prefix, so older
        // agents resuming from it never skip a hole.
        self.journal
            .advance_remote_sync_cursor(
                &self.session_id,
                &self.node_id,
                contiguous,
                contiguous >= tip,
            )
            .await?;
        let mut inner = self.inner.lock();
        self.bump(&mut inner);
        Ok(())
    }

    pub fn set_live_anchored(&self, anchored: bool) {
        self.inner.lock().live_anchored = anchored;
    }

    /// A relayed (or gap-recovered) live event with owner position `seq` was
    /// persisted. When a sync anchored the live stream, everything between the
    /// covered tip and `seq` is already held (the relay delivers in order and
    /// repairs gaps), so coverage follows it - unless a fetch is reserving
    /// that range, which will cover it itself.
    pub async fn note_live(&self, seq: i64) -> SessionResult<()> {
        // Decide only after taking the apply lock: an epoch reset or a fetch
        // may have changed the state while this call waited.
        let _guard = self.apply.lock().await;
        let range = {
            let inner = self.inner.lock();
            let max = inner.coverage.max_end().unwrap_or(0);
            if !inner.live_anchored
                || seq <= max
                || !inner.reserved.gaps(max + 1, seq).eq(&[(max + 1, seq)])
            {
                return Ok(());
            }
            (max + 1, seq)
        };
        self.journal
            .add_remote_coverage(&self.session_id, &self.node_id, range.0, range.1)
            .await?;
        let mut inner = self.inner.lock();
        inner.coverage.insert(range.0, range.1);
        inner.tip = inner.tip.max(range.1);
        self.bump(&mut inner);
        Ok(())
    }

    /// Claim the single background fill of this session.
    pub fn try_start_fill(&self) -> bool {
        let mut inner = self.inner.lock();
        !std::mem::replace(&mut inner.fill_running, true)
    }

    pub fn finish_fill(&self) {
        self.inner.lock().fill_running = false;
    }
}

/// Marks a running sync; see [`RemoteSyncState::wait_synced`].
pub struct SyncGuard(Arc<RemoteSyncState>);

impl Drop for SyncGuard {
    fn drop(&mut self) {
        let mut inner = self.0.inner.lock();
        inner.syncing = inner.syncing.saturating_sub(1);
        self.0.bump(&mut inner);
    }
}

/// Keeps the background fill yielding while an on-demand request is pending.
pub struct DemandGuard(Arc<RemoteSyncState>);

impl Drop for DemandGuard {
    fn drop(&mut self) {
        let mut inner = self.0.inner.lock();
        inner.demand = inner.demand.saturating_sub(1);
        self.0.bump(&mut inner);
    }
}

/// All sync states of one `EventSink`.
#[derive(Default)]
pub struct RemoteSyncRegistry {
    states: Mutex<HashMap<(String, String), Arc<RemoteSyncState>>>,
}

impl RemoteSyncRegistry {
    pub fn state(
        &self,
        session_id: &str,
        node_id: &str,
        journal: &Arc<dyn EventJournal>,
    ) -> Arc<RemoteSyncState> {
        self.states
            .lock()
            .entry((session_id.to_string(), node_id.to_string()))
            .or_insert_with(|| Arc::new(RemoteSyncState::new(session_id, node_id, journal.clone())))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;

    async fn state() -> (Arc<RemoteSyncState>, Arc<dyn EventJournal>) {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let journal = storage.event_journal();
        let registry = RemoteSyncRegistry::default();
        let state = registry.state("s1", "node-a", &journal);
        state.hydrate().await.unwrap();
        (state, journal)
    }

    #[tokio::test]
    async fn claim_reserves_missing_parts_and_reports_busy() {
        let (state, _) = state().await;
        state.mark_covered(10, 20, 20).await.unwrap();
        assert_eq!(state.claim(10, 20), Claim::Covered);
        assert_eq!(state.claim(1, 30), Claim::Part(1, 9));
        // The same request now sees the other missing part, never the first.
        assert_eq!(state.claim(1, 30), Claim::Part(21, 30));
        assert_eq!(state.claim(1, 30), Claim::Busy);
        state.release(1, 9);
        assert_eq!(state.claim(1, 30), Claim::Part(1, 9));
    }

    #[tokio::test]
    async fn reset_local_copy_clears_rows_coverage_and_cursor() {
        let (state, journal) = state().await;
        state.mark_covered(1, 20, 20).await.unwrap();
        assert!(state.has_data());
        state.reset_local_copy().await.unwrap();
        assert!(!state.has_data());
        assert_eq!(state.tip(), 0);
        assert_eq!(state.claim(1, 20), Claim::Part(1, 20));
        assert_eq!(
            journal.remote_sync_cursor("s1", "node-a").await.unwrap(),
            Some(0)
        );
        assert!(
            journal
                .remote_coverage("s1", "node-a")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn coverage_and_epoch_persist_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.db");
        {
            let storage = SqliteStorage::connect(path.clone()).await.unwrap();
            let journal = storage.event_journal();
            let state = RemoteSyncRegistry::default().state("s1", "node-a", &journal);
            state.hydrate().await.unwrap();
            assert_eq!(state.check_epoch(3).await.unwrap(), EpochCheck::Current);
            state.mark_covered(50, 60, 60).await.unwrap();
            state.mark_covered(61, 70, 70).await.unwrap();
            state.mark_covered(1, 9, 70).await.unwrap();
        }
        let storage = SqliteStorage::connect(path).await.unwrap();
        let journal = storage.event_journal();
        let state = RemoteSyncRegistry::default().state("s1", "node-a", &journal);
        state.hydrate().await.unwrap();
        assert_eq!(state.coverage().ranges(), &[(1, 9), (50, 70)]);
        assert_eq!(state.epoch(), Some(3));
        assert_eq!(state.tip(), 70);
        // The cursor only describes the contiguous prefix.
        assert_eq!(
            journal.remote_sync_cursor("s1", "node-a").await.unwrap(),
            Some(9)
        );
    }

    #[tokio::test]
    async fn epoch_mismatch_resets_and_stale_chunks_are_dropped() {
        let (state, journal) = state().await;
        assert_eq!(state.check_epoch(0).await.unwrap(), EpochCheck::Current);
        state.mark_covered(1, 10, 10).await.unwrap();
        assert_eq!(state.check_epoch(1).await.unwrap(), EpochCheck::Reset);
        assert!(!state.has_data());
        assert_eq!(state.epoch(), Some(1));
        assert_eq!(state.check_epoch(0).await.unwrap(), EpochCheck::Stale);
        assert_eq!(
            journal.remote_sync_cursor("s1", "node-a").await.unwrap(),
            Some(0)
        );
        assert!(
            journal
                .remote_coverage("s1", "node-a")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn live_events_extend_coverage_only_when_anchored_and_unreserved() {
        let (state, _) = state().await;
        state.mark_covered(1, 10, 10).await.unwrap();
        state.note_live(14).await.unwrap();
        assert_eq!(state.max_covered(), 10, "not anchored yet");
        state.set_live_anchored(true);
        assert_eq!(state.claim(11, 20), Claim::Part(11, 20));
        state.note_live(14).await.unwrap();
        assert_eq!(state.max_covered(), 10, "range is being fetched");
        state.release(11, 20);
        state.note_live(14).await.unwrap();
        assert_eq!(state.max_covered(), 14);
    }

    #[tokio::test]
    async fn live_note_waiting_for_apply_lock_revalidates_after_epoch_reset() {
        let (state, journal) = state().await;
        assert_eq!(state.check_epoch(0).await.unwrap(), EpochCheck::Current);
        state.mark_covered(1, 10, 10).await.unwrap();
        state.set_live_anchored(true);

        let guard = state.lock_apply().await;
        let waiting = {
            let state = state.clone();
            tokio::spawn(async move { state.note_live(14).await })
        };
        tokio::task::yield_now().await;
        // The reset runs while note_live is queued behind the apply lock.
        assert_eq!(state.check_epoch(1).await.unwrap(), EpochCheck::Reset);
        drop(guard);
        waiting.await.unwrap().unwrap();

        assert_eq!(state.max_covered(), 0, "no coverage after the reset");
        assert!(
            journal
                .remote_coverage("s1", "node-a")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn fill_yields_to_pending_on_demand_requests() {
        let (state, _) = state().await;
        assert_eq!(state.claim_fill(1, 10), Claim::Part(1, 10));
        state.release(1, 10);
        let guard = state.demand_guard();
        assert_eq!(state.claim_fill(1, 10), Claim::Busy);
        assert_eq!(state.claim(1, 10), Claim::Part(1, 10));
        drop(guard);
        state.release(1, 10);
        assert_eq!(state.claim_fill(1, 10), Claim::Part(1, 10));
    }

    #[tokio::test]
    async fn barrier_wakes_waiters_on_change() {
        let (state, _) = state().await;
        let mut rx = state.subscribe();
        let waiter = {
            let state = state.clone();
            tokio::spawn(async move {
                while !state.is_covered(1, 5) {
                    rx.changed().await.unwrap();
                }
            })
        };
        tokio::task::yield_now().await;
        state.mark_covered(1, 5, 5).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap();
    }
}

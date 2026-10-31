//! Fail-closed recorder for the attempt accounting lifecycle.
//!
//! The recorder writes through the session's awaited, ordered event stream
//! ([`SessionEventStream::emit_persisted`]), so:
//!
//! * [`AttemptRecorder::begin`] returns only after the `LlmAttemptStarted`
//!   record is persisted and published. A caller dispatches to the provider
//!   *after* `begin` succeeds, so a start failure means no paid dispatch.
//! * [`AttemptRecorder::settle`] returns only after the `LlmAttemptSettled`
//!   record is persisted and published. Callers publish derived completion
//!   events (for example `LlmRequestEnd`) after it returns.
//! * If a settlement cannot be persisted it is kept in memory and retried,
//!   under the same attempt identity, before any further attempt may begin.
//!   Until it is durable, no further paid work is permitted: persistence
//!   failure is an execution error, never a warning.
//!
//! A process crash between `begin` and `settle` leaves a durable start with no
//! settlement, which accounting reports as unresolved spend. The journal is
//! only as durable as its WAL/synchronization settings; nothing here promises
//! exact billing after an unobservable crash.

use super::{AccountingConflict, AttemptRef, AttemptSettlement, AttemptStart, SessionAccounting};
use crate::events::{AgentEventKind, DurableEvent};
use crate::session::error::SessionError;
use crate::session_event_stream::SessionEventStream;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Why accounting could not proceed. Always fatal to paid dispatch.
#[derive(Debug, thiserror::Error)]
pub enum AccountingError {
    /// The journal rejected or lost an accounting record.
    #[error("accounting record could not be persisted: {0}")]
    Storage(#[from] SessionError),
    /// A record was addressed to a different session or attempt than the
    /// recorder or handle it was submitted through.
    #[error("accounting record does not belong to this {0}")]
    Mismatch(&'static str),
    /// A durable record contradicts accounting already recorded.
    #[error(transparent)]
    Conflict(#[from] AccountingConflict),
    /// The cycle's known budget spend reached the configured cap, so no
    /// further paid attempt is dispatched.
    #[error("price limit reached: ${spent_usd:.4} >= ${cap_usd:.2}")]
    BudgetExhausted { cap_usd: f64, spent_usd: f64 },
}

/// Proof that an attempt's start is durable. Required to settle it.
#[derive(Debug)]
#[must_use = "a begun attempt must be settled"]
pub struct AttemptHandle {
    attempt: AttemptRef,
}

impl AttemptHandle {
    pub fn attempt(&self) -> &AttemptRef {
        &self.attempt
    }
}

/// Records attempt starts and settlements for one session.
pub struct AttemptRecorder {
    stream: Arc<SessionEventStream>,
    /// Settlements whose append failed, oldest first. The lock is held only
    /// across journal appends, never across provider generation.
    unpersisted: Arc<Mutex<Vec<AttemptSettlement>>>,
    /// Session projection fed with every committed record, when attached.
    projection: Option<Arc<SessionAccounting>>,
}

impl AttemptRecorder {
    pub fn new(stream: Arc<SessionEventStream>) -> Self {
        Self {
            stream,
            unpersisted: Arc::new(Mutex::new(Vec::new())),
            projection: None,
        }
    }

    /// A recorder that also applies each committed record to `projection`.
    pub fn with_projection(
        stream: Arc<SessionEventStream>,
        projection: Arc<SessionAccounting>,
    ) -> Self {
        // The backlog is shared session-wide so a settlement that could not
        // be persisted blocks paid work from every later operation.
        Self {
            unpersisted: projection.backlog(),
            projection: Some(projection),
            stream,
        }
    }

    fn observe(&self, event: &DurableEvent) -> Result<(), AccountingError> {
        if let Some(projection) = &self.projection {
            projection.observe(event)?;
        }
        Ok(())
    }

    pub fn session_id(&self) -> &str {
        self.stream.session_id()
    }

    /// True while a settlement is waiting to become durable.
    pub async fn has_unpersisted(&self) -> bool {
        !self.unpersisted.lock().await.is_empty()
    }

    /// Durably record the start of an attempt. Dispatch only on `Ok`.
    pub async fn begin(&self, start: AttemptStart) -> Result<AttemptHandle, AccountingError> {
        self.check_session(&start.attempt)?;
        let mut backlog = self.unpersisted.lock().await;
        self.drain(&mut backlog).await?;
        let attempt = start.attempt.clone();
        let event = self
            .stream
            .emit_persisted(AgentEventKind::LlmAttemptStarted { start })
            .await?;
        self.observe(&event)?;
        Ok(AttemptHandle { attempt })
    }

    /// Durably record an attempt's settlement. Consumes the handle so one
    /// begun attempt settles once through this path.
    ///
    /// On a storage failure the settlement is retained and retried (same
    /// identity) by the next `begin`/`settle`; the error is still returned.
    pub async fn settle(
        &self,
        handle: AttemptHandle,
        settlement: AttemptSettlement,
    ) -> Result<(), AccountingError> {
        if settlement.attempt != handle.attempt {
            return Err(AccountingError::Mismatch("attempt"));
        }
        let mut backlog = self.unpersisted.lock().await;
        backlog.push(settlement);
        self.drain(&mut backlog).await
    }

    fn check_session(&self, attempt: &AttemptRef) -> Result<(), AccountingError> {
        if attempt.session_id == self.stream.session_id() {
            Ok(())
        } else {
            Err(AccountingError::Mismatch("session"))
        }
    }

    /// Persist queued settlements in order, stopping at the first failure.
    async fn drain(&self, backlog: &mut Vec<AttemptSettlement>) -> Result<(), AccountingError> {
        while let Some(next) = backlog.first() {
            let event = self
                .stream
                .emit_persisted(AgentEventKind::LlmAttemptSettled {
                    settlement: next.clone(),
                })
                .await?;
            // Durable now: drop it from the backlog before observing so a
            // projection conflict can never cause a second append.
            backlog.remove(0);
            self.observe(&event)?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::accounting::tests::{sample_ref, sample_settlement, sample_start};
    use crate::event_fanout::EventFanout;
    use crate::events::DurableEvent;
    use crate::session::backend::StorageBackend;
    use crate::session::error::SessionResult;
    use crate::session::projection::{EventJournal, NewDurableEvent};
    use crate::session::sqlite_storage::SqliteStorage;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Journal that delegates to a real store but can be told to fail appends.
    pub(crate) struct Switchable {
        pub(crate) inner: Arc<dyn EventJournal>,
        pub(crate) fail: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl EventJournal for Switchable {
        async fn append_durable(&self, event: &NewDurableEvent) -> SessionResult<DurableEvent> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(SessionError::DatabaseError("injected".into()));
            }
            self.inner.append_durable(event).await
        }
        fn max_session_seq_blocking(&self, s: &str) -> i64 {
            self.inner.max_session_seq_blocking(s)
        }
        async fn load_session_stream(
            &self,
            s: &str,
            a: Option<i64>,
            l: Option<usize>,
        ) -> SessionResult<Vec<DurableEvent>> {
            self.inner.load_session_stream(s, a, l).await
        }
        async fn load_global_stream(
            &self,
            a: Option<i64>,
            l: Option<usize>,
        ) -> SessionResult<Vec<DurableEvent>> {
            self.inner.load_global_stream(a, l).await
        }
        async fn delete_session_events_from(&self, s: &str, f: i64) -> SessionResult<usize> {
            self.inner.delete_session_events_from(s, f).await
        }
        async fn append_durable_from_source(
            &self,
            e: &NewDurableEvent,
        ) -> SessionResult<Option<DurableEvent>> {
            self.inner.append_durable_from_source(e).await
        }
        async fn latest_source_seq(&self, s: &str, n: &str) -> SessionResult<Option<i64>> {
            self.inner.latest_source_seq(s, n).await
        }
        async fn remote_sync_cursor(&self, s: &str, n: &str) -> SessionResult<Option<i64>> {
            self.inner.remote_sync_cursor(s, n).await
        }
        async fn advance_remote_sync_cursor(
            &self,
            s: &str,
            n: &str,
            q: i64,
            c: bool,
        ) -> SessionResult<()> {
            self.inner.advance_remote_sync_cursor(s, n, q, c).await
        }
        async fn load_remote_session_stream(
            &self,
            s: &str,
            n: &str,
        ) -> SessionResult<Vec<DurableEvent>> {
            self.inner.load_remote_session_stream(s, n).await
        }
        async fn max_stream_seq(&self, s: &str) -> SessionResult<i64> {
            self.inner.max_stream_seq(s).await
        }
    }

    struct Fixture {
        recorder: AttemptRecorder,
        journal: Arc<dyn EventJournal>,
        fail: Arc<AtomicBool>,
        fanout: Arc<EventFanout>,
    }

    fn fixture_on(storage: &SqliteStorage) -> Fixture {
        let fail = Arc::new(AtomicBool::new(false));
        let journal: Arc<dyn EventJournal> = Arc::new(Switchable {
            inner: storage.event_journal(),
            fail: fail.clone(),
        });
        let fanout = Arc::new(EventFanout::new());
        let stream = SessionEventStream::new("sess-1", journal.clone(), fanout.clone());
        Fixture {
            recorder: AttemptRecorder::new(stream),
            journal,
            fail,
            fanout,
        }
    }

    async fn memory_fixture() -> Fixture {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        fixture_on(&storage)
    }

    /// `(kind, attempt_id)` for every accounting record, in journal order.
    async fn ledger(journal: &Arc<dyn EventJournal>) -> Vec<(&'static str, String)> {
        journal
            .load_session_stream("sess-1", None, None)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.kind {
                AgentEventKind::LlmAttemptStarted { start } => {
                    Some(("started", start.attempt.attempt_id))
                }
                AgentEventKind::LlmAttemptSettled { settlement } => {
                    Some(("settled", settlement.attempt.attempt_id))
                }
                _ => None,
            })
            .collect()
    }

    fn start_for(id: &str) -> AttemptStart {
        let mut start = sample_start();
        start.attempt.attempt_id = id.into();
        start
    }

    fn settlement_for(id: &str) -> AttemptSettlement {
        let mut settled = sample_settlement();
        settled.attempt.attempt_id = id.into();
        settled
    }

    #[tokio::test]
    async fn start_is_durable_and_published_before_begin_returns() {
        let f = memory_fixture().await;
        let mut rx = f.fanout.subscribe();
        let handle = f.recorder.begin(start_for("a1")).await.unwrap();
        assert_eq!(handle.attempt().attempt_id, "a1");
        // Already persisted and already on the fanout when begin returned.
        assert_eq!(ledger(&f.journal).await, vec![("started", "a1".into())]);
        assert!(matches!(
            rx.try_recv().expect("published").kind(),
            AgentEventKind::LlmAttemptStarted { .. }
        ));
        f.recorder
            .settle(handle, settlement_for("a1"))
            .await
            .unwrap();
        assert_eq!(
            ledger(&f.journal).await,
            vec![("started", "a1".into()), ("settled", "a1".into())]
        );
        assert!(matches!(
            rx.try_recv().expect("published").kind(),
            AgentEventKind::LlmAttemptSettled { .. }
        ));
    }

    #[tokio::test]
    async fn start_persistence_failure_prevents_dispatch() {
        let f = memory_fixture().await;
        f.fail.store(true, Ordering::SeqCst);
        let mut dispatched = 0;
        match f.recorder.begin(start_for("a1")).await {
            Ok(_) => dispatched += 1,
            Err(err) => assert!(matches!(err, AccountingError::Storage(_))),
        }
        assert_eq!(dispatched, 0, "must not dispatch without a durable start");
        f.fail.store(false, Ordering::SeqCst);
        assert!(ledger(&f.journal).await.is_empty());
    }

    #[tokio::test]
    async fn settlement_failure_blocks_further_paid_work_until_durable() {
        let f = memory_fixture().await;
        let handle = f.recorder.begin(start_for("a1")).await.unwrap();

        f.fail.store(true, Ordering::SeqCst);
        let err = f
            .recorder
            .settle(handle, settlement_for("a1"))
            .await
            .unwrap_err();
        assert!(matches!(err, AccountingError::Storage(_)));
        assert!(f.recorder.has_unpersisted().await);

        // Still failing: the next attempt cannot begin.
        assert!(f.recorder.begin(start_for("a2")).await.is_err());
        assert_eq!(ledger(&f.journal).await, vec![("started", "a1".into())]);

        // Journal recovers: the retained settlement lands first, same
        // identity, then the new attempt starts.
        f.fail.store(false, Ordering::SeqCst);
        let second = f.recorder.begin(start_for("a2")).await.unwrap();
        assert_eq!(second.attempt().attempt_id, "a2");
        assert!(!f.recorder.has_unpersisted().await);
        assert_eq!(
            ledger(&f.journal).await,
            vec![
                ("started", "a1".into()),
                ("settled", "a1".into()),
                ("started", "a2".into()),
            ]
        );
    }

    #[tokio::test]
    async fn retried_settlement_is_not_duplicated() {
        let f = memory_fixture().await;
        let handle = f.recorder.begin(start_for("a1")).await.unwrap();
        f.fail.store(true, Ordering::SeqCst);
        assert!(
            f.recorder
                .settle(handle, settlement_for("a1"))
                .await
                .is_err()
        );
        f.fail.store(false, Ordering::SeqCst);
        // A later successful begin drains once; a second begin adds no copy.
        let h2 = f.recorder.begin(start_for("a2")).await.unwrap();
        let h3 = f.recorder.begin(start_for("a3")).await.unwrap();
        drop((h2, h3));
        let settled = ledger(&f.journal)
            .await
            .into_iter()
            .filter(|(k, id)| *k == "settled" && id == "a1")
            .count();
        assert_eq!(settled, 1);
    }

    #[tokio::test]
    async fn rejects_records_for_another_session_or_attempt() {
        let f = memory_fixture().await;
        let mut foreign = start_for("a1");
        foreign.attempt.session_id = "other".into();
        assert!(matches!(
            f.recorder.begin(foreign).await,
            Err(AccountingError::Mismatch("session"))
        ));

        let handle = f.recorder.begin(start_for("a1")).await.unwrap();
        assert!(matches!(
            f.recorder.settle(handle, settlement_for("zzz")).await,
            Err(AccountingError::Mismatch("attempt"))
        ));
        assert_eq!(sample_ref().session_id, "sess-1");
    }

    #[tokio::test]
    async fn unsettled_start_is_visible_after_fresh_database_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounting.db");

        {
            let storage = SqliteStorage::connect(path.clone()).await.unwrap();
            let f = fixture_on(&storage);
            let done = f.recorder.begin(start_for("settled")).await.unwrap();
            f.recorder
                .settle(done, settlement_for("settled"))
                .await
                .unwrap();
            // Dispatched, then the process "crashes" before settlement.
            let _lost = f.recorder.begin(start_for("lost")).await.unwrap();
        }

        let reopened = SqliteStorage::connect(path).await.unwrap();
        let ledger = ledger(&reopened.event_journal()).await;
        assert_eq!(
            ledger,
            vec![
                ("started", "settled".into()),
                ("settled", "settled".into()),
                ("started", "lost".into()),
            ]
        );
        let unresolved: Vec<_> = ledger
            .iter()
            .filter(|(k, id)| *k == "started" && !ledger.contains(&("settled", id.clone())))
            .collect();
        assert_eq!(unresolved, vec![&("started", "lost".to_string())]);
    }
}

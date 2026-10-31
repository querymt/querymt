//! Session-owned, journal-backed accounting projection.
//!
//! [`SessionAccounting`] wraps an [`AccountingReducer`] with a journal cursor.
//! It is a rebuildable in-memory projection: there are no accounting tables
//! and no schema migration, because the journal already stores the
//! `LlmAttempt*` events as JSON.
//!
//! * **Hydration** pages through the session's journal once, when the session
//!   runtime loads, in bounded pages ([`PAGE_SIZE`]); full events are applied
//!   and dropped, never accumulated.
//! * **Hot path**: [`SessionAccounting::observe`] applies one newly committed
//!   record and advances the cursor, so a later [`SessionAccounting::catch_up`]
//!   reads only records after it.
//! * **Catch-up** recovers records that were committed but never observed
//!   (for example written by another component). Re-applying a record is
//!   idempotent, so overlap with `observe` is harmless.
//! * **Retirement**: the projection lives in the session runtime and is freed
//!   when the runtime is dropped.
//!
//! The cursor is the journal `stream_seq`. `observe` always applies the record
//! it is given, even if its `stream_seq` is below the cursor, so out-of-order
//! delivery between writers cannot lose a record that was observed.

use super::{AccountingConflict, AccountingError, AccountingReducer, Applied};
use crate::events::DurableEvent;
use crate::session::projection::EventJournal;
use parking_lot::Mutex;
use std::sync::Arc;

/// Journal events loaded per hydration/catch-up page.
pub const PAGE_SIZE: usize = 256;

struct State {
    reducer: AccountingReducer,
    /// Highest journal `stream_seq` applied or observed.
    cursor: i64,
    /// Journal records read by hydration/catch-up (diagnostics and tests).
    scanned: u64,
}

pub struct SessionAccounting {
    session_id: String,
    journal: Arc<dyn EventJournal>,
    page_size: usize,
    state: Mutex<State>,
    /// Serializes hydration/catch-up so concurrent callers do not rescan the
    /// same range. Never held while the state lock is.
    sync: tokio::sync::Mutex<()>,
    /// Settlements whose append failed, shared by every recorder of the
    /// session (see `AttemptRecorder`).
    backlog: Arc<tokio::sync::Mutex<Vec<super::AttemptSettlement>>>,
}

impl SessionAccounting {
    /// Build the projection from the session's journal.
    pub async fn hydrate(
        session_id: &str,
        journal: Arc<dyn EventJournal>,
    ) -> Result<Arc<Self>, AccountingError> {
        Self::hydrate_with_page_size(session_id, journal, PAGE_SIZE).await
    }

    pub(crate) async fn hydrate_with_page_size(
        session_id: &str,
        journal: Arc<dyn EventJournal>,
        page_size: usize,
    ) -> Result<Arc<Self>, AccountingError> {
        let this = Arc::new(Self {
            session_id: session_id.to_string(),
            journal,
            page_size: page_size.max(1),
            state: Mutex::new(State {
                reducer: AccountingReducer::new(session_id),
                cursor: 0,
                scanned: 0,
            }),
            sync: tokio::sync::Mutex::new(()),
            backlog: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        });
        this.catch_up().await?;
        Ok(this)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn backlog(&self) -> Arc<tokio::sync::Mutex<Vec<super::AttemptSettlement>>> {
        self.backlog.clone()
    }

    /// Apply journal records after the cursor, page by page. Returns how many
    /// records were read.
    pub async fn catch_up(&self) -> Result<u64, AccountingError> {
        let _gate = self.sync.lock().await;
        let mut read = 0u64;
        loop {
            let after = self.state.lock().cursor;
            let page = self
                .journal
                .load_session_page(&self.session_id, Some(after), Some(self.page_size))
                .await?;
            if page.is_empty() {
                return Ok(read);
            }
            let full = page.len() >= self.page_size;
            let mut state = self.state.lock();
            for event in &page {
                state.reducer.apply(&event.kind)?;
                state.cursor = state.cursor.max(event.stream_seq);
            }
            state.scanned += page.len() as u64;
            read += page.len() as u64;
            drop(state);
            if !full {
                return Ok(read);
            }
        }
    }

    /// Apply one newly committed record. Records of other sessions and
    /// non-accounting events are ignored.
    pub fn observe(&self, event: &DurableEvent) -> Result<Applied, AccountingConflict> {
        if event.session_id != self.session_id {
            return Ok(Applied::NotAccounting);
        }
        let mut state = self.state.lock();
        let applied = state.reducer.apply(&event.kind)?;
        state.cursor = state.cursor.max(event.stream_seq);
        Ok(applied)
    }

    /// Read the reducer under the lock. Keep `f` short and synchronous.
    pub fn read<R>(&self, f: impl FnOnce(&AccountingReducer) -> R) -> R {
        f(&self.state.lock().reducer)
    }

    pub fn cursor(&self) -> i64 {
        self.state.lock().cursor
    }

    /// Journal records read by hydration and catch-up so far.
    pub fn records_scanned(&self) -> u64 {
        self.state.lock().scanned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::AttemptRecorder;
    use crate::accounting::tests::{sample_settlement, sample_start};
    use crate::event_fanout::EventFanout;
    use crate::events::AgentEventKind;
    use crate::session::backend::StorageBackend;
    use crate::session::projection::NewDurableEvent;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::session_event_stream::SessionEventStream;

    fn start_for(id: &str, cycle: &str) -> crate::accounting::AttemptStart {
        let mut s = sample_start();
        s.attempt.attempt_id = id.into();
        s.attempt.cycle_id = cycle.into();
        s
    }

    fn settlement_for(id: &str, cycle: &str) -> crate::accounting::AttemptSettlement {
        let mut s = sample_settlement();
        s.attempt.attempt_id = id.into();
        s.attempt.cycle_id = cycle.into();
        s
    }

    fn noise() -> AgentEventKind {
        AgentEventKind::UserMessageStored {
            content: "hello".into(),
        }
    }

    /// Records three attempts (one left unresolved) interleaved with noise.
    async fn record_history(
        storage: &SqliteStorage,
        projection: Option<Arc<SessionAccounting>>,
    ) -> Arc<SessionEventStream> {
        let stream = SessionEventStream::new(
            "sess-1",
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        );
        let recorder = match projection {
            Some(p) => AttemptRecorder::with_projection(stream.clone(), p),
            None => AttemptRecorder::new(stream.clone()),
        };
        for (id, cycle) in [("a1", "c1"), ("a2", "c1"), ("a3", "c2")] {
            stream.emit_persisted(noise()).await.unwrap();
            let h = recorder.begin(start_for(id, cycle)).await.unwrap();
            if id != "a3" {
                recorder.settle(h, settlement_for(id, cycle)).await.unwrap();
            }
        }
        stream
    }

    #[tokio::test]
    async fn paginated_hydration_reproduces_live_totals_and_reasons() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let live = SessionAccounting::hydrate("sess-1", storage.event_journal())
            .await
            .unwrap();
        record_history(&storage, Some(live.clone())).await;

        // 3 noise + 3 starts + 2 settlements = 8 records; pages of 2.
        let hydrated =
            SessionAccounting::hydrate_with_page_size("sess-1", storage.event_journal(), 2)
                .await
                .unwrap();
        assert_eq!(hydrated.records_scanned(), 8);
        let (a, b) = (
            live.read(|r| r.session().clone()),
            hydrated.read(|r| r.session().clone()),
        );
        assert_eq!(a, b);
        assert_eq!(a.priced_attempts, 2);
        assert_eq!(a.unresolved_attempts, 1);
        assert_eq!(a.reasons(), b.reasons());
        for cycle in ["c1", "c2"] {
            assert_eq!(
                live.read(|r| r.cycle(cycle)),
                hydrated.read(|r| r.cycle(cycle))
            );
        }
        assert_eq!(live.cursor(), hydrated.cursor());
    }

    #[tokio::test]
    async fn hydration_pages_exactly_at_page_boundaries() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        record_history(&storage, None).await;
        for page in [1usize, 3, 8, 9, 1000] {
            let p =
                SessionAccounting::hydrate_with_page_size("sess-1", storage.event_journal(), page)
                    .await
                    .unwrap();
            assert_eq!(p.records_scanned(), 8, "page size {page}");
            assert_eq!(p.read(|r| r.attempt_count()), 3);
        }
    }

    #[tokio::test]
    async fn hot_path_updates_read_only_new_records() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let projection = SessionAccounting::hydrate("sess-1", storage.event_journal())
            .await
            .unwrap();
        let stream = record_history(&storage, Some(projection.clone())).await;
        assert_eq!(projection.records_scanned(), 0, "observe does not scan");
        // Everything was observed, so catch-up has nothing to read.
        assert_eq!(projection.catch_up().await.unwrap(), 0);

        // A record committed without observation is the only thing read.
        stream
            .emit_persisted(AgentEventKind::LlmAttemptStarted {
                start: start_for("a4", "c2"),
            })
            .await
            .unwrap();
        assert_eq!(projection.read(|r| r.attempt_count()), 3);
        assert_eq!(projection.catch_up().await.unwrap(), 1);
        assert_eq!(projection.read(|r| r.attempt_count()), 4);
        assert_eq!(projection.catch_up().await.unwrap(), 0);
        assert_eq!(projection.records_scanned(), 1);
    }

    #[tokio::test]
    async fn observe_and_catch_up_overlap_is_idempotent() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let projection = SessionAccounting::hydrate("sess-1", storage.event_journal())
            .await
            .unwrap();
        let stream = SessionEventStream::new(
            "sess-1",
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        );
        let ev = stream
            .emit_persisted(AgentEventKind::LlmAttemptSettled {
                settlement: settlement_for("a1", "c1"),
            })
            .await
            .unwrap();
        assert_eq!(projection.observe(&ev), Ok(Applied::New));
        assert_eq!(projection.observe(&ev), Ok(Applied::Duplicate));
        projection.catch_up().await.unwrap();
        assert_eq!(projection.read(|r| r.session().priced_attempts), 1);
    }

    #[tokio::test]
    async fn observe_ignores_other_sessions_and_non_accounting_events() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let projection = SessionAccounting::hydrate("sess-1", storage.event_journal())
            .await
            .unwrap();
        let other = SessionEventStream::new(
            "sess-2",
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        );
        let foreign = other
            .emit_persisted(AgentEventKind::LlmAttemptSettled {
                settlement: {
                    let mut s = settlement_for("x", "c");
                    s.attempt.session_id = "sess-2".into();
                    s
                },
            })
            .await
            .unwrap();
        assert_eq!(projection.observe(&foreign), Ok(Applied::NotAccounting));
        let own = SessionEventStream::new(
            "sess-1",
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        );
        let ev = own.emit_persisted(noise()).await.unwrap();
        assert_eq!(projection.observe(&ev), Ok(Applied::NotAccounting));
        assert_eq!(projection.read(|r| r.attempt_count()), 0);
    }

    #[tokio::test]
    async fn conflicting_journal_record_surfaces_an_error_on_hydration() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let journal = storage.event_journal();
        for (seq, input) in [(1, 100u32), (2, 7)] {
            let mut s = settlement_for("a1", "c1");
            s.usage = Some(querymt::Usage {
                input_tokens: input,
                ..Default::default()
            });
            journal
                .append_durable(&NewDurableEvent {
                    session_id: "sess-1".into(),
                    origin: crate::events::EventOrigin::Local,
                    source_node: None,
                    source_node_id: None,
                    source_seq: None,
                    session_seq: Some(seq),
                    kind: AgentEventKind::LlmAttemptSettled { settlement: s },
                })
                .await
                .unwrap();
        }
        let err = SessionAccounting::hydrate("sess-1", journal).await.err();
        assert!(matches!(err, Some(AccountingError::Conflict(_))), "{err:?}");
    }

    #[tokio::test]
    async fn reopen_reproduces_unresolved_reasons_from_a_fresh_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acct.db");
        let live_session = {
            let storage = SqliteStorage::connect(path.clone()).await.unwrap();
            let live = SessionAccounting::hydrate("sess-1", storage.event_journal())
                .await
                .unwrap();
            record_history(&storage, Some(live.clone())).await;
            live.read(|r| r.session().clone())
        };
        let storage = SqliteStorage::connect(path).await.unwrap();
        let reopened = SessionAccounting::hydrate("sess-1", storage.event_journal())
            .await
            .unwrap();
        assert_eq!(reopened.read(|r| r.session().clone()), live_session);
        assert_eq!(reopened.read(|r| r.unresolved().len()), 1);
    }

    #[tokio::test]
    async fn session_runtime_owns_the_projection_and_retirement_releases_it() {
        use crate::agent::core::{McpToolState, SessionRuntime};
        use std::collections::HashMap;

        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        record_history(&storage, None).await;

        let runtime = SessionRuntime::new(None, HashMap::new(), McpToolState::empty());
        let sink = crate::event_sink::EventSink::new(
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        );
        let first = runtime.accounting("sess-1", &sink).await.unwrap();
        let again = runtime.accounting("sess-1", &sink).await.unwrap();
        assert!(Arc::ptr_eq(&first, &again), "hydrated once per runtime");
        assert_eq!(first.records_scanned(), 8);
        assert_eq!(first.read(|r| r.attempt_count()), 3);

        let weak = Arc::downgrade(&first);
        drop((first, again));
        assert!(weak.upgrade().is_some(), "runtime still owns it");
        drop(runtime);
        // The sink's shared registry also holds it until the session retires.
        assert!(weak.upgrade().is_some(), "sink keeps the shared projection");
        sink.retire_session_stream("sess-1").await;
        assert!(
            weak.upgrade().is_none(),
            "retiring the session releases the projection"
        );
    }
}

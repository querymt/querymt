//! Immutable per-execution-cycle accounting handle.
//!
//! A cycle handle names one execution cycle (`cycle_id`) and points at the
//! session's long-lived [`SessionAccounting`]. Nothing about the session's
//! spend lives in the handle or in per-cycle `AgentStats`, so starting a new
//! cycle yields zero *cycle* spend while *session* spend is retained. Work
//! that settles after a newer cycle began keeps its original handle and is
//! charged to its original cycle.

use super::{ScopeTotals, SessionAccounting};
use std::sync::Arc;

#[derive(Clone)]
pub struct CycleAccounting {
    cycle_id: Arc<str>,
    accounting: Arc<SessionAccounting>,
}

impl CycleAccounting {
    /// Start a new cycle with a fresh identity.
    pub fn begin(accounting: Arc<SessionAccounting>) -> Self {
        Self::with_id(accounting, uuid::Uuid::now_v7().to_string())
    }

    /// A dedicated cycle for maintenance work that runs outside any prompt
    /// (for example compaction triggered without a user turn). It follows the
    /// same identity rules as a prompt cycle and is distinguishable by id.
    pub fn begin_maintenance(accounting: Arc<SessionAccounting>) -> Self {
        Self::with_id(accounting, format!("maintenance-{}", uuid::Uuid::now_v7()))
    }

    pub fn is_maintenance(&self) -> bool {
        self.cycle_id.starts_with("maintenance-")
    }

    pub fn with_id(accounting: Arc<SessionAccounting>, cycle_id: impl Into<Arc<str>>) -> Self {
        Self {
            cycle_id: cycle_id.into(),
            accounting,
        }
    }

    pub fn cycle_id(&self) -> &str {
        &self.cycle_id
    }

    pub fn session_id(&self) -> &str {
        self.accounting.session_id()
    }

    /// The session accounting this cycle charges into.
    pub fn session_accounting(&self) -> &Arc<SessionAccounting> {
        &self.accounting
    }

    /// Committed accounting attributed to this cycle only.
    pub fn cycle_totals(&self) -> ScopeTotals {
        self.accounting.read(|r| r.cycle(&self.cycle_id))
    }

    /// Committed lifetime accounting for the session.
    pub fn session_totals(&self) -> ScopeTotals {
        self.accounting.read(|r| r.session().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::tests::{sample_settlement, sample_start};
    use crate::accounting::{AttemptRecorder, AttemptSettlement, AttemptStart};
    use crate::agent::core::{McpToolState, SessionRuntime};
    use crate::event_fanout::EventFanout;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::session_event_stream::SessionEventStream;
    use std::collections::HashMap;

    fn cost() -> f64 {
        (100.0 * 3.0 + 15.0 * 15.0 + 50.0 * 0.3 + 20.0 * 3.75) / 1e6
    }

    fn start_in(cycle: &CycleAccounting, id: &str) -> AttemptStart {
        let mut s = sample_start();
        s.attempt.attempt_id = id.into();
        s.attempt.cycle_id = cycle.cycle_id().into();
        s
    }

    fn settlement_in(cycle: &CycleAccounting, id: &str) -> AttemptSettlement {
        let mut s = sample_settlement();
        s.attempt.attempt_id = id.into();
        s.attempt.cycle_id = cycle.cycle_id().into();
        s
    }

    async fn spend(
        storage: &SqliteStorage,
        cycle: &CycleAccounting,
        id: &str,
    ) -> Arc<SessionEventStream> {
        let stream = SessionEventStream::new(
            "sess-1",
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        );
        let recorder =
            AttemptRecorder::with_projection(stream.clone(), cycle.session_accounting().clone());
        let handle = recorder.begin(start_in(cycle, id)).await.unwrap();
        recorder
            .settle(handle, settlement_in(cycle, id))
            .await
            .unwrap();
        stream
    }

    async fn runtime_cycle(storage: &SqliteStorage) -> CycleAccounting {
        // A fresh runtime models a restart or a newly loaded session.
        let runtime = SessionRuntime::new(None, HashMap::new(), McpToolState::empty());
        let sink = crate::event_sink::EventSink::new(
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        );
        let accounting = runtime.accounting("sess-1", &sink).await.unwrap();
        CycleAccounting::begin(accounting)
    }

    #[test]
    fn cycle_ids_are_unique() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let storage = rt
            .block_on(SqliteStorage::connect(":memory:".into()))
            .unwrap();
        let a = rt.block_on(runtime_cycle(&storage));
        let b = CycleAccounting::begin(a.session_accounting().clone());
        assert_ne!(a.cycle_id(), b.cycle_id());
    }

    #[tokio::test]
    async fn second_cycle_starts_at_zero_and_session_spend_is_retained() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let first = runtime_cycle(&storage).await;
        spend(&storage, &first, "a1").await;
        assert!((first.cycle_totals().known_subtotal_usd - cost()).abs() < 1e-12);

        // Normal continuation: same session accounting, new cycle.
        let second = CycleAccounting::begin(first.session_accounting().clone());
        assert_eq!(second.cycle_totals(), ScopeTotals::default());
        assert!((second.session_totals().known_subtotal_usd - cost()).abs() < 1e-12);

        spend(&storage, &second, "a2").await;
        assert!((second.cycle_totals().known_subtotal_usd - cost()).abs() < 1e-12);
        assert!((first.cycle_totals().known_subtotal_usd - cost()).abs() < 1e-12);
        assert!((second.session_totals().known_subtotal_usd - 2.0 * cost()).abs() < 1e-12);
    }

    #[tokio::test]
    async fn fresh_database_reopen_keeps_lifetime_spend_and_zeroes_new_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cycle.db");
        let (first_id, second_id) = {
            let storage = SqliteStorage::connect(path.clone()).await.unwrap();
            let first = runtime_cycle(&storage).await;
            spend(&storage, &first, "a1").await;
            let second = CycleAccounting::begin(first.session_accounting().clone());
            spend(&storage, &second, "a2").await;
            (first.cycle_id().to_string(), second.cycle_id().to_string())
        };

        let storage = SqliteStorage::connect(path).await.unwrap();
        let third = runtime_cycle(&storage).await;
        assert_eq!(third.cycle_totals(), ScopeTotals::default());
        assert!((third.session_totals().known_subtotal_usd - 2.0 * cost()).abs() < 1e-12);
        // Earlier cycles keep their own attribution after rehydration.
        let accounting = third.session_accounting();
        for id in [first_id, second_id] {
            let c = accounting.read(|r| r.cycle(&id));
            assert!((c.known_subtotal_usd - cost()).abs() < 1e-12, "{id}");
        }
    }

    #[tokio::test]
    async fn delayed_settlement_keeps_its_original_cycle_handle() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let first = runtime_cycle(&storage).await;
        let stream = SessionEventStream::new(
            "sess-1",
            storage.event_journal(),
            Arc::new(EventFanout::new()),
        );
        let recorder = AttemptRecorder::with_projection(stream, first.session_accounting().clone());
        let pending = recorder.begin(start_in(&first, "late")).await.unwrap();

        let second = CycleAccounting::begin(first.session_accounting().clone());
        assert_eq!(second.cycle_totals().unresolved_attempts, 0);
        assert_eq!(first.cycle_totals().unresolved_attempts, 1);

        recorder
            .settle(pending, settlement_in(&first, "late"))
            .await
            .unwrap();
        assert_eq!(second.cycle_totals(), ScopeTotals::default());
        assert!((first.cycle_totals().known_subtotal_usd - cost()).abs() < 1e-12);
    }
}

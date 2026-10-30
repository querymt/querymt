//! Failure-path integration (task 9.2).
//!
//! These drive the real `run_attempt` dispatch wrapper, the ordered journal
//! (with an injectable append failure), the `EventSink` remote-relay path and
//! range-deletion rewind, all on fresh databases.

use crate::accounting::recorder::tests::Switchable;
use crate::accounting::tests::{sample_settlement, sample_start};
use crate::accounting::{
    AccountingReason, AttemptOperation, AttemptPurpose, AttemptRecorder, CycleAccounting,
    SessionAccounting, run_attempt,
};
use crate::event_fanout::EventFanout;
use crate::event_sink::EventSink;
use crate::events::AgentEventKind;
use crate::session::backend::StorageBackend;
use crate::session::projection::EventJournal;
use crate::session::provider::BillingBasis;
use crate::session::sqlite_storage::SqliteStorage;
use crate::session_event_stream::SessionEventStream;
use querymt::chat::ChatOutput;
use querymt::error::LLMError;
use querymt::providers::{BillingClass, ModelPricing};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tokio_util::sync::CancellationToken;

const SESSION: &str = "sess-1";

fn pricing() -> ModelPricing {
    ModelPricing {
        input: Some(3.0),
        output: Some(15.0),
        cache_read: Some(0.3),
        cache_write: Some(3.75),
        ..Default::default()
    }
}

fn basis() -> BillingBasis {
    BillingBasis {
        billing: BillingClass::Metered,
        pricing: Some(pricing()),
    }
}

fn cost() -> f64 {
    (100.0 * 3.0 + 15.0 * 15.0 + 50.0 * 0.3 + 20.0 * 3.75) / 1e6
}

fn output() -> ChatOutput {
    let mut out = ChatOutput::from_projections(
        None,
        Some("ok".into()),
        None,
        None,
        Some(querymt::chat::FinishReason::Stop),
    );
    out.usage = sample_settlement().usage;
    out
}

struct Fixture {
    op: AttemptOperation,
    accounting: Arc<SessionAccounting>,
    journal: Arc<dyn EventJournal>,
    fail: Arc<AtomicBool>,
}

async fn fixture() -> Fixture {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let fail = Arc::new(AtomicBool::new(false));
    let journal: Arc<dyn EventJournal> = Arc::new(Switchable {
        inner: storage.event_journal(),
        fail: fail.clone(),
    });
    let accounting = SessionAccounting::hydrate(SESSION, journal.clone())
        .await
        .unwrap();
    let stream = SessionEventStream::new(SESSION, journal.clone(), Arc::new(EventFanout::new()));
    let recorder = Arc::new(AttemptRecorder::with_projection(stream, accounting.clone()));
    let cycle = CycleAccounting::with_id(accounting.clone(), "cycle-1");
    let op = AttemptOperation::new(recorder, &cycle, AttemptPurpose::MainResponse, "p", "m");
    Fixture {
        op,
        accounting,
        journal,
        fail,
    }
}

async fn ledger(journal: &Arc<dyn EventJournal>) -> Vec<&'static str> {
    journal
        .load_session_stream(SESSION, None, None)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            AgentEventKind::LlmAttemptStarted { .. } => Some("started"),
            AgentEventKind::LlmAttemptSettled { .. } => Some("settled"),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn cancellation_settles_the_attempt_instead_of_leaving_it_unresolved() {
    let f = fixture().await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = run_attempt(&f.op, basis(), &cancel, || {
        std::future::pending::<Result<ChatOutput, LLMError>>()
    })
    .await;
    assert!(matches!(result, Err(LLMError::Cancelled)));

    let totals = f.accounting.read(|r| r.session().clone());
    assert_eq!(totals.unresolved_attempts, 0, "cancel settles, never leaks");
    assert_eq!(
        totals.complete_total_usd(),
        None,
        "usage was not observed, so no complete total"
    );
    assert_eq!(ledger(&f.journal).await, vec!["started", "settled"]);
}

#[tokio::test]
async fn settlement_persistence_failure_blocks_all_further_paid_dispatch() {
    let f = fixture().await;
    let cancel = CancellationToken::new();
    let dispatched = Arc::new(AtomicU32::new(0));

    // The provider answers, but the journal goes down before settlement.
    let fail = f.fail.clone();
    let count = dispatched.clone();
    let first = run_attempt(&f.op, basis(), &cancel, || async move {
        count.fetch_add(1, Ordering::SeqCst);
        fail.store(true, Ordering::SeqCst);
        Ok(output())
    })
    .await;
    let error = first.expect_err("a storage failure is an execution error");
    assert!(error.to_string().contains("accounting failure"), "{error}");
    assert_eq!(dispatched.load(Ordering::SeqCst), 1);

    // While storage is down no further paid request may be sent.
    let count = dispatched.clone();
    let blocked = run_attempt(&f.op, basis(), &cancel, || async move {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(output())
    })
    .await;
    assert!(blocked.is_err());
    assert_eq!(
        dispatched.load(Ordering::SeqCst),
        1,
        "the provider must not be called without a durable start"
    );
    assert_eq!(ledger(&f.journal).await, vec!["started"]);

    // Recovery: the retained settlement is persisted exactly once, then the
    // next attempt proceeds.
    f.fail.store(false, Ordering::SeqCst);
    let count = dispatched.clone();
    run_attempt(&f.op, basis(), &cancel, || async move {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(output())
    })
    .await
    .unwrap();
    assert_eq!(dispatched.load(Ordering::SeqCst), 2);
    assert_eq!(
        ledger(&f.journal).await,
        vec!["started", "settled", "started", "settled"]
    );
    let totals = f.accounting.read(|r| r.session().clone());
    assert_eq!(totals.priced_attempts, 2);
    assert_eq!(totals.unresolved_attempts, 0);
    assert!((totals.known_subtotal_usd - 2.0 * cost()).abs() < 1e-12);
}

#[tokio::test]
async fn unsettled_start_after_a_crash_is_unresolved_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.db");
    {
        let storage = SqliteStorage::connect(path.clone()).await.unwrap();
        let journal = storage.event_journal();
        let accounting = SessionAccounting::hydrate(SESSION, journal.clone())
            .await
            .unwrap();
        let stream = SessionEventStream::new(SESSION, journal, Arc::new(EventFanout::new()));
        let recorder = AttemptRecorder::with_projection(stream, accounting);
        // Dispatched, then the process dies before settlement.
        let handle = recorder.begin(sample_start()).await.unwrap();
        std::mem::forget(handle);
    }
    let storage = SqliteStorage::connect(path).await.unwrap();
    let accounting = SessionAccounting::hydrate(SESSION, storage.event_journal())
        .await
        .unwrap();
    let totals = accounting.read(|r| r.session().clone());
    assert_eq!(totals.unresolved_attempts, 1);
    assert_eq!(totals.complete_total_usd(), None);
    assert_eq!(totals.known_subtotal_usd, 0.0, "no usage is fabricated");
    assert!(
        totals
            .reasons()
            .contains(&AccountingReason::UnresolvedAttempt)
    );
}

#[tokio::test]
async fn remote_redelivery_does_not_duplicate_the_charge() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal = storage.event_journal();
    let sink = EventSink::new(journal.clone(), Arc::new(EventFanout::new()));
    let start = AgentEventKind::LlmAttemptStarted {
        start: sample_start(),
    };
    let settled = AgentEventKind::LlmAttemptSettled {
        settlement: sample_settlement(),
    };
    let emit = |kind: AgentEventKind, seq: i64| {
        let sink = &sink;
        async move {
            sink.emit_durable_from_source(
                SESSION,
                kind,
                Some("host".into()),
                "node-1".into(),
                seq,
                None,
            )
            .await
            .unwrap()
        }
    };

    assert!(emit(start.clone(), 1).await.is_some());
    assert!(emit(settled.clone(), 2).await.is_some());
    // Replay after reconnect: the same source sequences are a journal no-op.
    assert!(emit(start.clone(), 1).await.is_none());
    assert!(emit(settled.clone(), 2).await.is_none());
    // A relay that re-sends the same attempt under new source sequences
    // (for example after the host re-numbered) is deduplicated by the
    // reducer, not charged twice.
    assert!(emit(start, 3).await.is_some());
    assert!(emit(settled, 4).await.is_some());

    assert_eq!(ledger(&journal).await.len(), 4);
    let accounting = SessionAccounting::hydrate(SESSION, journal).await.unwrap();
    let totals = accounting.read(|r| r.session().clone());
    assert_eq!(totals.priced_attempts, 1);
    assert!((totals.known_subtotal_usd - cost()).abs() < 1e-12);
    assert_eq!(totals.unresolved_attempts, 0);
}

#[tokio::test]
async fn rewind_after_a_cancelled_attempt_keeps_its_incurred_state() {
    let f = fixture().await;
    let cancel = CancellationToken::new();
    run_attempt(&f.op, basis(), &cancel, || async { Ok(output()) })
        .await
        .unwrap();
    cancel.cancel();
    let _ = run_attempt(&f.op, basis(), &cancel, || {
        std::future::pending::<Result<ChatOutput, LLMError>>()
    })
    .await;
    let before = f.accounting.read(|r| r.session().clone());

    // Rewind everything the conversation recorded.
    f.journal
        .delete_session_events_from(SESSION, 0)
        .await
        .unwrap();
    let after = SessionAccounting::hydrate(SESSION, f.journal.clone())
        .await
        .unwrap()
        .read(|r| r.session().clone());
    assert_eq!(
        after, before,
        "incurred spend and uncertainty survive rewind"
    );
    assert_eq!(ledger(&f.journal).await.len(), 4);
}

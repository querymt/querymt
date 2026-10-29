//! End-to-end accounting scenario on a fresh file database (task 9.1).
//!
//! One session records: mixed five-minute/one-hour cache writes, reasoning
//! followed by zero reasoning, a model switch, auxiliary compaction retries
//! (the first rejected by validation), a second execution cycle and a restart.
//! Runtime totals, cycle scopes, ACP replay metadata and the ATIF export must
//! all agree with the recorded attempts.

use crate::accounting::tests::{sample_settlement, sample_start};
use crate::accounting::{
    AccountingSnapshot, AttemptOutcome, AttemptPurpose, AttemptRecorder, CycleAccounting,
    SessionAccounting,
};
use crate::event_fanout::EventFanout;
use crate::events::AgentEvent;
use crate::session::backend::StorageBackend;
use crate::session::sqlite_storage::SqliteStorage;
use crate::session_event_stream::SessionEventStream;
use querymt::providers::{ModelPricing, value_usage};
use querymt::{CacheWriteTiers, Usage};
use std::sync::Arc;

const SESSION: &str = "sess-1";

fn model_a() -> ModelPricing {
    ModelPricing {
        input: Some(3.0),
        output: Some(15.0),
        cache_read: Some(0.3),
        cache_write: Some(3.75),
        cache_write_1h: Some(6.0),
        ..Default::default()
    }
}

fn model_b() -> ModelPricing {
    ModelPricing {
        input: Some(1.0),
        output: Some(2.0),
        cache_read: Some(0.1),
        cache_write: Some(1.25),
        ..Default::default()
    }
}

struct Spec {
    id: &'static str,
    cycle: &'static str,
    purpose: AttemptPurpose,
    model: &'static str,
    pricing: ModelPricing,
    usage: Usage,
    outcome: AttemptOutcome,
}

async fn record(recorder: &AttemptRecorder, spec: Spec) {
    let mut start = sample_start();
    start.attempt.attempt_id = spec.id.into();
    start.attempt.operation_id = format!("op-{}", spec.id);
    start.attempt.cycle_id = spec.cycle.into();
    start.attempt.purpose = spec.purpose;
    start.model = spec.model.into();
    start.pricing = Some(spec.pricing.clone());

    let mut settlement = sample_settlement();
    settlement.attempt = start.attempt.clone();
    settlement.outcome = spec.outcome;
    settlement.effective_model = Some(spec.model.into());
    settlement.valuation = value_usage(Some(&spec.pricing), Some(&spec.usage));
    settlement.pricing = Some(spec.pricing);
    settlement.usage = Some(spec.usage);

    let handle = recorder.begin(start).await.unwrap();
    recorder.settle(handle, settlement).await.unwrap();
}

fn usage(input: u32, output: u32, reasoning: u32, read: u32, write: u32) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: reasoning,
        cache_read: read,
        cache_write: write,
        cache_write_tiers: None,
    }
}

/// Hand-computed USD for each attempt (rates per million tokens).
mod expected {
    /// Model A, cycle 1: 100 in, (100 + 900 reasoning) out priced once,
    /// 50 cache reads, 300 writes split 200 five-minute / 100 one-hour.
    pub const A: f64 =
        (100.0 * 3.0 + 1000.0 * 15.0 + 50.0 * 0.3 + 200.0 * 3.75 + 100.0 * 6.0) / 1e6;
    /// Model B after the switch, no reasoning: 10 in, 100 out. A's tokens are
    /// not repriced at B's rates.
    pub const B: f64 = (10.0 * 1.0 + 100.0 * 2.0) / 1e6;
    /// Rejected compaction summary (still accountable), then its retry.
    pub const COMPACT_REJECTED: f64 = (20.0 * 1.0 + 5.0 * 2.0) / 1e6;
    pub const COMPACT_RETRY: f64 = (20.0 * 1.0 + 40.0 * 2.0 + 30.0 * 0.1) / 1e6;
    /// Model B in cycle 2.
    pub const C: f64 = (8.0 * 1.0 + 50.0 * 2.0 + 4.0 * 1.25) / 1e6;
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-12
}

async fn events(storage: &SqliteStorage) -> Vec<AgentEvent> {
    storage
        .event_journal()
        .load_session_stream(SESSION, None, None)
        .await
        .unwrap()
        .into_iter()
        .map(AgentEvent::from)
        .collect()
}

#[tokio::test]
async fn mixed_scenario_agrees_across_runtime_restart_acp_and_export() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e2e.db");
    let total = expected::A
        + expected::B
        + expected::COMPACT_REJECTED
        + expected::COMPACT_RETRY
        + expected::C;
    let cycle1 = expected::A + expected::B + expected::COMPACT_REJECTED + expected::COMPACT_RETRY;

    let (live_snapshot, cycle2_id) = {
        let storage = SqliteStorage::connect(path.clone()).await.unwrap();
        let journal = storage.event_journal();
        let stream =
            SessionEventStream::new(SESSION, journal.clone(), Arc::new(EventFanout::new()));
        let accounting = SessionAccounting::hydrate(SESSION, journal).await.unwrap();
        let recorder = AttemptRecorder::with_projection(stream, accounting.clone());

        let first = CycleAccounting::begin(accounting.clone());
        let c1 = first.cycle_id().to_string();
        let c1: &'static str = Box::leak(c1.into_boxed_str());

        // Reasoning first (900), mixed-TTL writes, model A.
        let mut a = usage(100, 100, 900, 50, 300);
        a.cache_write_tiers = Some(CacheWriteTiers {
            five_minute: Some(200),
            one_hour: Some(100),
        });
        record(
            &recorder,
            Spec {
                id: "a",
                cycle: c1,
                purpose: AttemptPurpose::MainResponse,
                model: "model-a",
                pricing: model_a(),
                usage: a,
                outcome: AttemptOutcome::Succeeded,
            },
        )
        .await;
        // Switch to model B; zero reasoning this time.
        record(
            &recorder,
            Spec {
                id: "b",
                cycle: c1,
                purpose: AttemptPurpose::MainResponse,
                model: "model-b",
                pricing: model_b(),
                usage: usage(10, 100, 0, 0, 0),
                outcome: AttemptOutcome::Succeeded,
            },
        )
        .await;
        // Auxiliary compaction: rejected by validation, then retried.
        record(
            &recorder,
            Spec {
                id: "compact-1",
                cycle: c1,
                purpose: AttemptPurpose::Compaction,
                model: "model-b",
                pricing: model_b(),
                usage: usage(20, 5, 0, 0, 0),
                outcome: AttemptOutcome::OutputRejected,
            },
        )
        .await;
        record(
            &recorder,
            Spec {
                id: "compact-2",
                cycle: c1,
                purpose: AttemptPurpose::Compaction,
                model: "model-b",
                pricing: model_b(),
                usage: usage(20, 40, 0, 30, 0),
                outcome: AttemptOutcome::Succeeded,
            },
        )
        .await;

        // Runtime scopes before the new cycle.
        assert!(close(first.cycle_totals().known_subtotal_usd, cycle1));
        assert!(close(first.session_totals().known_subtotal_usd, cycle1));

        // New cycle: cycle spend starts at zero, session spend is retained.
        let second = CycleAccounting::begin(accounting.clone());
        assert_eq!(second.cycle_totals().known_subtotal_usd, 0.0);
        assert!(close(second.session_totals().known_subtotal_usd, cycle1));
        let c2: &'static str = Box::leak(second.cycle_id().to_string().into_boxed_str());
        record(
            &recorder,
            Spec {
                id: "c",
                cycle: c2,
                purpose: AttemptPurpose::MainResponse,
                model: "model-b",
                pricing: model_b(),
                usage: usage(8, 50, 0, 0, 4),
                outcome: AttemptOutcome::Succeeded,
            },
        )
        .await;
        assert!(close(second.cycle_totals().known_subtotal_usd, expected::C));
        // The first cycle's spend is unchanged by the second cycle.
        assert!(close(first.cycle_totals().known_subtotal_usd, cycle1));
        assert!(close(second.session_totals().known_subtotal_usd, total));
        assert_eq!(
            second.session_totals().complete_total_usd(),
            Some(second.session_totals().known_subtotal_usd)
        );

        (
            accounting.read(AccountingSnapshot::from_reducer),
            c2.to_string(),
        )
    };

    // Restart: a fresh connection and a hydrated reducer.
    let storage = SqliteStorage::connect(path).await.unwrap();
    let restarted = SessionAccounting::hydrate(SESSION, storage.event_journal())
        .await
        .unwrap();
    let after_restart = restarted.read(AccountingSnapshot::from_reducer);
    assert_eq!(
        after_restart, live_snapshot,
        "restart reproduces live state"
    );

    let session = &after_restart.session;
    assert!(close(session.known_subtotal_usd, total));
    assert_eq!(session.complete_total_usd, Some(session.known_subtotal_usd));
    assert_eq!(session.priced_attempts, 5);
    assert_eq!(session.unresolved_attempts, 0);
    assert_eq!(session.incomplete_attempts, 0);
    // The active cycle is the most recent main-response cycle, with its own spend.
    let cycle = after_restart.cycle.as_ref().expect("active cycle");
    assert_eq!(cycle.cycle_id, cycle2_id);
    assert!(close(cycle.scope.known_subtotal_usd, expected::C));
    // A fresh runtime's next cycle starts at zero again.
    let third = CycleAccounting::begin(restarted.clone());
    assert_eq!(third.cycle_totals().known_subtotal_usd, 0.0);
    assert!(close(third.session_totals().known_subtotal_usd, total));

    // The journal-reduced snapshot (what SessionLoadSnapshot carries) matches.
    let journal_events = events(&storage).await;
    let from_journal =
        AccountingSnapshot::from_events(SESSION, journal_events.iter().map(|e| &e.kind));
    assert_eq!(from_journal, after_restart);

    // ACP replay carries the same accounting metadata.
    let replay = crate::acp::shared::replay_agent_events_materialized(
        SESSION,
        journal_events.clone(),
        &Default::default(),
        &Default::default(),
    );
    let replayed = replay
        .iter()
        .find_map(|n| {
            let json = serde_json::to_value(&n.update).ok()?;
            json["_meta"]["querymt"].get("accounting").cloned()
        })
        .expect("replay carries accounting");
    assert_eq!(replayed, serde_json::to_value(&after_restart).unwrap());
    // No context limit was ever reported, so no usage_update is fabricated.
    assert!(!replay.iter().any(|n| matches!(
        n.update,
        crate::acp::protocol::SessionUpdate::UsageUpdate(_)
    )));

    // The export's total and attempt list match the recorded attempts.
    let mut builder = crate::export::ATIFBuilder::new(
        SESSION.to_string(),
        &crate::export::AtifExportOptions::default(),
    );
    builder.process_events(&journal_events);
    let atif = builder.build().to_json_value().unwrap();
    let exported = atif["final_metrics"]["total_cost_usd"].as_f64().unwrap();
    assert!(close(exported, total));
    let attempts = atif["final_metrics"]["extra"]["attempts"]
        .as_array()
        .unwrap();
    assert_eq!(attempts.len(), 5);
    let compaction = attempts
        .iter()
        .filter(|a| a["purpose"] == "compaction")
        .count();
    assert_eq!(compaction, 2, "auxiliary attempts are listed, not dropped");
    // Auxiliary attempts do not fabricate assistant steps.
    assert!(atif["steps"].as_array().unwrap().is_empty());
    assert_eq!(
        atif["final_metrics"]["extra"]["accounting"],
        serde_json::to_value(&after_restart).unwrap()
    );
}

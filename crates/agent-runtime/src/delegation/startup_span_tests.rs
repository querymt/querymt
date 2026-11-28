//! Delegation startup spans: every phase between the claim and the delegate's
//! first prompt is a span under the handler span. Scripted delegates over a real
//! git repository and a real managed-workspace service, with spans collected by
//! an in-memory OTel exporter.

use super::handoff_tests::{Terminal, delegate, request, wait_terminal};
use super::{DelegationOrchestrator, DelegationSummarizer};
use crate::session::domain::DelegationStatus;
use crate::session::store::SessionStore as _;
use crate::test_utils::delegation::{DelegateBehavior, TestHarness};
use crate::test_utils::mocks::{MockChatResponse, MockLlmProvider, SharedLlmProvider};
use crate::test_utils::{TestProviderFactory, helpers::mock_plugin_registry};
use crate::worktree::DelegationIsolation;
use opentelemetry::Value;
use opentelemetry::trace::SpanId;
use opentelemetry_sdk::trace::SpanData;
use querymt::LLMParams;
use querymt::chat::FinishReason;
use querymt_agent_services::tools::trace_support::genai_trace::{attr, capture_local_tasks};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{Mutex, Notify};

/// Total startup time that no span may account for (spec: "No unattributed
/// startup time").
const MAX_UNATTRIBUTED: Duration = Duration::from_millis(50);

fn named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans.iter().filter(|span| span.name == name).collect()
}

fn one<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    let found = named(spans, name);
    assert_eq!(found.len(), 1, "expected exactly one `{name}` span");
    found[0]
}

fn has_ancestor(spans: &[SpanData], span: &SpanData, ancestor: SpanId) -> bool {
    let mut parent = span.parent_span_id;
    while parent != SpanId::INVALID {
        if parent == ancestor {
            return true;
        }
        match spans.iter().find(|s| s.span_context.span_id() == parent) {
            Some(next) => parent = next.parent_span_id,
            None => return false,
        }
    }
    false
}

fn with_delegation_id<'a>(spans: &'a [SpanData], name: &str, id: &str) -> &'a SpanData {
    named(spans, name)
        .into_iter()
        .find(|span| attr(span, "delegation_id") == Some(&Value::from(id.to_string())))
        .unwrap_or_else(|| panic!("no `{name}` span for delegation {id}"))
}

/// The part of `[from, to]` that no span in `covering` covers.
fn uncovered(from: SystemTime, to: SystemTime, covering: &[&SpanData]) -> Duration {
    let mut intervals: Vec<(SystemTime, SystemTime)> = covering
        .iter()
        .map(|span| (span.start_time.max(from), span.end_time.min(to)))
        .filter(|(start, end)| start < end)
        .collect();
    intervals.sort();
    let mut gap = Duration::ZERO;
    let mut cursor = from;
    for (start, end) in intervals {
        if start > cursor {
            gap += start.duration_since(cursor).unwrap_or_default();
        }
        cursor = cursor.max(end);
    }
    if to > cursor {
        gap += to.duration_since(cursor).unwrap_or_default();
    }
    gap
}

#[tokio::test(flavor = "current_thread")]
async fn no_unattributed_startup_time() {
    let (_, spans) = capture_local_tasks(async {
        let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
        delegate(&h, "agent", DelegationIsolation::Isolated, None)
            .await
            .1
            .completed();
    })
    .await;

    let handler = one(&spans, "delegation.orchestrator.handle_event");
    let handler_id = handler.span_context.span_id();
    let claim = one(&spans, "delegation.claim");
    let execute = one(&spans, "delegation.execute");
    let queue_wait = one(&spans, "delegation.queue_wait");
    let prompt = one(&spans, "remote.session_ref.prompt");
    assert_eq!(claim.parent_span_id, handler_id);
    assert_eq!(queue_wait.parent_span_id, handler_id);
    assert_eq!(execute.parent_span_id, handler_id);
    assert_eq!(attr(claim, "attempts"), Some(&Value::from(1_i64)));

    // The phases of `delegation.execute` sit beneath it.
    let execute_id = execute.span_context.span_id();
    for phase in [
        "delegation.admission",
        "delegation.resolve_parent",
        "delegation.allocate_workspace",
        "worktree.lock.wait",
        "worktree.capture",
        "worktree.provision",
        "delegation.start_hook",
        "delegation.create_session",
    ] {
        let found = named(&spans, phase);
        assert!(!found.is_empty(), "no `{phase}` span");
        for span in found {
            assert!(
                has_ancestor(&spans, span, execute_id),
                "`{phase}` is not under delegation.execute"
            );
        }
    }
    let allocate = one(&spans, "delegation.allocate_workspace");
    assert_eq!(attr(allocate, "handoff"), Some(&Value::from(false)));
    assert!(attr(allocate, "workspace_id").is_some());
    // The harness parent is a plain checkout: looked up, not bound.
    assert_eq!(
        attr(one(&spans, "delegation.resolve_parent"), "bound"),
        Some(&Value::from(false))
    );

    // From the end of the claim to the first prompt, time not inside any
    // descendant of the handler span other than `delegation.execute` itself
    // (which spans all of it and would hide uninstrumented work).
    let covering: Vec<&SpanData> = spans
        .iter()
        .filter(|span| {
            span.span_context.span_id() != execute_id && has_ancestor(&spans, span, handler_id)
        })
        .collect();
    let gap = uncovered(claim.end_time, prompt.start_time, &covering);
    assert!(
        gap <= MAX_UNATTRIBUTED,
        "{gap:?} of startup time is not covered by any span (limit {MAX_UNATTRIBUTED:?})"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn queued_delegation_shows_its_wait() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let ((first, second, released_at), spans) = capture_local_tasks(async {
        let mut h = TestHarness::new_with_workspace_members(vec![
            (
                "agent".into(),
                DelegateBehavior::PauseBeforeLlmCall {
                    entered: entered.clone(),
                    release: release.clone(),
                },
                None,
            ),
            ("agent1".into(), DelegateBehavior::AlwaysOk, None),
        ])
        .await;
        // One slot: the second delegation queues behind the first.
        h.stop_harness_orchestrator();
        let settings = crate::config::DelegationSettings {
            max_parallel: std::num::NonZeroUsize::new(1).unwrap(),
            ..Default::default()
        };
        let orchestrator = Arc::new(
            DelegationOrchestrator::new(
                &h.delegator,
                h.config.roster().expect("roster").clone(),
                &settings,
                None,
            )
            .with_workspace_service(h.workspace.as_ref().expect("workspace").service.clone()),
        );
        h.orchestrator_handle = Some(orchestrator.start_listening(h.config.event_sink.fanout()));

        let parent = h.env.session_id.clone();
        let (first, mut first_events) =
            request(&h, &parent, "agent", DelegationIsolation::Shared, None).await;
        entered.notified().await;
        let (second, mut second_events) =
            request(&h, &parent, "agent1", DelegationIsolation::Shared, None).await;
        // The second delegation is claimed, then waits for the slot.
        loop {
            let status = h
                .storage
                .get_delegation(&second.public_id)
                .await
                .expect("delegation lookup")
                .map(|d| d.status);
            if status != Some(DelegationStatus::Requested) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Let its task reach the semaphore before the slot is released.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let released_at = SystemTime::now();
        release.notify_one();
        assert!(matches!(
            wait_terminal(&mut first_events, &first.public_id).await,
            Terminal::Completed(_)
        ));
        assert!(matches!(
            wait_terminal(&mut second_events, &second.public_id).await,
            Terminal::Completed(_)
        ));
        (first.public_id, second.public_id, released_at)
    })
    .await;

    let first_execute = with_delegation_id(&spans, "delegation.execute", &first);
    let second_execute = with_delegation_id(&spans, "delegation.execute", &second);
    let queue_waits = named(&spans, "delegation.queue_wait");
    assert_eq!(queue_waits.len(), 2);
    let waits: Vec<&SpanData> = queue_waits
        .into_iter()
        .filter(|wait| {
            [first_execute, second_execute]
                .iter()
                .any(|execute| execute.parent_span_id == wait.parent_span_id)
        })
        .collect();
    assert_eq!(waits.len(), 2);
    let second_wait = waits
        .iter()
        .find(|wait| wait.parent_span_id == second_execute.parent_span_id)
        .unwrap();
    let second_claim = named(&spans, "delegation.claim")
        .into_iter()
        .find(|claim| claim.parent_span_id == second_execute.parent_span_id)
        .expect("claim span of the second delegation");
    // It opens after the claim, was already waiting when the first delegation
    // was released, lasts until that delegation gives up its slot, and the
    // second delegation starts executing only afterwards.
    assert!(second_wait.start_time >= second_claim.end_time);
    assert!(second_wait.start_time <= released_at);
    assert!(second_wait.end_time >= first_execute.end_time);
    assert!(first_execute.end_time >= released_at);
    assert!(second_execute.start_time >= second_wait.end_time);
}

#[tokio::test(flavor = "current_thread")]
async fn shared_delegation_has_no_allocation_spans() {
    let (_, spans) = capture_local_tasks(async {
        let h = TestHarness::new_with_workspace(DelegateBehavior::AlwaysOk).await;
        delegate(&h, "agent", DelegationIsolation::Shared, None)
            .await
            .1
            .completed();
    })
    .await;
    // The delegation ran, and recorded its generic startup phases only.
    one(&spans, "delegation.execute");
    one(&spans, "delegation.create_session");
    for name in [
        "delegation.allocate_workspace",
        "worktree.capture",
        "worktree.provision",
    ] {
        assert!(named(&spans, name).is_empty(), "unexpected `{name}` span");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn summarize_span_records_rejected_outcome() {
    let (_, spans) = capture_local_tasks(async {
        let h = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
        // A summary cut off by the length limit is rejected, whatever its text.
        let mut summary_mock = MockLlmProvider::new();
        summary_mock.expect_chat().times(1).returning(|_| {
            let mut output: querymt::chat::ChatOutput =
                MockChatResponse::text_only("truncated brief").into();
            output.finish_reason = Some(FinishReason::Length);
            Ok(output)
        });
        let factory = Arc::new(TestProviderFactory::new(SharedLlmProvider {
            inner: Arc::new(Mutex::new(summary_mock)),
            tools: Vec::new().into_boxed_slice(),
        }));
        let (registry, _dir) = mock_plugin_registry(factory).unwrap();
        let provider = crate::session::provider::SessionProvider::new(
            Arc::new(registry),
            h.config.provider.history_store(),
            LLMParams::new().provider("mock").model("summary-model"),
        );
        let summarizer = DelegationSummarizer::from_config(
            &crate::config::DelegationSummaryConfig {
                provider: "mock".into(),
                model: "summary-model".into(),
                min_history_tokens: 0,
                ..Default::default()
            },
            &provider,
        )
        .await
        .unwrap();
        let mut h = h;
        h.stop_harness_orchestrator();
        let orchestrator = Arc::new(
            DelegationOrchestrator::new(
                &h.delegator,
                h.config.roster().expect("roster").clone(),
                &Default::default(),
                None,
            )
            .with_summarizer(Some(Arc::new(summarizer))),
        );
        h.orchestrator_handle = Some(orchestrator.start_listening(h.config.event_sink.fanout()));
        delegate(&h, "agent", DelegationIsolation::Shared, None)
            .await
            .1
            .completed();
    })
    .await;

    let summarize = one(&spans, "delegation.summarize");
    assert_eq!(attr(summarize, "outcome"), Some(&Value::from("rejected")));
    assert_eq!(attr(summarize, "strategy"), Some(&Value::from("llm")));
    assert!(named(&spans, "delegation.inject_planning_context").is_empty());
}

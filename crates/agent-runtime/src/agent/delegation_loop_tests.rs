use crate::agent::execution::CycleOutcome;
use crate::delegation::DelegationOrchestrator;
use crate::middleware::{
    AgentStats, ConversationContext, DelegationGuardMiddleware, ExecutionState, LlmResponse,
    MiddlewareDriver,
};
use querymt_agent_core::events::{AgentEventKind, StopType};
use querymt_agent_core::model::{AgentMessage, MessagePart};

use crate::session::domain::{Delegation, DelegationStatus};
use crate::session::store::SessionStore;
use crate::test_utils::delegation::{DelegateBehavior, TestHarness};
use crate::test_utils::{
    MockChatResponse, MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory,
    mock_querymt_tool_call,
};
use kameo::actor::Spawn;
use mockall::Sequence;
use querymt::LLMParams;
use querymt::chat::FinishReason;
use querymt::chat::{ChatRole, ReasoningEffort};
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::{Mutex, Notify};

// Mock implementations moved to crate::test_utils::mocks

/// An orchestrator over the roster of `config`, bound to a fresh root handle.
/// The root is leaked: the orchestrator holds it weakly and tests end soon after.
fn orchestrator_for(
    config: &Arc<crate::agent::agent_config::AgentConfig>,
) -> DelegationOrchestrator {
    orchestrator_with(config, &Default::default())
}

/// [`orchestrator_for`] with explicit delegation settings.
fn orchestrator_with(
    config: &Arc<crate::agent::agent_config::AgentConfig>,
    settings: &crate::config::DelegationSettings,
) -> DelegationOrchestrator {
    let root = Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone()));
    let orchestrator = DelegationOrchestrator::new(
        &root,
        config.roster().expect("config has a roster").clone(),
        settings,
        None,
    );
    std::mem::forget(root);
    orchestrator
}

/// The built local member `id` of `config`'s roster.
fn delegate_of(
    config: &crate::agent::agent_config::AgentConfig,
    id: &str,
) -> Arc<crate::agent::LocalAgentHandle> {
    config
        .roster()
        .expect("roster")
        .snapshot()
        .get(id)
        .and_then(|member| member.agent().and_then(|agent| agent.as_local().cloned()))
        .expect("built local delegate")
}

#[tokio::test(flavor = "current_thread")]
async fn genai_delegation_concurrent_parent_prompts_reach_child_agents_without_persisting_context()
{
    use crate::acp::protocol::{ContentBlock, LoadSessionRequest, PromptRequest, TextContent};
    use crate::send_agent::SendAgent as _;
    use opentelemetry::Value;
    use querymt_agent_services::tools::trace_support::genai_trace::{
        assert_private, attr, capture_local_tasks,
    };

    let ((parents, children), spans) = capture_local_tasks(async {
        async fn run(trace: &str, unsuccessful_summary: bool) -> (String, String) {
            let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
            harness.expect_single_delegation().await;
            harness.stop_harness_orchestrator();
            tokio::task::yield_now().await;
            let mut summary_mock = MockLlmProvider::new();
            summary_mock.expect_chat().times(1).returning(move |_| {
                let mut output: querymt::chat::ChatOutput =
                    MockChatResponse::text_only("SECRET_RESPONSE").into();
                output.finish_reason = Some(FinishReason::Stop);
                if unsuccessful_summary {
                    output.status = Some(querymt::chat::ChatOutputStatus::Incomplete);
                }
                Ok(output)
            });
            let factory = Arc::new(TestProviderFactory::new(SharedLlmProvider {
                inner: Arc::new(Mutex::new(summary_mock)),
                tools: Vec::new().into_boxed_slice(),
            }));
            let (registry, _summary_dir) =
                crate::test_utils::helpers::mock_plugin_registry(factory).unwrap();
            let provider = crate::session::provider::SessionProvider::new(
                Arc::new(registry),
                harness.config.provider.history_store(),
                LLMParams::new().provider("mock").model("summary-model"),
            );
            let summarizer = crate::delegation::DelegationSummarizer::from_config(
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
            let config = harness.config.clone();
            let orchestrator =
                Arc::new(orchestrator_for(&config).with_summarizer(Some(Arc::new(summarizer))));
            harness.orchestrator_handle =
                Some(orchestrator.start_listening(config.event_sink.fanout()));
            let handle = crate::agent::LocalAgentHandle::from_config(harness.config.clone());
            let parent_id = harness.env.session_id.clone();
            handle
                .load_session(LoadSessionRequest::new(
                    parent_id.clone(),
                    std::path::PathBuf::new(),
                ))
                .await
                .unwrap();
            let mut request = PromptRequest::new(
                parent_id.clone(),
                vec![ContentBlock::Text(TextContent::new("SECRET_PROMPT"))],
            );
            request.meta = Some(
                serde_json::json!({
                    "traceparent": format!("00-{trace}-1234567890abcdef-01"),
                    "tracestate": "vendor=value"
                })
                .as_object()
                .unwrap()
                .clone(),
            );
            tokio::time::timeout(std::time::Duration::from_secs(5), handle.prompt(request))
                .await
                .unwrap()
                .unwrap();
            let children = harness.child_sessions().await;
            assert_eq!(children.len(), 1);
            harness
                .provider_mut()
                .await
                .expect_chat()
                .times(1)
                .returning(|_| Ok(MockChatResponse::text_only("SECRET_RESPONSE").into()));
            let followup_trace = if unsuccessful_summary {
                "44444444444444444444444444444444"
            } else {
                "33333333333333333333333333333333"
            };
            let mut followup = PromptRequest::new(
                parent_id.clone(),
                vec![ContentBlock::Text(TextContent::new("SECRET_PROMPT"))],
            );
            followup.meta = Some(serde_json::Map::from_iter([(
                "traceparent".into(),
                serde_json::Value::from(format!("00-{followup_trace}-fedcba0987654321-01")),
            )]));
            tokio::time::timeout(std::time::Duration::from_secs(5), handle.prompt(followup))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(harness.child_sessions().await, children);
            let events = harness
                .config
                .event_sink
                .journal()
                .load_session_stream(&parent_id, None, None)
                .await
                .unwrap();
            let child_events = delegate_of(&harness.config, "agent")
                .config
                .event_sink
                .journal()
                .load_session_stream(&children[0], None, None)
                .await
                .unwrap();
            let delegations = harness
                .config
                .provider
                .history_store()
                .list_delegations(&parent_id)
                .await
                .unwrap();
            assert_eq!(delegations.len(), 1);
            assert_eq!(delegations[0].status, DelegationStatus::Complete);
            assert_eq!(
                delegations[0].planning_summary.as_deref(),
                if unsuccessful_summary {
                    None
                } else {
                    Some("SECRET_RESPONSE")
                }
            );
            for json in [
                serde_json::to_string(&events).unwrap(),
                serde_json::to_string(&child_events).unwrap(),
                serde_json::to_string(&delegations).unwrap(),
            ] {
                assert!(!json.contains("traceparent"));
                assert!(!json.contains("tracestate"));
                assert!(!json.contains(trace));
                assert!(!json.contains("vendor=value"));
            }
            harness.stop_harness_orchestrator();
            handle.config.shutdown().await;
            tokio::task::yield_now().await;
            (parent_id, children[0].clone())
        }
        let (first, second) = tokio::join!(
            run("11111111111111111111111111111111", false),
            run("22222222222222222222222222222222", true)
        );
        ([first.0, second.0], [first.1, second.1])
    })
    .await;
    for (index, trace) in [
        "11111111111111111111111111111111",
        "22222222222222222222222222222222",
    ]
    .iter()
    .enumerate()
    {
        let parent = spans
            .iter()
            .find(|span| {
                span.name == "invoke_agent"
                    && attr(span, "gen_ai.conversation.id")
                        == Some(&Value::from(parents[index].clone()))
            })
            .unwrap();
        let child = spans
            .iter()
            .find(|span| {
                span.name == "invoke_agent"
                    && attr(span, "gen_ai.conversation.id")
                        == Some(&Value::from(children[index].clone()))
            })
            .unwrap();
        assert_eq!(parent.span_context.trace_id().to_string(), *trace);
        assert_eq!(
            child.span_context.trace_id(),
            parent.span_context.trace_id()
        );
        assert_eq!(parent.parent_span_id.to_string(), "1234567890abcdef");
        assert_eq!(
            attr(parent, "gen_ai.agent.id"),
            Some(&Value::from("parent"))
        );
        assert_eq!(attr(child, "gen_ai.agent.id"), Some(&Value::from("reader")));
        assert_ne!(parents[index], children[index]);
        for (span, owner) in [(parent, &parents[index]), (child, &children[index])] {
            assert_eq!(attr(span, "session.id"), Some(&Value::from(owner.clone())));
            assert_eq!(
                attr(span, "session.id"),
                attr(span, "gen_ai.conversation.id")
            );
        }
        for span in spans.iter().filter(|span| {
            span.span_context.trace_id() == parent.span_context.trace_id()
                && attr(span, "gen_ai.operation.name").is_some()
        }) {
            let owner = attr(span, "gen_ai.conversation.id").unwrap();
            assert!(
                owner == &Value::from(parents[index].clone())
                    || owner == &Value::from(children[index].clone())
            );
            assert_eq!(attr(span, "session.id"), Some(owner));
            if span.name != "chat summary-model" {
                assert_eq!(
                    attr(span, "gen_ai.agent.id"),
                    Some(&Value::from(
                        if owner == &Value::from(parents[index].clone()) {
                            "parent"
                        } else {
                            "reader"
                        }
                    ))
                );
            }
        }
        let followup_trace = [
            "33333333333333333333333333333333",
            "44444444444444444444444444444444",
        ][index];
        let followup_spans: Vec<_> = spans
            .iter()
            .filter(|span| {
                span.span_context.trace_id().to_string() == followup_trace
                    && attr(span, "gen_ai.operation.name").is_some()
            })
            .collect();
        assert_eq!(followup_spans.len(), 2);
        assert_eq!(
            followup_spans
                .iter()
                .filter(|span| span.name == "invoke_agent")
                .count(),
            1
        );
        assert_eq!(
            followup_spans
                .iter()
                .filter(|span| attr(span, "gen_ai.operation.name") == Some(&Value::from("chat")))
                .count(),
            1
        );
        for span in followup_spans {
            assert_eq!(attr(span, "gen_ai.agent.id"), Some(&Value::from("parent")));
            assert_eq!(
                attr(span, "session.id"),
                Some(&Value::from(parents[index].clone()))
            );
            assert_eq!(
                attr(span, "session.id"),
                attr(span, "gen_ai.conversation.id")
            );
        }
        assert_eq!(child.span_context.trace_state().header(), "vendor=value");
        let execute = spans
            .iter()
            .find(|span| {
                span.name == "delegation.execute"
                    && attr(span, "child_session_id") == Some(&Value::from(children[index].clone()))
            })
            .unwrap();
        assert_eq!(child.parent_span_id, execute.span_context.span_id());
        let handlers: Vec<_> = spans
            .iter()
            .filter(|span| {
                span.name == "delegation.orchestrator.handle_event"
                    && span.span_context.trace_id() == parent.span_context.trace_id()
            })
            .collect();
        assert_eq!(handlers.len(), 1);
        let handler = handlers[0];
        assert_eq!(execute.parent_span_id, handler.span_context.span_id());
        assert_eq!(
            attr(handler, "event_kind"),
            Some(&Value::from("DelegationRequested"))
        );
        let summary = spans
            .iter()
            .find(|span| {
                span.name == "chat summary-model"
                    && span.span_context.trace_id() == parent.span_context.trace_id()
            })
            .unwrap();
        assert!(attr(summary, "gen_ai.agent.id").is_none());
        assert_eq!(
            attr(summary, "gen_ai.conversation.id"),
            Some(&Value::from(parents[index].clone()))
        );
        assert_eq!(
            attr(summary, "session.id"),
            attr(summary, "gen_ai.conversation.id")
        );
        assert_eq!(
            summary.status,
            if index == 1 {
                opentelemetry::trace::Status::error("")
            } else {
                opentelemetry::trace::Status::Unset
            }
        );
        let mut ancestor = summary;
        while ancestor.span_context.span_id() != execute.span_context.span_id() {
            ancestor = spans
                .iter()
                .find(|span| span.span_context.span_id() == ancestor.parent_span_id)
                .expect("summary must descend from its delegation worker");
        }
        // The emitter is the original side-effect span, not the consuming listener.
        let emitter = spans
            .iter()
            .find(|span| span.span_context.span_id() == handler.parent_span_id)
            .unwrap();
        assert_eq!(emitter.name, "agent.tool.side_effects");
        assert_eq!(
            emitter.span_context.trace_id(),
            parent.span_context.trace_id()
        );
    }
    assert!(
        spans
            .iter()
            .all(|span| span.name != "invoke_workflow" && span.name != "delegation.dispatch")
    );
    assert_private(&spans);
}

#[tokio::test(flavor = "current_thread")]
async fn genai_delegation_absent_remote_and_replayed_events_do_not_inherit_consumer_context() {
    use opentelemetry::{Value, trace::SpanId};
    use querymt_agent_core::events::{DurableEvent, EventEnvelope, EventOrigin};
    use querymt_agent_services::tools::trace_support::genai_trace::{
        assert_private, attr, capture_local_tasks,
    };
    use tracing::{Instrument, Span};

    for route in ["absent", "remote", "replay"] {
        let (_, spans) = capture_local_tasks(async {
            let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
            harness.stop_harness_orchestrator();
            tokio::task::yield_now().await;
            let config = harness.config.clone();
            let store = config.provider.history_store();
            let orchestrator = Arc::new(orchestrator_for(&config));
            let consumer = tracing::info_span!(parent: None, "unrelated-consumer");
            let listener =
                consumer.in_scope(|| orchestrator.start_listening(config.event_sink.fanout()));
            let session = store
                .get_session(&harness.env.session_id)
                .await
                .unwrap()
                .unwrap();
            let delegation = store
                .create_delegation(Delegation {
                    isolation: Default::default(),
                    id: 0,
                    public_id: String::new(),
                    session_id: session.id,
                    task_id: None,
                    target_agent_id: "agent".into(),
                    objective: "SECRET_ARGUMENT".into(),
                    objective_hash: querymt_agent_core::hash::RapidHash::new(b"SECRET_ARGUMENT"),
                    context: Some("SECRET_PROMPT".into()),
                    constraints: None,
                    expected_output: None,
                    verification_spec: None,
                    planning_summary: None,
                    status: DelegationStatus::Requested,
                    retry_count: 0,
                    created_at: OffsetDateTime::now_utc(),
                    completed_at: None,
                })
                .await
                .unwrap();
            let kind = AgentEventKind::DelegationRequested {
                delegation: delegation.clone(),
                tool_call_id: None,
                batch_id: None,
                cycle_id: None,
            };
            let unrelated = tracing::info_span!(parent: None, "unrelated-relay");
            async {
                match route {
                    "remote" => {
                        config
                            .event_sink
                            .emit_durable_with_origin(
                                &harness.env.session_id,
                                kind,
                                EventOrigin::Remote,
                                Some("peer".into()),
                            )
                            .await
                            .unwrap();
                    }
                    "replay" => {
                        config
                            .event_sink
                            .fanout()
                            .publish(EventEnvelope::Durable(DurableEvent {
                                session_seq: 0,
                                event_id: "replayed".into(),
                                stream_seq: 1,
                                session_id: harness.env.session_id.clone(),
                                timestamp: 0,
                                origin: EventOrigin::Local,
                                source_node: None,
                                kind,
                            }));
                    }
                    _ => {
                        config
                            .event_sink
                            .emit_durable(&harness.env.session_id, kind)
                            .instrument(Span::none())
                            .await
                            .unwrap();
                    }
                }
            }
            .instrument(if route == "absent" {
                Span::none()
            } else {
                unrelated
            })
            .await;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if store
                        .get_delegation(&delegation.public_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .status
                        == DelegationStatus::Complete
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            orchestrator.cancel_active_delegations().await;
            listener.abort();
            let _ = listener.await;
        })
        .await;
        let handlers: Vec<_> = spans
            .iter()
            .filter(|span| span.name == "delegation.orchestrator.handle_event")
            .collect();
        assert_eq!(handlers.len(), 1);
        let handler = handlers[0];
        assert!(spans.iter().all(|span| span.name != "delegation.dispatch"));
        assert_eq!(handler.parent_span_id, SpanId::INVALID);
        let child = spans
            .iter()
            .find(|span| span.name == "invoke_agent")
            .unwrap();
        assert_eq!(
            child.span_context.trace_id(),
            handler.span_context.trace_id()
        );
        let execute = spans
            .iter()
            .find(|span| span.name == "delegation.execute")
            .unwrap();
        assert_eq!(child.parent_span_id, execute.span_context.span_id());
        assert_eq!(execute.parent_span_id, handler.span_context.span_id());
        for unrelated in spans
            .iter()
            .filter(|span| span.name == "unrelated-consumer" || span.name == "unrelated-relay")
        {
            assert_ne!(
                child.span_context.trace_id(),
                unrelated.span_context.trace_id()
            );
        }
        assert_eq!(
            attr(child, "gen_ai.operation.name"),
            Some(&Value::from("invoke_agent"))
        );
        assert_private(&spans);
    }
}

// Helper functions moved to crate::test_utils::helpers

#[tokio::test]
async fn delegate_session_inherits_parent_connection_bridge() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    let bridge_handle = crate::agent::LocalAgentHandle::from_config(harness.config.clone());
    let parent_runtime = crate::agent::core::SessionRuntime::new(
        None,
        HashMap::new(),
        crate::agent::core::McpToolState::empty(),
    );
    let parent_actor = crate::agent::SessionActor::new(
        harness.config.clone(),
        harness.env.session_id.clone(),
        parent_runtime,
    );
    let parent_actor = crate::agent::SessionActor::spawn(parent_actor);
    bridge_handle
        .registry
        .lock()
        .await
        .insert(harness.env.session_id.clone(), parent_actor);
    let (bridge_tx, _bridge_rx) = tokio::sync::mpsc::channel(4);
    let bridge = crate::acp::client_bridge::ClientBridgeSender::for_connection(bridge_tx, "conn");
    bridge_handle
        .set_session_bridge(&harness.env.session_id, bridge)
        .await
        .expect("set parent bridge");

    harness.run_single_delegation().await;
    let child_session_id = harness
        .child_sessions()
        .await
        .into_iter()
        .next()
        .expect("child session");
    let target = delegate_of(&harness.config, "agent");
    let child = target
        .registry
        .lock()
        .await
        .get(&child_session_id)
        .cloned()
        .expect("child actor");
    #[cfg(feature = "remote")]
    let crate::agent::remote::SessionActorRef::Local(child) = child else {
        panic!("expected local child actor");
    };
    #[cfg(not(feature = "remote"))]
    let crate::agent::remote::SessionActorRef::Local(child) = child;
    let routed_child = target
        .config
        .session_bridges
        .lock()
        .expect("child bridge routes")
        .get(&child_session_id)
        .cloned()
        .expect("child bridge route");
    assert_eq!(routed_child.bridge.connection_id(), Some("conn"));

    target.registry.lock().await.remove(&child_session_id);
    child
        .tell(crate::agent::messages::Shutdown)
        .await
        .expect("stop child actor");
    child.wait_for_shutdown().await;
    assert!(
        bridge_handle
            .clear_session_bridge(&child_session_id, Arc::from("conn"))
            .await,
        "disconnect cleanup should release a route for a stopped child actor"
    );
    assert!(
        target
            .config
            .session_bridges
            .lock()
            .expect("child bridge routes")
            .get(&child_session_id)
            .is_none(),
        "disconnect cleanup should remove the inherited child actor route"
    );
}

#[tokio::test]
async fn delegate_setup_failure_removes_inherited_bridge_route() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    let bridge_handle = crate::agent::LocalAgentHandle::from_config(harness.config.clone());
    let parent_actor = crate::agent::SessionActor::spawn(crate::agent::SessionActor::new(
        harness.config.clone(),
        harness.env.session_id.clone(),
        crate::agent::core::SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        ),
    ));
    bridge_handle
        .registry
        .lock()
        .await
        .insert(harness.env.session_id.clone(), parent_actor);
    let (bridge_tx, _bridge_rx) = tokio::sync::mpsc::channel(4);
    bridge_handle
        .set_session_bridge(
            &harness.env.session_id,
            crate::acp::client_bridge::ClientBridgeSender::for_connection(bridge_tx, "conn"),
        )
        .await
        .expect("set parent bridge");
    harness
        .config
        .provider
        .history_store()
        .set_delegate_assignment(
            &harness.env.session_id,
            "agent",
            Some(crate::delegation::DelegateModelOverride {
                model_id: "missing/override-model".into(),
                node_id: None,
            }),
            Some(0),
        )
        .await
        .expect("set failing model override");

    let outcome = harness.run_single_delegation().await;
    assert_eq!(outcome, CycleOutcome::Completed);

    let child_session_id = harness
        .child_sessions()
        .await
        .into_iter()
        .next()
        .expect("child session");
    assert!(harness.session_lifecycle(&child_session_id).await.resolved);
    let target = delegate_of(&harness.config, "agent");
    assert!(
        target
            .config
            .session_bridges
            .lock()
            .expect("child bridge routes")
            .get(&child_session_id)
            .is_none(),
        "failed delegate setup should remove the inherited bridge route"
    );
}

#[tokio::test]
async fn duplicate_orchestrators_create_one_child_for_one_request() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    let config = harness.config.clone();
    let duplicate = Arc::new(
        orchestrator_for(&config)
            .with_delegate_model_overrides(config.delegate_model_overrides.clone()),
    );
    let duplicate_listener = duplicate.start_listening(config.event_sink.fanout());
    assert_eq!(config.event_sink.fanout().subscriber_count(), 2);

    let outcome = harness.run_single_delegation().await;
    let children = harness.child_sessions().await;
    duplicate_listener.abort();

    assert_eq!(outcome, CycleOutcome::Completed);
    assert_eq!(children.len(), 1);
}

#[tokio::test]
async fn profile_orchestrator_does_not_poison_unbound_session() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    let config = harness.config.clone();
    let spectator = Arc::new(orchestrator_for(&config).with_profile_id("owner"));
    let spectator_listener = spectator.start_listening(config.event_sink.fanout());

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        harness.run_single_delegation(),
    )
    .await
    .expect("unbound delegation timed out");
    let children = harness.child_sessions().await;
    spectator_listener.abort();

    assert_eq!(outcome, CycleOutcome::Completed);
    assert_eq!(children.len(), 1);
}

#[tokio::test]
async fn unrelated_profile_orchestrator_does_not_claim_delegation() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    harness.stop_harness_orchestrator();
    let config = harness.config.clone();
    let store = config.provider.history_store();
    store
        .set_profile_binding(&harness.env.session_id, "owner")
        .await
        .expect("bind parent profile");
    let owner = Arc::new(orchestrator_for(&config).with_profile_id("owner"));
    let unrelated = Arc::new(orchestrator_for(&config).with_profile_id("other"));
    let owner_listener = owner.start_listening(config.event_sink.fanout());
    let unrelated_listener = unrelated.start_listening(config.event_sink.fanout());

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        harness.run_single_delegation(),
    )
    .await
    .expect("profile-isolated delegation timed out");
    let children = harness.child_sessions().await;
    owner_listener.abort();
    unrelated_listener.abort();

    assert_eq!(outcome, CycleOutcome::Completed);
    assert_eq!(children.len(), 1);
}

#[tokio::test]
async fn missing_target_fails_and_unblocks_parent() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    harness.stop_harness_orchestrator();
    let config = harness.config.clone();
    let (empty_roster, empty_dir) = crate::delegation::roster::test_roster().await;
    std::mem::forget(empty_dir);
    let missing_target = Arc::new(DelegationOrchestrator::new(
        &harness.delegator,
        empty_roster,
        &Default::default(),
        None,
    ));
    let listener = missing_target.start_listening(config.event_sink.fanout());

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        harness.run_single_delegation_without_child_tool(),
    )
    .await
    .expect("missing target did not unblock the parent");
    listener.abort();

    assert_eq!(outcome, CycleOutcome::Completed);
    let delegations = config
        .provider
        .history_store()
        .list_delegations(&harness.env.session_id)
        .await
        .expect("delegations");
    assert_eq!(delegations.len(), 1);
    assert_eq!(delegations[0].status, DelegationStatus::Failed);
}

#[tokio::test]
async fn timeout_cleanup_cancels_requested_delegation_before_it_can_start() {
    let harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    let store = harness.config.provider.history_store();
    let session = store
        .get_session(&harness.env.session_id)
        .await
        .expect("session lookup")
        .expect("parent session");
    let delegation = Delegation {
        isolation: Default::default(),
        id: 0,
        public_id: String::new(),
        session_id: session.id,
        task_id: None,
        target_agent_id: "agent".to_string(),
        objective: "not started yet".to_string(),
        objective_hash: querymt_agent_core::hash::RapidHash::new(b"not started yet"),
        context: None,
        constraints: None,
        expected_output: None,
        verification_spec: None,
        planning_summary: None,
        status: DelegationStatus::Requested,
        retry_count: 0,
        created_at: OffsetDateTime::now_utc(),
        completed_at: None,
    };
    let delegation = store
        .create_delegation(delegation)
        .await
        .expect("create delegation");

    crate::agent::execution::cleanup_timed_out_delegations(
        &harness.config,
        &harness.env,
        &harness.turn,
        std::slice::from_ref(&delegation.public_id),
    )
    .await;

    let delegation = store
        .get_delegation(&delegation.public_id)
        .await
        .expect("delegation lookup")
        .expect("delegation");
    assert_eq!(delegation.status, DelegationStatus::Cancelled);
    assert_eq!(
        store
            .claim_delegation(&delegation.public_id)
            .await
            .expect("claim result"),
        crate::session::domain::DelegationClaim::InvalidState(DelegationStatus::Cancelled)
    );
}

#[tokio::test]
async fn timeout_cleanup_does_not_overwrite_terminal_delegation() {
    let harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    let store = harness.config.provider.history_store();
    let session = store
        .get_session(&harness.env.session_id)
        .await
        .expect("session lookup")
        .expect("parent session");
    let delegation = Delegation {
        isolation: Default::default(),
        id: 0,
        public_id: String::new(),
        session_id: session.id,
        task_id: None,
        target_agent_id: "agent".to_string(),
        objective: "already complete".to_string(),
        objective_hash: querymt_agent_core::hash::RapidHash::new(b"already complete"),
        context: None,
        constraints: None,
        expected_output: None,
        verification_spec: None,
        planning_summary: None,
        status: DelegationStatus::Complete,
        retry_count: 0,
        created_at: OffsetDateTime::now_utc(),
        completed_at: None,
    };
    let delegation = store
        .create_delegation(delegation)
        .await
        .expect("create delegation");
    let mut event_rx = harness.config.event_sink.fanout().subscribe();

    crate::agent::execution::cleanup_timed_out_delegations(
        &harness.config,
        &harness.env,
        &harness.turn,
        std::slice::from_ref(&delegation.public_id),
    )
    .await;

    assert_eq!(
        store
            .get_delegation(&delegation.public_id)
            .await
            .expect("delegation lookup")
            .expect("delegation")
            .status,
        DelegationStatus::Complete
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), event_rx.recv())
            .await
            .is_err(),
        "terminal delegations must not emit timeout cancellation events"
    );
}

#[tokio::test]
async fn test_delegate_inherits_parent_reasoning_effort() {
    let mut harness = TestHarness::new_with_delegate_reasoning(
        vec![],
        DelegateBehavior::AlwaysOk,
        Some(ReasoningEffort::Low),
    )
    .await;
    harness
        .set_parent_reasoning_effort(Some(ReasoningEffort::High))
        .await;

    let outcome = harness.run_single_delegation().await;

    assert_eq!(outcome, CycleOutcome::Completed);
    let (_, child_params) = harness.child_llm_params().await;
    assert_eq!(child_params.reasoning_effort, Some(ReasoningEffort::High));
}

#[tokio::test]
async fn test_delegate_inherits_parent_auto_reasoning_effort() {
    let mut harness = TestHarness::new_with_delegate_reasoning(
        vec![],
        DelegateBehavior::AlwaysOk,
        Some(ReasoningEffort::Low),
    )
    .await;

    let outcome = harness.run_single_delegation().await;

    assert_eq!(outcome, CycleOutcome::Completed);
    let (_, child_params) = harness.child_llm_params().await;
    assert_eq!(child_params.reasoning_effort, None);
}

#[tokio::test]
async fn test_delegate_reasoning_override_wins_over_parent() {
    let mut harness = TestHarness::new_with_delegate_reasoning(
        vec![],
        DelegateBehavior::AlwaysOk,
        Some(ReasoningEffort::Low),
    )
    .await;
    harness
        .set_parent_reasoning_effort(Some(ReasoningEffort::High))
        .await;
    harness
        .config
        .provider
        .history_store()
        .set_delegate_assignment_with_reasoning(
            &harness.env.session_id,
            "agent",
            None,
            Some(Some(crate::delegation::DelegateReasoningEffort::Medium)),
            Some(0),
        )
        .await
        .unwrap();

    let outcome = harness.run_single_delegation().await;

    assert_eq!(outcome, CycleOutcome::Completed);
    let (_, child_params) = harness.child_llm_params().await;
    assert_eq!(child_params.reasoning_effort, Some(ReasoningEffort::Medium));
}

#[tokio::test]
async fn test_delegate_explicit_auto_reasoning_overrides_parent() {
    let mut harness = TestHarness::new_with_delegate_reasoning(
        vec![],
        DelegateBehavior::AlwaysOk,
        Some(ReasoningEffort::Low),
    )
    .await;
    harness
        .set_parent_reasoning_effort(Some(ReasoningEffort::High))
        .await;
    harness
        .config
        .provider
        .history_store()
        .set_delegate_assignment_with_reasoning(
            &harness.env.session_id,
            "agent",
            None,
            Some(Some(crate::delegation::DelegateReasoningEffort::Auto)),
            Some(0),
        )
        .await
        .unwrap();

    let outcome = harness.run_single_delegation().await;

    assert_eq!(outcome, CycleOutcome::Completed);
    let (_, child_params) = harness.child_llm_params().await;
    assert_eq!(child_params.reasoning_effort, None);
}

#[tokio::test]
async fn test_delegate_model_override_applies_before_prompt() {
    let mut harness = TestHarness::new_with_delegate_reasoning(
        vec![],
        DelegateBehavior::AlwaysOk,
        Some(ReasoningEffort::Low),
    )
    .await;
    harness
        .set_parent_reasoning_effort(Some(ReasoningEffort::High))
        .await;
    harness
        .config
        .provider
        .history_store()
        .set_delegate_assignment(
            &harness.env.session_id,
            "agent",
            Some(crate::delegation::DelegateModelOverride {
                model_id: "mock/override-model".into(),
                node_id: None,
            }),
            Some(0),
        )
        .await
        .unwrap();

    let outcome = harness.run_single_delegation().await;

    assert_eq!(outcome, CycleOutcome::Completed);
    let (child_model, child_params) = harness.child_llm_params().await;
    assert_eq!(child_model, "override-model");
    assert_eq!(child_params.reasoning_effort, Some(ReasoningEffort::High));
    let events = harness
        .config
        .event_sink
        .journal()
        .load_session_stream(&harness.env.session_id, None, None)
        .await
        .unwrap();
    let fork = events
        .iter()
        .find_map(|event| match &event.kind {
            AgentEventKind::SessionForked {
                selected_model_id,
                selected_provider_node_id,
                ..
            } => Some((
                selected_model_id.as_deref(),
                selected_provider_node_id.as_deref(),
            )),
            _ => None,
        })
        .expect("delegation fork event");
    assert_eq!(fork, (Some("mock/override-model"), None));
}

#[tokio::test]
async fn test_multiple_sequential_delegations() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;

    let delegate_call_1 = mock_querymt_tool_call(
        "call-1",
        "delegate",
        r#"{"target_agent_id":"agent1","objective":"task1"}"#,
    );
    let delegate_call_2 = mock_querymt_tool_call(
        "call-2",
        "delegate",
        r#"{"target_agent_id":"agent2","objective":"task2"}"#,
    );

    let mut seq = Sequence::new();

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(
                MockChatResponse::with_tools("Delegating task 1", vec![delegate_call_1.clone()])
                    .into(),
            )
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(
                MockChatResponse::with_tools("Delegating task 2", vec![delegate_call_2.clone()])
                    .into(),
            )
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(MockChatResponse::text_only("All tasks complete").into()));

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "ok".to_string(),
            }])
        })
        .times(2);

    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

#[tokio::test]
async fn test_delegation_failure_recovery() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysFail).await;
    let delegate_call = mock_querymt_tool_call(
        "call-1",
        "delegate",
        r#"{"target_agent_id":"agent","objective":"task"}"#,
    );
    let mut seq = Sequence::new();

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(MockChatResponse::with_tools("Delegating task", vec![delegate_call.clone()]).into())
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|messages| {
            let last_msg = messages.last().unwrap();
            assert!(last_msg.text().contains("Delegation failed"));
            assert!(last_msg.text().contains("Patch Application Failure"));
            Ok(MockChatResponse::text_only("I'll handle it differently").into())
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "ok".to_string(),
            }])
        })
        .times(1);

    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
    let child_session_id = harness
        .child_sessions()
        .await
        .into_iter()
        .next()
        .expect("failed delegation child");
    let child_lifecycle = harness.session_lifecycle(&child_session_id).await;
    assert!(child_lifecycle.resolved);
    assert!(!child_lifecycle.archived);
    let parent_session_id = harness.env.session_id.clone();
    let parent_lifecycle = harness.session_lifecycle(&parent_session_id).await;
    assert!(!parent_lifecycle.resolved);
    assert!(!parent_lifecycle.archived);

    let store = harness.config.provider.history_store();
    let failed_delegation = store
        .list_delegations(&parent_session_id)
        .await
        .expect("list failed delegation")
        .into_iter()
        .find(|delegation| delegation.status == DelegationStatus::Failed)
        .expect("failed delegation record");
    store
        .patch_session_lifecycle(
            &child_session_id,
            &crate::session::lifecycle::SessionLifecyclePatch {
                resolved: Some(false),
                archived: None,
            },
        )
        .await
        .expect("explicitly reopen failed delegate");
    assert_eq!(
        store
            .claim_delegation(&failed_delegation.public_id)
            .await
            .expect("replay claim"),
        crate::session::domain::DelegationClaim::InvalidState(DelegationStatus::Failed)
    );
    assert!(
        !harness.session_lifecycle(&child_session_id).await.resolved,
        "replaying a failed attempt must not undo an explicit reopen"
    );
}

#[tokio::test]
async fn test_delegation_completion_message_format_and_replay_safety() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    let delegate_call = mock_querymt_tool_call(
        "call-1",
        "delegate",
        r#"{"target_agent_id":"agent","objective":"task"}"#,
    );
    let mut seq = Sequence::new();

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(MockChatResponse::with_tools("", vec![delegate_call.clone()]).into())
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|messages| {
            let last_msg = messages.last().unwrap();
            assert!(last_msg.text().contains("Delegation completed"));
            assert!(last_msg.text().contains("Delegation ID:"));
            assert!(last_msg.text().contains("Please review the changes"));
            assert!(last_msg.text().contains("=== Delegate Agent Results ==="));
            Ok(MockChatResponse::text_only("Perfect, task complete").into())
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "ok".to_string(),
            }])
        })
        .times(1);

    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
    let store = harness.config.provider.history_store();
    let parent_session_id = harness.env.session_id.clone();
    let child_session_id = harness
        .child_sessions()
        .await
        .into_iter()
        .next()
        .expect("completed delegation child");
    let child_lifecycle = harness.session_lifecycle(&child_session_id).await;
    assert!(child_lifecycle.resolved);
    assert!(!child_lifecycle.archived);
    let parent_lifecycle = harness.session_lifecycle(&parent_session_id).await;
    assert!(!parent_lifecycle.resolved);
    assert!(!parent_lifecycle.archived);

    store
        .patch_session_lifecycle(
            &child_session_id,
            &crate::session::lifecycle::SessionLifecyclePatch {
                resolved: Some(false),
                archived: None,
            },
        )
        .await
        .expect("explicitly reopen completed delegate");
    let completed_delegation = store
        .list_delegations(&parent_session_id)
        .await
        .expect("list completed delegations")
        .into_iter()
        .find(|delegation| delegation.status == DelegationStatus::Complete)
        .expect("completed delegation record");

    // A replayed terminal request is ignored, while a new delegation remains allowed.
    harness.config.event_sink.emit_ephemeral(
        &parent_session_id,
        AgentEventKind::DelegationRequested {
            delegation: completed_delegation,
            tool_call_id: None,
            batch_id: None,
            cycle_id: None,
        },
    );
    let parent = store
        .get_session(&parent_session_id)
        .await
        .expect("parent lookup")
        .expect("parent session");
    let retry_delegation = store
        .create_delegation(Delegation {
            id: 0,
            public_id: String::new(),
            session_id: parent.id,
            task_id: None,
            target_agent_id: "agent".to_string(),
            objective: "Retry task".to_string(),
            objective_hash: querymt_agent_core::hash::RapidHash::new(b"Retry task"),
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: None,
            status: DelegationStatus::Requested,
            isolation: Default::default(),
            retry_count: 0,
            created_at: OffsetDateTime::now_utc(),
            completed_at: None,
            planning_summary: None,
        })
        .await
        .expect("create a new delegation attempt");
    harness.config.event_sink.emit_ephemeral(
        &parent_session_id,
        AgentEventKind::DelegationRequested {
            delegation: retry_delegation.clone(),
            tool_call_id: None,
            batch_id: None,
            cycle_id: None,
        },
    );
    // The delegation status is persisted before the child session is resolved,
    // so wait for both to avoid observing the gap between the two updates.
    let retry_child_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let current = store
                .get_delegation(&retry_delegation.public_id)
                .await
                .expect("retry delegation lookup")
                .expect("retry delegation");
            if current.status == DelegationStatus::Complete
                && let Some(retry_child_id) = harness
                    .child_sessions()
                    .await
                    .into_iter()
                    .find(|session_id| session_id != &child_session_id)
                && harness.session_lifecycle(&retry_child_id).await.resolved
            {
                break retry_child_id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("new delegation attempt did not complete and resolve its child");
    assert!(harness.session_lifecycle(&retry_child_id).await.resolved);
    assert!(
        !harness.session_lifecycle(&child_session_id).await.resolved,
        "replaying a completed attempt must not undo an explicit reopen"
    );
    harness.stop_harness_orchestrator();
}

#[tokio::test]
async fn wait_timeout_cancellation_resolves_child_and_preserves_reopen() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut harness = TestHarness::new(
        vec![],
        DelegateBehavior::PauseBeforeLlmCall {
            entered: entered.clone(),
            release: release.clone(),
        },
    )
    .await;
    let mut config = (*harness.config).clone();
    config.delegation_wait_policy = crate::config::DelegationWaitPolicy::All;
    config.delegation_wait_timeout_secs = 1;
    harness.config = Arc::new(config);
    let store = harness.config.provider.history_store();
    let parent_session_id = harness.env.session_id.clone();
    let mut run = Box::pin(harness.run_single_delegation());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            outcome = &mut run => panic!("cycle ended before child creation: {outcome:?}"),
            _ = entered.notified() => {}
        }
    })
    .await
    .expect("child did not reach cancellation gate");
    let child_session_id = store
        .list_child_sessions(&parent_session_id)
        .await
        .expect("child sessions")
        .into_iter()
        .next()
        .expect("created child");
    let lifecycle = store
        .patch_session_lifecycle(
            &child_session_id,
            &crate::session::lifecycle::SessionLifecyclePatch {
                resolved: None,
                archived: Some(true),
            },
        )
        .await
        .expect("archive still-running child");
    assert!(!lifecycle.resolved);

    // Do not request cancellation here: the real wait-all timer must do it.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("wait timeout did not unblock the parent");
    assert_eq!(outcome, CycleOutcome::Completed);
    let history = store
        .get_effective_history(&parent_session_id)
        .await
        .expect("parent history");
    assert!(history.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(part,
                MessagePart::Text { content }
                    if content.contains("timed out after 1s of inactivity (cancel requested)")
            )
        })
    }));
    let delegation = store
        .list_delegations(&parent_session_id)
        .await
        .expect("delegations")
        .into_iter()
        .next()
        .expect("delegation");
    let finalized = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let current = store
                .get_delegation(&delegation.public_id)
                .await
                .expect("delegation lookup")
                .expect("delegation");
            let lifecycle = harness.session_lifecycle(&child_session_id).await;
            if current.status == DelegationStatus::Cancelled && lifecycle.resolved {
                assert!(lifecycle.archived);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    release.notify_one();
    finalized.expect("wait-timeout cancelled attempt did not resolve its child");
    assert!(!harness.session_lifecycle(&parent_session_id).await.resolved);
    assert!(!harness.session_lifecycle(&parent_session_id).await.archived);

    store
        .patch_session_lifecycle(
            &child_session_id,
            &crate::session::lifecycle::SessionLifecyclePatch {
                resolved: Some(false),
                archived: None,
            },
        )
        .await
        .expect("explicit reopen");
    crate::agent::execution::cleanup_timed_out_delegations(
        &harness.config,
        &harness.env,
        &harness.turn,
        std::slice::from_ref(&delegation.public_id),
    )
    .await;
    harness.config.event_sink.emit_ephemeral(
        &parent_session_id,
        AgentEventKind::DelegationCancelRequested {
            delegation_id: delegation.public_id.clone(),
        },
    );
    harness.config.event_sink.emit_ephemeral(
        &parent_session_id,
        AgentEventKind::DelegationRequested {
            delegation,
            tool_call_id: None,
            batch_id: None,
            cycle_id: None,
        },
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let lifecycle = harness.session_lifecycle(&child_session_id).await;
    assert!(
        !lifecycle.resolved,
        "terminal replay must not undo a reopen"
    );
    assert!(lifecycle.archived);
    assert_eq!(harness.child_sessions().await.len(), 1);
    harness.stop_harness_orchestrator();
}

#[tokio::test]
async fn cancelled_delegation_resolves_child_after_terminal_claim() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut harness = TestHarness::new(
        vec![],
        DelegateBehavior::PauseBeforeLlmCall {
            entered: entered.clone(),
            release: release.clone(),
        },
    )
    .await;
    let config = harness.config.clone();
    let store = config.provider.history_store();
    let parent_session_id = harness.env.session_id.clone();
    let mut run = Box::pin(harness.run_single_delegation());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            outcome = &mut run => {
                panic!("delegated cycle ended before the child reached its cancellation gate: {outcome:?}");
            }
            _ = entered.notified() => {}
        }
    })
    .await
    .expect("delegated child did not reach the cancellation gate");

    let child_session_id = store
        .list_child_sessions(&parent_session_id)
        .await
        .expect("child sessions")
        .into_iter()
        .next()
        .expect("created child session");
    assert!(
        !store
            .get_session_lifecycle(&child_session_id)
            .await
            .expect("child lifecycle")
            .resolved,
        "a newly created child starts open before a terminal outcome"
    );
    let delegation = store
        .list_delegations(&parent_session_id)
        .await
        .expect("list delegation")
        .into_iter()
        .find(|delegation| delegation.status == DelegationStatus::Running)
        .expect("running delegation");
    config.event_sink.emit_ephemeral(
        &parent_session_id,
        AgentEventKind::DelegationCancelRequested {
            delegation_id: delegation.public_id.clone(),
        },
    );

    let cancellation_result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let current = store
                .get_delegation(&delegation.public_id)
                .await
                .expect("delegation lookup")
                .expect("delegation");
            let lifecycle = store
                .get_session_lifecycle(&child_session_id)
                .await
                .expect("child lifecycle");
            if current.status == DelegationStatus::Cancelled && lifecycle.resolved {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    release.notify_one();
    cancellation_result.expect("cancelled attempt did not resolve its child");
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("delegation cycle did not finish after cancellation");
    assert_eq!(outcome, CycleOutcome::Completed);

    let child_lifecycle = harness.session_lifecycle(&child_session_id).await;
    assert!(child_lifecycle.resolved);
    assert!(!child_lifecycle.archived);
    let parent_lifecycle = harness.session_lifecycle(&parent_session_id).await;
    assert!(!parent_lifecycle.resolved);
    assert!(!parent_lifecycle.archived);
    harness.stop_harness_orchestrator();
}

#[tokio::test]
async fn managed_verification_cancel_keeps_idle_child_and_live_cleanup_distinct() {
    use crate::control::delegation_notifications::{DelegationActivity, DelegationStage};
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    harness.stop_harness_orchestrator();
    let config = harness.config.clone();
    let store = config.provider.history_store();
    let parent_id = harness.env.session_id.clone();
    let parent = store.get_session(&parent_id).await.unwrap().unwrap();
    let release_file = harness._temp_dir.path().join("verification-release");
    let entered_file = harness._temp_dir.path().join("verification-entered");
    let delegation = store
        .create_delegation(Delegation {
            id: 0,
            public_id: String::new(),
            session_id: parent.id,
            task_id: None,
            target_agent_id: "agent".into(),
            objective: "bounded verification cleanup".into(),
            objective_hash: querymt_agent_core::hash::RapidHash::new(b"bounded verification cleanup"),
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: Some(querymt_agent_services::verification::VerificationSpec {
                description: "wait for test release".into(),
                strategy: Default::default(),
                steps: vec![querymt_agent_services::verification::VerificationStep::ToolCall {
                    tool_name: "shell".into(),
                    arguments: serde_json::json!({"command": "sh", "args": ["-c", ": > \"$2\"; i=0; while test ! -f \"$1\" && test $i -lt 500; do i=$((i+1)); sleep 0.01; done; test -f \"$1\"", "qmt-verification", release_file, entered_file], "workdir": harness._temp_dir.path()}),
                    expectation: querymt_agent_services::verification::Expectation::Success,
                    error_message: None,
                }],
            }),
            status: DelegationStatus::Requested,
            isolation: Default::default(),
            retry_count: 0,
            created_at: OffsetDateTime::now_utc(),
            completed_at: None,
            planning_summary: None,
        })
        .await
        .unwrap();
    let mut verification_tools = crate::tools::ToolRegistry::new();
    verification_tools.add(Arc::new(crate::tools::builtins::ShellTool::new()));
    let settings = crate::config::DelegationSettings {
        verification: true,
        cancel_grace_secs: 2,
        ..Default::default()
    };
    let orchestrator = Arc::new({
        let mut orchestrator = orchestrator_with(&config, &settings);
        orchestrator.tool_registry = Arc::new(verification_tools);
        orchestrator
    });
    let listener = orchestrator.start_listening(config.event_sink.fanout());
    let mut events = config.subscribe_events();
    config.emit_event(
        &parent_id,
        AgentEventKind::DelegationRequested {
            delegation: delegation.clone(),
            tool_call_id: None,
            batch_id: None,
            cycle_id: None,
        },
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(
                event.kind(),
                AgentEventKind::DelegationStageChanged {
                    stage: Some(DelegationStage::Verifying),
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !entered_file.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual verification shell must be waiting before cancellation");
    let activity = config.event_sink.live_delegation_activity(&parent_id);
    let child_id = activity[0].child_session_id.clone().unwrap();
    let local = delegate_of(&config, "agent");
    let child = local
        .observe_session_runtimes(std::slice::from_ref(&child_id))
        .await
        .remove(&child_id)
        .unwrap();
    assert_eq!(
        child.phase,
        crate::agent::messages::SessionRuntimePhase::Idle
    );
    assert_eq!(activity[0].stage, Some(DelegationStage::Verifying));
    #[cfg(feature = "remote")]
    {
        use crate::send_agent::SendAgent;
        use crate::session::backend::StorageBackend;
        use kameo::actor::Spawn;
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
        let owner = crate::agent::LocalAgentHandle::from_config(config.clone());
        owner.set_mesh(mesh.clone());
        owner
            .load_session(crate::acp::protocol::LoadSessionRequest::new(
                parent_id.clone(),
                std::path::PathBuf::new(),
            ))
            .await
            .unwrap();
        let expected = owner
            .observe_session_runtimes(std::slice::from_ref(&parent_id))
            .await
            .remove(&parent_id)
            .unwrap();
        assert_eq!(
            expected.phase,
            crate::agent::messages::SessionRuntimePhase::Idle
        );
        assert_eq!(
            expected.delegation_activity[0].stage,
            Some(DelegationStage::Verifying)
        );
        let manager = crate::agent::remote::RemoteNodeManager::spawn(
            crate::agent::remote::RemoteNodeManager::new(
                config.clone(),
                owner.registry.clone(),
                Some(mesh.clone()),
                owner.scheduler_handle.clone(),
            )
            .with_node_name("verifier-owner".to_string()),
        );
        mesh.register_actor(
            manager.clone(),
            crate::agent::remote::scope::scoped_node_manager_for_peer(
                &crate::agent::remote::scope::MeshScopeId::lan_default(),
                mesh.peer_id(),
            ),
        )
        .await;
        let viewer = crate::test_utils::TestAgent::new().await;
        viewer.handle.set_mesh(mesh.clone());
        let node = mesh.peer_id().to_string();
        viewer
            .storage
            .session_store()
            .save_remote_session_bookmark(&crate::session::store::RemoteSessionBookmark {
                session_id: parent_id.clone(),
                node_id: node.clone(),
                peer_label: "verifier-owner".into(),
                cwd: None,
                created_at: 0,
                title: None,
                fork_origin: None,
            })
            .await
            .unwrap();
        let loaded = viewer
            .handle
            .load_session(crate::acp::protocol::LoadSessionRequest::new(
                parent_id.clone(),
                std::path::PathBuf::new(),
            ))
            .await
            .unwrap();
        let attached = viewer
            .handle
            .ext_method(crate::acp::protocol::ExtRequest::new(
                "querymt/remote/attachSession",
                Arc::from(
                    serde_json::value::to_raw_value(
                        &serde_json::json!({"node_id": node, "session_id": parent_id}),
                    )
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        let attached: serde_json::Value = serde_json::from_str(attached.0.get()).unwrap();
        for snapshot in [
            &loaded.meta.as_ref().unwrap()["querymt/sessionLoadSnapshot.v1"],
            &attached["snapshot"],
        ] {
            assert_eq!(
                snapshot["runtimeStatus"],
                serde_json::to_value(&expected).unwrap(),
                "actual remote route omitted existing live verifier"
            );
            let live = snapshot["delegationUpdates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["delegationId"] == delegation.public_id)
                .unwrap();
            assert_eq!(live["activity"], "live");
            assert_eq!(live["stage"], "verifying");
            assert_eq!(
                live["sessionSeq"],
                serde_json::to_value(activity[0].session_seq).unwrap()
            );
            assert!(!snapshot["audit"]["events"].as_array().unwrap().is_empty());
            let events: Vec<querymt_agent_core::events::AgentEvent> =
                serde_json::from_value(snapshot["audit"]["events"].clone()).unwrap();
            assert_eq!(
                snapshot["cursor"],
                serde_json::to_value(crate::session::cursor_from_events(&events)).unwrap()
            );
            assert_eq!(
                snapshot["accounting"],
                serde_json::to_value(
                    querymt_agent_services::accounting::AccountingSnapshot::from_events(
                        &parent_id,
                        events.iter().map(|event| &event.kind),
                    )
                )
                .unwrap(),
                "idle/verifying owner snapshot must include accounting at its audit cursor"
            );
        }
        viewer.handle.shutdown().await;
        manager.kill();
        manager.wait_for_shutdown().await;
        owner.shutdown().await;
    }
    assert!(
        store
            .transition_delegation_status(
                &delegation.public_id,
                DelegationStatus::Running,
                DelegationStatus::Cancelled
            )
            .await
            .unwrap()
    );
    config.emit_event(
        &parent_id,
        AgentEventKind::DelegationCancelRequested {
            delegation_id: delegation.public_id.clone(),
        },
    );
    config.emit_event(
        &parent_id,
        AgentEventKind::DelegationCancelled {
            delegation_id: delegation.public_id.clone(),
        },
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(
                event.kind(),
                AgentEventKind::DelegationStageChanged {
                    stage: Some(DelegationStage::CancelRequested),
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let during = config.event_sink.live_delegation_activity(&parent_id);
    assert_eq!(during[0].activity, DelegationActivity::Live);
    assert_eq!(
        during[0].state,
        crate::control::delegation_notifications::DelegationUpdateState::Cancelled
    );
    assert_eq!(during[0].stage, Some(DelegationStage::CancelRequested));
    tokio::fs::write(release_file, "released").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(
                event.kind(),
                AgentEventKind::DelegationStageChanged { stage: None, .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        config
            .event_sink
            .live_delegation_activity(&parent_id)
            .is_empty()
    );
    orchestrator.cancel_active_delegations().await;
    listener.abort();
}

#[tokio::test]
async fn verification_failure_resolves_terminal_delegate_child() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
    harness.stop_harness_orchestrator();
    let config = harness.config.clone();
    let store = config.provider.history_store();
    let parent_session_id = harness.env.session_id.clone();
    let parent = store
        .get_session(&parent_session_id)
        .await
        .expect("parent lookup")
        .expect("parent session");
    let delegation = store
        .create_delegation(Delegation {
            id: 0,
            public_id: String::new(),
            session_id: parent.id,
            task_id: None,
            target_agent_id: "agent".to_string(),
            objective: "run and fail verification".to_string(),
            objective_hash: querymt_agent_core::hash::RapidHash::new(b"run and fail verification"),
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: Some(querymt_agent_services::verification::VerificationSpec {
                description: "require a missing file".to_string(),
                steps: vec![
                    querymt_agent_services::verification::VerificationStep::FileAssertion {
                        path: harness._temp_dir.path().join("missing-verification-file"),
                        exists: true,
                        contains: None,
                        matches_regex: None,
                    },
                ],
                strategy: Default::default(),
            }),
            status: DelegationStatus::Requested,
            isolation: Default::default(),
            retry_count: 0,
            created_at: OffsetDateTime::now_utc(),
            completed_at: None,
            planning_summary: None,
        })
        .await
        .expect("create verification delegation");
    let settings = crate::config::DelegationSettings {
        verification: true,
        ..Default::default()
    };
    let orchestrator = Arc::new(orchestrator_with(&config, &settings));
    let listener = orchestrator.start_listening(config.event_sink.fanout());
    config.event_sink.emit_ephemeral(
        &parent_session_id,
        AgentEventKind::DelegationRequested {
            delegation: delegation.clone(),
            tool_call_id: None,
            batch_id: None,
            cycle_id: None,
        },
    );

    let child_session_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let current = store
                .get_delegation(&delegation.public_id)
                .await
                .expect("delegation lookup")
                .expect("delegation");
            if current.status == DelegationStatus::Failed {
                let children = store
                    .list_child_sessions(&parent_session_id)
                    .await
                    .expect("list delegate children");
                if let Some(child_session_id) = children.first() {
                    let lifecycle = store
                        .get_session_lifecycle(child_session_id)
                        .await
                        .expect("child lifecycle");
                    if lifecycle.resolved {
                        break child_session_id.clone();
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("verification failure did not resolve its child");
    tokio::time::timeout(std::time::Duration::from_millis(500), async {
        loop {
            if config
                .event_sink
                .live_delegation_activity(&parent_session_id)
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("finished failed verification must release live workflow ownership");
    listener.abort();

    let child_lifecycle = harness.session_lifecycle(&child_session_id).await;
    assert!(child_lifecycle.resolved);
    assert!(!child_lifecycle.archived);
    let parent_lifecycle = harness.session_lifecycle(&parent_session_id).await;
    assert!(!parent_lifecycle.resolved);
    assert!(!parent_lifecycle.archived);
}

#[tokio::test]
async fn test_delegation_failure_message_format() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysFail).await;
    let delegate_call = mock_querymt_tool_call(
        "call-1",
        "delegate",
        r#"{"target_agent_id":"agent","objective":"task"}"#,
    );
    let mut seq = Sequence::new();

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(MockChatResponse::with_tools("", vec![delegate_call.clone()]).into())
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|messages| {
            let last_msg = messages.last().unwrap();
            assert!(last_msg.text().contains("Delegation failed"));
            assert!(last_msg.text().contains("Error Type:"));
            assert!(last_msg.text().contains("Patch Application Failure"));
            assert!(last_msg.text().contains("Do NOT immediately retry"));
            Ok(MockChatResponse::text_only("I'll try a different approach").into())
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "ok".to_string(),
            }])
        })
        .times(1);

    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

#[tokio::test]
async fn test_delegation_guard_blocks_duplicate() {
    let mut store = MockSessionStore::new();
    let session_id = "sess-guard".to_string();
    let history = vec![AgentMessage {
        id: "msg-1".to_string(),
        session_id: session_id.clone(),
        role: ChatRole::Assistant,
        parts: vec![MessagePart::ToolUse(mock_querymt_tool_call(
            "call-1",
            "delegate",
            r#"{"target_agent_id":"agent","objective":"task"}"#,
        ))],
        created_at: OffsetDateTime::now_utc().unix_timestamp(),
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    }];

    let delegation = Delegation {
        isolation: Default::default(),
        id: 1,
        public_id: "del-1".to_string(),
        session_id: 1,
        task_id: None,
        target_agent_id: "agent".to_string(),
        objective: "task".to_string(),
        objective_hash: querymt_agent_core::hash::RapidHash::new(b"task"),
        context: None,
        constraints: None,
        expected_output: None,
        verification_spec: None,
        status: DelegationStatus::Running,
        retry_count: 0,
        created_at: OffsetDateTime::now_utc(),
        completed_at: None,
        planning_summary: None,
    };

    store
        .expect_get_history()
        .returning(move |_| Ok(history.clone()))
        .times(1);
    store
        .expect_list_delegations()
        .returning(move |_| Ok(vec![delegation.clone()]))
        .times(1);

    let store: Arc<dyn SessionStore> = Arc::new(store);
    let middleware = DelegationGuardMiddleware::new(store);

    let state = ExecutionState::AfterLlm {
        response: Arc::new(LlmResponse::new(
            "".to_string(),
            vec![],
            None,
            Some(FinishReason::Stop),
        )),
        context: Arc::new(ConversationContext::new(
            session_id.into(),
            Arc::from([]),
            Arc::new(AgentStats::default()),
            "mock".into(),
            "mock-model".into(),
        )),
    };

    let result = middleware.on_after_llm(state, None).await.unwrap();

    assert!(matches!(
        result,
        ExecutionState::Stopped {
            stop_type: StopType::DelegationBlocked,
            ..
        }
    ));
}

#[tokio::test]
async fn test_delegation_guard_blocks_max_retries() {
    let mut store = MockSessionStore::new();
    let session_id = "sess-guard".to_string();
    let history = vec![AgentMessage {
        id: "msg-1".to_string(),
        session_id: session_id.clone(),
        role: ChatRole::Assistant,
        parts: vec![MessagePart::ToolUse(mock_querymt_tool_call(
            "call-1",
            "delegate",
            r#"{"target_agent_id":"agent","objective":"task"}"#,
        ))],
        created_at: OffsetDateTime::now_utc().unix_timestamp(),
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    }];

    let delegation = Delegation {
        isolation: Default::default(),
        id: 1,
        public_id: "del-1".to_string(),
        session_id: 1,
        task_id: None,
        target_agent_id: "agent".to_string(),
        objective: "task".to_string(),
        objective_hash: querymt_agent_core::hash::RapidHash::new(b"task"),
        context: None,
        constraints: None,
        expected_output: None,
        verification_spec: None,
        status: DelegationStatus::Failed,
        retry_count: 3,
        created_at: OffsetDateTime::now_utc(),
        completed_at: Some(OffsetDateTime::now_utc() - time::Duration::seconds(10)),
        planning_summary: None,
    };

    store
        .expect_get_history()
        .returning(move |_| Ok(history.clone()))
        .times(1);
    store
        .expect_list_delegations()
        .returning(move |_| Ok(vec![delegation.clone()]))
        .times(1);

    let store: Arc<dyn SessionStore> = Arc::new(store);
    let middleware = DelegationGuardMiddleware::new(store);

    let state = ExecutionState::AfterLlm {
        response: Arc::new(LlmResponse::new(
            "".to_string(),
            vec![],
            None,
            Some(FinishReason::Stop),
        )),
        context: Arc::new(ConversationContext::new(
            session_id.into(),
            Arc::from([]),
            Arc::new(AgentStats::default()),
            "mock".into(),
            "mock-model".into(),
        )),
    };

    let result = middleware.on_after_llm(state, None).await.unwrap();

    assert!(matches!(
        result,
        ExecutionState::Stopped {
            stop_type: StopType::DelegationBlocked,
            ..
        }
    ));
}

/// When a delegate is stopped by middleware (e.g. context threshold after failed
/// compaction), the delegation should be treated as a failure — not a success
/// with truncated output.
#[tokio::test]
async fn test_delegation_premature_stop_is_failure() {
    let mut harness = TestHarness::new(vec![], DelegateBehavior::StoppedByMiddleware).await;
    let delegate_call = mock_querymt_tool_call(
        "call-1",
        "delegate",
        r#"{"target_agent_id":"agent","objective":"task"}"#,
    );
    let mut seq = Sequence::new();

    // First LLM call: planner delegates to the agent
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(MockChatResponse::with_tools("Delegating task", vec![delegate_call.clone()]).into())
        });

    // Second LLM call: planner receives the delegation failure result.
    // The injected message should indicate failure, NOT success.
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|messages| {
            let last_msg = messages.last().unwrap();
            // Must be a failure, not a "Delegation completed" success
            assert!(
                last_msg.text().contains("Delegation failed"),
                "Expected 'Delegation failed' in message, got: {}",
                last_msg.text()
            );
            assert!(
                last_msg.text().contains("stopped prematurely"),
                "Expected 'stopped prematurely' in message, got: {}",
                last_msg.text()
            );
            Ok(MockChatResponse::text_only(
                "I see the delegate was stopped. Let me try differently.",
            )
            .into())
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "ok".to_string(),
            }])
        })
        .times(1);

    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

/// When a delegate hits the context threshold and auto-compaction **succeeds**,
/// the delegate should resume execution and complete normally. The delegation
/// orchestrator should see `StopReason::EndTurn` and mark it as a success.
///
/// This is the happy-path counterpart to `test_delegation_compaction_failure_is_delegation_failure`.
#[tokio::test]
async fn test_delegation_compaction_success_continues() {
    let mut harness =
        TestHarness::new(vec![], DelegateBehavior::ContextThresholdCompactionSucceeds).await;
    let delegate_call = mock_querymt_tool_call(
        "call-1",
        "delegate",
        r#"{"target_agent_id":"agent","objective":"task"}"#,
    );
    let mut seq = Sequence::new();

    // First LLM call: planner delegates to the agent
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(MockChatResponse::with_tools("Delegating task", vec![delegate_call.clone()]).into())
        });

    // Second LLM call: planner receives the delegation result.
    // Since compaction succeeded, the delegate completed normally and the
    // planner should see a success message, NOT a failure.
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|messages| {
            let last_msg = messages.last().unwrap();
            assert!(
                last_msg.text().contains("Delegation completed"),
                "Expected 'Delegation completed' after successful compaction, got: {}",
                last_msg.text()
            );
            assert!(
                !last_msg.text().contains("Delegation failed"),
                "Should NOT contain 'Delegation failed' after successful compaction, got: {}",
                last_msg.text()
            );
            Ok(MockChatResponse::text_only(
                "Great, the delegate completed successfully after compaction.",
            )
            .into())
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "ok".to_string(),
            }])
        })
        .times(1);

    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

/// When a delegate hits the context threshold and auto-compaction **fails**
/// (e.g. compaction LLM call errors out), the delegate's state machine falls
/// through to `Stopped(MaxTokens)`. The delegation orchestrator must treat
/// this as a failure — not silently swallow the error or report success.
///
/// This directly tests the bug path identified in the analysis:
///   ContextMiddleware -> ContextThreshold -> run_ai_compaction fails ->
///   CycleOutcome::Stopped(MaxTokens) -> PromptResponse(MaxTokens) ->
///   execute_delegation sees stop_reason != EndTurn -> fail_delegation
#[tokio::test]
async fn test_delegation_compaction_failure_is_delegation_failure() {
    let mut harness =
        TestHarness::new(vec![], DelegateBehavior::ContextThresholdCompactionFails).await;
    let delegate_call = mock_querymt_tool_call(
        "call-1",
        "delegate",
        r#"{"target_agent_id":"agent","objective":"task"}"#,
    );
    let mut seq = Sequence::new();

    // First LLM call: planner delegates to the agent
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(MockChatResponse::with_tools("Delegating task", vec![delegate_call.clone()]).into())
        });

    // Second LLM call: planner receives the delegation failure result.
    // The compaction LLM call failed, so the delegate was stopped with MaxTokens.
    // The orchestrator should report this as a delegation failure.
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|messages| {
            let last_msg = messages.last().unwrap();
            assert!(
                last_msg.text().contains("Delegation failed"),
                "Expected 'Delegation failed' after compaction failure, got: {}",
                last_msg.text()
            );
            assert!(
                last_msg.text().contains("stopped prematurely"),
                "Expected 'stopped prematurely' after compaction failure, got: {}",
                last_msg.text()
            );
            Ok(MockChatResponse::text_only(
                "The delegate failed due to compaction failure. I'll try a different approach.",
            )
            .into())
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "ok".to_string(),
            }])
        })
        .times(1);

    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

use crate::agent::agent_config::AgentConfig;
use crate::agent::core::ToolPolicy;
use crate::agent::execution::CycleOutcome;
use crate::agent::execution_context::ExecutionContext;
use crate::delegation::{AgentInfo, DefaultAgentRegistry, DelegationOrchestrator};
use crate::events::{AgentEventKind, StopType};
use crate::middleware::{
    AgentStats, ConversationContext, DelegationGuardMiddleware, ExecutionState, LlmResponse,
    MiddlewareDriver,
};
use crate::model::{AgentMessage, MessagePart};

use crate::session::backend::StorageBackend;
use crate::session::domain::{Delegation, DelegationStatus};
use crate::session::runtime::RuntimeContext;
use crate::session::sqlite_storage::SqliteStorage;
use crate::session::store::{SessionExecutionConfig, SessionStore};
use crate::test_utils::{
    MockChatResponse, MockLlmProvider, MockSessionStore, SharedLlmProvider, TestPluginLoader,
    TestProviderFactory, mock_querymt_tool_call,
};
use kameo::actor::Spawn;
use mockall::Sequence;
use querymt::LLMParams;
use querymt::chat::FinishReason;
use querymt::chat::{ChatRole, ReasoningEffort};
use querymt::plugin::host::PluginRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::sync::Mutex;

// Mock implementations moved to crate::test_utils::mocks

#[tokio::test(flavor = "current_thread")]
async fn genai_delegation_concurrent_parent_prompts_reach_child_agents_without_persisting_context()
{
    use crate::acp::protocol::{ContentBlock, LoadSessionRequest, PromptRequest, TextContent};
    use crate::agent::handle::AgentHandle as _;
    use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture_local_tasks};
    use opentelemetry::Value;

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
            let orchestrator = Arc::new(
                DelegationOrchestrator::new(
                    Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone())),
                    config.event_sink.clone(),
                    config.provider.history_store(),
                    config.agent_registry.clone(),
                    config.tool_registry_arc(),
                    config.hooks.clone(),
                    None,
                )
                .with_result_injection(false)
                .with_summarizer(Some(Arc::new(summarizer))),
            );
            harness.orchestrator_handle =
                Some(orchestrator.start_listening(config.event_sink.fanout()));
            let handle = crate::agent::LocalAgentHandle::from_config(harness.config.clone());
            let parent_id = harness.exec_ctx.session_id.clone();
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
            let target = harness.config.agent_registry.get_handle("agent").unwrap();
            let child_events = target
                .as_any()
                .downcast_ref::<crate::agent::LocalAgentHandle>()
                .unwrap()
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
    use crate::events::{DurableEvent, EventEnvelope, EventOrigin};
    use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture_local_tasks};
    use opentelemetry::{Value, trace::SpanId};
    use tracing::{Instrument, Span};

    for route in ["absent", "invalid", "remote", "replay"] {
        let (_, spans) = capture_local_tasks(async {
            let mut harness = TestHarness::new(vec![], DelegateBehavior::AlwaysOk).await;
            harness.stop_harness_orchestrator();
            tokio::task::yield_now().await;
            let config = harness.config.clone();
            let store = config.provider.history_store();
            let orchestrator = Arc::new(
                DelegationOrchestrator::new(
                    Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone())),
                    config.event_sink.clone(),
                    store.clone(),
                    config.agent_registry.clone(),
                    config.tool_registry_arc(),
                    config.hooks.clone(),
                    None,
                )
                .with_result_injection(false),
            );
            let consumer = tracing::info_span!(parent: None, "unrelated-consumer");
            let listener =
                consumer.in_scope(|| orchestrator.start_listening(config.event_sink.fanout()));
            let session = store
                .get_session(&harness.exec_ctx.session_id)
                .await
                .unwrap()
                .unwrap();
            let delegation = store
                .create_delegation(Delegation {
                    id: 0,
                    public_id: String::new(),
                    session_id: session.id,
                    task_id: None,
                    target_agent_id: "agent".into(),
                    objective: "SECRET_ARGUMENT".into(),
                    objective_hash: crate::hash::RapidHash::new(b"SECRET_ARGUMENT"),
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
            };
            let unrelated = tracing::info_span!(parent: None, "unrelated-relay");
            async {
                match route {
                    "remote" => {
                        config
                            .event_sink
                            .emit_durable_with_origin(
                                &harness.exec_ctx.session_id,
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
                                event_id: "replayed".into(),
                                stream_seq: 1,
                                session_id: harness.exec_ctx.session_id.clone(),
                                timestamp: 0,
                                origin: EventOrigin::Local,
                                source_node: None,
                                kind,
                            }));
                    }
                    "invalid" => {
                        let invalid = serde_json::json!({"traceparent":"invalid"});
                        assert!(
                            crate::acp::trace_context::extract_acp_trace_context(&invalid)
                                .is_none()
                        );
                        let root = tracing::info_span!(parent: None, "invalid-request-emitter");
                        // No valid request context is installed, matching invoke_agent's fallback.
                        config
                            .event_sink
                            .emit_durable(&harness.exec_ctx.session_id, kind)
                            .instrument(root)
                            .await
                            .unwrap();
                    }
                    _ => {
                        config
                            .event_sink
                            .emit_durable(&harness.exec_ctx.session_id, kind)
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
        if route != "invalid" {
            assert_eq!(handler.parent_span_id, SpanId::INVALID);
        }
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

/// A middleware that immediately stops execution with `StepLimit`,
/// simulating what happens when a delegate is stopped by middleware before
/// completing its work. Uses `StepLimit` (maps to `StopReason::MaxTurnRequests`)
/// rather than `ContextThreshold` to avoid triggering the auto-compaction loop
/// in the execution state machine.
struct AlwaysStopMiddleware;

#[async_trait::async_trait]
impl MiddlewareDriver for AlwaysStopMiddleware {
    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> crate::middleware::Result<ExecutionState> {
        match state {
            ExecutionState::BeforeLlmCall { ref context } => Ok(ExecutionState::Stopped {
                message: "Step limit reached".into(),
                stop_type: StopType::StepLimit,
                context: Some(context.clone()),
            }),
            other => Ok(other),
        }
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "AlwaysStopMiddleware"
    }
}

/// A middleware that fires `ContextThreshold` on the **first** `BeforeLlmCall`
/// and then passes through on subsequent calls. This simulates the real
/// `ContextMiddleware` detecting that the context window is full, which causes
/// the execution state machine to attempt AI compaction before continuing.
struct ContextThresholdOnceMiddleware {
    fired: std::sync::atomic::AtomicBool,
}

impl ContextThresholdOnceMiddleware {
    fn new() -> Self {
        Self {
            fired: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl MiddlewareDriver for ContextThresholdOnceMiddleware {
    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> crate::middleware::Result<ExecutionState> {
        match state {
            ExecutionState::BeforeLlmCall { ref context }
                if !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst) =>
            {
                Ok(ExecutionState::Stopped {
                    message: "Context token threshold reached, requesting compaction".into(),
                    stop_type: StopType::ContextThreshold,
                    context: Some(context.clone()),
                })
            }
            other => Ok(other),
        }
    }

    fn reset(&self) {
        self.fired.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    fn name(&self) -> &'static str {
        "ContextThresholdOnceMiddleware"
    }
}

#[derive(Debug, Clone)]
enum DelegateBehavior {
    AlwaysOk,
    AlwaysFail,
    /// Delegate's execution is stopped by middleware (e.g. context threshold).
    /// This simulates a premature stop that should be treated as a delegation failure.
    StoppedByMiddleware,
    /// Delegate hits ContextThreshold, auto-compaction runs and succeeds, then
    /// the delegate resumes and completes normally. Verifies that the delegation
    /// orchestrator sees `EndTurn` (success) when compaction recovers the session.
    ContextThresholdCompactionSucceeds,
    /// Delegate hits ContextThreshold, auto-compaction runs but the LLM call
    /// fails. The state machine falls through to `Stopped(MaxTokens)`, which
    /// the delegation orchestrator must treat as a failure.
    ContextThresholdCompactionFails,
}

struct TestHarness {
    config: Arc<AgentConfig>,
    exec_ctx: ExecutionContext,
    provider: Arc<Mutex<MockLlmProvider>>,
    orchestrator_handle: Option<tokio::task::JoinHandle<()>>,
    _temp_dir: TempDir,
}

impl TestHarness {
    async fn new(history: Vec<AgentMessage>, behavior: DelegateBehavior) -> Self {
        Self::new_with_delegate_reasoning(history, behavior, None).await
    }

    async fn new_with_delegate_reasoning(
        history: Vec<AgentMessage>,
        behavior: DelegateBehavior,
        delegate_reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared_provider = SharedLlmProvider {
            inner: provider.clone(),
            tools: Vec::new().into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared_provider));
        let temp_dir = TempDir::new().expect("temp dir");
        let wasm_path = temp_dir.path().join("mock.wasm");
        std::fs::write(&wasm_path, "").expect("write wasm");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(
            &config_path,
            format!(
                "[[providers]]\nname = \"mock\"\npath = \"{}\"\n",
                wasm_path.display()
            ),
        )
        .expect("write config");

        let mut registry = PluginRegistry::from_path(&config_path).expect("registry");
        registry.register_loader(Box::new(TestPluginLoader { factory }));
        let registry = Arc::new(registry);

        // Use a single shared in-memory SQLite store for orchestrator and delegates.
        let shared_storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("shared sqlite storage"),
        );
        let store: Arc<dyn SessionStore> = shared_storage.clone();

        let provider_context = crate::session::provider::SessionProvider::new(
            registry,
            store.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .with_agent_id(Some("parent".into()));
        let provider_context = Arc::new(provider_context);

        // Create the parent session via SessionProvider so LLM config is set up.
        let context = provider_context
            .create_session(None, None, &SessionExecutionConfig::default())
            .await
            .expect("create parent session");
        let session_id = context.session().public_id.clone();

        // Seed any initial history into the real store.
        for msg in &history {
            store
                .add_message(&session_id, msg.clone())
                .await
                .expect("seed history message");
        }

        let mut runtime_context = RuntimeContext::new(store.clone(), session_id.clone())
            .await
            .expect("runtime context");
        runtime_context
            .load_working_context()
            .await
            .expect("load context");

        // Create delegate agents as real LocalAgentHandles.
        // All delegates share the same in-memory SQLite store so parent_session_id
        // resolves correctly when creating delegation sessions.
        let mut agent_registry = DefaultAgentRegistry::new();
        for id in ["agent", "agent1", "agent2"] {
            let delegate_handle =
                build_delegate_handle(behavior.clone(), store.clone(), delegate_reasoning_effort)
                    .await;
            agent_registry.register_handle(
                agent_info(id),
                delegate_handle as Arc<dyn crate::agent::handle::AgentHandle>,
            );
        }
        let agent_registry: Arc<DefaultAgentRegistry> = Arc::new(agent_registry);

        let config = Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
                shared_storage.clone(),
                provider_context,
                shared_storage.event_journal(),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .with_agent_registry_only(agent_registry.clone())
            .build(),
        );

        // Create a LocalAgentHandle for delegation (wraps its own SessionRegistry)
        let delegator: Arc<dyn crate::agent::handle::AgentHandle> =
            Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone()));

        let orchestrator = Arc::new(
            DelegationOrchestrator::new(
                delegator,
                config.event_sink.clone(),
                store.clone(),
                agent_registry.clone(),
                config.tool_registry_arc(),
                config.hooks.clone(),
                None,
            )
            .with_delegate_model_overrides(config.delegate_model_overrides.clone()),
        );
        let orchestrator_handle = Some(orchestrator.start_listening(config.event_sink.fanout()));

        // Create a SessionRuntime for the execution context
        let session_runtime = crate::agent::core::SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );

        let exec_ctx = ExecutionContext::new(
            session_id,
            session_runtime,
            runtime_context,
            context,
            crate::agent::core::ToolConfig::default(),
        );

        Self {
            config,
            exec_ctx,
            provider,
            orchestrator_handle,
            _temp_dir: temp_dir,
        }
    }

    fn stop_harness_orchestrator(&mut self) {
        if let Some(handle) = self.orchestrator_handle.take() {
            handle.abort();
        }
    }

    async fn run(&mut self) -> CycleOutcome {
        crate::agent::execution::execute_cycle_state_machine(
            &self.config,
            &mut self.exec_ctx,
            None,
            crate::agent::core::AgentMode::Build,
        )
        .await
        .expect("state machine")
    }

    async fn set_parent_reasoning_effort(&self, effort: Option<ReasoningEffort>) {
        let store = self.config.provider.history_store();
        let current = store
            .get_session_llm_config(&self.exec_ctx.session_id)
            .await
            .expect("parent config lookup")
            .expect("parent config");
        let mut params = current
            .params
            .map(serde_json::from_value::<LLMParams>)
            .transpose()
            .expect("deserialize parent params")
            .unwrap_or_default()
            .provider(&current.provider)
            .model(&current.model);
        params.reasoning_effort = effort;
        let updated = store
            .create_or_get_llm_config(&params)
            .await
            .expect("create parent config");
        store
            .set_session_llm_config(&self.exec_ctx.session_id, updated.id)
            .await
            .expect("set parent config");
    }

    async fn expect_single_delegation(&mut self) {
        let delegate_call = mock_querymt_tool_call(
            "call-1",
            "delegate",
            r#"{"target_agent_id":"agent","objective":"task"}"#,
        );
        let mut seq = Sequence::new();
        self.provider_mut()
            .await
            .expect_chat()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move |_| {
                Ok(
                    MockChatResponse::with_tools("Delegating task", vec![delegate_call.clone()])
                        .into(),
                )
            });
        self.provider_mut()
            .await
            .expect_chat()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_| Ok(MockChatResponse::text_only("Done").into()));
        self.provider_mut()
            .await
            .expect_call_tool()
            .returning(|_, _| {
                Ok(vec![querymt::chat::ToolResultPart::Text {
                    text: "ok".to_string(),
                }])
            })
            .times(1);
        self.provider_mut()
            .await
            .expect_tools()
            .return_const(None)
            .times(0..);
    }

    async fn run_single_delegation(&mut self) -> CycleOutcome {
        self.expect_single_delegation().await;
        self.run().await
    }

    async fn run_single_delegation_without_child_tool(&mut self) -> CycleOutcome {
        let delegate_call = mock_querymt_tool_call(
            "call-1",
            "delegate",
            r#"{"target_agent_id":"agent","objective":"task"}"#,
        );
        let mut seq = Sequence::new();
        self.provider_mut()
            .await
            .expect_chat()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move |_| {
                Ok(
                    MockChatResponse::with_tools("Delegating task", vec![delegate_call.clone()])
                        .into(),
                )
            });
        self.provider_mut()
            .await
            .expect_chat()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_| Ok(MockChatResponse::text_only("Done").into()));
        self.provider_mut()
            .await
            .expect_call_tool()
            .returning(|_, _| {
                Ok(vec![querymt::chat::ToolResultPart::Text {
                    text: "ok".to_string(),
                }])
            })
            .times(1);
        self.provider_mut()
            .await
            .expect_tools()
            .return_const(None)
            .times(0..);

        self.run().await
    }

    async fn child_sessions(&self) -> Vec<String> {
        self.config
            .provider
            .history_store()
            .list_child_sessions(&self.exec_ctx.session_id)
            .await
            .expect("child sessions")
    }

    async fn child_llm_params(&self) -> (String, LLMParams) {
        let store = self.config.provider.history_store();
        let child_sessions = store
            .list_child_sessions(&self.exec_ctx.session_id)
            .await
            .expect("child sessions");
        let child_session_id = child_sessions.first().expect("child session id");
        let config = store
            .get_session_llm_config(child_session_id)
            .await
            .expect("child config lookup")
            .expect("child config");
        let params = config
            .params
            .map(serde_json::from_value::<LLMParams>)
            .transpose()
            .expect("deserialize child params")
            .unwrap_or_default();

        (config.model, params)
    }

    async fn provider_mut(&self) -> tokio::sync::MutexGuard<'_, MockLlmProvider> {
        self.provider.lock().await
    }
}

/// Build a delegate `AgentHandle` that shares the given store with the
/// orchestrator, ensuring parent_session_id resolves correctly.
async fn build_delegate_handle(
    behavior: DelegateBehavior,
    shared_store: Arc<dyn SessionStore>,
    reasoning_effort: Option<ReasoningEffort>,
) -> Arc<crate::agent::LocalAgentHandle> {
    let delegate_provider = Arc::new(Mutex::new(MockLlmProvider::new()));
    {
        let mut mock = delegate_provider.lock().await;
        match behavior {
            DelegateBehavior::AlwaysOk => {
                mock.expect_chat()
                    .times(0..)
                    .returning(|_| Ok(MockChatResponse::text_only("Task complete").into()));
            }
            DelegateBehavior::AlwaysFail => {
                mock.expect_chat().times(0..).returning(|_| {
                    Err(querymt::error::LLMError::ProviderError(
                        "Invalid patch: line mismatch".to_string(),
                    ))
                });
            }
            DelegateBehavior::StoppedByMiddleware => {
                // The LLM will never be called because the middleware stops execution
                // before the LLM call. Set up a fallback that would succeed if reached.
                mock.expect_chat()
                    .times(0..)
                    .returning(|_| Ok(MockChatResponse::text_only("Task complete").into()));
            }
            DelegateBehavior::ContextThresholdCompactionSucceeds => {
                // ContextThresholdOnceMiddleware fires ContextThreshold on the first
                // BeforeLlmCall. The state machine then calls run_ai_compaction which
                // calls provider.chat() for the compaction summary. After that succeeds,
                // the state machine loops back and the middleware passes through, so
                // provider.chat() is called again for the normal conversation turn.
                let mut seq = Sequence::new();
                // 1st chat call: compaction summary
                mock.expect_chat()
                    .times(1)
                    .in_sequence(&mut seq)
                    .returning(|_| {
                        Ok(
                            MockChatResponse::text_only(
                                "Summary of previous conversation context.",
                            )
                            .into(),
                        )
                    });
                // 2nd chat call: normal delegate completion
                mock.expect_chat()
                    .times(1)
                    .in_sequence(&mut seq)
                    .returning(|_| Ok(MockChatResponse::text_only("Task complete").into()));
            }
            DelegateBehavior::ContextThresholdCompactionFails => {
                // ContextThresholdOnceMiddleware fires ContextThreshold. The state
                // machine calls run_ai_compaction which calls provider.chat() — this
                // fails. The state machine falls through to Stopped(MaxTokens).
                // No retries: we set max_retries=0 in the compaction config.
                mock.expect_chat().times(0..).returning(|_| {
                    Err(querymt::error::LLMError::ProviderError(
                        "Compaction LLM call failed: service unavailable".to_string(),
                    ))
                });
            }
        }
        mock.expect_tools().return_const(None).times(0..);
    }

    let delegate_shared = SharedLlmProvider {
        inner: delegate_provider,
        tools: Vec::new().into_boxed_slice(),
    };
    let delegate_factory = Arc::new(TestProviderFactory::new(delegate_shared));

    let delegate_temp_dir = TempDir::new().expect("temp dir");
    let delegate_wasm_path = delegate_temp_dir.path().join("mock.wasm");
    std::fs::write(&delegate_wasm_path, "").expect("write wasm");
    let delegate_config_path = delegate_temp_dir.path().join("providers.toml");
    std::fs::write(
        &delegate_config_path,
        format!(
            "[[providers]]\nname = \"mock\"\npath = \"{}\"\n",
            delegate_wasm_path.display()
        ),
    )
    .expect("write config");

    let mut delegate_plugin_registry =
        PluginRegistry::from_path(&delegate_config_path).expect("registry");
    delegate_plugin_registry.register_loader(Box::new(TestPluginLoader {
        factory: delegate_factory,
    }));
    let delegate_plugin_registry = Arc::new(delegate_plugin_registry);

    let mut delegate_params = LLMParams::new().provider("mock").model("mock-model");
    delegate_params.reasoning_effort = reasoning_effort;
    let delegate_session_provider = Arc::new(
        crate::session::provider::SessionProvider::new(
            delegate_plugin_registry,
            shared_store,
            delegate_params,
        )
        .with_agent_id(Some("reader".into())),
    );
    let delegate_event_storage = Arc::new(
        SqliteStorage::connect(":memory:".into())
            .await
            .expect("create delegate event journal storage"),
    );

    let mut builder = crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
        delegate_event_storage.clone(),
        delegate_session_provider,
        delegate_event_storage.event_journal(),
    )
    .with_tool_policy(ToolPolicy::ProviderOnly)
    .with_max_steps(1)
    .with_execution_timeout_secs(30);

    if matches!(behavior, DelegateBehavior::StoppedByMiddleware) {
        builder = builder.with_middleware(AlwaysStopMiddleware);
    }

    if matches!(
        behavior,
        DelegateBehavior::ContextThresholdCompactionSucceeds
            | DelegateBehavior::ContextThresholdCompactionFails
    ) {
        // Install middleware that triggers ContextThreshold once, then passes through.
        builder = builder.with_middleware(ContextThresholdOnceMiddleware::new());

        // Enable auto-compaction with zero retries so the test doesn't sleep.
        builder = builder.with_compaction_config(crate::config::CompactionConfig {
            auto: true,
            provider: None,
            model: None,
            retry: crate::config::RetryConfig {
                max_retries: 0,
                initial_backoff_ms: 0,
                backoff_multiplier: 1.0,
            },
        });

        // Allow more steps so the delegate can continue after compaction.
        builder = builder.with_max_steps(5);
    }

    let delegate_config = Arc::new(builder.build());

    // Leak the TempDir so its contents survive the test
    std::mem::forget(delegate_temp_dir);

    Arc::new(crate::agent::LocalAgentHandle::from_config(delegate_config))
}

fn agent_info(id: &str) -> AgentInfo {
    AgentInfo {
        id: id.to_string(),
        name: format!("{} name", id),
        description: format!("{} description", id),
        capabilities: vec![],
        required_capabilities: vec![],
        meta: None,
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
        harness.exec_ctx.session_id.clone(),
        parent_runtime,
    );
    let parent_actor = crate::agent::SessionActor::spawn(parent_actor);
    bridge_handle
        .registry
        .lock()
        .await
        .insert(harness.exec_ctx.session_id.clone(), parent_actor);
    let (bridge_tx, _bridge_rx) = tokio::sync::mpsc::channel(4);
    let bridge = crate::acp::client_bridge::ClientBridgeSender::for_connection(bridge_tx, "conn");
    bridge_handle
        .set_session_bridge(&harness.exec_ctx.session_id, bridge)
        .await
        .expect("set parent bridge");

    harness.run_single_delegation().await;
    let child_session_id = harness
        .child_sessions()
        .await
        .into_iter()
        .next()
        .expect("child session");
    let target = harness
        .config
        .agent_registry
        .get_handle("agent")
        .expect("delegate handle");
    let target = target
        .as_any()
        .downcast_ref::<crate::agent::LocalAgentHandle>()
        .expect("local delegate");
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
    let inherited = child
        .ask(crate::agent::messages::GetBridge)
        .await
        .expect("query child bridge")
        .expect("child bridge");
    assert_eq!(inherited.connection_id(), Some("conn"));
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
        harness.exec_ctx.session_id.clone(),
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
        .insert(harness.exec_ctx.session_id.clone(), parent_actor);
    let (bridge_tx, _bridge_rx) = tokio::sync::mpsc::channel(4);
    bridge_handle
        .set_session_bridge(
            &harness.exec_ctx.session_id,
            crate::acp::client_bridge::ClientBridgeSender::for_connection(bridge_tx, "conn"),
        )
        .await
        .expect("set parent bridge");
    harness
        .config
        .provider
        .history_store()
        .set_delegate_assignment(
            &harness.exec_ctx.session_id,
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
    let target = harness
        .config
        .agent_registry
        .get_handle("agent")
        .expect("delegate handle");
    let target = target
        .as_any()
        .downcast_ref::<crate::agent::LocalAgentHandle>()
        .expect("local delegate");
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
    let store = config.provider.history_store();
    let duplicate = Arc::new(
        DelegationOrchestrator::new(
            Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone())),
            config.event_sink.clone(),
            store,
            config.agent_registry.clone(),
            config.tool_registry_arc(),
            config.hooks.clone(),
            None,
        )
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
    let spectator = Arc::new(
        DelegationOrchestrator::new(
            Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone())),
            config.event_sink.clone(),
            config.provider.history_store(),
            config.agent_registry.clone(),
            config.tool_registry_arc(),
            config.hooks.clone(),
            None,
        )
        .with_profile_id("owner"),
    );
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
        .set_profile_binding(&harness.exec_ctx.session_id, "owner")
        .await
        .expect("bind parent profile");
    let owner = Arc::new(
        DelegationOrchestrator::new(
            Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone())),
            config.event_sink.clone(),
            store.clone(),
            config.agent_registry.clone(),
            config.tool_registry_arc(),
            config.hooks.clone(),
            None,
        )
        .with_profile_id("owner"),
    );
    let unrelated = Arc::new(
        DelegationOrchestrator::new(
            Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone())),
            config.event_sink.clone(),
            store,
            config.agent_registry.clone(),
            config.tool_registry_arc(),
            config.hooks.clone(),
            None,
        )
        .with_profile_id("other"),
    );
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
    let missing_target = Arc::new(DelegationOrchestrator::new(
        Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone())),
        config.event_sink.clone(),
        config.provider.history_store(),
        Arc::new(DefaultAgentRegistry::new()),
        config.tool_registry_arc(),
        config.hooks.clone(),
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
        .list_delegations(&harness.exec_ctx.session_id)
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
        .get_session(&harness.exec_ctx.session_id)
        .await
        .expect("session lookup")
        .expect("parent session");
    let delegation = Delegation {
        id: 0,
        public_id: String::new(),
        session_id: session.id,
        task_id: None,
        target_agent_id: "agent".to_string(),
        objective: "not started yet".to_string(),
        objective_hash: crate::hash::RapidHash::new(b"not started yet"),
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
        &harness.exec_ctx,
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
        .get_session(&harness.exec_ctx.session_id)
        .await
        .expect("session lookup")
        .expect("parent session");
    let delegation = Delegation {
        id: 0,
        public_id: String::new(),
        session_id: session.id,
        task_id: None,
        target_agent_id: "agent".to_string(),
        objective: "already complete".to_string(),
        objective_hash: crate::hash::RapidHash::new(b"already complete"),
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
        &harness.exec_ctx,
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
            &harness.exec_ctx.session_id,
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
            &harness.exec_ctx.session_id,
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
            &harness.exec_ctx.session_id,
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
        .load_session_stream(&harness.exec_ctx.session_id, None, None)
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
}

#[tokio::test]
async fn test_delegation_completion_message_format() {
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
        id: 1,
        public_id: "del-1".to_string(),
        session_id: 1,
        task_id: None,
        target_agent_id: "agent".to_string(),
        objective: "task".to_string(),
        objective_hash: crate::hash::RapidHash::new(b"task"),
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
        id: 1,
        public_id: "del-1".to_string(),
        session_id: 1,
        task_id: None,
        target_agent_id: "agent".to_string(),
        objective: "task".to_string(),
        objective_hash: crate::hash::RapidHash::new(b"task"),
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

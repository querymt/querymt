use crate::agent::agent_config::AgentConfig;
use crate::agent::core::ToolPolicy;
use crate::agent::execution::CycleOutcome;
use crate::agent::execution_context::ExecutionContext;
use crate::delegation::{AgentInfo, DefaultAgentRegistry, DelegationOrchestrator};
use crate::events::StopType;
use crate::middleware::{
    AgentStats, ConversationContext, DelegationGuardMiddleware, ExecutionState, LlmResponse,
    MiddlewareDriver,
};
use crate::model::{AgentMessage, MessagePart};

use crate::session::backend::StorageBackend;
use crate::session::domain::{Delegation, DelegationStatus};
use crate::session::provider::SessionHandle;
use crate::session::runtime::RuntimeContext;
use crate::session::store::SessionStore;
use crate::test_utils::{
    MockChatResponse, MockLlmProvider, MockSessionStore, SharedLlmProvider, TestPluginLoader,
    TestProviderFactory, mock_llm_config, mock_querymt_tool_call, mock_session,
};
use mockall::Sequence;
use querymt::LLMParams;
use querymt::chat::ChatRole;
use querymt::chat::FinishReason;
use querymt::plugin::host::PluginRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::sync::Mutex;

// Mock implementations moved to crate::test_utils::mocks

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
    _temp_dir: TempDir,
}

impl TestHarness {
    async fn new(history: Vec<AgentMessage>, behavior: DelegateBehavior) -> Self {
        let session_id = "sess-test".to_string();
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared_provider = SharedLlmProvider {
            inner: provider.clone(),
            tools: Vec::new().into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory {
            provider: shared_provider,
        });
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

        let mut store = MockSessionStore::new();
        let session = mock_session(&session_id);
        let session_for_context = session.clone();
        let session_for_expectation = session.clone();
        let llm_config = mock_llm_config();
        let history = Arc::new(history);

        store
            .expect_get_session()
            .returning(move |_| Ok(Some(session_for_expectation.clone())))
            .times(0..);
        let history_for_effective = history.clone();
        store
            .expect_get_history()
            .returning(move |_| Ok((*history).clone()))
            .times(0..);
        store
            .expect_get_effective_history()
            .returning(move |_| Ok((*history_for_effective).clone()))
            .times(0..);
        store
            .expect_get_session_llm_config()
            .returning(move |_| Ok(Some(llm_config.clone())))
            .times(0..);
        let llm_config_for_handle = mock_llm_config();
        store
            .expect_get_llm_config()
            .returning(move |_| Ok(Some(llm_config_for_handle.clone())))
            .times(0..);
        store
            .expect_get_session_execution_config()
            .returning(|_| Ok(None))
            .times(0..);
        store
            .expect_get_session_provider_node_id()
            .returning(|_| Ok(None))
            .times(0..);
        store
            .expect_add_message()
            .returning(|_, _| Ok(()))
            .times(0..);
        store
            .expect_append_progress_entry()
            .returning(|_| Ok(()))
            .times(0..);
        store
            .expect_get_current_intent_snapshot()
            .returning(|_| Ok(None))
            .times(0..);
        store
            .expect_list_delegations()
            .returning(|_| Ok(vec![]))
            .times(0..);
        store
            .expect_mark_tool_results_compacted()
            .returning(|_, _| Ok(0))
            .times(0..);
        store
            .expect_create_delegation()
            .returning(|mut delegation| {
                // Assign a DB ID if not set
                if delegation.id == 0 {
                    delegation.id = 1;
                }
                Ok(delegation)
            })
            .times(0..);
        store
            .expect_update_delegation_status()
            .returning(|_, _| Ok(()))
            .times(0..);

        let store: Arc<dyn SessionStore> = Arc::new(store);
        let provider_context = crate::session::provider::SessionProvider::new(
            registry,
            store.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        );
        let provider_context = Arc::new(provider_context);

        let context = SessionHandle::new(provider_context.clone(), session_for_context)
            .await
            .expect("context");

        let mut runtime_context = RuntimeContext::new(store.clone(), session_id.clone())
            .await
            .expect("runtime context");
        runtime_context
            .load_working_context()
            .await
            .expect("load context");

        // Create delegate agents as real LocalAgentHandles.
        // Each delegate gets its own in-memory SQLite store and mock LLM provider
        // configured for the desired behavior (success/failure).
        let mut agent_registry = DefaultAgentRegistry::new();
        for id in ["agent", "agent1", "agent2"] {
            let delegate_handle = build_delegate_handle(behavior.clone()).await;
            agent_registry.register_handle(
                agent_info(id),
                delegate_handle as Arc<dyn crate::agent::handle::AgentHandle>,
            );
        }
        let agent_registry: Arc<DefaultAgentRegistry> = Arc::new(agent_registry);

        let event_journal_storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create event journal storage"),
        );

        let config = Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
                provider_context,
                event_journal_storage.event_journal(),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .with_agent_registry_only(agent_registry.clone())
            .build(),
        );

        // Create a LocalAgentHandle for delegation (wraps its own SessionRegistry)
        let delegator: Arc<dyn crate::agent::handle::AgentHandle> =
            Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone()));

        let orchestrator = Arc::new(DelegationOrchestrator::new(
            delegator,
            config.event_sink.clone(),
            store.clone(),
            agent_registry.clone(),
            config.tool_registry_arc(),
            None,
        ));
        let _orchestrator_handle = orchestrator.start_listening(config.event_sink.fanout());

        // Create a SessionRuntime for the execution context
        let session_runtime = crate::agent::core::SessionRuntime::new(
            None,
            HashMap::new(),
            HashMap::new(),
            Vec::new(),
        );

        let session_id = "sess-test".to_string();
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
            _temp_dir: temp_dir,
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

    async fn provider_mut(&self) -> tokio::sync::MutexGuard<'_, MockLlmProvider> {
        self.provider.lock().await
    }
}

/// Build a delegate `AgentHandle` with its own in-memory SQLite store and
/// mock LLM provider configured for the desired behavior.
async fn build_delegate_handle(behavior: DelegateBehavior) -> Arc<crate::agent::LocalAgentHandle> {
    use crate::session::sqlite_storage::SqliteStorage;

    let delegate_store: Arc<dyn SessionStore> =
        Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());

    let delegate_provider = Arc::new(Mutex::new(MockLlmProvider::new()));
    {
        let mut mock = delegate_provider.lock().await;
        match behavior {
            DelegateBehavior::AlwaysOk => {
                mock.expect_chat()
                    .times(0..)
                    .returning(|_| Ok(Box::new(MockChatResponse::text_only("Task complete"))));
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
                    .returning(|_| Ok(Box::new(MockChatResponse::text_only("Task complete"))));
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
                        Ok(Box::new(MockChatResponse::text_only(
                            "Summary of previous conversation context.",
                        )))
                    });
                // 2nd chat call: normal delegate completion
                mock.expect_chat()
                    .times(1)
                    .in_sequence(&mut seq)
                    .returning(|_| Ok(Box::new(MockChatResponse::text_only("Task complete"))));
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
    let delegate_factory = Arc::new(TestProviderFactory {
        provider: delegate_shared,
    });

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

    let delegate_session_provider = Arc::new(crate::session::provider::SessionProvider::new(
        delegate_plugin_registry,
        delegate_store,
        LLMParams::new().provider("mock").model("mock-model"),
    ));
    let event_journal_storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .expect("create event journal storage"),
    );

    let mut builder = crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
        delegate_session_provider,
        event_journal_storage.event_journal(),
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
            Ok(Box::new(MockChatResponse::with_tools(
                "Delegating task 1",
                vec![delegate_call_1.clone()],
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(Box::new(MockChatResponse::with_tools(
                "Delegating task 2",
                vec![delegate_call_2.clone()],
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(Box::new(MockChatResponse::text_only("All tasks complete"))));

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Ok("ok".to_string()))
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
            Ok(Box::new(MockChatResponse::with_tools(
                "Delegating task",
                vec![delegate_call.clone()],
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|messages| {
            let last_msg = messages.last().unwrap();
            assert!(last_msg.content.contains("Delegation failed"));
            assert!(last_msg.content.contains("Patch Application Failure"));
            Ok(Box::new(MockChatResponse::text_only(
                "I'll handle it differently",
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Ok("ok".to_string()))
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
            Ok(Box::new(MockChatResponse::with_tools(
                "",
                vec![delegate_call.clone()],
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|messages| {
            let last_msg = messages.last().unwrap();
            assert!(last_msg.content.contains("Delegation completed"));
            assert!(last_msg.content.contains("Delegation ID:"));
            assert!(last_msg.content.contains("Please review the changes"));
            assert!(last_msg.content.contains("=== Delegate Agent Results ==="));
            Ok(Box::new(MockChatResponse::text_only(
                "Perfect, task complete",
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Ok("ok".to_string()))
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
            Ok(Box::new(MockChatResponse::with_tools(
                "",
                vec![delegate_call.clone()],
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|messages| {
            let last_msg = messages.last().unwrap();
            assert!(last_msg.content.contains("Delegation failed"));
            assert!(last_msg.content.contains("Error Type:"));
            assert!(last_msg.content.contains("Patch Application Failure"));
            assert!(last_msg.content.contains("Do NOT immediately retry"));
            Ok(Box::new(MockChatResponse::text_only(
                "I'll try a different approach",
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Ok("ok".to_string()))
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
            Ok(Box::new(MockChatResponse::with_tools(
                "Delegating task",
                vec![delegate_call.clone()],
            )))
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
                last_msg.content.contains("Delegation failed"),
                "Expected 'Delegation failed' in message, got: {}",
                last_msg.content
            );
            assert!(
                last_msg.content.contains("stopped prematurely"),
                "Expected 'stopped prematurely' in message, got: {}",
                last_msg.content
            );
            Ok(Box::new(MockChatResponse::text_only(
                "I see the delegate was stopped. Let me try differently.",
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Ok("ok".to_string()))
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
            Ok(Box::new(MockChatResponse::with_tools(
                "Delegating task",
                vec![delegate_call.clone()],
            )))
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
                last_msg.content.contains("Delegation completed"),
                "Expected 'Delegation completed' after successful compaction, got: {}",
                last_msg.content
            );
            assert!(
                !last_msg.content.contains("Delegation failed"),
                "Should NOT contain 'Delegation failed' after successful compaction, got: {}",
                last_msg.content
            );
            Ok(Box::new(MockChatResponse::text_only(
                "Great, the delegate completed successfully after compaction.",
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Ok("ok".to_string()))
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
            Ok(Box::new(MockChatResponse::with_tools(
                "Delegating task",
                vec![delegate_call.clone()],
            )))
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
                last_msg.content.contains("Delegation failed"),
                "Expected 'Delegation failed' after compaction failure, got: {}",
                last_msg.content
            );
            assert!(
                last_msg.content.contains("stopped prematurely"),
                "Expected 'stopped prematurely' after compaction failure, got: {}",
                last_msg.content
            );
            Ok(Box::new(MockChatResponse::text_only(
                "The delegate failed due to compaction failure. I'll try a different approach.",
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Ok("ok".to_string()))
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

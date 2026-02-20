use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{
    AgentMode, DelegationContextConfig, DelegationContextTiming, SnapshotPolicy, ToolConfig,
    ToolPolicy,
};
use crate::agent::execution::CycleOutcome;
use crate::agent::execution_context::ExecutionContext;
use crate::config::RuntimeExecutionPolicy;
use crate::delegation::{AgentInfo, DefaultAgentRegistry, DelegationOrchestrator};
use crate::event_bus::EventBus;
use crate::events::StopType;
use crate::index::{WorkspaceIndexManagerActor, WorkspaceIndexManagerConfig};
use crate::middleware::{
    AgentStats, ConversationContext, DelegationGuardMiddleware, ExecutionState, LlmResponse,
    MiddlewareDriver,
};
use crate::model::{AgentMessage, MessagePart};
use crate::send_agent::SendAgent;
use crate::session::domain::{Delegation, DelegationStatus};
use crate::session::provider::SessionHandle;
use crate::session::runtime::RuntimeContext;
use crate::session::store::SessionStore;
use crate::test_utils::{
    MockChatResponse, MockLlmProvider, MockSessionStore, SharedLlmProvider, TestPluginLoader,
    TestProviderFactory, mock_llm_config, mock_querymt_tool_call, mock_session,
};
use crate::tools::ToolRegistry;
use agent_client_protocol::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, Error, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ProtocolVersion, StopReason,
};
use async_trait::async_trait;
use mockall::Sequence;
use querymt::LLMParams;
use querymt::chat::ChatRole;
use querymt::chat::FinishReason;
use querymt::plugin::host::PluginRegistry;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::sync::Mutex;

// Mock implementations moved to crate::test_utils::mocks

#[derive(Debug, Clone)]
enum DelegateBehavior {
    AlwaysOk,
    AlwaysFail,
}

#[derive(Debug)]
struct StubDelegateAgent {
    behavior: Arc<StdMutex<DelegateBehavior>>,
    prompt_calls: AtomicUsize,
    session_counter: AtomicUsize,
}

impl StubDelegateAgent {
    fn new(behavior: DelegateBehavior) -> Self {
        Self {
            behavior: Arc::new(StdMutex::new(behavior)),
            prompt_calls: AtomicUsize::new(0),
            session_counter: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SendAgent for StubDelegateAgent {
    async fn initialize(&self, _req: InitializeRequest) -> Result<InitializeResponse, Error> {
        Ok(InitializeResponse::new(ProtocolVersion::LATEST))
    }

    async fn authenticate(&self, _req: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, _req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        let count = self.session_counter.fetch_add(1, Ordering::SeqCst);
        Ok(NewSessionResponse::new(format!(
            "delegate-session-{}",
            count
        )))
    }

    async fn prompt(&self, _req: PromptRequest) -> Result<PromptResponse, Error> {
        self.prompt_calls.fetch_add(1, Ordering::SeqCst);
        let behavior = self.behavior.lock().unwrap().clone();
        match behavior {
            DelegateBehavior::AlwaysOk => Ok(PromptResponse::new(StopReason::EndTurn)),
            DelegateBehavior::AlwaysFail => Err(Error::from(crate::error::AgentError::Internal(
                "Invalid patch: line mismatch".to_string(),
            ))),
        }
    }

    async fn cancel(&self, _notif: CancelNotification) -> Result<(), Error> {
        Ok(())
    }

    async fn load_session(
        &self,
        _req: agent_client_protocol::LoadSessionRequest,
    ) -> Result<agent_client_protocol::LoadSessionResponse, Error> {
        Ok(agent_client_protocol::LoadSessionResponse::new())
    }

    async fn list_sessions(
        &self,
        _req: agent_client_protocol::ListSessionsRequest,
    ) -> Result<agent_client_protocol::ListSessionsResponse, Error> {
        Ok(agent_client_protocol::ListSessionsResponse::new(vec![]))
    }

    async fn fork_session(
        &self,
        _req: agent_client_protocol::ForkSessionRequest,
    ) -> Result<agent_client_protocol::ForkSessionResponse, Error> {
        Ok(agent_client_protocol::ForkSessionResponse::new(
            "forked-session",
        ))
    }

    async fn resume_session(
        &self,
        _req: agent_client_protocol::ResumeSessionRequest,
    ) -> Result<agent_client_protocol::ResumeSessionResponse, Error> {
        Ok(agent_client_protocol::ResumeSessionResponse::new())
    }

    async fn set_session_model(
        &self,
        _req: agent_client_protocol::SetSessionModelRequest,
    ) -> Result<agent_client_protocol::SetSessionModelResponse, Error> {
        Ok(agent_client_protocol::SetSessionModelResponse::new())
    }

    async fn ext_method(
        &self,
        _req: agent_client_protocol::ExtRequest,
    ) -> Result<agent_client_protocol::ExtResponse, Error> {
        let raw_value = serde_json::value::RawValue::from_string("null".to_string())
            .map_err(|e| Error::internal_error().data(e.to_string()))?;
        Ok(agent_client_protocol::ExtResponse::new(Arc::from(
            raw_value,
        )))
    }

    async fn ext_notification(
        &self,
        _notif: agent_client_protocol::ExtNotification,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
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
        store
            .expect_get_history()
            .returning(move |_| Ok((*history).clone()))
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

        let delegate_agent = Arc::new(StubDelegateAgent::new(behavior));
        let mut agent_registry = DefaultAgentRegistry::new();
        for id in ["agent", "agent1", "agent2"] {
            agent_registry.register(agent_info(id), delegate_agent.clone());
        }
        let agent_registry: Arc<DefaultAgentRegistry> = Arc::new(agent_registry);

        let event_bus = Arc::new(EventBus::new());
        let tool_registry = ToolRegistry::new();

        let config = Arc::new(AgentConfig {
            provider: provider_context,
            event_bus: event_bus.clone(),
            agent_registry: agent_registry.clone(),
            workspace_manager_actor: WorkspaceIndexManagerActor::new(
                WorkspaceIndexManagerConfig::default(),
            ),
            default_mode: Arc::new(std::sync::Mutex::new(AgentMode::Build)),
            tool_config: ToolConfig {
                policy: ToolPolicy::ProviderOnly,
                ..ToolConfig::default()
            },
            tool_registry: tool_registry.clone(),
            middleware_drivers: Vec::new(),
            auth_methods: Vec::new(),
            max_steps: None,
            snapshot_policy: SnapshotPolicy::None,
            assume_mutating: true,
            mutating_tools: HashSet::new(),
            max_prompt_bytes: None,
            execution_timeout_secs: 300,
            execution_policy: RuntimeExecutionPolicy::default(),
            compaction: crate::session::compaction::SessionCompaction::new(),
            snapshot_backend: None,
            snapshot_gc_config: crate::snapshot::GcConfig::default(),
            delegation_context_config: DelegationContextConfig {
                timing: DelegationContextTiming::FirstTurnOnly,
                auto_inject: true,
            },
            pending_elicitations: Arc::new(Mutex::new(HashMap::new())),
            mcp_servers: Vec::new(),
        });

        // Create an AgentHandle for delegation (wraps its own SessionRegistry)
        let delegator: Arc<dyn SendAgent> =
            Arc::new(crate::agent::AgentHandle::from_config(config.clone()));

        let orchestrator = Arc::new(DelegationOrchestrator::new(
            delegator,
            event_bus.clone(),
            store.clone(),
            agent_registry.clone(),
            Arc::new(tool_registry),
            None,
        ));
        config.add_observer(orchestrator);

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

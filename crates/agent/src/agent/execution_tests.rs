use crate::agent::core::{QueryMTAgent, SnapshotPolicy, ToolConfig, ToolPolicy};
use crate::agent::execution::CycleOutcome;
use crate::delegation::DefaultAgentRegistry;
use crate::event_bus::EventBus;
use crate::events::AgentEventKind;
use crate::session::provider::SessionContext;
use crate::session::runtime::RuntimeContext;
use crate::session::store::SessionStore;
use crate::test_utils::{
    MockChatResponse, MockLlmProvider, MockSessionStore, SharedLlmProvider, StopOnBeforeTurn,
    TestPluginLoader, TestProviderFactory, mock_llm_config, mock_querymt_tool_call, mock_session,
};
use crate::tools::ToolRegistry;
use agent_client_protocol::StopReason;
use mockall::Sequence;
use querymt::LLMParams;
use querymt::chat::{FunctionTool, Tool};
use querymt::error::LLMError;
use querymt::plugin::host::PluginRegistry;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use tempfile::TempDir;
use tokio::sync::{Mutex, oneshot, watch};

// Mock implementations moved to crate::test_utils::mocks

struct TestHarness {
    agent: QueryMTAgent,
    session_id: String,
    context: SessionContext,
    runtime_context: RuntimeContext,
    provider: Arc<Mutex<MockLlmProvider>>,
    _temp_dir: TempDir,
}

impl TestHarness {
    async fn new(
        history: Vec<crate::model::AgentMessage>,
        delegation_sender: Option<oneshot::Sender<String>>,
    ) -> Self {
        Self::new_with_tools(history, delegation_sender, Vec::new()).await
    }

    async fn new_with_tools(
        history: Vec<crate::model::AgentMessage>,
        delegation_sender: Option<oneshot::Sender<String>>,
        tools: Vec<Tool>,
    ) -> Self {
        let session_id = "sess-test".to_string();
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared_provider = SharedLlmProvider {
            inner: provider.clone(),
            tools: tools.into_boxed_slice(),
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
        let delegation_sender = Arc::new(StdMutex::new(delegation_sender));

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
            .returning(move |mut delegation| {
                if let Ok(mut sender) = delegation_sender.lock()
                    && let Some(tx) = sender.take()
                {
                    let _ = tx.send(delegation.public_id.clone());
                }
                // Assign a DB ID if not set
                if delegation.id == 0 {
                    delegation.id = 1;
                }
                Ok(delegation)
            })
            .times(0..);

        let store: Arc<dyn SessionStore> = Arc::new(store);
        let provider_context = crate::session::provider::SessionProvider::new(
            registry,
            store.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        );
        let provider_context = Arc::new(provider_context);

        let context = SessionContext::new(provider_context.clone(), session_for_context)
            .await
            .expect("context");

        let mut runtime_context = RuntimeContext::new(store.clone(), session_id.clone())
            .await
            .expect("runtime context");
        runtime_context
            .load_working_context()
            .await
            .expect("load context");

        let agent = QueryMTAgent {
            provider: provider_context,
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
            session_runtime: Arc::new(Mutex::new(HashMap::new())),
            max_steps: None,
            snapshot_root: None,
            snapshot_policy: SnapshotPolicy::None,
            assume_mutating: false,
            mutating_tools: HashSet::new(),
            max_prompt_bytes: None,
            tool_config: Arc::new(StdMutex::new(ToolConfig::default())),
            tool_registry: Arc::new(StdMutex::new(ToolRegistry::new())),
            middleware_drivers: Arc::new(std::sync::Mutex::new(Vec::new())),
            plan_mode_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            event_bus: Arc::new(EventBus::new()),
            client_state: Arc::new(StdMutex::new(None)),
            auth_methods: Arc::new(StdMutex::new(Vec::new())),
            client: Arc::new(StdMutex::new(None)),
            bridge: Arc::new(StdMutex::new(None)),
            agent_registry: Arc::new(DefaultAgentRegistry::new()),
            delegation_context_config: crate::agent::core::DelegationContextConfig {
                timing: crate::agent::core::DelegationContextTiming::FirstTurnOnly,
                auto_inject: true,
            },
            workspace_index_manager: Arc::new(crate::index::WorkspaceIndexManager::new(
                crate::index::WorkspaceIndexManagerConfig::default(),
            )),
        };

        if let Ok(mut config) = agent.tool_config.lock() {
            config.policy = ToolPolicy::ProviderOnly;
        }

        Self {
            agent,
            session_id,
            context,
            runtime_context,
            provider,
            _temp_dir: temp_dir,
        }
    }

    async fn run(&mut self, cancel_rx: watch::Receiver<bool>) -> CycleOutcome {
        self.agent
            .execute_cycle_state_machine(&self.context, None, &mut self.runtime_context, cancel_rx)
            .await
            .expect("state machine")
    }

    async fn provider_mut(&self) -> tokio::sync::MutexGuard<'_, MockLlmProvider> {
        self.provider.lock().await
    }
}

#[tokio::test]
async fn test_simple_completion_no_tools() {
    let mut harness = TestHarness::new(vec![], None).await;
    harness
        .provider_mut()
        .await
        .expect_chat()
        .returning(|_| Ok(Box::new(MockChatResponse::text_only("done"))))
        .times(1);
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let (_tx, rx) = watch::channel(false);
    let outcome = harness.run(rx).await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

#[tokio::test]
async fn test_provider_tools_passed_to_llm() {
    let tool = Tool {
        tool_type: "function".to_string(),
        function: FunctionTool {
            name: "remote_tool".to_string(),
            description: "test tool".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": [],
            }),
        },
    };
    let mut harness = TestHarness::new_with_tools(vec![], None, vec![tool.clone()]).await;
    let mut seq = Sequence::new();

    harness.provider_mut().await.expect_chat().times(0);
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_, tools| {
            let tools = tools.expect("tools provided");
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].function.name, tool.function.name);
            Ok(Box::new(MockChatResponse::text_only("done")))
        });

    let (_tx, rx) = watch::channel(false);
    let outcome = harness.run(rx).await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

#[tokio::test]
async fn test_single_tool_call_cycle() {
    let mut harness = TestHarness::new(vec![], None).await;
    let tool_call = mock_querymt_tool_call("call-1", "remote_tool", "{}");
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
                vec![tool_call.clone()],
            )))
        });
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(Box::new(MockChatResponse::text_only("done"))));
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Ok("tool output".to_string()))
        .times(1);
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let (_tx, rx) = watch::channel(false);
    let outcome = harness.run(rx).await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

#[tokio::test]
async fn test_multiple_tool_calls_batch() {
    let mut harness = TestHarness::new(vec![], None).await;
    let tool_calls = vec![
        mock_querymt_tool_call("call-1", "remote_tool", "{}"),
        mock_querymt_tool_call("call-2", "remote_tool", "{}"),
    ];
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
                tool_calls.clone(),
            )))
        });
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(Box::new(MockChatResponse::text_only("done"))));
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Ok("tool output".to_string()))
        .times(2);
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let (_tx, rx) = watch::channel(false);
    let outcome = harness.run(rx).await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

#[tokio::test]
async fn test_cancel_before_llm_call() {
    let mut harness = TestHarness::new(vec![], None).await;
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let (_tx, rx) = watch::channel(true);
    let outcome = harness.run(rx).await;

    assert_eq!(outcome, CycleOutcome::Cancelled);
}

#[tokio::test]
async fn test_llm_error_returns_err() {
    let mut harness = TestHarness::new(vec![], None).await;
    harness
        .provider_mut()
        .await
        .expect_chat()
        .returning(|_| Err(LLMError::ProviderError("boom".into())))
        .times(1);
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let (_tx, rx) = watch::channel(false);
    let result = harness
        .agent
        .execute_cycle_state_machine(&harness.context, None, &mut harness.runtime_context, rx)
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_middleware_stops_execution() {
    let mut harness = TestHarness::new(vec![], None).await;
    if let Ok(mut drivers) = harness.agent.middleware_drivers.lock() {
        drivers.push(Arc::new(StopOnBeforeTurn));
    }
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let (_tx, rx) = watch::channel(false);
    let outcome = harness.run(rx).await;

    assert_eq!(outcome, CycleOutcome::Stopped(StopReason::EndTurn));
}

#[tokio::test]
async fn test_tool_error_continues() {
    let mut harness = TestHarness::new(vec![], None).await;
    let tool_call = mock_querymt_tool_call("call-1", "remote_tool", "{}");
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
                vec![tool_call.clone()],
            )))
        });
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(Box::new(MockChatResponse::text_only("done"))));
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Err(LLMError::ProviderError("fail".into())))
        .times(1);
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let (_tx, rx) = watch::channel(false);
    let outcome = harness.run(rx).await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

#[tokio::test]
async fn test_waiting_for_event_delegation() {
    let (delegation_tx, delegation_rx) = oneshot::channel();
    let mut harness = TestHarness::new(vec![], Some(delegation_tx)).await;
    let tool_call = mock_querymt_tool_call(
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
                vec![tool_call.clone()],
            )))
        });
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(Box::new(MockChatResponse::text_only("done"))));
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

    let session_id = harness.session_id.clone();
    let event_bus = harness.agent.event_bus();
    tokio::spawn(async move {
        let delegation_id = delegation_rx.await.expect("delegation id");
        event_bus.publish(
            &session_id,
            AgentEventKind::DelegationCompleted {
                delegation_id,
                result: Some("done".to_string()),
            },
        );
    });

    let (_tx, rx) = watch::channel(false);
    let outcome = harness.run(rx).await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use mockall::Sequence;
use querymt::LLMParams;
use querymt::chat::{Content, FunctionTool, Tool};
use querymt::error::LLMError;
use serde_json::json;
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::sync::{Mutex, oneshot};

use crate::acp::protocol::StopReason;
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::ToolPolicy;
use crate::agent::execution::CycleOutcome;
use crate::agent::execution_context::ExecutionContext;
use crate::events::AgentEventKind;
use crate::session::backend::StorageBackend;
use crate::session::domain::{Task, TaskKind, TaskStatus};
use crate::session::provider::SessionHandle;
use crate::session::runtime::RuntimeContext;
use crate::session::store::SessionStore;
use crate::test_utils::{
    MockChatResponse, MockLlmProvider, MockSessionStore, SharedLlmProvider, StopOnBeforeLlmCall,
    TestProviderFactory, mock_llm_config, mock_plugin_registry, mock_querymt_tool_call,
    mock_session,
};
use crate::tools::{Tool as AgentTool, ToolContext, ToolError, ToolExecutionClass, ToolRegistry};

// Mock implementations moved to crate::test_utils::mocks

struct TestHarness {
    config: Arc<AgentConfig>,
    session_id: String,
    exec_ctx: ExecutionContext,
    provider: Arc<Mutex<MockLlmProvider>>,
    stored_messages: Arc<StdMutex<Vec<crate::model::AgentMessage>>>,
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
        let factory = Arc::new(TestProviderFactory::new(shared_provider));

        // Use shared helper to create mock plugin registry
        let (registry, temp_dir) = mock_plugin_registry(factory).expect("mock registry");
        let registry = Arc::new(registry);

        let mut store = MockSessionStore::new();
        let session = mock_session(&session_id);
        let session_for_context = session.clone();
        let session_for_expectation = session.clone();
        let llm_config = mock_llm_config();
        let history = Arc::new(history);
        let delegation_sender = Arc::new(StdMutex::new(delegation_sender));
        let stored_messages = Arc::new(StdMutex::new(Vec::new()));

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
            .expect_get_session_control()
            .returning(|_| Ok(None))
            .times(0..);
        store
            .expect_get_session_provider_node_id()
            .returning(|_| Ok(None))
            .times(0..);
        let stored_messages_for_mock = stored_messages.clone();
        store
            .expect_add_message()
            .returning(move |_, message| {
                stored_messages_for_mock.lock().unwrap().push(message);
                Ok(())
            })
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
            .expect_create_and_set_current_intent_snapshot()
            .returning(|_, mut snapshot| {
                snapshot.id = 1;
                Ok(snapshot)
            })
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

        let event_journal_storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create event journal storage"),
        );

        let config = Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
                event_journal_storage.clone(),
                provider_context,
                event_journal_storage.event_journal(),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .build(),
        );

        // Create a SessionRuntime for the execution context
        let session_runtime = crate::agent::core::SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );

        let exec_ctx = ExecutionContext::new(
            session_id.clone(),
            session_runtime,
            runtime_context,
            context,
            crate::agent::core::ToolConfig::default(),
        );

        Self {
            config,
            session_id,
            exec_ctx,
            provider,
            stored_messages,
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

    fn with_builtin_tools(&mut self, tools: Vec<Arc<dyn AgentTool>>) {
        let mut registry = ToolRegistry::new();
        registry.extend(tools);
        self.config = Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
                self.config.storage.clone(),
                self.config.provider.clone(),
                self.config.event_sink.journal().clone(),
            )
            .with_tool_policy(ToolPolicy::BuiltInOnly)
            .with_tool_registry(registry)
            .build(),
        );
        self.exec_ctx.tool_config.policy = ToolPolicy::BuiltInOnly;
    }

    fn enable_task_completion_guard(&mut self, enabled: bool) {
        let mut policy = self.config.execution_policy.clone();
        policy.task_completion_guard = enabled;
        self.config = Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
                self.config.storage.clone(),
                self.config.provider.clone(),
                self.config.event_sink.journal().clone(),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .with_execution_policy(policy)
            .build(),
        );
    }

    fn set_task(&mut self, status: TaskStatus, kind: TaskKind) {
        let now = OffsetDateTime::now_utc();
        self.exec_ctx.state.active_task = Some(Task {
            id: 1,
            public_id: "task-1".to_string(),
            session_id: 1,
            kind,
            status,
            expected_deliverable: Some("finish work".to_string()),
            acceptance_criteria: None,
            revision: 1,
            creation_key: None,
            completion_evidence: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
        });
    }
}

struct SchedulingTool {
    name: &'static str,
    class: ToolExecutionClass,
    delay_ms: u64,
    result: &'static str,
    log: Arc<StdMutex<Vec<String>>>,
    cancel_on_start: bool,
}

#[async_trait]
impl AgentTool for SchedulingTool {
    fn name(&self) -> &str {
        self.name
    }

    fn definition(&self) -> Tool {
        provider_tool(self.name)
    }

    fn execution_class(&self) -> ToolExecutionClass {
        self.class
    }

    async fn call(
        &self,
        _args: serde_json::Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<Content>, ToolError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("start:{}", self.name));
        if self.cancel_on_start {
            context.cancellation_token().cancel();
        }
        if self.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        }
        self.log.lock().unwrap().push(format!("end:{}", self.name));
        Ok(vec![Content::text(self.result)])
    }
}

fn scheduling_tool(
    name: &'static str,
    class: ToolExecutionClass,
    delay_ms: u64,
    result: &'static str,
    log: &Arc<StdMutex<Vec<String>>>,
) -> Arc<dyn AgentTool> {
    Arc::new(SchedulingTool {
        name,
        class,
        delay_ms,
        result,
        log: log.clone(),
        cancel_on_start: false,
    })
}

fn provider_tool(name: &str) -> Tool {
    Tool {
        tool_type: "function".to_string(),
        function: FunctionTool {
            name: name.to_string(),
            description: "test tool".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        },
    }
}

async fn run_completion_guard_case(
    policy_enabled: bool,
    tools: Vec<Tool>,
    status: TaskStatus,
    kind: TaskKind,
    expected_requests: usize,
) {
    let mut harness = TestHarness::new_with_tools(vec![], None, tools).await;
    harness.enable_task_completion_guard(policy_enabled);
    harness.set_task(status, kind);
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .returning(|_, _| Ok(Box::new(MockChatResponse::text_only("done"))))
        .times(expected_requests);

    assert_eq!(harness.run().await, CycleOutcome::Completed);
}

#[tokio::test]
async fn completion_guard_continues_exactly_once_in_execution_flow() {
    run_completion_guard_case(
        true,
        vec![provider_tool("complete_task")],
        TaskStatus::Active,
        TaskKind::Finite,
        2,
    )
    .await;
}

#[tokio::test]
async fn completion_guard_does_not_continue_when_disabled() {
    run_completion_guard_case(
        false,
        vec![provider_tool("complete_task")],
        TaskStatus::Active,
        TaskKind::Finite,
        1,
    )
    .await;
}

#[tokio::test]
async fn completion_guard_does_not_continue_without_complete_task() {
    run_completion_guard_case(
        true,
        vec![provider_tool("update_task")],
        TaskStatus::Active,
        TaskKind::Finite,
        1,
    )
    .await;
}

#[tokio::test]
async fn completion_guard_does_not_continue_for_non_finite_task() {
    for kind in [TaskKind::Recurring, TaskKind::Evolving] {
        run_completion_guard_case(
            true,
            vec![provider_tool("complete_task")],
            TaskStatus::Active,
            kind,
            1,
        )
        .await;
    }
}

#[tokio::test]
async fn completion_guard_does_not_continue_for_non_active_task() {
    for status in [TaskStatus::Done, TaskStatus::Paused, TaskStatus::Cancelled] {
        run_completion_guard_case(
            true,
            vec![provider_tool("complete_task")],
            status,
            TaskKind::Finite,
            1,
        )
        .await;
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

    let outcome = harness.run().await;

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

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

/// Parallel tool calls must produce a single User message with all tool_result
/// blocks rather than separate messages per result. The Anthropic API requires
/// every `tool_use` id in an assistant message to have a matching `tool_result`
/// in the *immediately following* user message; splitting results across
/// multiple consecutive user messages violates this constraint.
#[tokio::test]
async fn test_parallel_tool_results_in_single_user_message() {
    let mut harness = TestHarness::new(vec![], None).await;
    let tool_calls = vec![
        mock_querymt_tool_call("call-1", "remote_tool", r#"{"a":1}"#),
        mock_querymt_tool_call("call-2", "remote_tool", r#"{"b":2}"#),
        mock_querymt_tool_call("call-3", "remote_tool", r#"{"c":3}"#),
    ];

    let seen_history = Arc::new(StdMutex::new(None::<Vec<querymt::chat::ChatMessage>>));
    let seen_history_clone = seen_history.clone();
    let mut seq = Sequence::new();

    // First LLM call returns 3 parallel tool calls.
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| {
            Ok(Box::new(MockChatResponse::with_tools(
                "thinking",
                tool_calls.clone(),
            )))
        });
    // Second LLM call — capture the history to assert on message structure.
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |messages| {
            *seen_history_clone.lock().unwrap() = Some(messages.to_vec());
            Ok(Box::new(MockChatResponse::text_only("done")))
        });
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| Ok(vec![Content::text("tool output")]))
        .times(3);
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let outcome = harness.run().await;
    assert_eq!(outcome, CycleOutcome::Completed);

    let history = seen_history
        .lock()
        .unwrap()
        .clone()
        .expect("second chat should capture history");

    // Count User messages that contain at least one ToolResult block.
    let user_tool_result_messages: Vec<_> = history
        .iter()
        .filter(|msg| {
            msg.role == querymt::chat::ChatRole::User
                && msg.content.iter().any(|b| b.is_tool_result())
        })
        .collect();

    // All 3 tool results must be in a SINGLE user message, not 3 separate ones.
    assert_eq!(
        user_tool_result_messages.len(),
        1,
        "expected 1 user message with all tool results, got {}",
        user_tool_result_messages.len()
    );

    // That single message should contain exactly 3 ToolResult blocks.
    let tool_result_count = user_tool_result_messages[0]
        .content
        .iter()
        .filter(|b| b.is_tool_result())
        .count();
    assert_eq!(
        tool_result_count, 3,
        "expected 3 tool results in the single user message, got {}",
        tool_result_count
    );
}

#[tokio::test]
async fn mixed_scheduler_runs_contiguous_parallel_groups_before_stateful_boundaries() {
    let mut harness = TestHarness::new(vec![], None).await;
    let log = Arc::new(StdMutex::new(Vec::new()));
    harness.with_builtin_tools(vec![
        scheduling_tool("parallel_a", ToolExecutionClass::ParallelSafe, 5, "a", &log),
        scheduling_tool(
            "parallel_b",
            ToolExecutionClass::ParallelSafe,
            30,
            "b",
            &log,
        ),
        scheduling_tool("serial", ToolExecutionClass::SerialStateful, 0, "s", &log),
        scheduling_tool("parallel_c", ToolExecutionClass::ParallelSafe, 0, "c", &log),
    ]);
    let calls = vec![
        mock_querymt_tool_call("call-a", "parallel_a", "{}"),
        mock_querymt_tool_call("call-b", "parallel_b", "{}"),
        mock_querymt_tool_call("call-s", "serial", "{}"),
        mock_querymt_tool_call("call-c", "parallel_c", "{}"),
    ];
    let mut seq = Sequence::new();
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_, _| Ok(Box::new(MockChatResponse::with_tools("", calls.clone()))));
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| Ok(Box::new(MockChatResponse::text_only("done"))));

    assert_eq!(harness.run().await, CycleOutcome::Completed);
    let log = log.lock().unwrap().clone();
    let position = |entry: &str| log.iter().position(|item| item == entry).unwrap();
    assert!(position("start:parallel_b") < position("end:parallel_a"));
    assert!(position("end:parallel_a") < position("start:serial"));
    assert!(position("end:parallel_b") < position("start:serial"));
    assert!(position("end:serial") < position("start:parallel_c"));
}

#[tokio::test]
async fn mixed_scheduler_skips_suffix_after_clarification_boundary() {
    let mut harness = TestHarness::new(vec![], None).await;
    let log = Arc::new(StdMutex::new(Vec::new()));
    harness.with_builtin_tools(vec![
        scheduling_tool(
            "clarify",
            ToolExecutionClass::ClarificationBoundary,
            0,
            "use option b",
            &log,
        ),
        scheduling_tool(
            "suffix",
            ToolExecutionClass::ParallelSafe,
            0,
            "suffix",
            &log,
        ),
    ]);
    let calls = vec![
        mock_querymt_tool_call("call-q", "clarify", "{}"),
        mock_querymt_tool_call("call-suffix", "suffix", "{}"),
    ];
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .returning(move |_, _| Ok(Box::new(MockChatResponse::with_tools("", calls.clone()))))
        .times(1);
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .returning(|_, _| Ok(Box::new(MockChatResponse::text_only("done"))))
        .times(1);
    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
    assert!(
        !log.lock()
            .unwrap()
            .iter()
            .any(|entry| entry.contains("suffix"))
    );
}

#[tokio::test]
async fn mixed_scheduler_cancellation_stores_one_result_per_call() {
    let mut harness = TestHarness::new(vec![], None).await;
    let log = Arc::new(StdMutex::new(Vec::new()));
    let cancelling: Arc<dyn AgentTool> = Arc::new(SchedulingTool {
        name: "cancel",
        class: ToolExecutionClass::SerialStateful,
        delay_ms: 0,
        result: "cancelled",
        log: log.clone(),
        cancel_on_start: true,
    });
    harness.with_builtin_tools(vec![
        cancelling,
        scheduling_tool("after_a", ToolExecutionClass::ParallelSafe, 0, "a", &log),
        scheduling_tool("after_b", ToolExecutionClass::ParallelSafe, 0, "b", &log),
    ]);
    let calls = vec![
        mock_querymt_tool_call("call-cancel", "cancel", "{}"),
        mock_querymt_tool_call("call-a", "after_a", "{}"),
        mock_querymt_tool_call("call-b", "after_b", "{}"),
    ];
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .returning(move |_, _| Ok(Box::new(MockChatResponse::with_tools("", calls.clone()))))
        .times(1);

    assert_eq!(harness.run().await, CycleOutcome::Cancelled);
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.starts_with("start:"))
            .count(),
        1
    );
    let stored = harness.stored_messages.lock().unwrap();
    let result_call_ids: BTreeSet<_> = stored
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match part {
            crate::model::MessagePart::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        result_call_ids,
        BTreeSet::from(["call-a", "call-b", "call-cancel"])
    );
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
        .returning(|_, _| Ok(vec![querymt::chat::Content::text("tool output")]))
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
        .returning(|_, _| Ok(vec![querymt::chat::Content::text("tool output")]))
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
async fn test_cancel_before_llm_call() {
    let mut harness = TestHarness::new(vec![], None).await;
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    harness.exec_ctx.cancellation_token.cancel();
    let outcome = harness.run().await;

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

    let result = crate::agent::execution::execute_cycle_state_machine(
        &harness.config,
        &mut harness.exec_ctx,
        None,
        crate::agent::core::AgentMode::Build,
    )
    .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_middleware_stops_execution() {
    let mut harness = TestHarness::new(vec![], None).await;
    // Rebuild config with the StopOnBeforeLlmCall middleware
    harness.config = Arc::new(
        crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
            harness.config.storage.clone(),
            harness.config.provider.clone(),
            harness.config.event_sink.journal().clone(),
        )
        .with_tool_policy(ToolPolicy::ProviderOnly)
        .with_middleware(StopOnBeforeLlmCall)
        .build(),
    );
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let outcome = harness.run().await;

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

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

#[tokio::test]
async fn test_tool_binary_output_survives_follow_up_turn_until_compaction() {
    let mut harness = TestHarness::new(vec![], None).await;
    let tool_call = mock_querymt_tool_call("call-1", "remote_tool", "{}");
    let seen_history = Arc::new(StdMutex::new(None::<Vec<querymt::chat::ChatMessage>>));
    let seen_history_clone = seen_history.clone();
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
        .returning(move |messages| {
            *seen_history_clone.lock().unwrap() = Some(messages.to_vec());
            Ok(Box::new(MockChatResponse::text_only("done")))
        });
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![
                Content::image("image/png", vec![0u8; 32]),
                Content::pdf(vec![1u8; 64]),
                Content::text("small text"),
            ])
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

    let history = seen_history
        .lock()
        .unwrap()
        .clone()
        .expect("second chat should capture history");
    let tool_result_message = history
        .iter()
        .find(|msg| {
            msg.content
                .iter()
                .any(|block| matches!(block, Content::ToolResult { .. }))
        })
        .expect("history should contain tool result message");

    let tool_result_content = tool_result_message
        .content
        .iter()
        .find_map(|block| match block {
            Content::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .expect("tool result block should exist");

    assert!(matches!(&tool_result_content[0], Content::Image { .. }));
    assert!(matches!(&tool_result_content[1], Content::Pdf { .. }));
    assert!(matches!(&tool_result_content[2], Content::Text { text } if text == "small text"));
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
        .returning(|_, _| Ok(vec![querymt::chat::Content::text("ok")]))
        .times(1);
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let session_id = harness.session_id.clone();
    let config = harness.config.clone();
    tokio::spawn(async move {
        let delegation_id = delegation_rx.await.expect("delegation id");
        config.emit_event(
            &session_id,
            AgentEventKind::DelegationCompleted {
                delegation_id,
                result: Some("done".to_string()),
            },
        );
    });

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
}

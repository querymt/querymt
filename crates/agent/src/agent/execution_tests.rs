use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use mockall::Sequence;
use querymt::LLMParams;
use querymt::chat::{
    ChatFunctionCallItem, ChatOutput, ChatOutputItem, ChatOutputProvenance, ChatOutputStatus,
    ChatReasoningItem, ChatReasoningPart, FinishReason, FunctionTool, Tool, ToolResultPart,
};
use querymt::error::LLMError;
use serde_json::json;
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::sync::{Mutex, oneshot};

use crate::acp::protocol::{ContentBlock, ImageContent, StopReason};
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
    history_writes: Arc<AtomicUsize>,
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
        let history_writes = Arc::new(AtomicUsize::new(0));
        let stored_messages_for_mock = stored_messages.clone();
        let history_writes_for_mock = history_writes.clone();
        store
            .expect_add_message()
            .returning(move |_, message| {
                stored_messages_for_mock.lock().unwrap().push(message);
                history_writes_for_mock.fetch_add(1, Ordering::SeqCst);
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
            history_writes,
            _temp_dir: temp_dir,
        }
    }

    async fn run(&mut self) -> CycleOutcome {
        crate::agent::execution::execute_cycle_state_machine(
            &self.config,
            &mut self.exec_ctx,
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

    /// Rebuild the config with one extra middleware and a stop-recovery limit.
    fn install_stop_recovery(
        &mut self,
        middleware: impl crate::middleware::MiddlewareDriver + 'static,
        max_stop_recoveries: u8,
    ) {
        let mut policy = self.config.execution_policy.clone();
        policy.max_stop_recoveries = max_stop_recoveries;
        self.config = Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
                self.config.storage.clone(),
                self.config.provider.clone(),
                self.config.event_sink.journal().clone(),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .with_execution_policy(policy)
            .with_middleware(middleware)
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
    /// File this tool claims to write (drives conflict-aware scheduling).
    write_path: Option<std::path::PathBuf>,
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

    fn file_access(
        &self,
        _args: &serde_json::Value,
        _cwd: Option<&std::path::Path>,
    ) -> crate::tools::FileAccess {
        match &self.write_path {
            Some(path) => crate::tools::FileAccess::writes([path.clone()]),
            None => crate::tools::FileAccess::None,
        }
    }

    async fn call(
        &self,
        _args: serde_json::Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
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
        Ok(vec![ToolResultPart::text(self.result)])
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
        write_path: None,
    })
}

/// A `ParallelSafe` tool that declares a write to `path`.
fn writing_tool(
    name: &'static str,
    path: &std::path::Path,
    delay_ms: u64,
    cancel_on_start: bool,
    log: &Arc<StdMutex<Vec<String>>>,
) -> Arc<dyn AgentTool> {
    Arc::new(SchedulingTool {
        name,
        class: ToolExecutionClass::ParallelSafe,
        delay_ms,
        result: "ok",
        log: log.clone(),
        cancel_on_start,
        write_path: Some(path.to_path_buf()),
    })
}

fn provider_tool(name: &str) -> Tool {
    Tool {
        tool_type: "function".to_string(),
        function: FunctionTool {
            name: name.to_string(),
            description: "test tool".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
            strict: None,
        },
    }
}

/// Stops at step start (for the first `stop_steps` steps) with a model token
/// limit, and recovers every stop it is offered.
struct StopThenRecover {
    stop_steps: usize,
    steps: AtomicUsize,
    recoveries_offered: AtomicUsize,
}

impl StopThenRecover {
    fn new(stop_steps: usize) -> Self {
        Self {
            stop_steps,
            steps: AtomicUsize::new(0),
            recoveries_offered: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl crate::middleware::MiddlewareDriver for StopThenRecover {
    async fn on_step_start(
        &self,
        state: crate::middleware::ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> crate::middleware::Result<crate::middleware::ExecutionState> {
        let step = self.steps.fetch_add(1, Ordering::SeqCst);
        match state {
            crate::middleware::ExecutionState::BeforeLlmCall { context }
                if step < self.stop_steps =>
            {
                Ok(crate::middleware::ExecutionState::Stopped {
                    message: "cut off".into(),
                    stop_type: crate::events::StopType::ModelTokenLimit,
                    context: Some(context),
                })
            }
            other => Ok(other),
        }
    }

    async fn on_stopped(
        &self,
        state: crate::middleware::ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> crate::middleware::Result<crate::middleware::ExecutionState> {
        self.recoveries_offered.fetch_add(1, Ordering::SeqCst);
        match state {
            crate::middleware::ExecutionState::Stopped {
                context: Some(context),
                ..
            } => Ok(crate::middleware::ExecutionState::BeforeLlmCall { context }),
            other => Ok(other),
        }
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "StopThenRecover"
    }
}

/// Count `MiddlewareStopped` events currently buffered on `events`.
async fn drain_middleware_stopped(
    events: &mut tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
) -> usize {
    let mut stopped = 0;
    while let Ok(Ok(envelope)) =
        tokio::time::timeout(Duration::from_millis(50), events.recv()).await
    {
        stopped += usize::from(matches!(
            envelope.kind(),
            AgentEventKind::MiddlewareStopped { .. }
        ));
    }
    stopped
}

#[tokio::test]
async fn recovering_middleware_suppresses_the_stop_and_the_run_continues() {
    let mut harness = TestHarness::new(vec![], None).await;
    let middleware = Arc::new(StopThenRecover::new(1));
    harness.install_stop_recovery(SharedDriver(middleware.clone()), 5);
    harness
        .provider_mut()
        .await
        .expect_chat()
        .returning(|_| Ok(MockChatResponse::text_only("done").into()))
        .times(1);
    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);
    let mut events = harness.config.subscribe_events();

    let outcome = harness.run().await;

    assert_eq!(outcome, CycleOutcome::Completed);
    assert_eq!(middleware.recoveries_offered.load(Ordering::SeqCst), 1);
    assert_eq!(
        drain_middleware_stopped(&mut events).await,
        0,
        "a recovered stop must not be published"
    );
}

#[tokio::test]
async fn recovery_guard_publishes_the_stop_after_the_limit() {
    let mut harness = TestHarness::new(vec![], None).await;
    // Stops on every step and recovers every time it is asked.
    let middleware = Arc::new(StopThenRecover::new(usize::MAX));
    harness.install_stop_recovery(SharedDriver(middleware.clone()), 2);
    let mut events = harness.config.subscribe_events();

    let outcome = harness.run().await;

    assert!(
        matches!(
            &outcome,
            CycleOutcome::Stopped {
                stop_reason: StopReason::MaxTokens,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        middleware.recoveries_offered.load(Ordering::SeqCst),
        2,
        "middleware is consulted at most max_stop_recoveries times"
    );
    assert_eq!(
        middleware.steps.load(Ordering::SeqCst),
        3,
        "the initial attempt plus one per recovery"
    );
    assert_eq!(drain_middleware_stopped(&mut events).await, 1);
}

#[tokio::test]
async fn stop_without_a_recovering_middleware_is_published_immediately() {
    let mut harness = TestHarness::new(vec![], None).await;
    harness.install_stop_recovery(StopOnBeforeLlmCall, 5);
    let mut events = harness.config.subscribe_events();

    let outcome = harness.run().await;

    assert!(matches!(outcome, CycleOutcome::Stopped { .. }));
    assert_eq!(drain_middleware_stopped(&mut events).await, 1);
}

/// Lets a test keep a handle on a middleware installed by value.
struct SharedDriver<T>(Arc<T>);

#[async_trait]
impl<T: crate::middleware::MiddlewareDriver> crate::middleware::MiddlewareDriver
    for SharedDriver<T>
{
    async fn on_step_start(
        &self,
        state: crate::middleware::ExecutionState,
        runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> crate::middleware::Result<crate::middleware::ExecutionState> {
        self.0.on_step_start(state, runtime).await
    }

    async fn on_stopped(
        &self,
        state: crate::middleware::ExecutionState,
        runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> crate::middleware::Result<crate::middleware::ExecutionState> {
        self.0.on_stopped(state, runtime).await
    }

    fn reset(&self) {
        self.0.reset();
    }

    fn name(&self) -> &'static str {
        self.0.name()
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
        .returning(|_, _| Ok(MockChatResponse::text_only("done").into()))
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
async fn invalid_steering_conversion_does_not_persist_history() {
    let mut harness = TestHarness::new(vec![], None).await;
    let inbox = Arc::new(crate::agent::turn_control::SteeringInbox::new(
        "run-1".to_string(),
    ));
    inbox
        .push(
            "input-1".to_string(),
            None,
            vec![ContentBlock::Image(ImageContent::new("%%%", "image/png"))],
        )
        .await
        .expect("direct inbox insertion bypasses admission validation");
    harness.exec_ctx = harness.exec_ctx.with_steering(inbox);
    let context = Arc::new(crate::middleware::ConversationContext::new(
        Arc::from(harness.session_id.as_str()),
        Arc::from([]),
        Arc::new(crate::middleware::AgentStats::default()),
        Arc::from("mock"),
        Arc::from("mock-model"),
    ));

    let result = crate::agent::execution::apply_pending_steering(
        &harness.config,
        &mut harness.exec_ctx,
        &context,
        "test",
        false,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(harness.history_writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_simple_completion_no_tools() {
    let mut harness = TestHarness::new(vec![], None).await;
    harness
        .provider_mut()
        .await
        .expect_chat()
        .returning(|_| Ok(MockChatResponse::text_only("done").into()))
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
            strict: None,
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
            Ok(MockChatResponse::text_only("done").into())
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
        .returning(
            move |_| Ok(MockChatResponse::with_tools("thinking", tool_calls.clone()).into()),
        );
    // Second LLM call — capture the history to assert on message structure.
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |messages| {
            *seen_history_clone.lock().unwrap() = Some(messages.to_vec());
            Ok(MockChatResponse::text_only("done").into())
        });
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "tool output".to_string(),
            }])
        })
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

    // Count User messages that contain at least one tool result input part.
    let user_tool_result_messages: Vec<_> = history
        .iter()
        .filter(|msg| msg.role == querymt::chat::ChatRole::User && msg.has_tool_result())
        .collect();

    // All 3 tool results must be in a SINGLE user message, not 3 separate ones.
    assert_eq!(
        user_tool_result_messages.len(),
        1,
        "expected 1 user message with all tool results, got {}",
        user_tool_result_messages.len()
    );

    // That single message should contain exactly 3 tool result parts.
    let tool_result_count = user_tool_result_messages[0]
        .portable_input_parts()
        .iter()
        .filter(|p| p.is_tool_result())
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
        .returning(move |_, _| Ok(MockChatResponse::with_tools("", calls.clone()).into()));
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| Ok(MockChatResponse::text_only("done").into()));

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
        .returning(move |_, _| Ok(MockChatResponse::with_tools("", calls.clone()).into()))
        .times(1);
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .returning(|_, _| Ok(MockChatResponse::text_only("done").into()))
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
        write_path: None,
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
        .returning(move |_, _| Ok(MockChatResponse::with_tools("", calls.clone()).into()))
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

/// Queue one provider response carrying `calls`, then a final text response.
async fn expect_tool_batch_then_done(
    harness: &TestHarness,
    calls: Vec<querymt::ToolCall>,
    final_expected: bool,
) {
    let mut seq = Sequence::new();
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_, _| Ok(MockChatResponse::with_tools("", calls.clone()).into()));
    if final_expected {
        harness
            .provider_mut()
            .await
            .expect_chat_with_tools()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _| Ok(MockChatResponse::text_only("done").into()));
    }
}

/// `(call_id, is_error)` of every stored tool result, in storage order.
fn stored_tool_results(harness: &TestHarness) -> Vec<(String, bool)> {
    harness
        .stored_messages
        .lock()
        .unwrap()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match part {
            crate::model::MessagePart::ToolResult {
                call_id, is_error, ..
            } => Some((call_id.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn edit_args(path: &std::path::Path, old: &str, new: &str) -> String {
    json!({
        "filePath": path.to_str().unwrap(),
        "oldString": old,
        "newString": new,
    })
    .to_string()
}

/// Regression for the lost-update incident: several real `edit` calls on one
/// file in a single response must all persist.
#[tokio::test]
async fn same_file_edits_in_one_batch_all_persist() {
    let mut harness = TestHarness::new(vec![], None).await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("design.md");
    let lines: Vec<String> = (0..8).map(|i| format!("line-{i}")).collect();
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    harness.with_builtin_tools(vec![Arc::new(crate::tools::builtins::EditTool::new())]);
    let calls: Vec<_> = (0..8)
        .map(|i| {
            mock_querymt_tool_call(
                &format!("call-{i}"),
                "edit",
                &edit_args(&path, &format!("line-{i}"), &format!("edited-{i}")),
            )
        })
        .collect();
    expect_tool_batch_then_done(&harness, calls, true).await;

    assert_eq!(harness.run().await, CycleOutcome::Completed);

    let content = std::fs::read_to_string(&path).unwrap();
    for i in 0..8 {
        assert!(
            content.contains(&format!("edited-{i}")),
            "edit {i} was lost: {content}"
        );
        assert!(!content.contains(&format!("line-{i}\n")));
    }
    let results = stored_tool_results(&harness);
    let expected: Vec<_> = (0..8).map(|i| (format!("call-{i}"), false)).collect();
    assert_eq!(results, expected, "results must be in call order, all ok");
}

/// Edit B's `oldString` only exists once edit A has been applied, so the batch
/// only succeeds if A runs to completion before B starts.
#[tokio::test]
async fn dependent_same_file_edits_run_in_model_order() {
    let mut harness = TestHarness::new(vec![], None).await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.md");
    std::fs::write(&path, "alpha\n").unwrap();

    harness.with_builtin_tools(vec![Arc::new(crate::tools::builtins::EditTool::new())]);
    let calls = vec![
        mock_querymt_tool_call("call-a", "edit", &edit_args(&path, "alpha", "beta")),
        mock_querymt_tool_call("call-b", "edit", &edit_args(&path, "beta", "gamma")),
    ];
    expect_tool_batch_then_done(&harness, calls, true).await;

    assert_eq!(harness.run().await, CycleOutcome::Completed);

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "gamma\n");
    assert_eq!(
        stored_tool_results(&harness),
        vec![("call-a".to_string(), false), ("call-b".to_string(), false)]
    );
}

/// End to end: one response with two `edit` calls on the same file. Both edits
/// persist, and the next model request carries the warn-mode guidance after the
/// tool results (never between a tool call and its result).
#[tokio::test]
async fn same_file_edits_persist_and_next_request_carries_warn_guidance() {
    let mut harness = TestHarness::new(vec![], None).await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("design.md");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();

    harness.with_builtin_tools(vec![Arc::new(crate::tools::builtins::EditTool::new())]);
    let guard = Arc::new(crate::middleware::ParallelEditGuard::new(
        harness.config.tool_registry.clone(),
        crate::middleware::ParallelEditMode::Warn,
    ));
    harness.config = Arc::new((*harness.config).clone().with_middleware(vec![guard]));

    let calls = vec![
        mock_querymt_tool_call("call-1", "edit", &edit_args(&path, "alpha", "ALPHA")),
        mock_querymt_tool_call("call-2", "edit", &edit_args(&path, "beta", "BETA")),
    ];
    // (role-is-user, has_tool_result, text) of every message in the 2nd request.
    let second_request: Arc<StdMutex<Vec<(bool, bool, String)>>> =
        Arc::new(StdMutex::new(Vec::new()));
    let captured = second_request.clone();
    let mut seq = Sequence::new();
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_, _| Ok(MockChatResponse::with_tools("", calls.clone()).into()));
    harness
        .provider_mut()
        .await
        .expect_chat_with_tools()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |messages, _| {
            *captured.lock().unwrap() = messages
                .iter()
                .map(|m| {
                    (
                        matches!(m.role, querymt::chat::ChatRole::User),
                        m.has_tool_result(),
                        m.text(),
                    )
                })
                .collect();
            Ok(MockChatResponse::text_only("done").into())
        });

    assert_eq!(harness.run().await, CycleOutcome::Completed);

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ALPHA\nBETA\n");
    assert_eq!(
        stored_tool_results(&harness),
        vec![("call-1".to_string(), false), ("call-2".to_string(), false)]
    );

    let request = second_request.lock().unwrap();
    let last_result = request
        .iter()
        .rposition(|(_, has_result, _)| *has_result)
        .expect("second request carries the tool results");
    let note = request
        .iter()
        .position(|(is_user, has_result, text)| {
            *is_user && !*has_result && text.contains("design.md") && text.contains("multiedit")
        })
        .expect("second request carries the warn guidance");
    assert!(
        note > last_result,
        "guidance must come after the tool results: {request:?}"
    );
}

/// Calls on different files, and calls from tools that declare no access, are
/// not serialized behind a conflicting pair.
#[tokio::test]
async fn non_conflicting_writes_still_overlap_while_conflicting_ones_serialize() {
    let mut harness = TestHarness::new(vec![], None).await;
    let log = Arc::new(StdMutex::new(Vec::new()));
    let shared = std::path::PathBuf::from("/ws/shared.md");
    let other = std::path::PathBuf::from("/ws/other.md");
    harness.with_builtin_tools(vec![
        writing_tool("w1", &shared, 30, false, &log),
        writing_tool("w2", &shared, 0, false, &log),
        writing_tool("w3", &other, 30, false, &log),
    ]);
    let calls = vec![
        mock_querymt_tool_call("call-1", "w1", "{}"),
        mock_querymt_tool_call("call-2", "w2", "{}"),
        mock_querymt_tool_call("call-3", "w3", "{}"),
    ];
    expect_tool_batch_then_done(&harness, calls, true).await;

    assert_eq!(harness.run().await, CycleOutcome::Completed);

    let log = log.lock().unwrap().clone();
    let position = |entry: &str| log.iter().position(|item| item == entry).unwrap();
    // Same file: w2 starts only after w1 finished.
    assert!(position("end:w1") < position("start:w2"));
    // Different file: w3 overlaps with w1.
    assert!(position("start:w3") < position("end:w1"));
}

/// A call waiting on a conflicting earlier call still gets exactly one
/// (cancelled) result when the user cancels, and never starts.
#[tokio::test]
async fn cancellation_while_conflicting_call_waits_yields_one_result_per_call() {
    let mut harness = TestHarness::new(vec![], None).await;
    let log = Arc::new(StdMutex::new(Vec::new()));
    let shared = std::path::PathBuf::from("/ws/shared.md");
    harness.with_builtin_tools(vec![
        writing_tool("first", &shared, 0, true, &log),
        writing_tool("second", &shared, 0, false, &log),
        writing_tool("third", &shared, 0, false, &log),
    ]);
    let calls = vec![
        mock_querymt_tool_call("call-1", "first", "{}"),
        mock_querymt_tool_call("call-2", "second", "{}"),
        mock_querymt_tool_call("call-3", "third", "{}"),
    ];
    expect_tool_batch_then_done(&harness, calls, false).await;

    assert_eq!(harness.run().await, CycleOutcome::Cancelled);

    let started: Vec<_> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with("start:"))
        .cloned()
        .collect();
    assert_eq!(started, vec!["start:first".to_string()]);

    let results = stored_tool_results(&harness);
    let ids: Vec<_> = results.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["call-1", "call-2", "call-3"], "one result per call");
    // The calls that were still waiting report cancellation.
    assert!(results[1].1 && results[2].1);
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
        .returning(move |_| Ok(MockChatResponse::with_tools("", vec![tool_call.clone()]).into()));
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(MockChatResponse::text_only("done").into()));
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "tool output".to_string(),
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

/// Same-turn tool loop must keep encrypted reasoning on the second request.
///
/// `store=false` Responses continuation needs `encrypted_content` on the
/// assistant turn that is actually sent after the tool result. The live loop
/// already has that output; converting it with no target portable-projects it
/// away before `BeforeLlmCall`.
#[tokio::test]
async fn same_turn_tool_loop_keeps_encrypted_reasoning_on_second_request() {
    let mut harness = TestHarness::new(vec![], None).await;
    let mut seq = Sequence::new();
    let second_request = Arc::new(StdMutex::new(Vec::new()));
    let second_request_for_mock = second_request.clone();

    const ENCRYPTED: &str = "encrypted-continuation";
    let first_output = ChatOutput {
        response_id: Some("resp_loop".into()),
        status: Some(ChatOutputStatus::Completed),
        finish_reason: Some(FinishReason::ToolCalls),
        provenance: Some(ChatOutputProvenance {
            provider: "mock".into(),
            protocol: "responses".into(),
            model: "mock-model".into(),
            endpoint: "https://api.openai.com/v1/responses".into(),
        }),
        items: vec![
            ChatOutputItem::Reasoning(ChatReasoningItem {
                id: Some("rs_1".into()),
                summary: vec![ChatReasoningPart::text("thinking")],
                content: Vec::new(),
                encrypted_content: Some(ENCRYPTED.into()),
                signature: Some("sig".into()),
                status: None,
                extensions: Default::default(),
            }),
            ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                item_id: Some("fc_1".into()),
                call_id: "call-1".into(),
                name: "remote_tool".into(),
                arguments: "{}".into(),
                status: None,
                extensions: Default::default(),
            }),
        ],
        ..ChatOutput::default()
    };

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |_| Ok(first_output.clone()));
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |messages| {
            *second_request_for_mock.lock().unwrap() = messages.to_vec();
            Ok(MockChatResponse::text_only("done").into())
        });
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "tool output".to_string(),
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

    let second_request = second_request.lock().unwrap();
    let encrypted = second_request.iter().find_map(|message| {
        message.output()?.items.iter().find_map(|item| match item {
            ChatOutputItem::Reasoning(reasoning) => reasoning.encrypted_content.clone(),
            _ => None,
        })
    });
    assert_eq!(
        encrypted.as_deref(),
        Some(ENCRYPTED),
        "second same-turn request must keep encrypted reasoning on the outbound assistant turn"
    );
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
        .returning(move |_| Ok(MockChatResponse::with_tools("", tool_calls.clone()).into()));
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(MockChatResponse::text_only("done").into()));
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![querymt::chat::ToolResultPart::Text {
                text: "tool output".to_string(),
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

    assert!(
        matches!(
            &outcome,
            CycleOutcome::Stopped {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ),
        "{outcome:?}"
    );
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
        .returning(move |_| Ok(MockChatResponse::with_tools("", vec![tool_call.clone()]).into()));
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(MockChatResponse::text_only("done").into()));
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
        .returning(move |_| Ok(MockChatResponse::with_tools("", vec![tool_call.clone()]).into()));
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move |messages| {
            *seen_history_clone.lock().unwrap() = Some(messages.to_vec());
            Ok(MockChatResponse::text_only("done").into())
        });
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .returning(|_, _| {
            Ok(vec![
                querymt::chat::ToolResultPart::Attachment(Box::new(
                    querymt::chat::MediaPart::new(
                        querymt::chat::MediaKind::Image,
                        Some("image/png".parse().unwrap()),
                        querymt::chat::MediaSource::Inline {
                            data: vec![0u8; 32],
                        },
                    )
                    .unwrap(),
                )),
                querymt::chat::ToolResultPart::Attachment(Box::new(
                    querymt::chat::MediaPart::new(
                        querymt::chat::MediaKind::Document,
                        Some("application/pdf".parse().unwrap()),
                        querymt::chat::MediaSource::Inline {
                            data: vec![1u8; 64],
                        },
                    )
                    .unwrap(),
                )),
                querymt::chat::ToolResultPart::Text {
                    text: "small text".to_string(),
                },
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
        .find(|msg| msg.has_tool_result())
        .expect("history should contain tool result message");

    let tool_result_content = tool_result_message
        .portable_input_parts()
        .into_iter()
        .find_map(|part| match part {
            querymt::chat::ChatInputPart::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("tool result part should exist");

    assert!(matches!(
        &tool_result_content.parts[0],
        querymt::chat::ToolResultPart::Attachment(_)
    ));
    assert!(matches!(
        &tool_result_content.parts[1],
        querymt::chat::ToolResultPart::Attachment(_)
    ));
    assert!(matches!(
        &tool_result_content.parts[2],
        querymt::chat::ToolResultPart::Text { text } if text == "small text"
    ));
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
        .returning(move |_| Ok(MockChatResponse::with_tools("", vec![tool_call.clone()]).into()));
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(MockChatResponse::text_only("done").into()));
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

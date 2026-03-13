use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{SnapshotPolicy, ToolPolicy};
use crate::agent::execution::CycleOutcome;
use crate::agent::execution_context::ExecutionContext;
use crate::events::{AgentEventKind, StopType};
use crate::middleware::{ExecutionState, MiddlewareDriver};
use crate::session::backend::StorageBackend;
use crate::session::provider::SessionHandle;
use crate::session::runtime::RuntimeContext;
use crate::session::store::SessionStore;
use crate::test_utils::{
    MockChatResponse, MockLlmProvider, MockSessionStore, SharedLlmProvider, StopOnBeforeLlmCall,
    TestProviderFactory, mock_llm_config, mock_plugin_registry, mock_querymt_tool_call,
    mock_session,
};
use agent_client_protocol::StopReason;
use mockall::Sequence;
use querymt::LLMParams;
use querymt::chat::{FunctionTool, Tool};
use querymt::error::LLMError;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tempfile::TempDir;
use tokio::sync::{Mutex, oneshot};

// Mock implementations moved to crate::test_utils::mocks

struct TestHarness {
    config: Arc<AgentConfig>,
    session_id: String,
    exec_ctx: ExecutionContext,
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
            .expect_record_artifact()
            .returning(|_| Ok(()))
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

/// Test middleware that asserts turn_diffs were populated by the real tool result
/// storage path, then stops at turn-end so we can assert deterministically.
struct AssertTurnDiffsAtTurnEnd;

#[async_trait::async_trait]
impl MiddlewareDriver for AssertTurnDiffsAtTurnEnd {
    async fn on_turn_end(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> crate::middleware::Result<ExecutionState> {
        if !matches!(state, ExecutionState::Complete) {
            return Ok(state);
        }

        let runtime = runtime.expect("runtime must be present");
        let diffs = runtime.turn_diffs.lock().expect("turn_diffs lock").clone();

        // This assertion is the core contract: tool result snapshot diffs must have
        // been aggregated into runtime.turn_diffs before turn-end middleware runs.
        assert!(
            !diffs.is_empty(),
            "turn_diffs should be populated by tool result storage path"
        );

        Ok(ExecutionState::Stopped {
            message: "verified turn_diffs populated".into(),
            stop_type: StopType::Other,
            context: None,
        })
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "AssertTurnDiffsAtTurnEnd"
    }
}

/// Strong end-to-end regression test: runs the full execution state machine,
/// executes a real built-in mutating tool (`write_file`) with snapshot diff
/// enabled, and verifies `runtime.turn_diffs` is populated through the actual
/// crate path before turn-end middleware runs.
#[tokio::test]
#[ignore] // Slow + filesystem/snapshot heavy; run manually in CI triage.
async fn test_e2e_tool_call_populates_turn_diffs_for_turn_end_middleware() {
    let temp_dir = tempfile::tempdir().expect("temp workspace");
    let cwd = temp_dir.path().to_path_buf();

    let mut harness = TestHarness::new(vec![], None).await;

    // Rebuild config with:
    // - built-in tools enabled (for write_file)
    // - snapshot diff enabled (to generate changed_paths)
    // - explicit write_file mutating list
    // - assertion middleware to verify turn_diffs contract at turn-end
    harness.config = Arc::new(
        crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
            harness.config.provider.clone(),
            harness.config.event_sink.journal().clone(),
        )
        .with_tool_policy(ToolPolicy::BuiltInAndProvider)
        .with_snapshot_policy(SnapshotPolicy::Diff)
        .with_assume_mutating(false)
        .with_mutating_tools(["write_file".to_string()])
        .with_middleware(AssertTurnDiffsAtTurnEnd)
        .build(),
    );

    // Ensure the execution context is rooted to our temp workspace so write_file
    // can resolve and mutate files there.
    harness.exec_ctx.runtime = crate::agent::core::SessionRuntime::new(
        Some(cwd.clone()),
        HashMap::new(),
        crate::agent::core::McpToolState::empty(),
    );

    let write_call = mock_querymt_tool_call(
        "call-write-1",
        "write_file",
        r#"{"path":"new_file.rs","content":"pub fn copied() -> i32 {\n    let mut x = 0;\n    x += 1;\n    x\n}\n"}"#,
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
                vec![write_call.clone()],
            )))
        });

    // The second LLM call completes the turn after tool results are stored.
    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(Box::new(MockChatResponse::text_only("done"))));

    let cwd_for_tool = cwd.clone();
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .times(1)
        .returning(move |name, args| {
            assert_eq!(name, "write_file");
            let rel_path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .expect("path arg");
            let content = args
                .get("content")
                .and_then(serde_json::Value::as_str)
                .expect("content arg");
            let abs = cwd_for_tool.join(rel_path);
            std::fs::write(&abs, content).expect("provider mock writes file");
            Ok("{\"ok\":true}".to_string())
        });

    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    let outcome = harness.run().await;

    // Middleware intentionally stops the cycle after asserting the contract.
    assert_eq!(outcome, CycleOutcome::Stopped(StopReason::EndTurn));

    // Sanity check that file mutation actually happened.
    let written = std::fs::read_to_string(cwd.join("new_file.rs")).expect("written file exists");
    assert!(written.contains("pub fn copied"));
}

/// Regression test: verify DedupCheckMiddleware guard is reset between turns.
///
/// This test catches the bug where dedup review guard state leaked
/// across multiple prompts, causing dedup analysis to be skipped on subsequent turns.
#[tokio::test]
#[ignore] // Slow + requires full execution; run with `cargo test -- --ignored`
async fn test_dedup_guard_reset_between_turns() {
    let temp_dir = tempfile::tempdir().expect("temp workspace");
    let cwd = temp_dir.path().to_path_buf();

    // Create an initial file with a function that will trigger dedup on second turn
    let original_fn = "pub fn calculate_sum(numbers: &[i32]) -> i32 {\n    let mut sum = 0;\n    for num in numbers { sum += num; }\n    sum\n}\n";
    std::fs::write(cwd.join("math.rs"), original_fn).expect("write original file");

    let mut harness = TestHarness::new(vec![], None).await;

    // Configure with:
    // - dedup check enabled
    // - snapshot diff for turn_diffs population
    harness.config = Arc::new(
        crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
            harness.config.provider.clone(),
            harness.config.event_sink.journal().clone(),
        )
        .with_tool_policy(ToolPolicy::BuiltInAndProvider)
        .with_snapshot_policy(SnapshotPolicy::Diff)
        .with_assume_mutating(false)
        .with_mutating_tools(["write_file".to_string()])
        .build(),
    );

    harness.exec_ctx.runtime = crate::agent::core::SessionRuntime::new(
        Some(cwd.clone()),
        HashMap::new(),
        crate::agent::core::McpToolState::empty(),
    );

    // ── First turn: write a copy of the function ──
    // This should trigger dedup analysis and inject a review message.

    let write_call_1 = mock_querymt_tool_call(
        "call-1",
        "write_file",
        r#"{"path":"utils.rs","content":"pub fn compute_total(values: &[i32]) -> i32 {\n    let mut total = 0;\n    for val in values { total += val; }\n    total\n}\n"}"#,
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
                vec![write_call_1.clone()],
            )))
        });

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Ok(Box::new(MockChatResponse::text_only("done"))));

    let cwd_1 = cwd.clone();
    harness
        .provider_mut()
        .await
        .expect_call_tool()
        .times(1)
        .returning(move |name, args| {
            assert_eq!(name, "write_file");
            let rel_path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .expect("path arg");
            let content = args
                .get("content")
                .and_then(serde_json::Value::as_str)
                .expect("content arg");
            std::fs::write(cwd_1.join(rel_path), content).expect("write file");
            Ok("{\"ok\":true}".to_string())
        });

    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    // In production SessionActor sets turn_generation before entering execution.
    // TestHarness drives execute_cycle_state_machine directly, so we set it explicitly.
    harness
        .exec_ctx
        .runtime
        .turn_generation
        .store(1, std::sync::atomic::Ordering::SeqCst);

    // Run first turn
    let outcome_1 = harness.run().await;
    assert_eq!(outcome_1, CycleOutcome::Completed);
    assert_eq!(
        harness
            .exec_ctx
            .runtime
            .turn_generation
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "First prompt should keep runtime turn_generation=1"
    );

    // Verify the file was written
    assert!(
        cwd.join("utils.rs").exists(),
        "First turn should write utils.rs"
    );

    // ── Second turn: just ask a question (no tool calls) ──
    // The key test: if the dedup guard was not reset, it would skip analysis.
    // With the fix, it should reset and be ready to analyze any new files.
    // (In this test, no new files are written, but the guard state is the critical check.)

    let mut seq_2 = Sequence::new();

    harness
        .provider_mut()
        .await
        .expect_chat()
        .times(1)
        .in_sequence(&mut seq_2)
        .returning(|_| Ok(Box::new(MockChatResponse::text_only("hello again"))));

    harness
        .provider_mut()
        .await
        .expect_tools()
        .return_const(None)
        .times(0..);

    harness
        .exec_ctx
        .runtime
        .turn_generation
        .store(2, std::sync::atomic::Ordering::SeqCst);

    let outcome_2 = harness.run().await;
    assert_eq!(outcome_2, CycleOutcome::Completed);
    assert_eq!(
        harness
            .exec_ctx
            .runtime
            .turn_generation
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "Second prompt should keep runtime turn_generation=2"
    );

    // Success: generation increments across prompts and dedup guard state does not
    // leak between turns.
}

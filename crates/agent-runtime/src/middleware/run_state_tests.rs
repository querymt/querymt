//! Middleware state is scoped to one run, through the real loop (spec
//! `agent-middleware`, "Middleware state is scoped to one run").

use super::context_warning_tests::{session_on_model, stepping_provider};
use crate::middleware::{
    HookCx, Middleware, ParallelEditGuard, ParallelEditMode, Result, StepVerdict, TurnInfo,
    TurnMiddleware,
};
use crate::test_utils::{
    Act, MockChatResponse, MockLlmProvider, ScriptedMiddleware, SharedLlmProvider, UndoTestFixture,
    mock_querymt_tool_call,
};
use crate::tools::{Tool, ToolRegistry, builtins::WriteFileTool};
use async_trait::async_trait;
use querymt::chat::ChatMessage;
use querymt_agent_core::model::{MessagePart, NoticeSource};
use querymt_agent_core::session::backend::StorageBackend;
use std::sync::{Arc, Mutex};

/// Acts once per run: at the run's first step it queues one notice. Its only
/// state is a flag on the per-run instance.
struct OncePerRun;

struct OnceTurn {
    acted: bool,
}

impl Middleware for OncePerRun {
    fn name(&self) -> &'static str {
        "OncePerRun"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(OnceTurn { acted: false })
    }
}

#[async_trait]
impl TurnMiddleware for OnceTurn {
    async fn on_step_start(&mut self, cx: &mut HookCx<'_>) -> Result<StepVerdict> {
        if !std::mem::replace(&mut self.acted, true) {
            cx.queue_notice(NoticeSource::DuplicateToolCall, "once per run");
        }
        Ok(StepVerdict::Proceed)
    }
}

/// Notices of `source` stored in `session`.
async fn stored_notices(fixture: &UndoTestFixture, session: &str, source: NoticeSource) -> usize {
    fixture
        .storage
        .session_store()
        .get_history(session)
        .await
        .expect("history")
        .into_iter()
        .flat_map(|message| message.parts)
        .filter(|part| matches!(part, MessagePart::Notice { source: s, .. } if *s == source))
        .count()
}

#[tokio::test]
async fn concurrent_sessions_do_not_share_middleware_state() {
    let fixture = UndoTestFixture::with_provider_and_middleware(
        stepping_provider(2, 10),
        vec![Arc::new(OncePerRun)],
    )
    .await
    .expect("fixture");
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let session_a = session_on_model(&fixture, dir_a.path(), "model-a").await;
    let session_b = session_on_model(&fixture, dir_b.path(), "model-b").await;

    let (run_a, run_b) = tokio::join!(
        fixture.prompt_to_completion(&session_a, "run a"),
        fixture.prompt_to_completion(&session_b, "run b"),
    );
    run_a.expect("run of session A");
    run_b.expect("run of session B");

    // Each session acted once: the fact recorded in one run did not reach the other.
    let source = NoticeSource::DuplicateToolCall;
    assert_eq!(stored_notices(&fixture, &session_a, source).await, 1);
    assert_eq!(stored_notices(&fixture, &session_b, source).await, 1);
}

#[tokio::test]
async fn middleware_state_does_not_carry_into_the_next_run() {
    let fixture = UndoTestFixture::with_provider_and_middleware(
        stepping_provider(2, 10),
        vec![Arc::new(OncePerRun)],
    )
    .await
    .expect("fixture");
    let session = session_on_model(&fixture, fixture.worktree.path(), "mock-model").await;
    let source = NoticeSource::DuplicateToolCall;

    fixture
        .prompt_to_completion(&session, "run 0")
        .await
        .expect("first run");
    assert_eq!(stored_notices(&fixture, &session, source).await, 1);
    fixture
        .prompt_to_completion(&session, "run 1")
        .await
        .expect("second run");

    assert_eq!(
        stored_notices(&fixture, &session, source).await,
        2,
        "the second run acts once again"
    );
}

/// Two `write_file` calls to one file in the first reply, text afterwards.
/// Every request's messages are recorded as text.
fn double_write_provider() -> (SharedLlmProvider, Arc<Mutex<Vec<String>>>) {
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = requests.clone();
    let respond = Arc::new(move |messages: &[ChatMessage]| {
        seen.lock().unwrap().push(
            messages
                .iter()
                .map(|message| message.text())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let first = messages.iter().all(|m| !m.text().starts_with("run 1"));
        let calls = messages.len() < 3 && first;
        if calls {
            let write = |id: &str| {
                mock_querymt_tool_call(
                    id,
                    "write_file",
                    &serde_json::json!({"path": "same.txt", "content": id}).to_string(),
                )
            };
            MockChatResponse::with_tools("writing", vec![write("w1"), write("w2")])
        } else {
            MockChatResponse::text_only("done")
        }
    });
    let mut mock = MockLlmProvider::new();
    let with_tools = respond.clone();
    mock.expect_chat_with_tools()
        .returning(move |messages, _| Ok(with_tools(messages).into()));
    mock.expect_chat()
        .returning(move |messages| Ok(respond(messages).into()));
    (
        SharedLlmProvider::new(mock, vec![WriteFileTool::new().definition()]),
        requests,
    )
}

#[tokio::test]
async fn cancelled_run_leaves_no_middleware_state_behind() {
    let (provider, requests) = double_write_provider();
    // The second step of the first run never returns, so the run is cancelled
    // after the guard queued its note and before the note was stored.
    let hang_on_second_step = ScriptedMiddleware::default().step_start([
        Act::Say(StepVerdict::Proceed),
        Act::Hang,
        Act::Say(StepVerdict::Proceed),
    ]);
    let fixture = UndoTestFixture::with_provider_and_middleware(
        provider,
        vec![
            Arc::new(ParallelEditGuard::new(
                ToolRegistry::new(),
                ParallelEditMode::Warn,
            )),
            Arc::new(hang_on_second_step.clone()),
        ],
    )
    .await
    .expect("fixture");
    let session = session_on_model(&fixture, fixture.worktree.path(), "mock-model").await;

    let (cancelled, ()) = tokio::join!(fixture.prompt_to_completion(&session, "run 0"), async {
        while hang_on_second_step.count("step_start") < 2 {
            tokio::task::yield_now().await;
        }
        fixture.cancel(&session).await.expect("cancel");
    });
    cancelled.expect("the cancelled run completes");
    let before_second_run = requests.lock().unwrap().len();
    assert_eq!(before_second_run, 1, "the cancelled run sent one request");
    assert_eq!(
        stored_notices(&fixture, &session, NoticeSource::ParallelEdit).await,
        0,
        "the note was never stored"
    );

    fixture
        .prompt_to_completion(&session, "run 1")
        .await
        .expect("second run");

    let first_of_second_run = requests.lock().unwrap()[before_second_run].clone();
    assert!(
        !first_of_second_run.contains("several separate tool calls"),
        "the new run's first request carries no note of the cancelled run"
    );
    // The stored history is unchanged by the note too.
    assert_eq!(
        stored_notices(&fixture, &session, NoticeSource::ParallelEdit).await,
        0
    );
}

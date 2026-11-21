//! The context-usage warning through the real run loop (spec
//! `agent-middleware`, "Context-usage warning is stored once per run").

use crate::middleware::{
    ContextConfig, ContextMiddleware, HookCx, Middleware, Result, StepVerdict, TurnInfo,
    TurnMiddleware,
};
use crate::test_utils::{
    MockChatResponse, MockLlmProvider, SharedLlmProvider, UndoTestFixture, mock_querymt_tool_call,
};
use crate::tools::{Tool, builtins::WriteFileTool};
use async_trait::async_trait;
use querymt::Usage;
use querymt::chat::ChatMessage;
use querymt_agent_core::model::{MessagePart, NoticeSource};
use querymt_agent_core::session::backend::StorageBackend;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::watch;

/// Context limit of the tests' manual model, in tokens.
const LIMIT: usize = 1000;

/// A reply that reports `used` tokens of context.
fn reply_using(used: u32, response: MockChatResponse) -> MockChatResponse {
    MockChatResponse {
        usage: Some(Usage {
            input_tokens: used,
            ..Usage::default()
        }),
        ..response
    }
}

/// A provider that answers with a `write_file` call until the request holds
/// `tool_steps` tool results, then with text. Every reply reports `used` tokens.
pub(super) fn stepping_provider(tool_steps: usize, used: u32) -> SharedLlmProvider {
    let respond = Arc::new(move |messages: &[ChatMessage]| {
        // Only the tool results of the current run count: it starts at the
        // last prompt, which every test words "run ...".
        let run_start = messages
            .iter()
            .rposition(|m| m.text().starts_with("run "))
            .unwrap_or(0);
        let step = messages[run_start..]
            .iter()
            .filter(|m| m.portable_input_parts().iter().any(|p| p.is_tool_result()))
            .count();
        let response = if step < tool_steps {
            MockChatResponse::with_tools(
                "writing",
                vec![mock_querymt_tool_call(
                    &format!("write-{step}-{}", messages.len()),
                    "write_file",
                    &serde_json::json!({
                        "path": format!("step-{step}.txt"),
                        "content": "x",
                    })
                    .to_string(),
                )],
            )
        } else {
            MockChatResponse::text_only("done")
        };
        reply_using(used, response)
    });
    let mut mock = MockLlmProvider::new();
    let with_tools = respond.clone();
    mock.expect_chat_with_tools()
        .returning(move |messages, _| Ok(with_tools(messages).into()));
    mock.expect_chat()
        .returning(move |messages| Ok(respond(messages).into()));
    SharedLlmProvider::new(mock, vec![WriteFileTool::new().definition()])
}

fn context_middleware(config: ContextConfig) -> Arc<dyn Middleware> {
    Arc::new(ContextMiddleware::new(config))
}

fn manual_limit() -> ContextConfig {
    ContextConfig::with_manual_limit(LIMIT)
}

pub(super) async fn session_on_model(
    fixture: &UndoTestFixture,
    cwd: &std::path::Path,
    model: &str,
) -> String {
    let session = fixture.create_session_with_cwd(cwd).await.expect("session");
    let store = fixture.storage.session_store();
    let llm_config = store
        .create_or_get_llm_config(&querymt::LLMParams::new().provider("mock").model(model))
        .await
        .expect("llm config");
    store
        .set_session_llm_config(&session, llm_config.id)
        .await
        .expect("bind llm config");
    session
}

/// Contents of the `context_usage` notices stored in `session`.
async fn usage_notices(fixture: &UndoTestFixture, session: &str) -> Vec<String> {
    fixture
        .storage
        .session_store()
        .get_history(session)
        .await
        .expect("history")
        .into_iter()
        .flat_map(|message| message.parts)
        .filter_map(|part| match part {
            MessagePart::Notice {
                source: NoticeSource::ContextUsage,
                content,
            } => Some(content),
            _ => None,
        })
        .collect()
}

/// One session, `runs` runs of `tool_steps` tool steps each, every reply
/// reporting `used` tokens. Returns the notices stored after each run.
async fn notices_after_each_run(
    config: ContextConfig,
    tool_steps: usize,
    used: u32,
    runs: usize,
) -> Vec<usize> {
    let fixture = UndoTestFixture::with_provider_and_middleware(
        stepping_provider(tool_steps, used),
        vec![context_middleware(config)],
    )
    .await
    .expect("fixture");
    let session = session_on_model(&fixture, fixture.worktree.path(), "mock-model").await;
    let mut counts = Vec::new();
    for run in 0..runs {
        fixture
            .prompt_to_completion(&session, &format!("run {run}"))
            .await
            .expect("run completes");
        counts.push(usage_notices(&fixture, &session).await.len());
    }
    counts
}

#[tokio::test]
async fn warning_stored_once_in_a_run() {
    // 82% of the limit on every step of a run with three tool steps.
    let counts = notices_after_each_run(manual_limit(), 3, 820, 1).await;

    assert_eq!(counts, [1]);
}

#[tokio::test]
async fn next_run_above_the_threshold_warns_again() {
    let counts = notices_after_each_run(manual_limit(), 1, 820, 2).await;

    assert_eq!(counts, [1, 2], "each run stores one notice");
}

#[tokio::test]
async fn usage_below_the_threshold_stores_no_warning() {
    let counts = notices_after_each_run(manual_limit(), 3, 500, 1).await;

    assert_eq!(counts, [0]);
}

#[tokio::test]
async fn warning_turned_off() {
    let counts = notices_after_each_run(manual_limit().warn_at_percent(0), 3, 820, 1).await;

    assert_eq!(counts, [0]);
}

/// Makes two sessions take their steps in turn: A's step `k` starts after B's
/// step `k - 1` has passed the context middleware, and B's step `k` after A's
/// step `k` has. `role` places it before or after that middleware.
#[derive(Clone)]
struct Alternate {
    role: Role,
    a: Arc<OnceLock<String>>,
    done: Arc<Done>,
    started: Arc<Mutex<HashMap<String, usize>>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    BeforeContext,
    AfterContext,
}

/// Steps that passed the context middleware, per session.
struct Done {
    a: watch::Sender<usize>,
    b: watch::Sender<usize>,
}

impl Alternate {
    fn is_a(&self, session: &str) -> bool {
        self.a.get().map(String::as_str) == Some(session)
    }
}

async fn reached(counter: &watch::Sender<usize>, steps: usize) {
    counter
        .subscribe()
        .wait_for(|done| *done >= steps)
        .await
        .expect("counter open");
}

impl Middleware for Alternate {
    fn name(&self) -> &'static str {
        "Alternate"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(self.clone())
    }
}

#[async_trait]
impl TurnMiddleware for Alternate {
    async fn on_step_start(&mut self, cx: &mut HookCx<'_>) -> Result<StepVerdict> {
        let session = cx.session_id().to_string();
        let a = self.is_a(&session);
        match self.role {
            Role::BeforeContext => {
                let step = {
                    let mut started = self.started.lock().unwrap();
                    let n = started.entry(session).or_default();
                    *n += 1;
                    *n
                };
                if a {
                    reached(&self.done.b, step - 1).await;
                } else {
                    reached(&self.done.a, step).await;
                }
            }
            Role::AfterContext => {
                let counter = if a { &self.done.a } else { &self.done.b };
                counter.send_modify(|done| *done += 1);
            }
        }
        Ok(StepVerdict::Proceed)
    }
}

#[tokio::test]
async fn sessions_on_different_models_each_get_one_warning() {
    let a = Arc::new(OnceLock::new());
    let done = Arc::new(Done {
        a: watch::channel(0).0,
        b: watch::channel(0).0,
    });
    let alternate = |role| -> Arc<dyn Middleware> {
        Arc::new(Alternate {
            role,
            a: a.clone(),
            done: done.clone(),
            started: Arc::default(),
        })
    };
    let fixture = UndoTestFixture::with_provider_and_middleware(
        stepping_provider(3, 820),
        vec![
            alternate(Role::BeforeContext),
            context_middleware(manual_limit()),
            alternate(Role::AfterContext),
        ],
    )
    .await
    .expect("fixture");
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let session_a = session_on_model(&fixture, dir_a.path(), "model-a").await;
    let session_b = session_on_model(&fixture, dir_b.path(), "model-b").await;
    a.set(session_a.clone()).unwrap();

    let both = async {
        tokio::join!(
            fixture.prompt_to_completion(&session_a, "run a"),
            fixture.prompt_to_completion(&session_b, "run b"),
        )
    };
    let (run_a, run_b) = tokio::time::timeout(std::time::Duration::from_secs(30), both)
        .await
        .expect("both runs finish");
    run_a.expect("run of session A");
    run_b.expect("run of session B");

    let stored = (
        usage_notices(&fixture, &session_a).await.len(),
        usage_notices(&fixture, &session_b).await.len(),
    );
    assert_eq!(stored, (1, 1), "notices stored for sessions A and B");
}

//! Turn usage on prompt responses (spec `acp-session-updates`, "Turn usage on
//! prompt responses").
//!
//! Every test drives `session/prompt` through the session actor with a mock
//! provider and asserts the `PromptResponse.usage` that comes back.

use crate::acp::protocol::{
    CancelNotification, ContentBlock, PromptRequest, PromptResponse, StopReason, TextContent,
};
use crate::middleware::EndVerdict;
use crate::send_agent::SendAgent;
use crate::test_utils::{Act, ScriptedMiddleware, UndoTestFixture};
use async_trait::async_trait;
use querymt::chat::{ChatCall, ChatCapabilities, ChatOutput, FinishReason, Tool};
use querymt::error::LLMError;
use querymt::plugin::{Fut, LLMProviderFactory};
use querymt::{LLMProvider, Usage};
use querymt_agent_core::session::backend::StorageBackend;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// What the mock provider does with request number `index`: answer with the
/// reported usage (`None`: no usage reported), or hang until the run is
/// cancelled.
#[derive(Clone, Copy)]
enum Step {
    Answer(Option<(u32, u32)>),
    Hang,
}

#[derive(Clone)]
struct MockProvider {
    script: Arc<dyn Fn(usize) -> Step + Send + Sync>,
    requests: Arc<AtomicUsize>,
    /// Notified when a request hangs, so the test can cancel the run.
    hung: Arc<Notify>,
}

#[async_trait]
impl querymt::chat::ChatProvider for MockProvider {
    fn capabilities(&self) -> ChatCapabilities {
        ChatCapabilities::NONE
    }

    async fn chat(&self, _call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        let index = self.requests.fetch_add(1, Ordering::SeqCst);
        match (self.script)(index) {
            Step::Answer(usage) => Ok(ChatOutput::from_projections(
                None,
                Some("done".into()),
                None,
                usage.map(|(input_tokens, output_tokens)| Usage {
                    input_tokens,
                    output_tokens,
                    ..Usage::default()
                }),
                Some(FinishReason::Stop),
            )),
            Step::Hang => {
                self.hung.notify_one();
                std::future::pending().await
            }
        }
    }
}

#[async_trait]
impl querymt::completion::CompletionProvider for MockProvider {
    async fn complete(
        &self,
        _: &querymt::completion::CompletionRequest,
    ) -> Result<querymt::completion::CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented("mock".into()))
    }
}

#[async_trait]
impl querymt::embedding::EmbeddingProvider for MockProvider {
    async fn embed(&self, _: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented("mock".into()))
    }
}

impl LLMProvider for MockProvider {
    fn tools(&self) -> Option<&[Tool]> {
        None
    }
}

struct MockFactory(MockProvider);

impl LLMProviderFactory for MockFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn config_schema(&self) -> String {
        "{}".to_string()
    }

    fn from_config(&self, _cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
        Ok(Box::new(self.0.clone()))
    }

    fn list_models<'a>(&'a self, _cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        Box::pin(async { Ok(vec!["mock-model".to_string()]) })
    }
}

/// A session bound to a mock provider that follows `script`. `continues` is
/// the number of times the end of a run is continued, so a run sends
/// `continues + 1` requests.
async fn session_with(
    continues: usize,
    script: impl Fn(usize) -> Step + Send + Sync + 'static,
) -> (UndoTestFixture, String, MockProvider) {
    let provider = MockProvider {
        script: Arc::new(script),
        requests: Arc::new(AtomicUsize::new(0)),
        hung: Arc::new(Notify::new()),
    };
    // The end of every run is continued `continues` times; later runs finish
    // at once (the script is exhausted into `Finish`).
    let mut acts: Vec<_> = (0..continues)
        .map(|_| Act::Notice("one more check", EndVerdict::Continue))
        .collect();
    acts.push(Act::Say(EndVerdict::Finish));
    let middleware = ScriptedMiddleware::default().turn_end(acts);
    let fixture = UndoTestFixture::with_factory(
        Arc::new(MockFactory(provider.clone())),
        None,
        vec![Arc::new(middleware)],
    )
    .await
    .expect("fixture");
    let store = fixture.storage.session_store();
    let cwd = fixture.worktree.path().to_path_buf();
    let session = store
        .create_session(None, Some(cwd.clone()), None, None)
        .await
        .expect("session")
        .public_id;
    let llm_config = store
        .create_or_get_llm_config(
            &querymt::LLMParams::new()
                .provider("mock")
                .model("mock-model"),
        )
        .await
        .expect("llm config");
    store
        .set_session_llm_config(&session, llm_config.id)
        .await
        .expect("bind llm config");
    fixture
        .register_session_actor_with_mcp(&session, &cwd, crate::agent::core::McpToolState::empty())
        .await;
    (fixture, session, provider)
}

async fn prompt(
    fixture: &UndoTestFixture,
    session: &str,
    text: &str,
) -> Result<PromptResponse, agent_client_protocol::Error> {
    let request = PromptRequest::new(
        session.to_owned(),
        vec![ContentBlock::Text(TextContent::new(text))],
    );
    tokio::time::timeout(
        Duration::from_secs(20),
        SendAgent::prompt(&fixture.handle, request),
    )
    .await
    .expect("the prompt finishes")
}

/// Scenario "Completed turn reports its usage".
#[tokio::test]
async fn completed_turn_reports_its_usage() {
    let (fixture, session, provider) = session_with(1, |index| match index {
        0 => Step::Answer(Some((1000, 200))),
        _ => Step::Answer(Some((500, 100))),
    })
    .await;

    let response = prompt(&fixture, &session, "hello").await.expect("prompt");

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(provider.requests.load(Ordering::SeqCst), 2, "two requests");
    let usage = response.usage.expect("the turn reports its usage");
    assert_eq!(usage.input_tokens, 1500);
    assert_eq!(usage.output_tokens, 300);
    assert_eq!(usage.thought_tokens, Some(0));
    assert_eq!(usage.cached_read_tokens, Some(0));
    assert_eq!(usage.cached_write_tokens, Some(0));
    assert_eq!(usage.total_tokens, 1800, "sum of every reported category");
}

/// Scenario "Turn without reported usage".
#[tokio::test]
async fn turn_without_reported_usage() {
    let (fixture, session, provider) = session_with(0, |_| Step::Answer(None)).await;

    let response = prompt(&fixture, &session, "hello").await.expect("prompt");

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(provider.requests.load(Ordering::SeqCst), 1);
    assert!(
        response.usage.is_none(),
        "unknown usage is omitted, not zero: {:?}",
        response.usage
    );
    let wire = serde_json::to_value(&response).expect("serializes");
    assert!(wire.get("usage").is_none(), "no usage on the wire: {wire}");
}

/// Scenario "Cancelled turn reports usage incurred so far": the first request
/// reports usage, the second hangs until the run is cancelled.
#[tokio::test]
async fn cancelled_turn_reports_usage_incurred_so_far() {
    let (fixture, session, provider) = session_with(1, |index| match index {
        0 => Step::Answer(Some((1000, 200))),
        _ => Step::Hang,
    })
    .await;

    let cancel = async {
        tokio::time::timeout(Duration::from_secs(10), provider.hung.notified())
            .await
            .expect("the second request starts");
        SendAgent::cancel(&fixture.handle, CancelNotification::new(session.clone()))
            .await
            .expect("cancel");
    };
    let (response, ()) = tokio::join!(prompt(&fixture, &session, "hello"), cancel);
    let response = response.expect("a cancelled run is not an error");

    assert_eq!(response.stop_reason, StopReason::Cancelled);
    let usage = response.usage.expect("usage incurred so far is reported");
    assert_eq!(usage.input_tokens, 1000);
    assert_eq!(usage.output_tokens, 200);
    assert_eq!(usage.total_tokens, 1200);
}

/// Scenario "Turn usage excludes earlier cycles".
#[tokio::test]
async fn turn_usage_excludes_earlier_cycles() {
    let (fixture, session, _provider) = session_with(0, |index| match index {
        0 => Step::Answer(Some((1000, 200))),
        _ => Step::Answer(Some((30, 7))),
    })
    .await;

    let first = prompt(&fixture, &session, "one").await.expect("first");
    let first = first.usage.expect("first usage");
    assert_eq!(first.input_tokens, 1000);

    let second = prompt(&fixture, &session, "two").await.expect("second");
    let second = second.usage.expect("second usage");
    assert_eq!(second.input_tokens, 30, "only the second cycle's tokens");
    assert_eq!(second.output_tokens, 7);
    assert_eq!(second.total_tokens, 37);
}

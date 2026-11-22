//! How a run obtains its model (spec `provider-resolution`, "A run resolves its
//! model once, before its first middleware hook" and "A run whose provider
//! cannot be built fails before any hook").
//!
//! Every test sends a prompt through the session actor with a provider factory
//! that counts its builds, so they hold wherever the run resolves its model.

use crate::acp::protocol::{
    CancelNotification, ContentBlock, PromptRequest, PromptResponse, StopReason, TextContent,
};
use crate::middleware::{EndVerdict, HookCx, Middleware, StepVerdict, TurnInfo, TurnMiddleware};
use crate::send_agent::SendAgent;
use crate::test_utils::{Act, ScriptedMiddleware, UnbuildableProviderFactory, UndoTestFixture};
use agent_client_protocol::ErrorCode;
use async_trait::async_trait;
use querymt::chat::{ChatCall, ChatCapabilities, ChatOutput, ChatRole, FinishReason, Tool};
use querymt::error::LLMError;
use querymt::plugin::{Fut, LLMProviderFactory};
use querymt::{LLMProvider, Usage};
use querymt_agent_core::events::AgentEventKind;
use querymt_agent_core::session::backend::StorageBackend;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The provider code of a failed provider, as the client sees it.
const PROVIDER_FAILURE: i32 = -32010;

// ---- a provider and a factory that count ----

/// A provider that answers every request with `done` and counts them.
#[derive(Clone)]
struct CountingProvider {
    chats: Arc<AtomicUsize>,
    capabilities: ChatCapabilities,
}

#[async_trait]
impl querymt::chat::ChatProvider for CountingProvider {
    fn capabilities(&self) -> ChatCapabilities {
        self.capabilities
    }

    async fn chat(&self, _call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        self.chats.fetch_add(1, Ordering::SeqCst);
        Ok(ChatOutput::from_projections(
            None,
            Some("done".into()),
            None,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            }),
            Some(FinishReason::Stop),
        ))
    }
}

#[async_trait]
impl querymt::completion::CompletionProvider for CountingProvider {
    async fn complete(
        &self,
        _: &querymt::completion::CompletionRequest,
    ) -> Result<querymt::completion::CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented("counting".into()))
    }
}

#[async_trait]
impl querymt::embedding::EmbeddingProvider for CountingProvider {
    async fn embed(&self, _: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented("counting".into()))
    }
}

impl LLMProvider for CountingProvider {
    fn tools(&self) -> Option<&[Tool]> {
        None
    }
}

/// A factory named `mock` whose first `failures` builds fail with a retryable
/// error that asks to wait `retry_after_secs`, and whose later builds return a
/// [`CountingProvider`].
struct FlakyFactory {
    provider: CountingProvider,
    failures: AtomicUsize,
    retry_after_secs: u64,
    /// Every call of `from_config`, failed or not.
    attempts: Arc<AtomicUsize>,
    /// The calls that built a provider.
    builds: Arc<AtomicUsize>,
}

impl LLMProviderFactory for FlakyFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn config_schema(&self) -> String {
        "{}".to_string()
    }

    fn from_config(&self, _cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let failing = self
            .failures
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if failing {
            return Err(LLMError::RateLimited {
                message: "busy".into(),
                retry_after_secs: Some(self.retry_after_secs),
            });
        }
        self.builds.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(self.provider.clone()))
    }

    fn list_models<'a>(&'a self, _cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        Box::pin(async { Ok(vec!["mock-model".to_string()]) })
    }
}

/// What a test reads after the run.
struct Counters {
    chats: Arc<AtomicUsize>,
    attempts: Arc<AtomicUsize>,
    builds: Arc<AtomicUsize>,
}

impl Counters {
    fn chats(&self) -> usize {
        self.chats.load(Ordering::SeqCst)
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }
}

fn flaky(
    failures: usize,
    retry_after_secs: u64,
    capabilities: ChatCapabilities,
) -> (Arc<FlakyFactory>, Counters) {
    let counters = Counters {
        chats: Arc::new(AtomicUsize::new(0)),
        attempts: Arc::new(AtomicUsize::new(0)),
        builds: Arc::new(AtomicUsize::new(0)),
    };
    let factory = FlakyFactory {
        provider: CountingProvider {
            chats: counters.chats.clone(),
            capabilities,
        },
        failures: AtomicUsize::new(failures),
        retry_after_secs,
        attempts: counters.attempts.clone(),
        builds: counters.builds.clone(),
    };
    (Arc::new(factory), counters)
}

// ---- a session to prompt ----

/// A session bound to the `mock` provider, with its actor registered.
async fn session_on(fixture: &UndoTestFixture) -> String {
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
    session
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
    SendAgent::prompt(&fixture.handle, request).await
}

/// Every event of `session` up to and including its next `RunCompleted`. The
/// events reach the subscriber through the fanout task, so a test that wants
/// all the events of a run waits for its end and does not drain what happens
/// to have arrived.
async fn events_until_run_completed(
    events: &mut tokio::sync::broadcast::Receiver<querymt_agent_core::events::EventEnvelope>,
    session: &str,
) -> Vec<AgentEventKind> {
    let mut seen = Vec::new();
    let wait = async {
        loop {
            let envelope = events.recv().await.expect("event stream");
            if envelope.session_id() != session {
                continue;
            }
            let done = matches!(envelope.kind(), AgentEventKind::RunCompleted { .. });
            seen.push(envelope.kind().clone());
            if done {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .expect("the run completes");
    seen
}

async fn user_prompts(fixture: &UndoTestFixture, session: &str) -> usize {
    fixture
        .storage
        .session_store()
        .get_history(session)
        .await
        .expect("history")
        .iter()
        .filter(|message| message.role == ChatRole::User)
        .count()
}

// ---- scenarios ----

/// Scenario "Provider cannot be built".
#[tokio::test]
async fn provider_cannot_be_built() {
    let watching = ScriptedMiddleware::default();
    let fixture = UndoTestFixture::with_factory(
        Arc::new(UnbuildableProviderFactory),
        None,
        vec![Arc::new(watching.clone())],
    )
    .await
    .expect("fixture");
    let session = session_on(&fixture).await;
    let mut events = fixture.handle.config.subscribe_events();

    let error = prompt(&fixture, &session, "hello")
        .await
        .expect_err("a provider that cannot be built fails the run");

    // The structured provider failure reaches the client; the `Error` event
    // carries the text that names the provider initialization.
    assert_eq!(error.code, ErrorCode::new(PROVIDER_FAILURE), "{error:?}");
    let messages: Vec<String> = events_until_run_completed(&mut events, &session)
        .await
        .into_iter()
        .filter_map(|kind| match kind {
            AgentEventKind::Error { message } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("provider initialization")),
        "an error names provider initialization: {messages:?}"
    );
    assert_eq!(
        user_prompts(&fixture, &session).await,
        1,
        "prompt is stored"
    );
    assert!(
        watching.calls().is_empty(),
        "no hook runs before the provider exists: {:?}",
        watching.calls()
    );
}

/// Scenario "Provider is built after a transient failure".
#[tokio::test]
async fn provider_is_built_after_a_transient_failure() {
    let (factory, counters) = flaky(1, 0, ChatCapabilities::NONE);
    let fixture = UndoTestFixture::with_factory(factory, None, vec![])
        .await
        .expect("fixture");
    let session = session_on(&fixture).await;

    let response = prompt(&fixture, &session, "hello")
        .await
        .expect("the run continues once the provider is built");

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(counters.attempts(), 2, "one failed build, one that worked");
    assert_eq!(counters.builds(), 1);
    assert_eq!(counters.chats(), 1, "the model request was sent");
}

/// Reads the capabilities of the model profile the first hook is given.
struct ProfileProbe(Arc<Mutex<Option<ChatCapabilities>>>);

impl Middleware for ProfileProbe {
    fn name(&self) -> &'static str {
        "ProfileProbe"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(ProbeTurn(self.0.clone()))
    }
}

struct ProbeTurn(Arc<Mutex<Option<ChatCapabilities>>>);

#[async_trait]
impl TurnMiddleware for ProbeTurn {
    async fn on_turn_start(
        &mut self,
        cx: &mut HookCx<'_>,
    ) -> crate::middleware::Result<StepVerdict> {
        *self.0.lock().unwrap() = Some(cx.profile().capabilities);
        Ok(StepVerdict::Proceed)
    }
}

/// Scenario "Turn-start hook sees the model's capabilities". The first build
/// fails, so a profile read around that failure would be empty.
#[tokio::test]
async fn turn_start_hook_sees_the_models_capabilities() {
    let declared: ChatCapabilities = serde_json::from_str(
        r#"{"streaming":false,"options":{"max_tokens":true,"reasoning_effort":false}}"#,
    )
    .expect("capabilities parse");
    let seen = Arc::new(Mutex::new(None));
    let (factory, _counters) = flaky(1, 0, declared);
    let fixture =
        UndoTestFixture::with_factory(factory, None, vec![Arc::new(ProfileProbe(seen.clone()))])
            .await
            .expect("fixture");
    let session = session_on(&fixture).await;

    prompt(&fixture, &session, "hello").await.expect("run");

    let seen = seen.lock().unwrap().expect("the first hook ran");
    assert_eq!(seen, declared);
    assert!(seen.options.max_tokens);
}

/// Scenario "Run cancelled while its provider is resolved". The provider
/// cannot be built and asks for a wait an hour long; the run is cancelled
/// during that wait.
#[tokio::test]
async fn run_cancelled_while_its_provider_is_resolved() {
    let (factory, counters) = flaky(usize::MAX, 3600, ChatCapabilities::NONE);
    let fixture = UndoTestFixture::with_factory(factory, None, vec![])
        .await
        .expect("fixture");
    let session = session_on(&fixture).await;
    let mut events = fixture.handle.config.subscribe_events();

    let cancel = async {
        let wait = async {
            loop {
                let envelope = events.recv().await.expect("event stream");
                if envelope.session_id() == session
                    && matches!(
                        envelope.kind(),
                        AgentEventKind::RateLimited { .. } | AgentEventKind::LlmRetryWait { .. }
                    )
                {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .expect("the run waits to retry building its provider");
        SendAgent::cancel(&fixture.handle, CancelNotification::new(session.clone()))
            .await
            .expect("cancel");
    };
    let (response, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(prompt(&fixture, &session, "hello"), cancel)
    })
    .await
    .expect("the cancelled run ends");

    assert_eq!(
        response
            .expect("a cancelled run is not an error")
            .stop_reason,
        StopReason::Cancelled
    );
    assert_eq!(counters.chats(), 0, "no model request is sent");
    assert_eq!(counters.attempts(), 1, "the wait was cut short");
}

/// Scenario "Provider is built once for a run with several requests": the end
/// of the run is continued twice, so the run sends three requests.
#[tokio::test]
async fn provider_is_built_once_for_a_run_with_several_requests() {
    let continuer = ScriptedMiddleware::default().turn_end([
        Act::Notice("one more check", EndVerdict::Continue),
        Act::Notice("and another", EndVerdict::Continue),
        Act::Say(EndVerdict::Finish),
    ]);
    let (factory, counters) = flaky(0, 0, ChatCapabilities::NONE);
    let fixture = UndoTestFixture::with_factory(factory, None, vec![Arc::new(continuer)])
        .await
        .expect("fixture");
    let session = session_on(&fixture).await;

    prompt(&fixture, &session, "hello").await.expect("run");

    assert_eq!(counters.chats(), 3, "three model requests");
    assert_eq!(counters.attempts(), 1, "the provider was built once");
    assert_eq!(counters.builds(), 1);
}

/// Scenario "Manual compaction with a provider that cannot be built". The
/// first prompt fails and leaves its message in the history, so there is
/// something to compact.
#[tokio::test]
async fn manual_compaction_with_a_provider_that_cannot_be_built() {
    let fixture = UndoTestFixture::with_factory(Arc::new(UnbuildableProviderFactory), None, vec![])
        .await
        .expect("fixture");
    let session = session_on(&fixture).await;
    // The prompt returns before its run is recorded as completed, so wait for
    // that event: left in the stream, it would end the collection below at
    // the first run instead of the `/compact` run.
    let mut events = fixture.handle.config.subscribe_events();
    prompt(&fixture, &session, "hello")
        .await
        .expect_err("the provider cannot be built");
    events_until_run_completed(&mut events, &session).await;

    let error = prompt(&fixture, &session, "/compact")
        .await
        .expect_err("compaction needs the provider");

    assert_eq!(error.code, ErrorCode::new(PROVIDER_FAILURE), "{error:?}");
    let compaction_events: Vec<AgentEventKind> = events_until_run_completed(&mut events, &session)
        .await
        .into_iter()
        .filter(|kind| {
            matches!(
                kind,
                AgentEventKind::CompactionStart { .. } | AgentEventKind::CompactionEnd { .. }
            )
        })
        .collect();
    assert!(
        compaction_events.is_empty(),
        "nothing of the compaction started: {compaction_events:?}"
    );
}

//! The history a run sends to the model against the history stored for the
//! session (spec `run-history-consistency`, "History sent in a run matches the
//! stored session").
//!
//! Every test runs the real loop with a scripted provider. At each model
//! request the provider loads the session from the store, as a later run or a
//! restarted process would, and compares what that load would send with what
//! the run sent. Cache hints are left out of the comparison.

use crate::agent::core::McpToolState;
use crate::hooks::{HookCommandConfig, HookHandlerConfig, Hooks, HooksConfig, MatcherGroupConfig};
use crate::middleware::{
    AgentModeMiddleware, ContextConfig, ContextMiddleware, HookCx, Middleware, PreparedBatch,
    PreparedCallVerdict, ReplyVerdict, StepVerdict, TurnInfo, TurnMiddleware,
};
use crate::session::provider::SessionProvider;
use crate::test_utils::{Act, ScriptedMiddleware, UndoTestFixture, mock_querymt_tool_call};
use crate::tools::{Tool as _, builtins::WriteFileTool};
use async_trait::async_trait;
use querymt::chat::{
    ChatCall, ChatCapabilities, ChatMessage, ChatOutput, ChatOutputItem, ChatOutputStatus,
    ChatReasoningItem, ChatReasoningPart, ChatRole, FinishReason, Tool,
};
use querymt::error::LLMError;
use querymt::plugin::{Fut, LLMProviderFactory};
use querymt::{LLMProvider, Usage};
use querymt_agent_core::model::MessagePart;
use querymt_agent_core::projection::{RequestTarget, project_request};
use querymt_agent_core::session::backend::StorageBackend;
use std::sync::{Arc, Mutex, OnceLock};

/// What the scripted provider answers request number `index` with.
type Script = dyn Fn(usize) -> ChatOutput + Send + Sync;

/// Runs inside the provider while it answers a request, before the reply is
/// returned.
type Hook = Arc<dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// One request as the provider received it.
#[derive(Clone)]
struct Seen {
    messages: Vec<ChatMessage>,
    /// The names of the tools the request offered.
    tools: Vec<String>,
}

struct Inner {
    script: Box<Script>,
    tools: Vec<Tool>,
    tool_updates: bool,
    seen: Mutex<Vec<Seen>>,
    /// Differences found between a request and a reload, one line each.
    divergences: Mutex<Vec<String>>,
    /// The store to reload from and the session to reload.
    reload: OnceLock<(Arc<SessionProvider>, String)>,
    /// A hook to run while answering request number `.0`.
    before_reply: OnceLock<(usize, Hook)>,
    /// What the provider says it stamps on its output (`None`: it does not
    /// say). Read when a run resolves its model, so a change applies from the
    /// next run.
    provenance: Mutex<Option<querymt::chat::ChatOutputProvenance>>,
}

/// A provider that answers from a script and checks every request against a
/// reload of the session.
#[derive(Clone)]
struct ScriptedProvider(Arc<Inner>);

impl ScriptedProvider {
    fn new(
        tool_updates: bool,
        script: impl Fn(usize) -> ChatOutput + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(Inner {
            script: Box::new(script),
            tools: vec![WriteFileTool::new().definition()],
            tool_updates,
            seen: Mutex::default(),
            divergences: Mutex::default(),
            reload: OnceLock::new(),
            before_reply: OnceLock::new(),
            provenance: Mutex::new(None),
        }))
    }

    /// A provider that states the provenance it stamps, as OpenAI, xAI and
    /// Codex do.
    fn stating(self, endpoint: &str) -> Self {
        self.state_endpoint(endpoint);
        self
    }

    /// Point the provider at another endpoint: its next run's model states it.
    fn state_endpoint(&self, endpoint: &str) {
        *self.0.provenance.lock().unwrap() = Some(querymt::chat::ChatOutputProvenance {
            provider: "mock".into(),
            protocol: "responses".into(),
            model: "mock-model".into(),
            endpoint: endpoint.into(),
        });
    }

    fn seen(&self) -> Vec<Seen> {
        self.0.seen.lock().unwrap().clone()
    }

    fn divergences(&self) -> Vec<String> {
        self.0.divergences.lock().unwrap().clone()
    }

    /// Compare `call` with the request a fresh load of the session prepares.
    async fn compare_with_reload(&self, index: usize, call: &ChatCall<'_>) {
        let Some((provider, session)) = self.0.reload.get() else {
            return;
        };
        let handle = provider.with_session(session).await.expect("session loads");
        let stored = handle
            .get_effective_agent_history()
            .await
            .expect("history loads");
        // A later run resolves its model first and takes the identity from it.
        // A later run resolves its model first and takes the identity from it.
        let model = handle.resolve_model().await.expect("model resolves");
        let target = handle.output_target(&model, &stored);
        let projected = project_request(
            &stored,
            &RequestTarget {
                output: target.as_ref(),
                max_prompt_bytes: handle
                    .execution_config()
                    .and_then(|config| config.max_prompt_bytes),
                tool_changes: self.0.tool_updates,
            },
        )
        .expect("history projects");
        let (expected_tools, expected) = (projected.baseline_tools, projected.messages);

        let sent = durable(call.messages);
        let reloaded = durable(&expected);
        if let Some(difference) = first_difference(&sent, &reloaded) {
            self.0
                .divergences
                .lock()
                .unwrap()
                .push(format!("request {index}: messages: {difference}"));
        }
        if self.0.tool_updates {
            let sent_tools = serde_json::to_value(call.tools).unwrap();
            let reloaded_tools = serde_json::to_value(expected_tools).unwrap();
            if sent_tools != reloaded_tools {
                self.0.divergences.lock().unwrap().push(format!(
                    "request {index}: tools: sent {sent_tools}, reload {reloaded_tools}"
                ));
            }
        }
    }
}

/// The messages as JSON, without cache hints.
fn durable(messages: &[ChatMessage]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            message.cache = None;
            serde_json::to_value(message).unwrap()
        })
        .collect()
}

fn first_difference(sent: &[serde_json::Value], reloaded: &[serde_json::Value]) -> Option<String> {
    let at = sent.iter().zip(reloaded).position(|(a, b)| a != b);
    match at {
        Some(i) => Some(format!(
            "first difference at message {i}: sent {}, reload {}",
            sent[i], reloaded[i]
        )),
        None if sent.len() != reloaded.len() => Some(format!(
            "sent {} messages, reload {}; the first message only one side has is {}",
            sent.len(),
            reloaded.len(),
            sent.get(reloaded.len())
                .or_else(|| reloaded.get(sent.len()))
                .map_or_else(String::new, ToString::to_string)
        )),
        None => None,
    }
}

#[async_trait]
impl querymt::chat::ChatProvider for ScriptedProvider {
    fn capabilities(&self) -> ChatCapabilities {
        ChatCapabilities::NONE.with_tool_updates(self.0.tool_updates)
    }

    fn output_provenance(&self) -> Option<querymt::chat::ChatOutputProvenance> {
        self.0.provenance.lock().unwrap().clone()
    }

    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        let index = {
            let mut seen = self.0.seen.lock().unwrap();
            seen.push(Seen {
                messages: call.messages.to_vec(),
                tools: call
                    .tools
                    .map(|tools| {
                        tools
                            .iter()
                            .map(|tool| tool.function.name.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
            });
            seen.len() - 1
        };
        self.compare_with_reload(index, call).await;
        if let Some((at, hook)) = self.0.before_reply.get()
            && *at == index
        {
            hook().await;
        }
        Ok((self.0.script)(index))
    }
}

#[async_trait]
impl querymt::completion::CompletionProvider for ScriptedProvider {
    async fn complete(
        &self,
        _: &querymt::completion::CompletionRequest,
    ) -> Result<querymt::completion::CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented("scripted".into()))
    }
}

#[async_trait]
impl querymt::embedding::EmbeddingProvider for ScriptedProvider {
    async fn embed(&self, _: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented("scripted".into()))
    }
}

impl LLMProvider for ScriptedProvider {
    fn tools(&self) -> Option<&[Tool]> {
        Some(&self.0.tools)
    }
}

struct ScriptedFactory(ScriptedProvider);

impl LLMProviderFactory for ScriptedFactory {
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

// ---- scripted replies ----

fn usage(input_tokens: u32) -> Usage {
    Usage {
        input_tokens,
        output_tokens: 5,
        ..Usage::default()
    }
}

/// A reply that calls `write_file` once and reports `used` tokens of context.
fn write_call(index: usize, used: u32) -> ChatOutput {
    ChatOutput::from_projections(
        None,
        Some("writing".into()),
        Some(vec![mock_querymt_tool_call(
            &format!("write-{index}"),
            "write_file",
            &serde_json::json!({"path": format!("step-{index}.txt"), "content": "x"}).to_string(),
        )]),
        Some(usage(used)),
        Some(FinishReason::ToolCalls),
    )
}

fn done(used: u32) -> ChatOutput {
    ChatOutput::from_projections(
        None,
        Some("done".into()),
        None,
        Some(usage(used)),
        Some(FinishReason::Stop),
    )
}

/// A reply with a signed, encrypted reasoning item, text and a `write_file`
/// call. `with_provenance` records where it came from, as the providers that
/// produce native reasoning do (protocol and endpoint included).
fn write_call_with_reasoning(index: usize, with_provenance: bool) -> ChatOutput {
    let mut output = write_call(index, 10);
    output.items.insert(
        0,
        ChatOutputItem::Reasoning(ChatReasoningItem {
            id: Some("reasoning-1".into()),
            summary: Vec::new(),
            content: vec![ChatReasoningPart::text("weighing the options")],
            encrypted_content: Some("enc-1".into()),
            signature: Some("sig-1".into()),
            status: None,
            extensions: Default::default(),
        }),
    );
    output.status = Some(ChatOutputStatus::Completed);
    if with_provenance {
        output.provenance = Some(querymt::chat::ChatOutputProvenance {
            provider: "mock".into(),
            protocol: "responses".into(),
            model: "mock-model".into(),
            endpoint: "https://mock.invalid/v1/responses".into(),
        });
    }
    output
}

// ---- the run ----

struct Run {
    fixture: UndoTestFixture,
    provider: ScriptedProvider,
    session: String,
}

#[derive(Default)]
struct Setup {
    hooks: Option<Hooks>,
    middleware: Vec<Arc<dyn Middleware>>,
    mcp: Option<Arc<McpToolState>>,
}

impl Run {
    async fn new(provider: ScriptedProvider, setup: Setup) -> Self {
        let fixture = UndoTestFixture::with_factory(
            Arc::new(ScriptedFactory(provider.clone())),
            setup.hooks,
            setup.middleware,
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
            .register_session_actor_with_mcp(
                &session,
                &cwd,
                setup.mcp.unwrap_or_else(McpToolState::empty),
            )
            .await;
        provider
            .0
            .reload
            .set((fixture.handle.config.provider.clone(), session.clone()))
            .ok()
            .expect("the reload target is set once");
        Self {
            fixture,
            provider,
            session,
        }
    }

    async fn prompt(&self, text: &str) {
        self.fixture
            .prompt_to_completion(&self.session, text)
            .await
            .expect("run completes");
    }

    /// Accept `text` as steering of the active run while request number `at`
    /// is being answered, so it is waiting when the reply's tool calls start.
    async fn steer_at_request(&self, at: usize, text: &'static str) {
        let hook = self.steering_hook(text).await;
        self.provider
            .0
            .before_reply
            .set((at, hook))
            .ok()
            .expect("one steering hook");
    }

    /// A hook that accepts `text` as steering of the session's active run.
    async fn steering_hook(&self, text: &'static str) -> Hook {
        use crate::acp::protocol::{ContentBlock, TextContent};
        use querymt_agent_core::events::AgentEventKind;

        let actor = self
            .fixture
            .handle
            .session_ref_for_agent_session(&self.session)
            .await
            .expect("session actor");
        let events = Arc::new(tokio::sync::Mutex::new(
            self.fixture.handle.config.subscribe_events(),
        ));
        let session = self.session.clone();
        Arc::new(move || {
            let (actor, events, session) = (actor.clone(), events.clone(), session.clone());
            Box::pin(async move {
                let mut events = events.lock().await;
                let run_id = loop {
                    let envelope = events.try_recv().expect("the run has started");
                    if envelope.session_id() == session
                        && let AgentEventKind::RunStarted { run_id, .. } = envelope.kind()
                    {
                        break run_id.clone();
                    }
                };
                actor
                    .steer(
                        session,
                        run_id,
                        vec![ContentBlock::Text(TextContent::new(text))],
                    )
                    .await
                    .expect("steering is accepted");
            })
        })
    }

    async fn stored(&self) -> Vec<querymt_agent_core::model::AgentMessage> {
        self.fixture
            .storage
            .session_store()
            .get_history(&self.session)
            .await
            .expect("history")
    }

    /// The run made `requests` model requests and every one matched a reload.
    fn assert_matches_a_reload(&self, requests: usize) {
        assert_eq!(
            self.provider.seen().len(),
            requests,
            "number of model requests"
        );
        let divergences = self.provider.divergences();
        assert!(
            divergences.is_empty(),
            "a request differs from a reload of the session:\n{}",
            divergences.join("\n")
        );
    }
}

fn text_of(messages: &[ChatMessage]) -> String {
    serde_json::to_string(messages).unwrap()
}

/// What the stored messages of a run with tool calls and steering are, in
/// order: `calls`, `results:N` and `steering`. Other messages are left out.
fn tool_batch_order(stored: &[querymt_agent_core::model::AgentMessage]) -> Vec<String> {
    stored
        .iter()
        .filter_map(|message| {
            let results = message
                .parts
                .iter()
                .filter(|part| matches!(part, MessagePart::ToolResult { .. }))
                .count();
            if message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::Steering { .. }))
            {
                Some("steering".to_string())
            } else if results > 0 {
                Some(format!("results:{results}"))
            } else if message.parts.iter().any(|part| match part {
                MessagePart::ToolUse(_) => true,
                MessagePart::Output { output } => output
                    .items
                    .iter()
                    .any(|item| matches!(item, ChatOutputItem::FunctionCall(_))),
                _ => false,
            }) {
                Some("calls".to_string())
            } else {
                None
            }
        })
        .collect()
}

/// A reply that calls `write_file` twice.
fn two_write_calls(index: usize) -> ChatOutput {
    let call = |n: usize| {
        mock_querymt_tool_call(
            &format!("write-{index}-{n}"),
            "write_file",
            &serde_json::json!({"path": format!("step-{index}-{n}.txt"), "content": "x"})
                .to_string(),
        )
    };
    ChatOutput::from_projections(
        None,
        Some("writing".into()),
        Some(vec![call(0), call(1)]),
        Some(usage(10)),
        Some(FinishReason::ToolCalls),
    )
}

/// Accepts steering the first time a tool batch is prepared, which is after the
/// run drained its inbox for that batch: the steering arrives while the tools run.
struct SteerDuringBatch(Arc<OnceLock<Hook>>);

impl Middleware for SteerDuringBatch {
    fn name(&self) -> &'static str {
        "SteerDuringBatch"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(SteerInBatch {
            hook: self.0.clone(),
            fired: false,
        })
    }
}

struct SteerInBatch {
    hook: Arc<OnceLock<Hook>>,
    fired: bool,
}

#[async_trait]
impl TurnMiddleware for SteerInBatch {
    async fn on_prepared_tool_calls(
        &mut self,
        _cx: &mut HookCx<'_>,
        batch: &PreparedBatch<'_>,
    ) -> crate::middleware::Result<Vec<PreparedCallVerdict>> {
        if !self.fired
            && let Some(hook) = self.hook.get()
        {
            self.fired = true;
            hook().await;
        }
        Ok(batch
            .calls
            .iter()
            .map(|_| PreparedCallVerdict::Proceed)
            .collect())
    }
}

// ---- scenarios ----

#[tokio::test]
async fn request_after_a_tool_step_matches_a_reload() {
    let provider = ScriptedProvider::new(false, |index| match index {
        0 => write_call(0, 10),
        _ => done(10),
    });
    let run = Run::new(provider, Setup::default()).await;

    run.prompt("run 0").await;

    run.assert_matches_a_reload(2);
}

async fn reasoning_run(with_provenance: bool) {
    let provider = ScriptedProvider::new(false, move |index| match index {
        0 => write_call_with_reasoning(0, with_provenance),
        _ => done(10),
    });
    let run = Run::new(provider, Setup::default()).await;

    run.prompt("run 0").await;

    // The reasoning is replayed to the model that produced it: the request
    // carries the native item, not its portable form.
    let second = run.provider.seen().remove(1);
    assert!(
        text_of(&second.messages).contains("enc-1"),
        "the second request carries the native reasoning: {}",
        text_of(&second.messages)
    );
    run.assert_matches_a_reload(2);
}

/// Output that records its provenance, as every provider with native reasoning
/// does. The session's stored LLM config has no protocol or endpoint, so the
/// replay identity is completed from the latest reply of the same provider and
/// model (`SessionHandle::output_target`); without that a load would never match
/// the provenance and would send the portable form.
#[tokio::test]
async fn request_after_a_reply_with_provider_reasoning_matches_a_reload() {
    reasoning_run(true).await;
}

/// The endpoint the scripted reply with reasoning is stamped with.
const STAMPED_ENDPOINT: &str = "https://mock.invalid/v1/responses";

/// A provider that states its provenance gets its reasoning back natively when
/// the stated identity is the one the reply was stamped with, and the identity
/// comes from the provider, not from the stored reply.
#[tokio::test]
async fn request_after_a_reply_from_a_provider_that_states_its_provenance_matches_a_reload() {
    let provider = ScriptedProvider::new(false, |index| match index {
        0 => write_call_with_reasoning(0, true),
        _ => done(10),
    })
    .stating(STAMPED_ENDPOINT);
    let run = Run::new(provider, Setup::default()).await;

    run.prompt("run 0").await;

    let second = run.provider.seen().remove(1);
    assert!(
        text_of(&second.messages).contains("enc-1"),
        "the second request carries the native reasoning: {}",
        text_of(&second.messages)
    );
    run.assert_matches_a_reload(2);
}

/// Spec `run-history-consistency`: the configured endpoint changes between two
/// requests of one session. The earlier reply was stamped at the old endpoint,
/// so it goes to the new one in portable form, even though the session's
/// latest stored reply of this provider and model carries the old endpoint (the
/// fallback for providers that do not state their identity would trust it).
#[tokio::test]
async fn reply_stamped_at_another_endpoint_is_sent_in_portable_form() {
    let provider = ScriptedProvider::new(false, |index| match index {
        0 => write_call_with_reasoning(0, true),
        1 => done(10),
        _ => done(20),
    })
    .stating(STAMPED_ENDPOINT);
    let run = Run::new(provider, Setup::default()).await;
    run.prompt("run 0").await;
    assert!(
        text_of(&run.provider.seen()[1].messages).contains("enc-1"),
        "at the endpoint it came from, the reply is native"
    );

    run.provider
        .state_endpoint("https://other.invalid/v1/responses");
    run.prompt("run 1").await;

    let third = run.provider.seen().remove(2);
    let sent = text_of(&third.messages);
    assert!(
        !sent.contains("enc-1") && !sent.contains("sig-1"),
        "the earlier reply is portable at another endpoint: {sent}"
    );
    assert!(
        sent.contains("weighing the options"),
        "the visible reasoning is kept: {sent}"
    );
    run.assert_matches_a_reload(3);
}

/// Output from before provenance was recorded replays by provider and model,
/// which a load can match. Guards the part that works today.
#[tokio::test]
async fn request_after_a_reply_with_legacy_reasoning_matches_a_reload() {
    reasoning_run(false).await;
}

#[tokio::test]
async fn request_after_a_stored_tool_change_matches_a_reload() {
    fn mcp_tool(name: &str) -> Tool {
        Tool {
            tool_type: "function".into(),
            function: querymt::chat::FunctionTool {
                name: name.into(),
                description: name.into(),
                parameters: serde_json::json!({"type": "object"}),
                strict: None,
            },
        }
    }
    let mcp = McpToolState::new(Default::default(), vec![mcp_tool("alpha")]);
    // An MCP server adds a tool while the first reply is being produced.
    let appearing = mcp.clone();
    let provider = ScriptedProvider::new(true, move |index| match index {
        0 => {
            appearing.rcu(|snapshot| {
                let mut next = snapshot.clone();
                next.tool_defs.push(mcp_tool("beta"));
                next
            });
            write_call(0, 10)
        }
        _ => done(10),
    });
    let run = Run::new(
        provider,
        Setup {
            mcp: Some(mcp),
            ..Setup::default()
        },
    )
    .await;

    run.prompt("run 0").await;

    let second = run.provider.seen().remove(1);
    assert!(
        second.messages.iter().any(ChatMessage::has_tool_change),
        "the second request carries the tool change: {}",
        text_of(&second.messages)
    );
    run.assert_matches_a_reload(2);
}

/// Spec `provider-resolution`, "MCP tools change between two requests of one
/// run": the model of a run is fixed, the tools it offers are not. A provider
/// without tool-change messages is sent the whole list every time.
#[tokio::test]
async fn mcp_tools_change_between_two_requests_of_one_run() {
    fn mcp_tool(name: &str) -> Tool {
        Tool {
            tool_type: "function".into(),
            function: querymt::chat::FunctionTool {
                name: name.into(),
                description: name.into(),
                parameters: serde_json::json!({"type": "object"}),
                strict: None,
            },
        }
    }
    let mcp = McpToolState::new(Default::default(), vec![mcp_tool("alpha")]);
    // An MCP server adds a tool while the first reply is being produced.
    let appearing = mcp.clone();
    let provider = ScriptedProvider::new(false, move |index| match index {
        0 => {
            appearing.rcu(|snapshot| {
                let mut next = snapshot.clone();
                next.tool_defs.push(mcp_tool("beta"));
                next
            });
            write_call(0, 10)
        }
        _ => done(10),
    });
    let run = Run::new(
        provider,
        Setup {
            mcp: Some(mcp),
            ..Setup::default()
        },
    )
    .await;

    run.prompt("run 0").await;

    let seen = run.provider.seen();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[0].tools.contains(&"alpha".to_string()),
        "{:?}",
        seen[0].tools
    );
    assert!(
        !seen[0].tools.contains(&"beta".to_string()),
        "{:?}",
        seen[0].tools
    );
    assert!(
        seen[1].tools.contains(&"beta".to_string()),
        "the second request offers the new tool: {:?}",
        seen[1].tools
    );
}

#[tokio::test]
async fn request_after_a_stored_notice_matches_a_reload() {
    let provider = ScriptedProvider::new(false, |index| match index {
        0 => write_call(0, 10),
        _ => done(10),
    });
    let run = Run::new(
        provider,
        Setup {
            middleware: vec![Arc::new(
                ScriptedMiddleware::default()
                    .step_start([Act::Notice("check the notice", StepVerdict::Proceed)]),
            )],
            ..Setup::default()
        },
    )
    .await;

    run.prompt("run 0").await;

    assert!(
        text_of(&run.provider.seen()[1].messages).contains("check the notice"),
        "the second request carries the notice"
    );
    run.assert_matches_a_reload(2);
}

fn custom_summary_hooks(summary: &str) -> Hooks {
    Hooks::new(HooksConfig {
        enabled: true,
        pre_compaction: vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![HookHandlerConfig::Command(HookCommandConfig {
                command: format!(
                    "printf '%s' '{}'",
                    serde_json::json!({"hook_specific_output": {
                        "hook_event_name": "pre_compaction",
                        "compaction": {"summary": summary}
                    }})
                ),
                timeout_sec: Some(5),
                ..HookCommandConfig::default()
            })],
        }],
        ..HooksConfig::default()
    })
    .unwrap()
}

#[tokio::test]
async fn request_after_a_mid_run_compaction_matches_a_reload() {
    // The first reply reports 90% of the context, so the loop compacts before
    // its next request.
    let provider = ScriptedProvider::new(false, |index| match index {
        0 => write_call(0, 900),
        _ => done(10),
    });
    let run = Run::new(
        provider,
        Setup {
            hooks: Some(custom_summary_hooks("what happened so far")),
            middleware: vec![Arc::new(ContextMiddleware::new(
                ContextConfig::with_manual_limit(1000),
            ))],
            ..Setup::default()
        },
    )
    .await;

    run.prompt("run 0").await;

    assert!(
        run.stored().await.iter().any(|message| message
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::Compaction { .. }))),
        "the run compacted its history"
    );
    assert!(
        text_of(&run.provider.seen()[1].messages).contains("what happened so far"),
        "the request after the compaction starts from the summary"
    );
    run.assert_matches_a_reload(2);
}

#[tokio::test]
async fn steering_arrives_before_a_tool_batch() {
    let provider = ScriptedProvider::new(false, |index| match index {
        0 => two_write_calls(0),
        _ => done(10),
    });
    let run = Run::new(provider, Setup::default()).await;
    // Accepted while the model answers, so it is applied before either call runs.
    run.steer_at_request(0, "use another file name").await;

    run.prompt("run 0").await;

    let order = tool_batch_order(&run.stored().await);
    let divergences = run.provider.divergences();
    assert!(
        order == ["calls", "results:2", "steering"] && divergences.is_empty(),
        "stored order: {order:?}; a reload sends differently from the run: {divergences:?}"
    );
    assert_eq!(run.provider.seen().len(), 2);
    // Neither call ran: both are skipped.
    assert!(!run.fixture.worktree.path().join("step-0-0.txt").exists());
    assert!(!run.fixture.worktree.path().join("step-0-1.txt").exists());
}

#[tokio::test]
async fn first_request_of_the_next_run_continues_the_previous_one() {
    let provider = ScriptedProvider::new(false, |index| match index {
        0 => write_call(0, 10),
        _ => done(10),
    });
    let run = Run::new(provider, Setup::default()).await;

    run.prompt("run 0").await;
    run.prompt("run 1").await;

    run.assert_matches_a_reload(3);
    let seen = run.provider.seen();
    let last_of_first = durable(&seen[1].messages);
    let first_of_second = durable(&seen[2].messages);
    assert!(first_of_second.len() > last_of_first.len());
    assert_eq!(
        first_difference(&last_of_first, &first_of_second[..last_of_first.len()]),
        None,
        "the next run's first request begins with the previous run's last request"
    );
    assert_eq!(
        seen[2].messages.last().map(|m| (m.role, m.text())),
        Some((ChatRole::User, "run 1".to_string()))
    );
}

/// run-history-consistency / Steering arrives while tools run.
#[tokio::test]
async fn steering_arrives_while_tools_run() {
    let slot = Arc::new(OnceLock::new());
    let provider = ScriptedProvider::new(false, |index| match index {
        0 => write_call(0, 10),
        _ => done(10),
    });
    let run = Run::new(
        provider,
        Setup {
            middleware: vec![Arc::new(SteerDuringBatch(slot.clone()))],
            ..Setup::default()
        },
    )
    .await;
    slot.set(run.steering_hook("use another file name").await)
        .ok()
        .expect("one steering hook");

    run.prompt("run 0").await;

    // The batch finished: its result is stored directly after the call, and
    // the steering after it, before the next model request.
    let order = tool_batch_order(&run.stored().await);
    assert_eq!(order, ["calls", "results:1", "steering"]);
    assert!(run.fixture.worktree.path().join("step-0.txt").exists());
    run.assert_matches_a_reload(2);
    assert!(text_of(&run.provider.seen()[1].messages).contains("use another file name"));
}

/// The compaction in the middle of a run removes what the run had stored; the
/// next request stores it again.
fn compacting_run_setup(extra: Vec<Arc<dyn Middleware>>) -> Setup {
    let mut middleware: Vec<Arc<dyn Middleware>> = extra;
    middleware.push(Arc::new(ContextMiddleware::new(
        ContextConfig::with_manual_limit(1000),
    )));
    Setup {
        hooks: Some(custom_summary_hooks("what happened so far")),
        middleware,
        ..Setup::default()
    }
}

fn count_parts(
    stored: &[querymt_agent_core::model::AgentMessage],
    matches: impl Fn(&MessagePart) -> bool,
) -> usize {
    stored
        .iter()
        .flat_map(|message| &message.parts)
        .filter(|part| matches(part))
        .count()
}

/// run-history-consistency / Mode reminder compacted away is stored again.
#[tokio::test]
async fn mode_reminder_compacted_away_is_stored_again() {
    use crate::agent::core::AgentMode;
    use querymt_agent_core::model::NoticeSource;

    let provider = ScriptedProvider::new(false, |index| match index {
        0 => write_call(0, 900),
        _ => done(10),
    });
    let mut reminders = std::collections::HashMap::new();
    reminders.insert(AgentMode::Plan, "plan reminder".to_string());
    let run = Run::new(
        provider,
        compacting_run_setup(vec![Arc::new(AgentModeMiddleware::with_reminders(
            reminders,
        ))]),
    )
    .await;
    run.fixture
        .handle
        .session_ref_for_agent_session(&run.session)
        .await
        .expect("session actor")
        .set_mode(AgentMode::Plan)
        .await
        .expect("set mode");

    run.prompt("run 0").await;

    let stored = run.stored().await;
    assert!(
        count_parts(&stored, |part| matches!(
            part,
            MessagePart::Compaction { .. }
        )) > 0
    );
    assert_eq!(
        count_parts(&stored, |part| matches!(
            part,
            MessagePart::Notice {
                source: NoticeSource::AgentMode,
                ..
            }
        )),
        2,
        "stored before the compaction and again after it"
    );
    assert!(text_of(&run.provider.seen()[1].messages).contains("plan reminder"));
    run.assert_matches_a_reload(2);
}

/// run-history-consistency / Baseline compacted away is stored again.
#[tokio::test]
async fn baseline_compacted_away_is_stored_again() {
    let provider = ScriptedProvider::new(true, |index| match index {
        0 => write_call(0, 900),
        _ => done(10),
    });
    let run = Run::new(provider, compacting_run_setup(Vec::new())).await;

    run.prompt("run 0").await;

    let stored = run.stored().await;
    assert!(
        count_parts(&stored, |part| matches!(
            part,
            MessagePart::Compaction { .. }
        )) > 0
    );
    assert_eq!(
        count_parts(&stored, |part| matches!(
            part,
            MessagePart::ToolBaseline { .. }
        )),
        2,
        "a baseline before the compaction and a new one after it"
    );
    run.assert_matches_a_reload(2);
}

/// A history stored with the steering message before the results (the order
/// older builds wrote) still loads and prepares a valid request.
#[tokio::test]
async fn history_in_the_old_steering_order_still_loads() {
    use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};
    use querymt_agent_core::projection::{RequestTarget, project_request};

    let fixture =
        TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![])).await;
    let (env, _turn) = fixture.turn().await;
    let stored = |role: ChatRole, parts: Vec<MessagePart>| {
        let mut message =
            querymt_agent_core::model::AgentMessage::new(env.session_id.clone(), role);
        message.parts = parts;
        message
    };
    let call = querymt::ToolCall {
        id: "call-1".into(),
        call_type: "function".into(),
        function: querymt::FunctionCall {
            name: "read_file".into(),
            arguments: "{}".into(),
        },
    };
    for message in [
        stored(
            ChatRole::User,
            vec![MessagePart::Text {
                content: "go".into(),
            }],
        ),
        stored(ChatRole::Assistant, vec![MessagePart::ToolUse(call)]),
        // The old order: steering first, then the skipped result.
        stored(
            ChatRole::User,
            vec![MessagePart::Steering {
                run_id: "run".into(),
                client_input_id: None,
                blocks: vec![crate::acp::protocol::ContentBlock::Text(
                    crate::acp::protocol::TextContent::new("use another file name"),
                )],
            }],
        ),
        stored(
            ChatRole::User,
            vec![MessagePart::ToolResult {
                call_id: "call-1".into(),
                content: vec![querymt::chat::ToolResultPart::text("Skipped")],
                is_error: true,
                tool_name: Some("read_file".into()),
                tool_arguments: None,
                compacted_at: None,
            }],
        ),
    ] {
        env.session_handle.add_message(message).await.unwrap();
    }

    let loaded = env
        .session_handle
        .get_effective_agent_history()
        .await
        .unwrap();
    let mut request = project_request(
        &loaded,
        &RequestTarget {
            output: None,
            max_prompt_bytes: None,
            tool_changes: false,
        },
    )
    .unwrap()
    .messages;

    assert!(
        querymt_agent_core::model::answer_unanswered_tool_calls(&mut request).is_empty(),
        "every call is answered: {}",
        text_of(&request)
    );
    // The result follows the call in one message, with the steering text after it.
    assert_eq!(request.len(), 3);
    assert!(request[1].has_tool_use());
    assert!(request[2].has_tool_result());
    assert!(request[2].text().contains("use another file name"));
}

/// The provider is resolved when the run starts, after the user's prompt is
/// stored: a provider that cannot be built fails the run with a provider
/// failure, and the prompt is still in the session's history.
#[tokio::test]
async fn provider_failure_at_run_start_is_reported_after_the_prompt_is_stored() {
    use crate::send_agent::SendAgent;
    use agent_client_protocol::ErrorCode;

    let fixture = UndoTestFixture::with_factory(
        Arc::new(crate::test_utils::UnbuildableProviderFactory),
        None,
        vec![],
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
        .register_session_actor_with_mcp(&session, &cwd, McpToolState::empty())
        .await;

    let request = crate::acp::protocol::PromptRequest::new(
        session.clone(),
        vec![crate::acp::protocol::ContentBlock::Text(
            crate::acp::protocol::TextContent::new("hello"),
        )],
    );
    let error = SendAgent::prompt(&fixture.handle, request)
        .await
        .expect_err("a provider that cannot be built fails the run");

    // The structured provider failure reaches the client, not a plain
    // internal error.
    assert_eq!(error.code, ErrorCode::new(-32010), "{error:?}");
    let history = store.get_history(&session).await.expect("history");
    let prompts: Vec<_> = history
        .iter()
        .filter(|message| message.role == ChatRole::User)
        .collect();
    assert_eq!(prompts.len(), 1, "the prompt was stored: {history:?}");
}

// ---- a run that panics ----

/// The `outcome` of every `RunCompleted` of `session`, in order, once `count`
/// of them were published.
async fn run_outcomes(
    events: &mut tokio::sync::broadcast::Receiver<
        std::sync::Arc<querymt_agent_core::events::EventEnvelope>,
    >,
    session: &str,
    count: usize,
) -> Vec<String> {
    use querymt_agent_core::events::AgentEventKind;

    let mut outcomes = Vec::new();
    let wait = async {
        while outcomes.len() < count {
            let envelope = events.recv().await.expect("event stream");
            if envelope.session_id() == session
                && let AgentEventKind::RunCompleted { outcome, .. } = envelope.kind()
            {
                outcomes.push(outcome.clone());
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), wait)
        .await
        .unwrap_or_else(|_| panic!("timed out; saw {outcomes:?}"));
    outcomes
}

/// A run whose middleware panics at its start.
async fn run_that_panics_once() -> Run {
    let panics_once =
        ScriptedMiddleware::default().turn_start([Act::Panic, Act::Say(StepVerdict::Proceed)]);
    Run::new(
        ScriptedProvider::new(false, |_| done(10)),
        Setup {
            middleware: vec![Arc::new(panics_once)],
            ..Setup::default()
        },
    )
    .await
}

/// A panic is the run's failure: the client gets an error, the run is closed
/// as failed, and the session takes the next prompt.
#[tokio::test]
async fn a_panicking_run_fails_and_the_session_takes_the_next_prompt() {
    let run = run_that_panics_once().await;
    let mut events = run.fixture.handle.config.subscribe_events();

    let error = run
        .fixture
        .prompt(&run.session, "boom")
        .await
        .expect_err("a panicking run fails its prompt");
    assert!(format!("{error:#}").contains("run panicked"), "{error:#}");
    assert!(
        !format!("{error:#}").contains("dropped"),
        "the run was not lost: {error:#}"
    );
    assert_eq!(run_outcomes(&mut events, &run.session, 1).await, ["failed"]);

    // The slot is free again: a second prompt runs to completion.
    run.prompt("after").await;
    assert_eq!(run.provider.seen().len(), 1, "one model request, by run 2");
}

/// A prompt queued behind the run that panics starts when it is closed.
#[tokio::test]
async fn a_panicking_run_hands_over_to_the_queued_prompt() {
    use crate::send_agent::SendAgent;

    use querymt_agent_core::events::AgentEventKind;

    // The first run is held at its start until the second prompt is queued
    // behind it, then released to panic.
    let gate = Arc::new(tokio::sync::Notify::new());
    let panics_once = ScriptedMiddleware::default().turn_start([
        Act::PanicAfter(gate.clone()),
        Act::Say(StepVerdict::Proceed),
    ]);
    let run = Run::new(
        ScriptedProvider::new(false, |_| done(10)),
        Setup {
            middleware: vec![Arc::new(panics_once)],
            ..Setup::default()
        },
    )
    .await;
    let mut events = run.fixture.handle.config.subscribe_events();
    let request = |text: &str| {
        crate::acp::protocol::PromptRequest::new(
            run.session.clone(),
            vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new(text),
            )],
        )
    };

    let release_once_queued = async {
        while let Ok(envelope) = events.recv().await {
            if envelope.session_id() == run.session
                && matches!(envelope.kind(), AgentEventKind::InputQueued { .. })
            {
                gate.notify_one();
                return;
            }
        }
        panic!("the event stream ended before a prompt was queued");
    };
    let (first, second, ()) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        tokio::join!(
            SendAgent::prompt(&run.fixture.handle, request("boom")),
            SendAgent::prompt(&run.fixture.handle, request("queued")),
            release_once_queued,
        )
    })
    .await
    .expect("both prompts finish: the queued one is started when the run fails");

    let results = [first.is_ok(), second.is_ok()];
    assert_eq!(
        results.iter().filter(|ok| **ok).count(),
        1,
        "one run panicked and one completed: {first:?} {second:?}"
    );
    assert_eq!(
        run.provider.seen().len(),
        1,
        "the queued prompt made the one model request"
    );
}

/// A run that changed files and then panicked still records its workspace
/// changes, so the turn can be undone.
#[tokio::test]
async fn a_panicking_run_still_records_its_workspace_changes() {
    use querymt_agent_core::model::MessagePart;

    // The first reply writes a file; the panic comes after the second reply.
    let panics_later =
        ScriptedMiddleware::default().after_llm([Act::Say(ReplyVerdict::Proceed), Act::Panic]);
    let run = Run::new(
        ScriptedProvider::new(false, |index| match index {
            0 => write_call(0, 10),
            _ => done(10),
        }),
        Setup {
            middleware: vec![Arc::new(panics_later)],
            ..Setup::default()
        },
    )
    .await;

    let error = run
        .fixture
        .prompt(&run.session, "write then panic")
        .await
        .expect_err("the run panics");
    assert!(format!("{error:#}").contains("run panicked"), "{error:#}");
    assert!(run.fixture.worktree.path().join("step-0.txt").exists());

    let patches: Vec<Vec<String>> = run
        .stored()
        .await
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            MessagePart::TurnSnapshotPatch { changed_paths, .. } => Some(changed_paths.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(patches.len(), 1, "one patch for the turn: {patches:?}");
    assert!(
        patches[0].iter().any(|path| path.ends_with("step-0.txt")),
        "the patch lists the written file: {patches:?}"
    );
}

/// Steering that a failed run accepted but did not apply is discarded, not
/// left without a terminal event.
#[tokio::test]
async fn steering_a_failed_run_did_not_apply_is_discarded() {
    use querymt_agent_core::events::AgentEventKind;

    // The reply is followed by a panic before the run's next steering boundary.
    let panics = ScriptedMiddleware::default().after_llm([Act::Panic]);
    let run = Run::new(
        ScriptedProvider::new(false, |_| done(10)),
        Setup {
            middleware: vec![Arc::new(panics)],
            ..Setup::default()
        },
    )
    .await;
    run.steer_at_request(0, "too late").await;
    let mut events = run.fixture.handle.config.subscribe_events();

    run.fixture
        .prompt(&run.session, "boom")
        .await
        .expect_err("the run panics");

    let mut accepted = Vec::new();
    let mut terminal = Vec::new();
    let mut run_completed = false;
    let drain = async {
        while !run_completed {
            let envelope = events.recv().await.expect("event stream");
            if envelope.session_id() != run.session {
                continue;
            }
            match envelope.kind() {
                AgentEventKind::SteeringAccepted { input_id, .. } => {
                    accepted.push(input_id.clone())
                }
                AgentEventKind::SteeringApplied { input_id, .. } => {
                    terminal.push((input_id.clone(), "applied".to_string()))
                }
                AgentEventKind::SteeringDiscarded {
                    input_id, reason, ..
                } => terminal.push((input_id.clone(), reason.clone())),
                AgentEventKind::RunCompleted { .. } => run_completed = true,
                _ => {}
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), drain)
        .await
        .expect("the run completes");

    assert_eq!(accepted.len(), 1, "one steering input was accepted");
    assert_eq!(
        terminal,
        [(accepted[0].clone(), "run_failed".to_string())],
        "the accepted input ends with exactly one terminal event"
    );
}

/// A scheduled run reports a panic like any failure: the scheduler is told,
/// and the run is closed.
#[tokio::test]
async fn a_panicking_scheduled_run_emits_scheduled_failed() {
    use querymt_agent_core::events::AgentEventKind;

    let run = run_that_panics_once().await;
    // A scheduled run needs the active task it was fired for.
    let store = run.fixture.storage.session_store();
    let internal_id = store
        .get_session(&run.session)
        .await
        .expect("session lookup")
        .expect("session exists")
        .id;
    let now = time::OffsetDateTime::now_utc();
    store
        .create_task(crate::session::domain::Task {
            id: 0,
            public_id: "task-1".into(),
            session_id: internal_id,
            kind: crate::session::domain::TaskKind::Recurring,
            status: crate::session::domain::TaskStatus::Active,
            expected_deliverable: Some("scheduled work".into()),
            acceptance_criteria: None,
            revision: 1,
            creation_key: None,
            completion_evidence: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("task is created");
    let mut events = run.fixture.handle.config.subscribe_events();
    let actor = run
        .fixture
        .handle
        .session_ref_for_agent_session(&run.session)
        .await
        .expect("session actor");

    let cycle_run = querymt_agent_core::model::TurnId::new();
    actor
        .tell_scheduled_prompt(crate::agent::messages::ScheduledPrompt {
            schedule_public_id: "schedule-1".into(),
            turn_id: cycle_run,
            task_public_id: "task-1".into(),
            prompt_text: "scheduled work".into(),
            execution_limits: None,
            origin: crate::session::domain_schedule::ScheduleOrigin::Local,
        })
        .await
        .expect("scheduled prompt is accepted");

    let mut failure = None;
    let wait = async {
        loop {
            let envelope = events.recv().await.expect("event stream");
            if envelope.session_id() != run.session {
                continue;
            }
            if let AgentEventKind::ScheduledExecutionFailed {
                schedule_public_id,
                turn_id,
                error,
            } = envelope.kind()
            {
                failure = Some((schedule_public_id.clone(), turn_id.clone(), error.clone()));
                break;
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), wait)
        .await
        .expect("a scheduled failure is published");
    let (schedule, turn_id, error) = failure.unwrap();
    assert_eq!(schedule, "schedule-1");
    // The failure names the run the scheduler fired, so it cannot be taken
    // for the outcome of another cycle.
    assert_eq!(turn_id, Some(cycle_run.to_string()));
    assert!(error.contains("run panicked"), "{error}");
    assert_eq!(run_outcomes(&mut events, &run.session, 1).await, ["failed"]);
}

// ---- prompt response identity ----

/// The `_meta.querymt.messageId` of a prompt response.
fn response_message_id(response: &crate::acp::protocol::PromptResponse) -> Option<String> {
    response
        .meta
        .as_ref()?
        .get("querymt")?
        .get("messageId")?
        .as_str()
        .map(str::to_owned)
}

#[tokio::test]
async fn response_matches_the_echo() {
    use querymt_agent_core::events::AgentEventKind;

    let run = Run::new(ScriptedProvider::new(false, |_| done(10)), Setup::default()).await;
    let mut events = run.fixture.handle.config.subscribe_events();
    let response = run
        .fixture
        .prompt_to_completion(&run.session, "hello")
        .await
        .expect("run completes");

    let message_id = response_message_id(&response).expect("the response names the message");
    let mut echoed = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        if envelope.session_id() != run.session {
            continue;
        }
        if let AgentEventKind::UserPromptBlock { message_id, .. } = envelope.kind() {
            echoed.push(message_id.clone());
        }
    }
    assert!(!echoed.is_empty(), "the prompt is echoed as user chunks");
    assert!(echoed.iter().all(|id| *id == message_id), "{echoed:?}");
}

#[tokio::test]
async fn replay_keeps_the_id() {
    use crate::acp::protocol::SessionUpdate;
    use querymt_agent_core::events::{AgentEvent, AgentEventKind};

    let run = Run::new(ScriptedProvider::new(false, |_| done(10)), Setup::default()).await;
    let response = run
        .fixture
        .prompt_to_completion(&run.session, "hello")
        .await
        .expect("run completes");
    let message_id = response_message_id(&response).expect("the response names the message");

    let durable = run
        .fixture
        .storage
        .event_journal()
        .load_session_stream(&run.session, None, None)
        .await
        .expect("durable events");
    let stored: Vec<&String> = durable
        .iter()
        .filter_map(|event| match &event.kind {
            AgentEventKind::PromptReceived {
                message_id: Some(id),
                ..
            } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(stored, [&message_id], "the journal keeps the id");

    let replay = crate::acp::shared::replay_agent_events_to_session_notifications(
        &run.session,
        durable.into_iter().map(AgentEvent::from),
    );
    let replayed: Vec<String> = replay
        .iter()
        .filter_map(|notification| match &notification.update {
            SessionUpdate::UserMessageChunk(chunk) => {
                chunk.message_id.as_ref().map(|id| id.to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(replayed, [message_id]);
}

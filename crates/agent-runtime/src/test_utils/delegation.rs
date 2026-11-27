//! Scripted-provider helpers for delegation tests: a parent session driven by
//! a mock provider, real local delegate handles with scripted behaviours, and
//! the middleware stubs that produce them.

use crate::agent::agent_config::AgentConfig;
use crate::agent::core::ToolPolicy;
use crate::agent::execution::CycleOutcome;
use crate::agent::execution::turn::{Turn, TurnEnv};
use crate::delegation::roster::{Member, MemberAgent, MemberOrigin};
use crate::delegation::{AgentInfo, DelegationOrchestrator};
use crate::middleware::{HookCx, Middleware, StepVerdict, Stop, TurnInfo, TurnMiddleware};
use crate::session::backend::StorageBackend;
use crate::session::runtime::RuntimeContext;
use crate::session::sqlite_storage::SqliteStorage;
use crate::session::store::{SessionExecutionConfig, SessionStore};
use crate::test_utils::{
    Act, MockChatResponse, MockLlmProvider, ScriptedMiddleware, SharedLlmProvider,
    TestProviderFactory, mock_plugin_registry, mock_plugin_registry_in, mock_querymt_tool_call,
};
use mockall::Sequence;
use querymt::LLMParams;
use querymt::chat::ReasoningEffort;
use querymt_agent_core::events::StopType;
use querymt_agent_core::model::AgentMessage;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify};

/// Stops before the first model request with `StepLimit`, simulating a
/// delegate stopped by middleware before completing its work. `StepLimit`
/// (maps to `StopReason::MaxTurnRequests`) rather than `ContextThreshold`
/// avoids triggering the auto-compaction loop.
fn always_stop() -> ScriptedMiddleware {
    ScriptedMiddleware::default().step_start([Act::Say(StepVerdict::Stop(Stop::new(
        StopType::StepLimit,
        "Step limit reached",
    )))])
}

/// Fires `ContextThreshold` on the first step, then passes through. This
/// simulates a full context window that triggers AI compaction.
fn context_threshold_once() -> ScriptedMiddleware {
    ScriptedMiddleware::default().step_start([
        Act::Say(StepVerdict::Stop(Stop::new(
            StopType::ContextThreshold,
            "Context token threshold reached, requesting compaction",
        ))),
        Act::Say(StepVerdict::Proceed),
    ])
}

/// Hold a delegate before its first LLM call to make cancellation deterministic.
#[derive(Clone)]
pub struct CancellationGateMiddleware {
    pub entered: Arc<Notify>,
    pub release: Arc<Notify>,
}

impl Middleware for CancellationGateMiddleware {
    fn name(&self) -> &'static str {
        "CancellationGateMiddleware"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl TurnMiddleware for CancellationGateMiddleware {
    async fn on_step_start(
        &mut self,
        _cx: &mut HookCx<'_>,
    ) -> crate::middleware::Result<StepVerdict> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(StepVerdict::Proceed)
    }
}

#[derive(Debug, Clone)]
pub enum DelegateBehavior {
    AlwaysOk,
    AlwaysFail,
    PauseBeforeLlmCall {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    },
    /// Delegate's execution is stopped by middleware (e.g. context threshold).
    /// This simulates a premature stop that should be treated as a delegation failure.
    StoppedByMiddleware,
    /// The delegate's session-start hook fails, so a new delegate session is
    /// created (and owns its workspace) but its finalization fails after the
    /// actor started.
    SessionStartHookFails,
    /// Delegate hits ContextThreshold, auto-compaction runs and succeeds, then
    /// the delegate resumes and completes normally. Verifies that the delegation
    /// orchestrator sees `EndTurn` (success) when compaction recovers the session.
    ContextThresholdCompactionSucceeds,
    /// Delegate hits ContextThreshold, auto-compaction runs but the LLM call
    /// fails. The state machine falls through to `Stopped(MaxTokens)`, which
    /// the delegation orchestrator must treat as a failure.
    ContextThresholdCompactionFails,
}

/// One member of a workspace harness: id, behaviour and an optional MCP
/// attachment source.
pub type HarnessMember = (
    String,
    DelegateBehavior,
    Option<Arc<dyn crate::agent::session_mcp::SessionMcpAttachmentSource>>,
);

/// A real git repository and the managed-workspace service delegates share,
/// for isolated-delegation tests.
pub struct WorkspaceFixture {
    pub service: Arc<crate::worktree::WorkspaceService>,
    pub repo: TempDir,
    pub _storage_root: TempDir,
}

pub struct TestHarness {
    pub config: Arc<AgentConfig>,
    /// The root handle the orchestrator is weakly bound to.
    pub delegator: Arc<crate::agent::LocalAgentHandle>,
    pub(crate) env: TurnEnv,
    pub(crate) turn: Turn,
    pub provider: Arc<Mutex<MockLlmProvider>>,
    pub orchestrator_handle: Option<tokio::task::JoinHandle<()>>,
    pub workspace: Option<WorkspaceFixture>,
    pub storage: Arc<SqliteStorage>,
    pub _temp_dir: TempDir,
}

impl TestHarness {
    pub async fn new(history: Vec<AgentMessage>, behavior: DelegateBehavior) -> Self {
        Self::new_with_delegate_reasoning(history, behavior, None).await
    }

    pub async fn new_with_delegate_reasoning(
        history: Vec<AgentMessage>,
        behavior: DelegateBehavior,
        delegate_reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        let members = ["agent", "agent1", "agent2"]
            .map(|id| (id.to_string(), behavior.clone(), None))
            .to_vec();
        Self::build(history, members, delegate_reasoning_effort, false, None).await
    }

    /// A harness whose parent runs `hooks` (for example a `pre_delegation`
    /// command that records its input).
    pub async fn new_with_hooks(behavior: DelegateBehavior, hooks: crate::hooks::Hooks) -> Self {
        let members = ["agent", "agent1", "agent2"]
            .map(|id| (id.to_string(), behavior.clone(), None))
            .to_vec();
        Self::build(vec![], members, None, false, Some(hooks)).await
    }

    /// A harness whose parent session runs in a real git repository and whose
    /// delegates and orchestrator share a managed-workspace service, so
    /// isolated delegation allocates real worktrees.
    pub async fn new_with_workspace(behavior: DelegateBehavior) -> Self {
        let members = ["agent", "agent1", "agent2"]
            .map(|id| (id.to_string(), behavior.clone(), None))
            .to_vec();
        Self::new_with_workspace_members(members).await
    }

    /// [`Self::new_with_workspace`] with explicit members: each has its own
    /// behaviour and optionally an MCP attachment source (to inject a start
    /// failure).
    pub async fn new_with_workspace_members(members: Vec<HarnessMember>) -> Self {
        Self::build(vec![], members, None, true, None).await
    }

    /// [`Self::new_with_workspace_members`] whose root runs `hooks` (the
    /// orchestrator's delegation hooks).
    pub async fn new_with_workspace_members_and_hooks(
        members: Vec<HarnessMember>,
        hooks: crate::hooks::Hooks,
    ) -> Self {
        Self::build(vec![], members, None, true, Some(hooks)).await
    }

    async fn build(
        history: Vec<AgentMessage>,
        members: Vec<HarnessMember>,
        delegate_reasoning_effort: Option<ReasoningEffort>,
        with_workspace: bool,
        hooks: Option<crate::hooks::Hooks>,
    ) -> Self {
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared_provider = SharedLlmProvider {
            inner: provider.clone(),
            tools: Vec::new().into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared_provider));
        let (registry, temp_dir) = mock_plugin_registry(factory).expect("plugin registry");
        let registry = Arc::new(registry);

        // Use a single shared in-memory SQLite store for orchestrator and delegates.
        let shared_storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("shared sqlite storage"),
        );
        let store: Arc<dyn SessionStore> = shared_storage.clone();

        let provider_context = crate::session::provider::SessionProvider::new(
            registry.clone(),
            store.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .with_agent_id(Some("parent".into()));
        let provider_context = Arc::new(provider_context);

        let workspace = if with_workspace {
            let repo = crate::worktree::test_support::init_repo();
            let files = [("README.md", "hello\n")];
            crate::worktree::test_support::commit_files(repo.path(), "initial", &files);
            crate::worktree::test_support::materialize(repo.path(), &files);
            let storage_root = TempDir::new().expect("workspace storage root");
            let service = Arc::new(crate::worktree::WorkspaceService::new(
                Arc::new(crate::worktree::GixRepoOps::new()),
                shared_storage
                    .workspace_repository()
                    .expect("sqlite storage exposes a workspace repository"),
                storage_root.path(),
            ));
            Some(WorkspaceFixture {
                service,
                repo,
                _storage_root: storage_root,
            })
        } else {
            None
        };
        let workspace_service = workspace.as_ref().map(|fixture| fixture.service.clone());

        // Create the parent session via SessionProvider so LLM config is set up.
        let context = provider_context
            .create_session(
                workspace
                    .as_ref()
                    .map(|fixture| fixture.repo.path().to_path_buf()),
                None,
                &SessionExecutionConfig::default(),
            )
            .await
            .expect("create parent session");
        let session_id = context.session().public_id.clone();

        // Seed any initial history into the real store.
        for msg in &history {
            store
                .add_message(&session_id, msg.clone())
                .await
                .expect("seed history message");
        }

        let mut runtime_context = RuntimeContext::new(store.clone(), session_id.clone())
            .await
            .expect("runtime context");
        runtime_context
            .load_working_context()
            .await
            .expect("load context");

        // Create delegate agents as real LocalAgentHandles.
        // All delegates share the same in-memory SQLite store so parent_session_id
        // resolves correctly when creating delegation sessions.
        let roster =
            crate::delegation::roster::test_roster_on(registry.clone(), shared_storage.clone());
        for (id, behavior, attachments) in members {
            let delegate_handle = build_delegate_handle_in(
                behavior,
                store.clone(),
                delegate_reasoning_effort,
                workspace_service.clone(),
                attachments,
            )
            .await;
            roster
                .add(Member::built(
                    agent_info(&id),
                    MemberOrigin::Config,
                    MemberAgent::Local(delegate_handle),
                ))
                .expect("unique delegate id");
        }

        let mut config_builder =
            crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
                shared_storage.clone(),
                provider_context,
                shared_storage.event_journal(),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly)
            .with_roster(roster.clone());
        if let Some(service) = &workspace_service {
            config_builder = config_builder.with_workspace_service(service.clone());
        }
        if let Some(hooks) = hooks {
            config_builder = config_builder.with_hooks(hooks);
        }
        let config = Arc::new(config_builder.build());

        // The root handle owns its own session registry.
        let delegator = Arc::new(crate::agent::LocalAgentHandle::from_config(config.clone()));

        let mut orchestrator =
            DelegationOrchestrator::new(&delegator, roster, &Default::default(), None)
                .with_delegate_model_overrides(config.delegate_model_overrides.clone());
        if let Some(service) = &workspace_service {
            orchestrator = orchestrator.with_workspace_service(service.clone());
        }
        let orchestrator = Arc::new(orchestrator);
        let orchestrator_handle = Some(orchestrator.start_listening(config.event_sink.fanout()));

        // Create a SessionRuntime for the execution context
        let session_runtime = crate::agent::core::SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );

        let (env, turn) =
            crate::test_utils::turn_parts(&config, context, session_runtime, runtime_context).await;

        Self {
            config,
            delegator,
            env,
            turn,
            provider,
            orchestrator_handle,
            workspace,
            storage: shared_storage,
            _temp_dir: temp_dir,
        }
    }

    pub fn stop_harness_orchestrator(&mut self) {
        if let Some(handle) = self.orchestrator_handle.take() {
            handle.abort();
        }
    }

    pub async fn run(&mut self) -> CycleOutcome {
        self.turn.history = crate::agent::execution::turn::History::load(&self.env.session_handle)
            .await
            .expect("history loads");
        crate::agent::execution::run_turn(
            &self.config,
            &self.env,
            &mut self.turn,
            crate::agent::core::AgentMode::Build,
        )
        .await
        .expect("state machine")
    }

    pub async fn set_parent_reasoning_effort(&self, effort: Option<ReasoningEffort>) {
        let store = self.config.provider.history_store();
        let current = store
            .get_session_llm_config(&self.env.session_id)
            .await
            .expect("parent config lookup")
            .expect("parent config");
        let mut params = current
            .params
            .map(serde_json::from_value::<LLMParams>)
            .transpose()
            .expect("deserialize parent params")
            .unwrap_or_default()
            .provider(&current.provider)
            .model(&current.model);
        params.reasoning_effort = effort;
        let updated = store
            .create_or_get_llm_config(&params)
            .await
            .expect("create parent config");
        store
            .set_session_llm_config(&self.env.session_id, updated.id)
            .await
            .expect("set parent config");
    }

    pub async fn expect_single_delegation(&mut self) {
        let delegate_call = mock_querymt_tool_call(
            "call-1",
            "delegate",
            r#"{"target_agent_id":"agent","objective":"task"}"#,
        );
        let mut seq = Sequence::new();
        self.provider_mut()
            .await
            .expect_chat()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move |_| {
                Ok(
                    MockChatResponse::with_tools("Delegating task", vec![delegate_call.clone()])
                        .into(),
                )
            });
        self.provider_mut()
            .await
            .expect_chat()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_| Ok(MockChatResponse::text_only("Done").into()));
        self.provider_mut()
            .await
            .expect_call_tool()
            .returning(|_, _| {
                Ok(vec![querymt::chat::ToolResultPart::Text {
                    text: "ok".to_string(),
                }])
            })
            .times(1);
        self.provider_mut()
            .await
            .expect_tools()
            .return_const(None)
            .times(0..);
    }

    pub async fn run_single_delegation(&mut self) -> CycleOutcome {
        self.expect_single_delegation().await;
        self.run().await
    }

    pub async fn run_single_delegation_without_child_tool(&mut self) -> CycleOutcome {
        let delegate_call = mock_querymt_tool_call(
            "call-1",
            "delegate",
            r#"{"target_agent_id":"agent","objective":"task"}"#,
        );
        let mut seq = Sequence::new();
        self.provider_mut()
            .await
            .expect_chat()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move |_| {
                Ok(
                    MockChatResponse::with_tools("Delegating task", vec![delegate_call.clone()])
                        .into(),
                )
            });
        self.provider_mut()
            .await
            .expect_chat()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_| Ok(MockChatResponse::text_only("Done").into()));
        self.provider_mut()
            .await
            .expect_call_tool()
            .returning(|_, _| {
                Ok(vec![querymt::chat::ToolResultPart::Text {
                    text: "ok".to_string(),
                }])
            })
            .times(1);
        self.provider_mut()
            .await
            .expect_tools()
            .return_const(None)
            .times(0..);

        self.run().await
    }

    pub async fn child_sessions(&self) -> Vec<String> {
        self.config
            .provider
            .history_store()
            .list_child_sessions(&self.env.session_id)
            .await
            .expect("child sessions")
    }

    pub async fn session_lifecycle(
        &self,
        session_id: &str,
    ) -> crate::session::lifecycle::SessionLifecycle {
        self.config
            .provider
            .history_store()
            .get_session_lifecycle(session_id)
            .await
            .expect("session lifecycle")
    }

    pub async fn child_llm_params(&self) -> (String, LLMParams) {
        let store = self.config.provider.history_store();
        let child_sessions = store
            .list_child_sessions(&self.env.session_id)
            .await
            .expect("child sessions");
        let child_session_id = child_sessions.first().expect("child session id");
        let config = store
            .get_session_llm_config(child_session_id)
            .await
            .expect("child config lookup")
            .expect("child config");
        let params = config
            .params
            .map(serde_json::from_value::<LLMParams>)
            .transpose()
            .expect("deserialize child params")
            .unwrap_or_default();

        (config.model, params)
    }

    pub async fn provider_mut(&self) -> tokio::sync::MutexGuard<'_, MockLlmProvider> {
        self.provider.lock().await
    }
}

/// Build a delegate `AgentHandle` that shares the given store with the
/// orchestrator, ensuring parent_session_id resolves correctly.
pub async fn build_delegate_handle(
    behavior: DelegateBehavior,
    shared_store: Arc<dyn SessionStore>,
    reasoning_effort: Option<ReasoningEffort>,
) -> Arc<crate::agent::LocalAgentHandle> {
    build_delegate_handle_in(behavior, shared_store, reasoning_effort, None, None).await
}

/// [`build_delegate_handle`] that can run isolated delegations: it shares the
/// given managed-workspace service.
pub async fn build_delegate_handle_in(
    behavior: DelegateBehavior,
    shared_store: Arc<dyn SessionStore>,
    reasoning_effort: Option<ReasoningEffort>,
    workspace_service: Option<Arc<crate::worktree::WorkspaceService>>,
    attachments: Option<Arc<dyn crate::agent::session_mcp::SessionMcpAttachmentSource>>,
) -> Arc<crate::agent::LocalAgentHandle> {
    let delegate_provider = Arc::new(Mutex::new(MockLlmProvider::new()));
    {
        let mut mock = delegate_provider.lock().await;
        match &behavior {
            DelegateBehavior::AlwaysOk | DelegateBehavior::SessionStartHookFails => {
                mock.expect_chat()
                    .times(0..)
                    .returning(|_| Ok(MockChatResponse::text_only("Task complete").into()));
            }
            DelegateBehavior::AlwaysFail => {
                mock.expect_chat().times(0..).returning(|_| {
                    Err(querymt::error::LLMError::ProviderError(
                        "Invalid patch: line mismatch".to_string(),
                    ))
                });
            }
            DelegateBehavior::PauseBeforeLlmCall { .. } => {
                mock.expect_chat()
                    .times(0..)
                    .returning(|_| Ok(MockChatResponse::text_only("Task complete").into()));
            }
            DelegateBehavior::StoppedByMiddleware => {
                // The LLM will never be called because the middleware stops execution
                // before the LLM call. Set up a fallback that would succeed if reached.
                mock.expect_chat()
                    .times(0..)
                    .returning(|_| Ok(MockChatResponse::text_only("Task complete").into()));
            }
            DelegateBehavior::ContextThresholdCompactionSucceeds => {
                // ContextThresholdOnceMiddleware fires ContextThreshold on the first
                // BeforeLlmCall. The state machine then calls run_ai_compaction which
                // calls provider.chat() for the compaction summary. After that succeeds,
                // the state machine loops back and the middleware passes through, so
                // provider.chat() is called again for the normal conversation turn.
                let mut seq = Sequence::new();
                // 1st chat call: compaction summary
                mock.expect_chat()
                    .times(1)
                    .in_sequence(&mut seq)
                    .returning(|_| {
                        Ok(
                            MockChatResponse::text_only(
                                "Summary of previous conversation context.",
                            )
                            .into(),
                        )
                    });
                // 2nd chat call: normal delegate completion
                mock.expect_chat()
                    .times(1)
                    .in_sequence(&mut seq)
                    .returning(|_| Ok(MockChatResponse::text_only("Task complete").into()));
            }
            DelegateBehavior::ContextThresholdCompactionFails => {
                // ContextThresholdOnceMiddleware fires ContextThreshold. The state
                // machine calls run_ai_compaction which calls provider.chat() — this
                // fails. The state machine falls through to Stopped(MaxTokens).
                // No retries: we set max_retries=0 in the compaction config.
                mock.expect_chat().times(0..).returning(|_| {
                    Err(querymt::error::LLMError::ProviderError(
                        "Compaction LLM call failed: service unavailable".to_string(),
                    ))
                });
            }
        }
        mock.expect_tools().return_const(None).times(0..);
    }

    let delegate_shared = SharedLlmProvider {
        inner: delegate_provider,
        tools: Vec::new().into_boxed_slice(),
    };
    let delegate_factory = Arc::new(TestProviderFactory::new(delegate_shared));

    let delegate_temp_dir = TempDir::new().expect("temp dir");
    let delegate_plugin_registry = Arc::new(
        mock_plugin_registry_in(delegate_temp_dir.path(), delegate_factory)
            .expect("plugin registry"),
    );

    let mut delegate_params = LLMParams::new().provider("mock").model("mock-model");
    delegate_params.reasoning_effort = reasoning_effort;
    let delegate_session_provider = Arc::new(
        crate::session::provider::SessionProvider::new(
            delegate_plugin_registry,
            shared_store,
            delegate_params,
        )
        .with_agent_id(Some("reader".into())),
    );
    let delegate_event_storage = Arc::new(
        SqliteStorage::connect(":memory:".into())
            .await
            .expect("create delegate event journal storage"),
    );

    let mut builder = crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
        delegate_event_storage.clone(),
        delegate_session_provider,
        delegate_event_storage.event_journal(),
    )
    .with_tool_policy(ToolPolicy::ProviderOnly)
    .with_max_steps(1)
    .with_execution_timeout_secs(30);

    if let Some(service) = workspace_service {
        builder = builder.with_workspace_service(service);
    }
    if let Some(source) = attachments {
        builder = builder.with_session_mcp_attachment_source(source);
    }

    if let DelegateBehavior::PauseBeforeLlmCall { entered, release } = &behavior {
        builder = builder.with_middleware(CancellationGateMiddleware {
            entered: entered.clone(),
            release: release.clone(),
        });
    }

    if matches!(behavior, DelegateBehavior::StoppedByMiddleware) {
        builder = builder.with_middleware(always_stop());
    }

    if matches!(behavior, DelegateBehavior::SessionStartHookFails) {
        use crate::hooks::config::{HookHandlerConfig, HooksConfig, MatcherGroupConfig};
        let hooks = crate::hooks::Hooks::new(HooksConfig {
            enabled: true,
            bypass_trust: true,
            session_start: vec![MatcherGroupConfig {
                matcher: None,
                hooks: vec![HookHandlerConfig::Command(
                    crate::hooks::HookCommandConfig {
                        id: None,
                        command: "echo 'injected session start failure' >&2; exit 1".into(),
                        timeout_sec: Some(5),
                        status_message: None,
                        additional_context_limit: None,
                        env: Default::default(),
                    },
                )],
            }],
            ..HooksConfig::default()
        })
        .expect("session start hook");
        builder = builder.with_hooks(hooks);
    }

    if matches!(
        behavior,
        DelegateBehavior::ContextThresholdCompactionSucceeds
            | DelegateBehavior::ContextThresholdCompactionFails
    ) {
        // Install middleware that triggers ContextThreshold once, then passes through.
        builder = builder.with_middleware(context_threshold_once());

        // Enable auto-compaction with zero retries so the test doesn't sleep.
        builder = builder.with_compaction_config(crate::config::CompactionConfig {
            auto: true,
            provider: None,
            model: None,
            retry: crate::config::RetryConfig {
                max_retries: 0,
                initial_backoff_ms: 0,
                backoff_multiplier: 1.0,
            },
        });

        // Allow more steps so the delegate can continue after compaction.
        builder = builder.with_max_steps(5);
    }

    let delegate_config = Arc::new(builder.build());

    // Leak the TempDir so its contents survive the test
    std::mem::forget(delegate_temp_dir);

    Arc::new(crate::agent::LocalAgentHandle::from_config(delegate_config))
}

pub fn agent_info(id: &str) -> AgentInfo {
    AgentInfo {
        isolation: Default::default(),
        id: crate::delegation::roster::AgentId::try_from(id).expect("non-empty id"),
        name: format!("{} name", id),
        description: format!("{} description", id),
        capabilities: vec![],
        required_capabilities: vec![],
        meta: None,
    }
}

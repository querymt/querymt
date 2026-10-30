use crate::agent::handle::AgentHandle;
use crate::delegation::{AgentInfo, AgentRegistry, DefaultAgentRegistry, DelegationOrchestrator};
use crate::event_fanout::EventFanout;
use crate::events::EventEnvelope;

use crate::session::backend::{StorageBackend, default_agent_db_path};
use crate::session::error::SessionError;
use crate::session::projection::ViewStore;
use crate::session::sqlite_storage::SqliteStorage;
use crate::session::store::SessionStore;
use crate::tools::CapabilityRequirement;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type DelegateFactory =
    Box<dyn FnOnce(Arc<dyn StorageBackend>, Arc<EventFanout>) -> Arc<dyn AgentHandle> + Send>;

type PlannerFactory = Box<
    dyn FnOnce(
            Arc<dyn StorageBackend>,
            Arc<dyn AgentRegistry + Send + Sync>,
            Arc<EventFanout>,
        ) -> Arc<dyn AgentHandle>
        + Send,
>;

#[derive(Debug, thiserror::Error)]
pub enum AgentQuorumError {
    #[error("agent quorum requires a planner agent")]
    MissingPlanner,
    #[error("failed to create session store: {0}")]
    Store(#[from] SessionError),
    #[error("missing required capability: {0:?}")]
    MissingCapability(CapabilityRequirement),
    #[error("{agent_id} agent did not use the quorum event fanout")]
    EventFanoutMismatch { agent_id: String },
}

pub struct DelegateAgent {
    pub info: AgentInfo,
    pub agent: Arc<dyn AgentHandle>,
}

pub struct AgentQuorum {
    storage: Arc<dyn StorageBackend>,
    event_fanout: Arc<EventFanout>,
    registry: Arc<dyn AgentRegistry + Send + Sync>,
    planner: Arc<dyn AgentHandle>,
    delegates: Vec<DelegateAgent>,
    orchestrator: Option<Arc<DelegationOrchestrator>>,
    listener_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    cwd: Option<PathBuf>,
}

impl Drop for AgentQuorum {
    fn drop(&mut self) {
        // Recover the mutex guard even if poisoned to avoid panicking in Drop
        let mut guard = match self.listener_handle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }
}

impl AgentQuorum {
    pub async fn builder(db_path: Option<PathBuf>) -> Result<AgentQuorumBuilder, AgentQuorumError> {
        let path = match db_path {
            Some(path) => path,
            None => default_agent_db_path()?,
        };
        let backend = SqliteStorage::connect(path).await?;
        Ok(AgentQuorumBuilder::from_backend(Arc::new(backend)))
    }

    pub fn planner(&self) -> Arc<dyn AgentHandle> {
        self.planner.clone()
    }

    pub fn delegates(&self) -> &[DelegateAgent] {
        &self.delegates
    }

    pub fn delegate(&self, id: &str) -> Option<Arc<dyn AgentHandle>> {
        self.delegates
            .iter()
            .find(|entry| entry.info.id == id)
            .map(|entry| entry.agent.clone())
    }

    pub fn store(&self) -> Arc<dyn SessionStore> {
        self.storage.session_store().clone()
    }

    pub fn view_store(&self) -> Arc<dyn ViewStore> {
        self.storage
            .view_store()
            .expect("SqliteStorage required")
            .clone()
    }
    /// Subscribe to events via the fanout (live stream of EventEnvelope).
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<EventEnvelope> {
        self.event_fanout.subscribe()
    }

    /// Access the event fanout for live event subscriptions.
    pub fn event_fanout(&self) -> Arc<EventFanout> {
        self.event_fanout.clone()
    }

    pub fn registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        self.registry.clone()
    }

    pub fn orchestrator(&self) -> Option<Arc<DelegationOrchestrator>> {
        self.orchestrator.clone()
    }

    pub fn cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }

    pub async fn shutdown(&self) {
        if let Some(orchestrator) = &self.orchestrator {
            orchestrator.begin_shutdown();
        }
        let listener = self
            .listener_handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(mut handle) = listener {
            tokio::select! {
                _ = &mut handle => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    handle.abort();
                    let _ = handle.await;
                }
            }
        }
        if let Some(orchestrator) = &self.orchestrator {
            orchestrator.cancel_active_delegations().await;
        }
        self.planner.shutdown().await;
        for delegate in &self.delegates {
            delegate.agent.shutdown().await;
        }
    }
}

pub struct AgentQuorumBuilder {
    storage: Arc<dyn StorageBackend>,
    event_fanout: Arc<EventFanout>,
    cwd: Option<PathBuf>,
    delegate_factories: Vec<(AgentInfo, DelegateFactory)>,
    planner_factory: Option<PlannerFactory>,
    delegation_enabled: bool,
    verification_enabled: bool,
    wait_policy: crate::config::DelegationWaitPolicy,
    wait_timeout_secs: u64,
    cancel_grace_secs: u64,
    max_parallel_delegations: NonZeroUsize,
    delegation_summarizer: Option<Arc<crate::delegation::DelegationSummarizer>>,
    /// Pre-registered agents to merge into the registry before building (Phase 7).
    ///
    /// These are inserted into the `DefaultAgentRegistry` *before* the local delegates,
    /// so local delegates with the same ID will override remote ones.
    preregistered: Vec<(AgentInfo, Arc<dyn AgentHandle>)>,
    /// Optional routing snapshot handle for per-agent routing decisions.
    routing_snapshot: Option<crate::agent::remote::RoutingSnapshotHandle>,
    profile_id: Option<String>,
}

impl AgentQuorumBuilder {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            storage: storage.clone(),
            event_fanout: Arc::new(EventFanout::new()),
            cwd: None,
            delegate_factories: Vec::new(),
            planner_factory: None,
            delegation_enabled: true,
            verification_enabled: false,
            wait_policy: crate::config::DelegationWaitPolicy::default(),
            wait_timeout_secs: 120,
            cancel_grace_secs: 5,
            max_parallel_delegations: NonZeroUsize::new(5).expect("non-zero default"),
            delegation_summarizer: None,
            preregistered: Vec::new(),
            routing_snapshot: None,
            profile_id: None,
        }
    }

    /// Pre-register an agent into the delegation registry (Phase 7: remote agents).
    ///
    /// Pre-registered entries are inserted before local delegates; local delegates
    /// with the same ID will override them.
    pub fn preregister_agent(mut self, info: AgentInfo, handle: Arc<dyn AgentHandle>) -> Self {
        self.preregistered.push((info, handle));
        self
    }

    /// Create builder from a storage backend.
    pub fn from_backend(backend: Arc<dyn StorageBackend>) -> Self {
        Self {
            storage: backend.clone(),
            event_fanout: Arc::new(EventFanout::new()),
            cwd: None,
            delegate_factories: Vec::new(),
            planner_factory: None,
            delegation_enabled: true,
            verification_enabled: false,
            wait_policy: crate::config::DelegationWaitPolicy::default(),
            wait_timeout_secs: 120,
            cancel_grace_secs: 5,
            max_parallel_delegations: NonZeroUsize::new(5).expect("non-zero default"),
            delegation_summarizer: None,
            preregistered: Vec::new(),
            routing_snapshot: None,
            profile_id: None,
        }
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Use a caller-provided event bus for the planner, delegates, and orchestrator.
    ///
    /// The bus is passed to each factory and the build fails if a returned local agent uses a
    /// different bus.
    pub fn with_event_fanout(mut self, event_fanout: Arc<EventFanout>) -> Self {
        self.event_fanout = event_fanout;
        self
    }

    pub fn add_delegate_agent<F>(mut self, info: AgentInfo, factory: F) -> Self
    where
        F: FnOnce(Arc<dyn StorageBackend>, Arc<EventFanout>) -> Arc<dyn AgentHandle>
            + Send
            + 'static,
    {
        self.delegate_factories.push((info, Box::new(factory)));
        self
    }

    pub fn with_planner<F>(mut self, factory: F) -> Self
    where
        F: FnOnce(
                Arc<dyn StorageBackend>,
                Arc<dyn AgentRegistry + Send + Sync>,
                Arc<EventFanout>,
            ) -> Arc<dyn AgentHandle>
            + Send
            + 'static,
    {
        self.planner_factory = Some(Box::new(factory));
        self
    }

    pub fn with_delegation(mut self, enabled: bool) -> Self {
        self.delegation_enabled = enabled;
        self
    }

    pub fn with_verification(mut self, enabled: bool) -> Self {
        self.verification_enabled = enabled;
        self
    }

    pub fn with_wait_policy(mut self, policy: crate::config::DelegationWaitPolicy) -> Self {
        self.wait_policy = policy;
        self
    }

    pub fn with_wait_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.wait_timeout_secs = timeout_secs;
        self
    }

    pub fn with_cancel_grace_secs(mut self, grace_secs: u64) -> Self {
        self.cancel_grace_secs = grace_secs;
        self
    }

    pub fn with_max_parallel_delegations(mut self, max_parallel: usize) -> Self {
        if let Some(nz) = NonZeroUsize::new(max_parallel) {
            self.max_parallel_delegations = nz;
        }
        self
    }

    pub fn with_delegation_summarizer(
        mut self,
        summarizer: Option<Arc<crate::delegation::DelegationSummarizer>>,
    ) -> Self {
        self.delegation_summarizer = summarizer;
        self
    }

    /// Set the routing snapshot handle for per-agent routing decisions.
    ///
    /// The snapshot is passed to the `DelegationOrchestrator`, which reads it
    /// before creating each delegation session to apply routing policy.
    pub fn with_routing_snapshot(
        mut self,
        snapshot: crate::agent::remote::RoutingSnapshotHandle,
    ) -> Self {
        self.routing_snapshot = Some(snapshot);
        self
    }

    pub fn with_profile_id(mut self, profile_id: impl Into<String>) -> Self {
        self.profile_id = Some(profile_id.into());
        self
    }

    pub fn build(self) -> Result<AgentQuorum, AgentQuorumError> {
        // Capability validation
        let mut all_required_caps = std::collections::HashSet::new();
        for (info, _) in &self.delegate_factories {
            for cap in &info.required_capabilities {
                all_required_caps.insert(*cap);
            }
        }

        if all_required_caps.contains(&CapabilityRequirement::Filesystem) && self.cwd.is_none() {
            return Err(AgentQuorumError::MissingCapability(
                CapabilityRequirement::Filesystem,
            ));
        }

        let mut registry = DefaultAgentRegistry::new();
        let mut delegates = Vec::with_capacity(self.delegate_factories.len());

        // Phase 7: insert pre-registered agents (e.g. remote agents) first so that
        // local delegates with the same ID can override them.
        for (info, handle) in self.preregistered {
            log::debug!(
                "AgentQuorumBuilder: pre-registering agent '{}' (remote/config-driven)",
                info.id
            );
            registry.register_handle(info, handle);
        }

        for (info, factory) in self.delegate_factories {
            let agent = factory(self.storage.clone(), self.event_fanout.clone());
            if !Arc::ptr_eq(agent.event_fanout(), &self.event_fanout) {
                return Err(AgentQuorumError::EventFanoutMismatch {
                    agent_id: info.id.clone(),
                });
            }

            registry.register_handle(info.clone(), agent.clone());
            delegates.push(DelegateAgent { info, agent });
        }

        let registry = Arc::new(registry);

        let planner_factory = self
            .planner_factory
            .ok_or(AgentQuorumError::MissingPlanner)?;
        let planner = planner_factory(
            self.storage.clone(),
            registry.clone(),
            self.event_fanout.clone(),
        );
        if !Arc::ptr_eq(planner.event_fanout(), &self.event_fanout) {
            return Err(AgentQuorumError::EventFanoutMismatch {
                agent_id: "planner".to_string(),
            });
        }

        let orchestrator = if self.delegation_enabled {
            // We need to get the tool_registry. For now, we'll use a default/empty one
            // This will be properly addressed when we pass AgentHandle which has tool_registry()
            // For compatibility, we create a minimal tool registry here
            use crate::tools::ToolRegistry;
            let tool_registry = Arc::new(ToolRegistry::new());

            use crate::event_sink::EventSink;
            // Lifecycle events use the local planner's emitter. Stages and
            // summary accounting must reuse its stream registry AND projection.
            let delegation_sink = planner
                .as_any()
                .downcast_ref::<crate::agent::LocalAgentHandle>()
                .map(|handle| handle.config.event_sink.clone())
                .unwrap_or_else(|| {
                    Arc::new(EventSink::new(
                        self.storage.event_journal(),
                        self.event_fanout.clone(),
                    ))
                });

            let planner_hooks = planner
                .as_any()
                .downcast_ref::<crate::agent::LocalAgentHandle>()
                .map(|handle| handle.config.hooks.clone())
                .unwrap_or_else(crate::hooks::Hooks::disabled);

            let delegate_model_overrides = planner
                .as_any()
                .downcast_ref::<crate::agent::LocalAgentHandle>()
                .map(|handle| handle.config.delegate_model_overrides.clone())
                .unwrap_or_default();
            let workspace_service = planner
                .as_any()
                .downcast_ref::<crate::agent::LocalAgentHandle>()
                .and_then(|handle| handle.config.workspace_service());
            let mut orch = DelegationOrchestrator::new(
                planner.clone(),
                delegation_sink,
                self.storage.session_store().clone(),
                registry.clone(),
                tool_registry,
                planner_hooks,
                self.cwd.clone(),
            )
            .with_delegate_model_overrides(delegate_model_overrides)
            .with_verification(self.verification_enabled)
            .with_wait_policy(self.wait_policy.clone())
            .with_wait_timeout_secs(self.wait_timeout_secs)
            .with_cancel_grace_secs(self.cancel_grace_secs)
            .with_max_parallel_delegations(self.max_parallel_delegations)
            .with_summarizer(self.delegation_summarizer.clone())
            .with_budget_pricing(
                planner
                    .as_any()
                    .downcast_ref::<crate::agent::LocalAgentHandle>()
                    .and_then(|handle| handle.config.budget_pricing()),
            )
            .with_budget_cap(
                planner
                    .as_any()
                    .downcast_ref::<crate::agent::LocalAgentHandle>()
                    .and_then(|handle| handle.config.budget_cap_usd()),
            );
            if let Some(service) = workspace_service {
                orch = orch.with_workspace_service(service);
            }
            if let Some(snap) = self.routing_snapshot {
                orch = orch.with_routing_snapshot(snap);
            }
            if let Some(profile_id) = self.profile_id {
                orch = orch.with_profile_id(profile_id);
            }
            let orchestrator = Arc::new(orch);

            Some(orchestrator)
        } else {
            None
        };

        let listener_handle = orchestrator
            .as_ref()
            .map(|orchestrator| orchestrator.start_listening(&self.event_fanout));
        Ok(AgentQuorum {
            storage: self.storage,
            event_fanout: self.event_fanout,
            registry,
            planner,
            delegates,
            orchestrator,
            listener_handle: Mutex::new(listener_handle),
            cwd: self.cwd,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentConfigBuilder, LocalAgentHandle};
    use crate::test_utils::empty_plugin_registry;
    use querymt::LLMParams;

    #[tokio::test]
    async fn quorum_parent_clock_keeps_three_shell_delegations_terminal_on_wire() {
        use crate::acp::protocol::NewSessionRequest;
        use crate::control::delegation_notifications::{DelegationStage, DelegationUpdateState};
        use crate::events::AgentEventKind;
        use crate::session::domain::{Delegation, DelegationStatus};
        use crate::test_utils::{
            MockChatResponse, MockLlmProvider, SharedLlmProvider, TestProviderFactory,
        };
        use crate::tools::{Tool, ToolRegistry, builtins::ShellTool};
        use std::collections::{HashMap, HashSet};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(
            SqliteStorage::connect(dir.path().join("quorum.db"))
                .await
                .unwrap(),
        );
        let release = dir.path().join("release");
        let mut summary_mock = MockLlmProvider::new();
        summary_mock
            .expect_chat()
            .times(3)
            .returning(|_| Ok(MockChatResponse::text_only("fixture summary").into()));
        let summary_factory = Arc::new(TestProviderFactory::new(SharedLlmProvider::new(
            summary_mock,
            vec![],
        )));
        let summary_root = dir.path().join("summary");
        std::fs::create_dir(&summary_root).unwrap();
        let summary_registry =
            crate::test_utils::mock_plugin_registry_in(&summary_root, summary_factory).unwrap();
        let summary_provider = crate::session::provider::SessionProvider::new(
            Arc::new(summary_registry),
            storage.session_store(),
            LLMParams::new().provider("mock").model("summary"),
        );
        let summarizer = crate::delegation::DelegationSummarizer::from_config(
            &crate::config::DelegationSummaryConfig {
                provider: "mock".into(),
                model: "summary".into(),
                min_history_tokens: 0,
                ..Default::default()
            },
            &summary_provider,
        )
        .await
        .unwrap();
        let mut builder = AgentQuorumBuilder::from_backend(storage.clone())
            .cwd(dir.path())
            .with_verification(true)
            .with_max_parallel_delegations(3)
            .with_delegation_summarizer(Some(Arc::new(summarizer)));
        for target in ["coder", "explorer", "linus"] {
            let root = dir.path().to_path_buf();
            let release = release.clone();
            builder = builder.add_delegate_agent(AgentInfo {
                id: target.into(), name: target.into(), description: String::new(),
                capabilities: vec![], required_capabilities: vec![], meta: None, isolation: Default::default(),
            }, move |storage, fanout| {
                let mut mock = MockLlmProvider::new();
                let command = serde_json::json!({"command":"sh", "args":["-c", ": > \"$2\"; i=0; while test ! -f \"$1\" && test $i -lt 2000; do i=$((i+1)); sleep 0.01; done; test -f \"$1\" && sleep 0.02 && printf done > \"$3\"", "qmt-quorum", release, root.join(format!("entered-{target}")), root.join(format!("done-{target}"))],"workdir":root});
                let respond = Arc::new(move |messages: &[querymt::chat::ChatMessage]| {
                    if messages.last().is_some_and(|message| message.portable_input_parts().iter().any(|part| part.is_tool_result())) {
                        MockChatResponse::text_only("fixture child completed")
                    } else {
                        MockChatResponse::with_tools("fixture shell", vec![crate::test_utils::mock_querymt_tool_call(&format!("shell-{target}"), "shell", &command.to_string())])
                    }
                });
                let tools_response = respond.clone();
                mock.expect_chat_with_tools().returning(move |messages, _| Ok(tools_response(messages).into()));
                mock.expect_chat().returning(move |messages| Ok(respond(messages).into()));
                let factory = Arc::new(TestProviderFactory::new(SharedLlmProvider::new(mock, vec![ShellTool::new().definition()])));
                let registry_root = root.join(target);
                std::fs::create_dir(&registry_root).unwrap();
                let registry = crate::test_utils::mock_plugin_registry_in(&registry_root, factory).unwrap();
                let mut tools = ToolRegistry::new();
                tools.add(Arc::new(ShellTool::new()));
                let config = AgentConfigBuilder::new(Arc::new(registry), storage, LLMParams::new().provider("mock").model(format!("model-{target}")))
                    .with_agent_id(target).with_event_fanout(fanout).with_tool_registry(tools).build();
                Arc::new(LocalAgentHandle::from_config(Arc::new(config)))
            });
        }
        let (registry, _registry_dir) = empty_plugin_registry().unwrap();
        builder = builder.with_planner(move |storage, registry_agents, fanout| {
            Arc::new(LocalAgentHandle::from_config(Arc::new(
                AgentConfigBuilder::new(
                    Arc::new(registry),
                    storage,
                    LLMParams::new().provider("mock").model("planner"),
                )
                .with_agent_registry(registry_agents)
                .with_event_fanout(fanout)
                .build(),
            )))
        });
        // Do not construct an orchestrator here: exercise the production builder's wiring.
        let quorum = builder.build().unwrap();
        let planner = quorum.planner();
        let local = planner.as_any().downcast_ref::<LocalAgentHandle>().unwrap();
        let parent = planner
            .new_session(NewSessionRequest::new(dir.path().to_path_buf()))
            .await
            .unwrap()
            .session_id
            .to_string();
        let parent_row = quorum.store().get_session(&parent).await.unwrap().unwrap();
        // Hold the planner's existing projection before the orchestrator records summaries.
        let parent_accounting = local
            .config
            .event_sink
            .session_accounting(&parent)
            .await
            .unwrap();
        let mut rx = quorum.subscribe_events();
        // A real planner stream already exists before the first delegation, as in a prompt.
        for _ in 0..20 {
            planner.emit_event(
                &parent,
                AgentEventKind::HookNotice {
                    event_name: "fixture".into(),
                    message: "planner progress".into(),
                    is_error: false,
                },
            );
        }
        let mut ids = HashMap::new();
        for target in ["coder", "explorer", "linus"] {
            let delegation = quorum
                .store()
                .create_delegation(Delegation {
                    id: 0,
                    public_id: String::new(),
                    session_id: parent_row.id,
                    task_id: None,
                    target_agent_id: target.into(),
                    objective: format!("bounded fixture {target}"),
                    objective_hash: Default::default(),
                    context: None,
                    constraints: None,
                    expected_output: None,
                    verification_spec: Some(crate::verification::VerificationSpec {
                        description: "actual shell result exists".into(),
                        strategy: Default::default(),
                        steps: vec![crate::verification::VerificationStep::FileAssertion {
                            path: dir.path().join(format!("done-{target}")),
                            exists: true,
                            contains: Some("done".into()),
                            matches_regex: None,
                        }],
                    }),
                    planning_summary: None,
                    status: DelegationStatus::Requested,
                    retry_count: 0,
                    created_at: time::OffsetDateTime::now_utc(),
                    completed_at: None,
                    isolation: Default::default(),
                })
                .await
                .unwrap();
            ids.insert(delegation.public_id.clone(), target);
            local
                .config
                .emit_event_persisted(
                    &parent,
                    AgentEventKind::DelegationRequested {
                        delegation,
                        tool_call_id: Some(format!("dispatch-{target}")),
                        batch_id: None,
                        cycle_id: None,
                    },
                )
                .await
                .unwrap();
        }
        let entered = tokio::time::timeout(Duration::from_secs(20), async {
            while !["coder", "explorer", "linus"]
                .iter()
                .all(|id| dir.path().join(format!("entered-{id}")).exists())
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        if entered.is_err() {
            let mut diagnostic = Vec::new();
            while let Ok(event) = rx.try_recv() {
                diagnostic.push(serde_json::to_value(event).unwrap());
            }
            panic!("all three actual builtin shell tools must start: {diagnostic:?}");
        }
        tokio::fs::write(&release, "go").await.unwrap();
        let (events, wire) = tokio::time::timeout(Duration::from_secs(5), async {
            let mut translator = crate::acp::shared::AcpLiveEventTranslator::new();
            let mut events = Vec::new();
            let mut wire = Vec::new();
            let mut cleaned = HashSet::new();
            while cleaned.len() < 3 {
                let event = rx.recv().await.unwrap();
                if event.session_id() != parent {
                    continue;
                }
                wire.extend(translator.translate_notifications(&event));
                if let AgentEventKind::DelegationStageChanged {
                    delegation_id,
                    stage: None,
                    ..
                } = event.kind()
                {
                    cleaned.insert(delegation_id.clone());
                }
                events.push(event);
            }
            (events, wire)
        })
        .await
        .unwrap();
        for delegation in quorum.store().list_delegations(&parent).await.unwrap() {
            assert_eq!(
                delegation.status,
                DelegationStatus::Complete,
                "actual durable execution must succeed before wire assertion"
            );
        }
        let completed: Vec<_> = wire
            .iter()
            .filter(|message| message["params"]["state"] == "completed")
            .collect();
        assert_eq!(
            completed
                .iter()
                .filter_map(|message| message["params"]["delegationId"].as_str())
                .collect::<HashSet<_>>()
                .len(),
            3,
            "real quorum composition dropped durable completions from the actual translator: {wire:?}"
        );
        for (id, target) in ids {
            let observations: Vec<_> = wire
                .iter()
                .filter(|message| message["params"]["delegationId"] == id)
                .map(|message| &message["params"])
                .collect();
            for stage in ["queued", "executing", "verifying", "finalizing"] {
                assert!(
                    observations.iter().any(|row| row["stage"] == stage),
                    "missing {target} {stage}"
                );
            }
            assert!(observations.iter().any(|row| row["state"] == "forked"));
            let last = observations.last().unwrap();
            assert_eq!(last["state"], "completed");
            assert_eq!(last["activity"], "historical");
            assert!(last["stage"].is_null());
            assert!(last["childSessionId"].as_str().is_some());
            assert_eq!(last["selectedModelId"], format!("mock/model-{target}"));
            assert_eq!(last["toolCallId"], format!("dispatch-{target}"));
            let cached = quorum
                .event_fanout()
                .delegation_snapshots(std::slice::from_ref(&parent))
                .into_iter()
                .find(|row| row.delegation_id == id)
                .unwrap();
            assert_eq!(cached.state, DelegationUpdateState::Completed);
            assert_eq!(cached.stage, None);
            assert!(events.iter().any(|event| matches!(event.kind(), AgentEventKind::DelegationStageChanged { delegation_id, stage: Some(DelegationStage::Verifying), .. } if delegation_id == &id)));
        }
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].session_seq() < pair[1].session_seq()),
            "parent stages/durable events must use one increasing clock: {:?}",
            events
                .iter()
                .map(|event| event.session_seq())
                .collect::<Vec<_>>()
        );
        assert_eq!(events.iter().filter(|event| matches!(event.kind(), AgentEventKind::LlmAttemptSettled { settlement } if settlement.attempt.purpose == crate::accounting::AttemptPurpose::DelegationSummary)).count(), 3);
        parent_accounting.read(|reducer| {
            assert_eq!(reducer.attempt_count(), 3, "summary attempts must update the already-held planner projection without catch_up or rehydration");
            assert!(reducer.unresolved().is_empty());
        });
        quorum.shutdown().await;
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn quorum_preserves_nonlocal_planner_construction_acceptance() {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh()
            .await
            .clone();
        let planner = Arc::new(crate::agent::remote::RemoteAgentHandle::new(
            "fixture-peer".into(),
            None,
            mesh,
        ));
        let quorum = AgentQuorumBuilder::from_backend(storage)
            .with_event_fanout(planner.event_fanout().clone())
            .with_delegation(true)
            .with_planner(move |_, _, _| planner)
            .build()
            .unwrap();
        assert!(quorum.orchestrator().is_some());
        // Construction acceptance is unchanged; remote parent clock ownership is not proved here.
        quorum.shutdown().await;
    }

    #[tokio::test]
    async fn quorum_uses_injected_event_fanout_for_all_local_agents() {
        let backend = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let shared_fanout = Arc::new(EventFanout::new());
        let mut builder =
            AgentQuorumBuilder::from_backend(backend).with_event_fanout(shared_fanout.clone());

        let (plugin_registry, _temp_dir) = empty_plugin_registry().unwrap();
        let plugin_registry = Arc::new(plugin_registry);
        let delegate_registry = plugin_registry.clone();
        builder = builder.add_delegate_agent(
            AgentInfo {
                isolation: Default::default(),
                id: "coder".into(),
                name: "Coder".into(),
                description: String::new(),
                capabilities: vec![],
                required_capabilities: vec![],
                meta: None,
            },
            move |storage, event_fanout| {
                let config = Arc::new(
                    AgentConfigBuilder::new(
                        delegate_registry.clone(),
                        storage,
                        LLMParams::new().provider("mock").model("mock-model"),
                    )
                    .with_event_fanout(event_fanout)
                    .build(),
                );
                Arc::new(LocalAgentHandle::from_config(config)) as Arc<dyn AgentHandle>
            },
        );

        let planner_registry = plugin_registry.clone();
        builder = builder.with_planner(move |storage, agent_registry, event_fanout| {
            let config = Arc::new(
                AgentConfigBuilder::new(
                    planner_registry.clone(),
                    storage,
                    LLMParams::new().provider("mock").model("mock-model"),
                )
                .with_agent_registry(agent_registry)
                .with_event_fanout(event_fanout)
                .build(),
            );
            Arc::new(LocalAgentHandle::from_config(config)) as Arc<dyn AgentHandle>
        });

        let expected_fanout = builder.event_fanout.clone();
        let quorum = builder.build().unwrap();

        assert!(Arc::ptr_eq(&quorum.event_fanout(), &expected_fanout));
        assert!(Arc::ptr_eq(
            quorum.planner().event_fanout(),
            &expected_fanout
        ));
        assert!(Arc::ptr_eq(
            quorum.delegate("coder").unwrap().event_fanout(),
            &expected_fanout
        ));
    }

    #[tokio::test]
    async fn quorum_rejects_planner_with_a_different_event_fanout() {
        let backend = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let mut builder = AgentQuorumBuilder::from_backend(backend);
        let (plugin_registry, _temp_dir) = empty_plugin_registry().unwrap();
        let plugin_registry = Arc::new(plugin_registry);

        builder = builder.with_planner(move |storage, agent_registry, _event_fanout| {
            let config = Arc::new(
                AgentConfigBuilder::new(
                    plugin_registry.clone(),
                    storage,
                    LLMParams::new().provider("mock").model("mock-model"),
                )
                .with_agent_registry(agent_registry)
                .with_event_fanout(Arc::new(EventFanout::new()))
                .build(),
            );
            Arc::new(LocalAgentHandle::from_config(config)) as Arc<dyn AgentHandle>
        });

        assert!(matches!(
            builder.build(),
            Err(AgentQuorumError::EventFanoutMismatch { agent_id }) if agent_id == "planner"
        ));
    }

    #[tokio::test]
    async fn quorum_exposes_planner_event_fanout() {
        let backend = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let mut builder = AgentQuorumBuilder::from_backend(backend);

        let (plugin_registry, _temp_dir) = empty_plugin_registry().unwrap();
        let plugin_registry = Arc::new(plugin_registry);

        builder = builder.with_planner(move |storage, agent_registry, event_fanout| {
            let config = Arc::new(
                AgentConfigBuilder::new(
                    plugin_registry.clone(),
                    storage.clone(),
                    LLMParams::new().provider("mock").model("mock-model"),
                )
                .with_agent_registry(agent_registry)
                .with_event_fanout(event_fanout)
                .build(),
            );
            Arc::new(LocalAgentHandle::from_config(config)) as Arc<dyn AgentHandle>
        });

        let quorum = builder.build().unwrap();
        let planner = quorum.planner();

        assert!(Arc::ptr_eq(&quorum.event_fanout(), planner.event_fanout()));
    }
}

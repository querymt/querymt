use crate::delegation::{
    AgentActorHandle, AgentInfo, AgentRegistry, DefaultAgentRegistry, DelegationOrchestrator,
};
use crate::event_bus::EventBus;
use crate::send_agent::SendAgent;

use crate::session::backend::{StorageBackend, default_agent_db_path};
use crate::session::error::SessionError;
use crate::session::projection::ViewStore;
use crate::session::sqlite_storage::SqliteStorage;
use crate::session::store::SessionStore;
use crate::tools::CapabilityRequirement;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

type DelegateFactory =
    Box<dyn FnOnce(Arc<dyn SessionStore>, Arc<EventBus>) -> Arc<dyn SendAgent> + Send>;

type PlannerFactory = Box<
    dyn FnOnce(
            Arc<dyn SessionStore>,
            Arc<EventBus>,
            Arc<dyn AgentRegistry + Send + Sync>,
        ) -> Arc<dyn SendAgent>
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
}

pub struct DelegateAgent {
    pub info: AgentInfo,
    pub agent: Arc<dyn SendAgent>,
}

pub struct AgentQuorum {
    store: Arc<dyn SessionStore>,
    view_store: Arc<dyn ViewStore>,
    event_bus: Arc<EventBus>,
    registry: Arc<dyn AgentRegistry + Send + Sync>,
    planner: Arc<dyn SendAgent>,
    delegates: Vec<DelegateAgent>,
    orchestrator: Option<Arc<DelegationOrchestrator>>,
    cwd: Option<PathBuf>,
}

impl AgentQuorum {
    pub async fn builder(db_path: Option<PathBuf>) -> Result<AgentQuorumBuilder, AgentQuorumError> {
        let path = match db_path {
            Some(path) => path,
            None => default_agent_db_path()?,
        };
        let backend = SqliteStorage::connect(path).await?;
        Ok(AgentQuorumBuilder::from_backend(backend))
    }

    pub fn planner(&self) -> Arc<dyn SendAgent> {
        self.planner.clone()
    }

    pub fn delegates(&self) -> &[DelegateAgent] {
        &self.delegates
    }

    pub fn delegate(&self, id: &str) -> Option<Arc<dyn SendAgent>> {
        self.delegates
            .iter()
            .find(|entry| entry.info.id == id)
            .map(|entry| entry.agent.clone())
    }

    pub fn store(&self) -> Arc<dyn SessionStore> {
        self.store.clone()
    }

    pub fn event_bus(&self) -> Arc<EventBus> {
        self.event_bus.clone()
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

    pub fn view_store(&self) -> Arc<dyn ViewStore> {
        self.view_store.clone()
    }
}

pub struct AgentQuorumBuilder {
    store: Arc<dyn SessionStore>,
    view_store: Arc<dyn ViewStore>,
    event_bus: Arc<EventBus>,
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
    preregistered: Vec<(AgentInfo, Arc<dyn SendAgent>)>,
}

impl AgentQuorumBuilder {
    pub fn new(store: Arc<dyn SessionStore>, view_store: Arc<dyn ViewStore>) -> Self {
        Self {
            store,
            view_store,
            event_bus: Arc::new(EventBus::new()),
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
        }
    }

    /// Pre-register an agent into the delegation registry (Phase 7: remote agents).
    ///
    /// Pre-registered entries are inserted before local delegates; local delegates
    /// with the same ID will override them.
    pub fn preregister_agent(mut self, info: AgentInfo, instance: Arc<dyn SendAgent>) -> Self {
        self.preregistered.push((info, instance));
        self
    }

    /// Create builder from a storage backend (registers event observer automatically).
    pub fn from_backend(backend: SqliteStorage) -> Self {
        let event_bus = Arc::new(EventBus::new());
        event_bus.add_observer(backend.event_observer());
        Self {
            store: backend.session_store(),
            view_store: backend
                .view_store()
                .expect("SqliteStorage always provides ViewStore"),
            event_bus,
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
        }
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
        self.event_bus = event_bus;
        self
    }

    pub fn add_delegate_agent<F>(mut self, info: AgentInfo, factory: F) -> Self
    where
        F: FnOnce(Arc<dyn SessionStore>, Arc<EventBus>) -> Arc<dyn SendAgent> + Send + 'static,
    {
        self.delegate_factories.push((info, Box::new(factory)));
        self
    }

    pub fn with_planner<F>(mut self, factory: F) -> Self
    where
        F: FnOnce(
                Arc<dyn SessionStore>,
                Arc<EventBus>,
                Arc<dyn AgentRegistry + Send + Sync>,
            ) -> Arc<dyn SendAgent>
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
        for (info, agent) in self.preregistered {
            log::debug!(
                "AgentQuorumBuilder: pre-registering agent '{}' (remote/config-driven)",
                info.id
            );
            registry.register(info, agent);
        }

        for (info, factory) in self.delegate_factories {
            let agent = factory(self.store.clone(), self.event_bus.clone());

            // Try to extract AgentActorHandle::Local by downcasting to AgentHandle
            let actor_handle = agent
                .as_any()
                .downcast_ref::<crate::agent::AgentHandle>()
                .map(|handle| AgentActorHandle::Local {
                    config: handle.config.clone(),
                    registry: handle.registry.clone(),
                });

            if let Some(handle) = actor_handle {
                registry.register_with_handle(info.clone(), agent.clone(), handle);
            } else {
                registry.register(info.clone(), agent.clone());
            }
            delegates.push(DelegateAgent { info, agent });
        }

        let registry = Arc::new(registry);

        let planner_factory = self
            .planner_factory
            .ok_or(AgentQuorumError::MissingPlanner)?;
        let planner = planner_factory(self.store.clone(), self.event_bus.clone(), registry.clone());

        let orchestrator = if self.delegation_enabled {
            // We need to get the tool_registry. For now, we'll use a default/empty one
            // This will be properly addressed when we pass AgentHandle which has tool_registry()
            // For compatibility, we create a minimal tool registry here
            use crate::tools::ToolRegistry;
            let tool_registry = Arc::new(ToolRegistry::new());

            let orchestrator = Arc::new(
                DelegationOrchestrator::new(
                    planner.clone(),
                    self.event_bus.clone(),
                    self.store.clone(),
                    registry.clone(),
                    tool_registry,
                    self.cwd.clone(),
                )
                .with_verification(self.verification_enabled)
                .with_wait_policy(self.wait_policy.clone())
                .with_wait_timeout_secs(self.wait_timeout_secs)
                .with_cancel_grace_secs(self.cancel_grace_secs)
                .with_max_parallel_delegations(self.max_parallel_delegations)
                .with_summarizer(self.delegation_summarizer.clone()),
            );

            // Note: We can't call add_observer on Arc<dyn SendAgent> directly
            // This will be fixed when we use AgentHandle in the factories
            // For now, skip the observer registration
            // planner.add_observer(orchestrator.clone());

            Some(orchestrator)
        } else {
            None
        };

        Ok(AgentQuorum {
            store: self.store,
            view_store: self.view_store,
            event_bus: self.event_bus,
            registry,
            planner,
            delegates,
            orchestrator,
            cwd: self.cwd,
        })
    }
}

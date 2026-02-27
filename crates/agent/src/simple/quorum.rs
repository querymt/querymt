//! Multi-agent quorum implementation

use super::callbacks::EventCallbacksState;
use super::config::{AgentConfig, DelegateConfigBuilder, PlannerConfigBuilder};
use super::utils::{
    build_llm_config, default_registry, infer_required_capabilities, latest_assistant_message,
    to_absolute_path,
};
use crate::AgentQuorumBuilder;
use crate::agent::LocalAgentHandle as AgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::SnapshotPolicy;
use crate::agent::core::ToolPolicy;
use crate::config::{MiddlewareEntry, QuorumConfig, resolve_tools};
use crate::delegation::AgentInfo;

use crate::agent::handle::AgentHandle as AgentHandleTrait;
use crate::middleware::MIDDLEWARE_REGISTRY;
use crate::quorum::AgentQuorum;
use crate::runner::{ChatRunner, ChatSession};
#[cfg(feature = "dashboard")]
use crate::server::AgentServer;
use crate::session::backend::default_agent_db_path;
use crate::session::store::SessionStore;
use crate::session::{SqliteStorage, StorageBackend};
use crate::snapshot::GitSnapshotBackend;
use crate::tools::CapabilityRequirement;
use crate::tools::builtins::all_builtin_tools;
use agent_client_protocol::{ContentBlock, NewSessionRequest, PromptRequest, TextContent};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Builder for multi-agent quorum with closure-based configuration
pub struct QuorumBuilder {
    pub(super) cwd: Option<PathBuf>,
    pub(super) db_path: Option<PathBuf>,
    pub(super) planner_config: Option<AgentConfig>,
    pub(super) delegates: Vec<AgentConfig>,
    pub(super) delegation_enabled: bool,
    pub(super) verification_enabled: bool,
    pub(super) snapshot_policy: SnapshotPolicy,
    pub(super) delegation_summary_config: Option<crate::config::DelegationSummaryConfig>,
    pub(super) delegation_wait_policy: crate::config::DelegationWaitPolicy,
    pub(super) delegation_wait_timeout_secs: u64,
    pub(super) delegation_cancel_grace_secs: u64,
    pub(super) max_parallel_delegations: usize,
    /// Pre-built registry entries to merge before building (Phase 7: remote agents).
    ///
    /// When `Some`, the entries in this registry are merged with the local delegate agents
    /// before the planner is built, so the planner sees both local and remote agents as
    /// delegation targets.
    pub(super) initial_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
    /// Optional mesh handle to store on the planner's `AgentHandle` after build (Phase 7).
    #[cfg(feature = "remote")]
    pub(super) mesh: Option<crate::agent::remote::MeshHandle>,
    /// Controls whether provider lookup may fall back to mesh peers when
    /// `provider_node_id` is not explicitly set.
    #[cfg(feature = "remote")]
    pub(super) mesh_auto_fallback: bool,

    /// Delegates that have a `peer` set in their config: `(delegate_id, peer_name)`.
    ///
    /// After `build()`, these are resolved to a `NodeId` via `MeshHandle::resolve_peer_node_id`
    /// and the resulting node ID is stored on the delegate's `AgentHandle` so that
    /// `create_delegation_session` routes LLM calls to the peer.
    #[cfg(feature = "remote")]
    pub(super) peer_delegates: Vec<(String, String)>,
}

impl Default for QuorumBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl QuorumBuilder {
    pub fn new() -> Self {
        Self {
            cwd: None,
            db_path: None,
            planner_config: None,
            delegates: Vec::new(),
            delegation_enabled: true,
            verification_enabled: false,
            snapshot_policy: SnapshotPolicy::None,
            delegation_summary_config: None,
            delegation_wait_policy: crate::config::DelegationWaitPolicy::default(),
            delegation_wait_timeout_secs: 120,
            delegation_cancel_grace_secs: 5,
            max_parallel_delegations: 5,
            initial_registry: None,
            #[cfg(feature = "remote")]
            mesh: None,
            #[cfg(feature = "remote")]
            mesh_auto_fallback: false,
            #[cfg(feature = "remote")]
            peer_delegates: Vec::new(),
        }
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn db(mut self, path: impl Into<PathBuf>) -> Self {
        self.db_path = Some(path.into());
        self
    }

    /// Configure the planner agent using a closure
    pub fn planner<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(PlannerConfigBuilder) -> PlannerConfigBuilder,
    {
        let builder = PlannerConfigBuilder::new();
        self.planner_config = Some(configure(builder).build());
        self
    }

    /// Add a delegate agent configured via closure
    pub fn delegate<F>(mut self, id: impl Into<String>, configure: F) -> Self
    where
        F: FnOnce(DelegateConfigBuilder) -> DelegateConfigBuilder,
    {
        let builder = DelegateConfigBuilder::new(id);
        self.delegates.push(configure(builder).build());
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

    pub fn with_snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.snapshot_policy = policy;
        self
    }

    pub fn with_defaults(mut self) -> Self {
        self.delegation_enabled = true;
        self.verification_enabled = true;
        self
    }

    pub async fn build(self) -> Result<Quorum> {
        let planner_config = self
            .planner_config
            .ok_or_else(|| anyhow!("Planner configuration is required"))?;

        // Convert cwd to absolute path if provided
        let cwd = self.cwd.map(to_absolute_path).transpose()?;

        // Capability validation
        let mut all_required = HashSet::new();
        all_required.extend(infer_required_capabilities(&planner_config.tools));
        for delegate in &self.delegates {
            all_required.extend(&delegate.required_capabilities);
        }

        if all_required.contains(&CapabilityRequirement::Filesystem) && cwd.is_none() {
            return Err(anyhow!(
                "Working directory required: one or more agents require filesystem access. Use .cwd() to set one."
            ));
        }

        let registry = Arc::new(default_registry().await?);

        let path = match self.db_path {
            Some(path) => path,
            None => default_agent_db_path()?,
        };
        let backend = Arc::new(SqliteStorage::connect(path).await?);
        let mut builder = AgentQuorumBuilder::from_backend(backend.clone());

        if let Some(cwd_path) = cwd.clone() {
            builder = builder.cwd(cwd_path);
        }

        // Phase 7: inject pre-registered remote agents into the quorum's delegate registry.
        if let Some(ref initial_reg) = self.initial_registry {
            for info in initial_reg.list_agents() {
                if let Some(handle) = initial_reg.get_handle(&info.id) {
                    log::debug!(
                        "QuorumBuilder: pre-registering remote agent '{}' in quorum",
                        info.id
                    );
                    builder = builder.preregister_agent(info, handle);
                }
            }
        }

        for delegate in self.delegates {
            let agent_info = AgentInfo {
                id: delegate.id.clone(),
                name: delegate.id.clone(),
                description: delegate.description.clone().unwrap_or_default(),
                capabilities: delegate.capabilities.clone(),
                required_capabilities: delegate.required_capabilities.clone(),
                meta: None,
            };
            let llm_config = build_llm_config(&delegate)?;
            let tools = delegate.tools.clone();
            let middleware_entries = delegate.middleware.clone();
            let exec = delegate.execution.clone();
            let registry = registry.clone();
            let delegate_agent_id = delegate.id.clone();
            let snapshot_policy_for_delegate = self.snapshot_policy;
            let delegation_wait_policy_for_delegate = self.delegation_wait_policy.clone();
            let delegation_wait_timeout_for_delegate = self.delegation_wait_timeout_secs;
            let delegation_cancel_grace_for_delegate = self.delegation_cancel_grace_secs;
            builder = builder.add_delegate_agent(agent_info, move |store, event_journal| {
                use crate::config::RuntimeExecutionPolicy;
                let mut b = AgentConfigBuilder::new(
                    registry.clone(),
                    store.clone(),
                    event_journal.clone(),
                    llm_config.clone(),
                )
                .with_agent_id(delegate_agent_id.clone())
                .with_tool_policy(ToolPolicy::BuiltInOnly)
                .with_snapshot_policy(snapshot_policy_for_delegate);

                if snapshot_policy_for_delegate != SnapshotPolicy::None {
                    b = b.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
                }

                if !tools.is_empty() {
                    b = b.with_allowed_tools(tools.clone());
                }

                let auto_compact = exec.compaction.auto;
                b = b
                    .with_execution_policy(RuntimeExecutionPolicy::from(&exec))
                    .with_delegation_wait_policy(delegation_wait_policy_for_delegate.clone())
                    .with_delegation_wait_timeout_secs(delegation_wait_timeout_for_delegate)
                    .with_delegation_cancel_grace_secs(delegation_cancel_grace_for_delegate);
                apply_middleware_from_config(&mut b, &middleware_entries, auto_compact);

                match exec.snapshot.backend.as_str() {
                    "git" => {
                        b = b.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
                    }
                    "none" | "" => {}
                    other => {
                        log::warn!(
                            "Unknown snapshot backend '{}' for delegate, ignoring",
                            other
                        );
                    }
                }

                let config = Arc::new(b.build());
                Arc::new(AgentHandle::from_config(config)) as Arc<dyn AgentHandleTrait>
            });
        }

        // Create RoutingActor + snapshot handle if there are peer delegates.
        // The snapshot is shared with the orchestrator (via AgentQuorumBuilder)
        // for lock-free reads at delegation time. Created before the planner
        // factory so the planner can register routing tools.
        #[cfg(feature = "remote")]
        let routing_actor_ref = if !self.peer_delegates.is_empty() {
            use crate::agent::remote::routing::{
                ResolvePeer, RouteTarget, RoutingActor, SetProviderTarget,
                new_routing_snapshot_handle,
            };
            use kameo::actor::Spawn;

            let snapshot_handle = new_routing_snapshot_handle();
            let actor = RoutingActor::new(snapshot_handle.clone());
            let actor_ref = RoutingActor::spawn(actor);

            // Populate routes for each peer delegate.
            for (delegate_id, peer_name) in &self.peer_delegates {
                if let Err(e) = actor_ref
                    .ask(SetProviderTarget {
                        agent_id: delegate_id.clone(),
                        target: RouteTarget::Peer(peer_name.clone()),
                    })
                    .await
                {
                    log::warn!(
                        "RoutingActor: failed to set provider route for '{}': {:?}",
                        delegate_id,
                        e
                    );
                }

                // Attempt eager resolution if mesh is available.
                if let Some(ref mesh) = self.mesh {
                    if let Some(node_id) = mesh.resolve_peer_node_id(peer_name).await {
                        log::info!(
                            "peer_delegate '{}': resolved peer '{}' → node_id={} (routing table)",
                            delegate_id,
                            peer_name,
                            node_id
                        );
                        if let Err(e) = actor_ref
                            .ask(ResolvePeer {
                                peer_name: peer_name.clone(),
                                node_id: node_id.to_string(),
                            })
                            .await
                        {
                            log::warn!(
                                "RoutingActor: failed to resolve peer '{}': {:?}",
                                peer_name,
                                e
                            );
                        }
                    } else {
                        log::warn!(
                            "peer_delegate '{}': peer '{}' not found in mesh at build time. \
                             Will be resolved eagerly on PeerEvent::Discovered.",
                            delegate_id,
                            peer_name
                        );
                    }
                }
            }

            builder = builder.with_routing_snapshot(snapshot_handle);
            Some(actor_ref)
        } else {
            None
        };

        // Capture routing actor ref for the planner closure (feature-gated).
        #[cfg(feature = "remote")]
        let routing_actor_for_planner = routing_actor_ref.clone();

        let planner_llm = build_llm_config(&planner_config)?;
        let planner_tools = planner_config.tools.clone();
        let planner_middleware = planner_config.middleware.clone();
        let planner_exec = planner_config.execution.clone();
        let registry_for_planner = registry.clone();
        let snapshot_policy_for_planner = self.snapshot_policy;
        let delegation_wait_policy_for_planner = self.delegation_wait_policy.clone();
        let delegation_wait_timeout_for_planner = self.delegation_wait_timeout_secs;
        let delegation_cancel_grace_for_planner = self.delegation_cancel_grace_secs;
        builder = builder.with_planner(move |store, event_journal, agent_registry| {
            use crate::config::RuntimeExecutionPolicy;
            let mut b = AgentConfigBuilder::new(
                registry_for_planner.clone(),
                store.clone(),
                event_journal.clone(),
                planner_llm.clone(),
            )
            .with_agent_id("planner")
            .with_agent_registry(agent_registry)
            .with_snapshot_policy(snapshot_policy_for_planner);

            if snapshot_policy_for_planner != SnapshotPolicy::None {
                b = b.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
            }

            if !planner_tools.is_empty() {
                b = b
                    .with_tool_policy(ToolPolicy::BuiltInOnly)
                    .with_allowed_tools(planner_tools.clone());
            }

            let auto_compact = planner_exec.compaction.auto;
            b = b
                .with_execution_policy(RuntimeExecutionPolicy::from(&planner_exec))
                .with_delegation_wait_policy(delegation_wait_policy_for_planner.clone())
                .with_delegation_wait_timeout_secs(delegation_wait_timeout_for_planner)
                .with_delegation_cancel_grace_secs(delegation_cancel_grace_for_planner);
            apply_middleware_from_config(&mut b, &planner_middleware, auto_compact);

            match planner_exec.snapshot.backend.as_str() {
                "git" => {
                    b = b.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
                }
                "none" | "" => {}
                other => {
                    log::warn!("Unknown snapshot backend '{}' for planner, ignoring", other);
                }
            }

            // Register routing planner tools if RoutingActor is available.
            #[cfg(feature = "remote")]
            if let Some(actor_ref) = routing_actor_for_planner {
                use crate::tools::builtins::{RouteDelegationToPeerTool, UseRemoteProviderTool};
                b.extend_tool_registry([
                    Arc::new(RouteDelegationToPeerTool::new(actor_ref.clone()))
                        as Arc<dyn crate::tools::Tool>,
                    Arc::new(UseRemoteProviderTool::new(actor_ref))
                        as Arc<dyn crate::tools::Tool>,
                ]);
                log::info!(
                    "Registered routing planner tools: route_delegation_to_peer, use_remote_provider"
                );
            }

            let config = Arc::new(b.build());
            Arc::new(AgentHandle::from_config(config)) as Arc<dyn AgentHandleTrait>
        });

        builder = builder
            .with_delegation(self.delegation_enabled)
            .with_verification(self.verification_enabled)
            .with_wait_policy(self.delegation_wait_policy)
            .with_wait_timeout_secs(self.delegation_wait_timeout_secs)
            .with_cancel_grace_secs(self.delegation_cancel_grace_secs)
            .with_max_parallel_delegations(self.max_parallel_delegations);

        // Build delegation summarizer if configured
        if let Some(ref summary_config) = self.delegation_summary_config {
            if summary_config.enabled {
                match crate::delegation::DelegationSummarizer::from_config(
                    summary_config,
                    registry.clone(),
                )
                .await
                {
                    Ok(summarizer) => {
                        log::info!(
                            "Delegation summarizer enabled with provider: {}, model: {}",
                            summary_config.provider,
                            summary_config.model
                        );
                        builder = builder.with_delegation_summarizer(Some(Arc::new(summarizer)));
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to build delegation summarizer: {}. Proceeding without summary.",
                            e
                        );
                    }
                }
            } else {
                log::debug!("Delegation summarizer disabled in config");
            }
        }

        let quorum = builder.build().map_err(|e| anyhow!(e.to_string()))?;

        // Extract planner handle for dashboard use — downcast to LocalAgentHandle
        let planner = quorum.planner();
        let planner_handle = planner
            .as_any()
            .downcast_ref::<AgentHandle>()
            .ok_or_else(|| anyhow!("Planner is not a LocalAgentHandle"))?;
        let planner_handle = Arc::new(AgentHandle::from_config(planner_handle.config.clone()));

        // Phase 7: apply mesh routing policy and store the mesh handle.
        #[cfg(feature = "remote")]
        planner_handle.set_mesh_fallback(self.mesh_auto_fallback);

        #[cfg(feature = "remote")]
        if let Some(mesh) = self.mesh {
            planner_handle.set_mesh(mesh.clone());

            // Wire peer delegates: set mesh handle on each for connectivity.
            // Routing is handled by the RoutingActor (populated above).
            for (delegate_id, _peer_name) in &self.peer_delegates {
                let handle = match quorum.delegate(delegate_id) {
                    Some(h) => h,
                    None => {
                        log::warn!(
                            "peer_delegate '{}' not found in quorum — skipping mesh wiring",
                            delegate_id
                        );
                        continue;
                    }
                };

                // Always set the mesh handle so the delegate can reach the mesh.
                handle.set_mesh_handle(mesh.clone());
            }

            // Subscribe the RoutingActor to PeerEvent for eager resolution / failover.
            if let Some(ref actor_ref) = routing_actor_ref {
                let peer_delegates = self.peer_delegates.clone();
                let mesh_for_events = mesh.clone();
                let actor_for_events = actor_ref.clone();
                tokio::spawn(async move {
                    use crate::agent::remote::routing::{ResolvePeer, UnresolvePeer};
                    use crate::agent::remote::{GetNodeInfo, PeerEvent, RemoteNodeManager};
                    let mut rx = mesh_for_events.subscribe_peer_events();
                    loop {
                        match rx.recv().await {
                            Ok(PeerEvent::Discovered(peer_id)) => {
                                // Try to resolve any unresolved peer names.
                                let per_peer_name =
                                    crate::agent::remote::dht_name::node_manager_for_peer(
                                        &peer_id.to_string(),
                                    );
                                let node_manager = match mesh_for_events
                                    .lookup_actor::<RemoteNodeManager>(&per_peer_name)
                                    .await
                                {
                                    Ok(Some(nm)) => nm,
                                    _ => continue,
                                };
                                let node_info = match node_manager
                                    .ask::<GetNodeInfo>(&GetNodeInfo)
                                    .await
                                {
                                    Ok(info) => info,
                                    Err(e) => {
                                        log::debug!(
                                            "RoutingActor PeerEvent handler: GetNodeInfo failed for {}: {}",
                                            peer_id,
                                            e
                                        );
                                        continue;
                                    }
                                };
                                // Check if this hostname matches any peer delegate name.
                                for (_delegate_id, peer_name) in &peer_delegates {
                                    if node_info.hostname == *peer_name
                                        && let Err(e) = actor_for_events
                                            .ask(ResolvePeer {
                                                peer_name: peer_name.clone(),
                                                node_id: node_info.node_id.to_string(),
                                            })
                                            .await
                                    {
                                        log::warn!(
                                            "RoutingActor PeerEvent handler: ResolvePeer failed: {:?}",
                                            e
                                        );
                                    }
                                }
                            }
                            Ok(PeerEvent::Expired(peer_id)) => {
                                let node_id_str = peer_id.to_string();
                                if let Err(e) = actor_for_events
                                    .ask(UnresolvePeer {
                                        node_id: node_id_str.clone(),
                                    })
                                    .await
                                {
                                    log::warn!(
                                        "RoutingActor PeerEvent handler: UnresolvePeer failed for {}: {:?}",
                                        node_id_str,
                                        e
                                    );
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                log::warn!(
                                    "RoutingActor PeerEvent handler: lagged, skipped {} events",
                                    n
                                );
                            }
                        }
                    }
                });
            }
        }

        Ok(Quorum {
            inner: quorum,
            storage: backend,
            planner_handle,
            planner_session_id: Arc::new(Mutex::new(None)),
            cwd,
            callbacks: Arc::new(EventCallbacksState::new(None)),
        })
    }
}

pub struct Quorum {
    inner: AgentQuorum,
    #[cfg_attr(not(feature = "dashboard"), allow(dead_code))]
    storage: Arc<dyn StorageBackend>,
    #[cfg_attr(not(feature = "dashboard"), allow(dead_code))]
    planner_handle: Arc<AgentHandle>,
    planner_session_id: Arc<Mutex<Option<String>>>,
    cwd: Option<PathBuf>,
    callbacks: Arc<EventCallbacksState>,
}

impl Quorum {
    pub async fn chat(&self, prompt: &str) -> Result<String> {
        let session_id = self.ensure_planner_session().await?;
        let request = PromptRequest::new(
            session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        let planner = self.inner.planner();
        planner
            .prompt(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let history = self
            .inner
            .store()
            .get_history(&session_id)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        latest_assistant_message(&history).ok_or_else(|| anyhow!("No assistant response found"))
    }

    pub fn inner(&self) -> &AgentQuorum {
        &self.inner
    }

    /// Access the planner's `AgentHandle` for advanced configuration.
    ///
    /// The handle provides access to the session registry, event bus, and agent config.
    /// Use this when you need to interact with sessions directly or integrate with
    /// the kameo mesh (e.g., bootstrapping `RemoteNodeManager`).
    pub fn handle(&self) -> Arc<AgentHandle> {
        self.planner_handle.clone()
    }

    pub fn planner(&self) -> Arc<dyn AgentHandleTrait> {
        self.inner.planner()
    }

    pub fn delegate(&self, id: &str) -> Option<Arc<dyn AgentHandleTrait>> {
        self.inner.delegate(id)
    }

    #[cfg(feature = "dashboard")]
    pub fn dashboard(&self) -> AgentServer {
        AgentServer::new(
            self.planner_handle.clone(),
            self.storage.clone(),
            self.cwd.clone(),
        )
    }

    /// Start an ACP server with the specified transport.
    ///
    /// # Arguments
    /// * `transport` - Either "stdio" for stdin/stdout, or "ip:port" for WebSocket
    ///
    /// # Example
    /// ```rust,no_run
    /// # use querymt_agent::prelude::*;
    /// # #[tokio::main]
    /// # async fn main() -> anyhow::Result<()> {
    /// let quorum = Agent::multi()
    ///     .planner(|p| p.provider("openai", "gpt-4"))
    ///     .build()
    ///     .await?;
    ///     
    /// quorum.acp("stdio").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn acp(&self, transport: &str) -> Result<()> {
        match transport {
            "stdio" => crate::acp::serve_stdio(self.planner_handle.clone())
                .await
                .map_err(|e| anyhow!("ACP stdio error: {}", e)),
            addr if addr.contains(':') => Err(anyhow!(
                "WebSocket ACP not yet implemented for Quorum. Use .dashboard().run(\"{}\") for web access.",
                addr
            )),
            _ => Err(anyhow!(
                "Invalid ACP transport '{}'. Use 'stdio' or 'ip:port' format.",
                transport
            )),
        }
    }

    async fn ensure_planner_session(&self) -> Result<String> {
        if let Some(existing) = self.planner_session_id.lock().unwrap().clone() {
            return Ok(existing);
        }
        let planner = self.inner.planner();
        let request = match &self.cwd {
            Some(cwd) => NewSessionRequest::new(cwd.clone()),
            None => NewSessionRequest::new(PathBuf::new()),
        };
        let response = planner
            .new_session(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let session_id = response.session_id.to_string();
        *self.planner_session_id.lock().unwrap() = Some(session_id.clone());
        Ok(session_id)
    }

    async fn create_new_planner_session(&self) -> Result<String> {
        let planner = self.inner.planner();
        let request = match &self.cwd {
            Some(cwd) => NewSessionRequest::new(cwd.clone()),
            None => NewSessionRequest::new(PathBuf::new()),
        };
        let response = planner
            .new_session(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(response.session_id.to_string())
    }

    /// Build a Quorum from a quorum config
    pub async fn from_quorum_config(config: QuorumConfig) -> Result<Self> {
        let builder = Self::builder_from_quorum_config(config, None)?;
        builder.build().await
    }

    /// Build a `Quorum` from a quorum config, optionally injecting a pre-populated
    /// agent registry and mesh handle (Phase 7: config-driven remote agents).
    ///
    /// When `initial_registry` is `Some`, the remote agent entries from the registry
    /// are pre-registered in the quorum's delegation registry *before* local delegates,
    /// so local delegates with the same ID take precedence.
    ///
    /// When `mesh` is `Some`, the `MeshHandle` is stored on the planner's `AgentHandle`
    /// via `set_mesh()` so that mesh-aware methods (`list_remote_nodes`,
    /// `create_remote_session`, etc.) work.
    #[cfg(feature = "remote")]
    pub async fn from_quorum_config_with_registry(
        config: QuorumConfig,
        initial_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
        mesh: Option<crate::agent::remote::MeshHandle>,
        mesh_auto_fallback: bool,
    ) -> Result<Self> {
        let mut builder = Self::builder_from_quorum_config(config, initial_registry)?;
        builder.mesh = mesh;
        builder.mesh_auto_fallback = mesh_auto_fallback;
        builder.build().await
    }

    /// Shared helper: configure a `QuorumBuilder` from a `QuorumConfig`.
    fn builder_from_quorum_config(
        config: QuorumConfig,
        initial_registry: Option<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>>,
    ) -> Result<QuorumBuilder> {
        let mut builder = QuorumBuilder::new();

        if let Some(cwd) = config.quorum.cwd {
            builder = builder.cwd(cwd);
        }
        if let Some(db) = config.quorum.db {
            builder = builder.db(db);
        }

        builder.delegation_enabled = config.quorum.delegation;
        builder.verification_enabled = config.quorum.verification;
        builder.delegation_summary_config = config.quorum.delegation_summary;
        builder.delegation_wait_policy = config.quorum.delegation_wait_policy;
        builder.delegation_wait_timeout_secs = config.quorum.delegation_wait_timeout_secs;
        builder.delegation_cancel_grace_secs = config.quorum.delegation_cancel_grace_secs;
        builder.max_parallel_delegations = config.quorum.max_parallel_delegations;

        // Parse snapshot policy
        let snapshot_policy = parse_snapshot_policy(config.quorum.snapshot_policy)?;

        // Build the set of builtin tool names for validation
        let builtin_names: HashSet<String> = all_builtin_tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();

        // Configure planner with tool resolution
        let mut planner_config = AgentConfig::new("planner");
        let mut llm = querymt::LLMParams::new()
            .provider(config.planner.provider)
            .model(config.planner.model);
        for part in config.planner.system {
            if let crate::config::SystemPart::Inline(s) = part {
                llm = llm.system(s);
            }
        }
        if let Some(api_key) = config.planner.api_key {
            llm = llm.api_key(api_key);
        }
        if let Some(params) = config.planner.parameters {
            for (key, value) in params {
                llm = llm.parameter(key, value);
            }
        }
        planner_config.llm_config = Some(llm);

        // Resolve planner tools (validates builtin tools and prepares for MCP)
        let planner_resolved =
            resolve_tools(&config.planner.tools, &config.mcp, &[], &builtin_names)?;
        planner_config.tools = planner_resolved.builtins;

        // Note: MCP tools are not yet supported in the simple Quorum API.
        if !planner_resolved.mcp_servers.is_empty() {
            log::warn!(
                "MCP servers configured for planner, but MCP is not yet supported in Quorum. Only builtin tools will be available."
            );
        }

        planner_config.middleware = config.planner.middleware;
        planner_config.execution = config.planner.execution;

        builder.planner_config = Some(planner_config);

        // Configure delegates with tool resolution
        for delegate in config.delegates {
            // Capture peer before consuming delegate
            #[cfg(feature = "remote")]
            let delegate_peer = delegate.peer.clone();

            let mut delegate_config = AgentConfig::new(delegate.id.clone());
            let mut llm = querymt::LLMParams::new()
                .provider(delegate.provider)
                .model(delegate.model);
            for part in delegate.system {
                if let crate::config::SystemPart::Inline(s) = part {
                    llm = llm.system(s);
                }
            }
            if let Some(api_key) = delegate.api_key {
                llm = llm.api_key(api_key);
            }
            if let Some(params) = delegate.parameters {
                for (key, value) in params {
                    llm = llm.parameter(key, value);
                }
            }
            delegate_config.llm_config = Some(llm);
            delegate_config.description = delegate.description;
            delegate_config.capabilities = delegate.capabilities;

            let delegate_resolved =
                resolve_tools(&delegate.tools, &config.mcp, &delegate.mcp, &builtin_names)?;
            delegate_config.tools = delegate_resolved.builtins;

            if !delegate_resolved.mcp_servers.is_empty() {
                log::warn!(
                    "MCP servers configured for delegate '{}', but MCP is not yet supported in Quorum. Only builtin tools will be available.",
                    delegate.id
                );
            }

            delegate_config.required_capabilities =
                infer_required_capabilities(&delegate_config.tools)
                    .into_iter()
                    .collect();

            delegate_config.middleware = delegate.middleware;
            delegate_config.execution = delegate.execution;

            // Register peer delegate for mesh routing resolution in build()
            #[cfg(feature = "remote")]
            if let Some(peer_name) = delegate_peer {
                builder
                    .peer_delegates
                    .push((delegate_config.id.clone(), peer_name));
            }

            builder.delegates.push(delegate_config);
        }

        builder.snapshot_policy = snapshot_policy;
        builder.initial_registry = initial_registry;

        Ok(builder)
    }
}

#[async_trait]
impl ChatRunner for Quorum {
    async fn chat(&self, prompt: &str) -> Result<String> {
        Quorum::chat(self, prompt).await
    }

    async fn chat_session(&self) -> Result<Box<dyn ChatSession>> {
        let session_id = self.create_new_planner_session().await?;
        let session = QuorumSession::new(
            self.inner.planner(),
            self.inner.event_fanout(),
            self.inner.store(),
            session_id,
            self.cwd.clone(),
        );
        Ok(Box::new(session))
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::events::EventEnvelope> {
        self.inner.subscribe_events()
    }

    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>) {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_delegation_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_delegation(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    #[cfg(feature = "dashboard")]
    fn dashboard(&self) -> AgentServer {
        Quorum::dashboard(self)
    }
}

/// A session for interacting with a Quorum's planner agent
pub struct QuorumSession {
    planner: Arc<dyn AgentHandleTrait>,
    session_id: String,
    callbacks: Arc<EventCallbacksState>,
    event_fanout: Arc<crate::event_fanout::EventFanout>,
    store: Arc<dyn SessionStore>,
    #[allow(dead_code)]
    cwd: Option<PathBuf>,
}

impl QuorumSession {
    fn new(
        planner: Arc<dyn AgentHandleTrait>,
        event_fanout: Arc<crate::event_fanout::EventFanout>,
        store: Arc<dyn SessionStore>,
        session_id: String,
        cwd: Option<PathBuf>,
    ) -> Self {
        let callbacks = Arc::new(EventCallbacksState::new(Some(session_id.clone())));
        Self {
            planner,
            session_id,
            callbacks,
            event_fanout,
            store,
            cwd,
        }
    }
}

#[async_trait]
impl ChatSession for QuorumSession {
    fn id(&self) -> &str {
        &self.session_id
    }

    async fn chat(&self, prompt: &str) -> Result<String> {
        let request = PromptRequest::new(
            self.session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        self.planner
            .prompt(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let history = self
            .store
            .get_history(&self.session_id)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        latest_assistant_message(&history).ok_or_else(|| anyhow!("No assistant response found"))
    }

    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>) {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.event_fanout.subscribe());
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.event_fanout.subscribe());
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.event_fanout.subscribe());
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.event_fanout.subscribe());
    }
}

/// Helper to parse snapshot policy string to enum
fn parse_snapshot_policy(policy: Option<String>) -> Result<SnapshotPolicy> {
    match policy.as_deref() {
        None => Ok(SnapshotPolicy::None),
        Some("none") => Ok(SnapshotPolicy::None),
        Some("metadata") => Ok(SnapshotPolicy::Metadata),
        Some("diff") => Ok(SnapshotPolicy::Diff),
        Some(other) => Err(anyhow!(
            "Invalid snapshot_policy '{}'. Valid options: 'none', 'metadata', 'diff'",
            other
        )),
    }
}

/// Helper to apply middleware from config entries to a builder.
///
/// `factory_config` is a snapshot of the config built so far (before middleware
/// are appended). Middleware factories only need `compaction_config` and
/// `event_journal` from it.
fn apply_middleware_from_config(
    builder: &mut AgentConfigBuilder,
    entries: &[MiddlewareEntry],
    auto_compact: bool,
) {
    // Build a lightweight AgentConfig snapshot that factory closures can read.
    // Middleware factories only consult compaction_config and event_journal.
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::config::RuntimeExecutionPolicy;
    use std::sync::Arc;

    let ephemeral_policy = RuntimeExecutionPolicy {
        compaction: builder.compaction_config().clone(),
        ..Default::default()
    };

    let factory_config = Arc::new(
        AgentConfigBuilder::from_provider(builder.provider().clone(), builder.event_journal())
            .with_agent_registry(builder.agent_registry())
            .with_execution_policy(ephemeral_policy)
            .build(),
    );

    for entry in entries {
        match MIDDLEWARE_REGISTRY.create(&entry.middleware_type, &entry.config, &factory_config) {
            Ok(middleware) => {
                builder.push_middleware(middleware);
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("disabled") {
                    log::warn!(
                        "Failed to create middleware '{}': {}",
                        entry.middleware_type,
                        e
                    );
                }
            }
        }
    }

    let has_context_middleware = entries.iter().any(|e| e.middleware_type == "context");
    if auto_compact && !has_context_middleware {
        log::info!("Auto-enabling ContextMiddleware for compaction");
        builder.push_middleware(Arc::new(crate::middleware::ContextMiddleware::new(
            crate::middleware::ContextConfig::default().auto_compact(true),
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quorum_delegate_builder_system() {
        let builder = QuorumBuilder::new()
            .cwd(PathBuf::from("/tmp"))
            .planner(|p| {
                p.provider("openai", "gpt-4")
                    .system("Planner system prompt")
                    .tools(["delegate"])
            })
            .delegate("coder", |d| {
                d.provider("ollama", "model")
                    .system("Coder system prompt")
                    .tools(["shell"])
            });

        // Test that the delegate was added with correct system prompt
        assert_eq!(builder.delegates.len(), 1);
        let delegate = &builder.delegates[0];
        assert_eq!(
            delegate.llm_config.as_ref().map(|c| c.system.clone()),
            Some(vec!["Coder system prompt".to_string()])
        );
    }

    #[test]
    fn test_parse_snapshot_policy() {
        assert_eq!(parse_snapshot_policy(None).unwrap(), SnapshotPolicy::None);
        assert_eq!(
            parse_snapshot_policy(Some("none".to_string())).unwrap(),
            SnapshotPolicy::None
        );
        assert_eq!(
            parse_snapshot_policy(Some("metadata".to_string())).unwrap(),
            SnapshotPolicy::Metadata
        );
        assert_eq!(
            parse_snapshot_policy(Some("diff".to_string())).unwrap(),
            SnapshotPolicy::Diff
        );
        assert!(parse_snapshot_policy(Some("invalid".to_string())).is_err());
    }

    #[test]
    fn test_quorum_builder_snapshot_policy() {
        let builder = QuorumBuilder::new().with_snapshot_policy(SnapshotPolicy::Diff);
        assert_eq!(builder.snapshot_policy, SnapshotPolicy::Diff);
    }

    // ── peer_delegates wiring ────────────────────────────────────────────────

    #[cfg(feature = "remote")]
    #[test]
    fn test_builder_from_quorum_config_captures_peer_delegates() {
        use crate::config::QuorumConfig;

        let toml = r#"
[quorum]
cwd = "/tmp"

[planner]
provider = "openai"
model = "gpt-4"

[[delegates]]
id = "coder"
provider = "llama_cpp"
model = "qwen3"
peer = "gpu-node"
"#;
        let config: QuorumConfig = toml::from_str(toml).expect("parse QuorumConfig");
        let builder =
            Quorum::builder_from_quorum_config(config, None).expect("builder_from_quorum_config");

        // The builder must have registered the peer delegate
        assert_eq!(builder.peer_delegates.len(), 1);
        assert_eq!(builder.peer_delegates[0].0, "coder");
        assert_eq!(builder.peer_delegates[0].1, "gpu-node");
    }

    #[cfg(feature = "remote")]
    #[test]
    fn test_builder_from_quorum_config_no_peer_no_peer_delegates() {
        let toml = r#"
[quorum]
cwd = "/tmp"

[planner]
provider = "openai"
model = "gpt-4"

[[delegates]]
id = "writer"
provider = "anthropic"
model = "claude-haiku"
"#;
        let config: crate::config::QuorumConfig = toml::from_str(toml).expect("parse QuorumConfig");
        let builder =
            Quorum::builder_from_quorum_config(config, None).expect("builder_from_quorum_config");

        assert!(builder.peer_delegates.is_empty());
    }

    /// When a delegate has `peer` set, `builder_from_quorum_config` must populate
    /// `peer_delegates`. The `build()` method must not fail if the peer is unknown
    /// — it should log a warning and proceed. Integration tests with a real mesh
    /// cover the happy path (peer found → NodeId stored).
    #[cfg(feature = "remote")]
    #[test]
    fn peer_delegates_populated_for_peer_field_in_delegate() {
        let toml = r#"
[quorum]
cwd = "/tmp"

[planner]
provider = "openai"
model = "gpt-4"

[[delegates]]
id = "coder"
provider = "openai"
model = "gpt-4"
peer = "gpu-node"

[[delegates]]
id = "writer"
provider = "openai"
model = "gpt-4"
"#;
        let config: crate::config::QuorumConfig = toml::from_str(toml).expect("parse QuorumConfig");
        let builder =
            Quorum::builder_from_quorum_config(config, None).expect("builder_from_quorum_config");

        // Only "coder" has a peer
        assert_eq!(builder.peer_delegates.len(), 1);
        assert_eq!(builder.peer_delegates[0].0, "coder");
        assert_eq!(builder.peer_delegates[0].1, "gpu-node");
        // "writer" has no peer
        assert!(!builder.peer_delegates.iter().any(|(id, _)| id == "writer"));
    }
}

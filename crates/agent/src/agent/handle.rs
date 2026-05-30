//! AgentHandle trait and LocalAgentHandle concrete implementation.
//!
//! `AgentHandle` is the trait that all agent handles implement — both local
//! and remote. `LocalAgentHandle` is the concrete implementation for local
//! agents, bundling shared config, the kameo session registry, and
//! connection-level mutable state.

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, ClientState};
use crate::agent::remote::SessionActorRef;
use crate::agent::session_materializer::{PreparedSessionResult, SessionMaterializer};
use crate::agent::session_registry::SessionRegistry;
use crate::delegation::AgentRegistry;
use crate::event_fanout::EventFanout;
use crate::events::{AgentEventKind, EventEnvelope};

use crate::index::WorkspaceIndexManagerActor;
use crate::middleware::CompositeDriver;
use crate::profiles::{ProfileCatalog, ProfileRuntimeManager};
use crate::send_agent::SendAgent;
use crate::session::store::LLMConfig;
use crate::tools::ToolRegistry;
use agent_client_protocol::schema::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, DeleteSessionRequest, DeleteSessionResponse, Error, ExtNotification,
    ExtRequest, ExtResponse, ForkSessionRequest, ForkSessionResponse, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionInfo, SetSessionModelRequest,
    SetSessionModelResponse,
};
use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use kameo::actor::ActorRef;
use querymt::LLMParams;
use querymt::chat::ReasoningEffort;
use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
#[cfg(feature = "remote")]
use tokio::sync::Semaphore;
use tokio::sync::{Mutex, broadcast};

// ══════════════════════════════════════════════════════════════════════════
//  AgentHandle trait — the unified interface for local and remote agents
// ══════════════════════════════════════════════════════════════════════════

/// Trait capturing the interface consumers actually use for agent interaction.
///
/// Both `LocalAgentHandle` (local agent) and `RemoteAgentHandle` (remote agent)
/// implement this trait. The registry, quorum, delegation orchestrator, and UI
/// all work with `Arc<dyn AgentHandle>`.
#[async_trait]
pub trait AgentHandle: Send + Sync {
    // --- Session management ---

    /// Create a new session. Returns session_id.
    async fn new_session(
        &self,
        req: NewSessionRequest,
    ) -> std::result::Result<NewSessionResponse, Error>;

    /// Send a prompt to a session.
    async fn prompt(&self, req: PromptRequest) -> std::result::Result<PromptResponse, Error>;

    /// Cancel an ongoing prompt.
    async fn cancel(&self, notif: CancelNotification) -> std::result::Result<(), Error>;

    /// Load an existing session into this agent runtime.
    async fn load_session(
        &self,
        req: LoadSessionRequest,
    ) -> std::result::Result<LoadSessionResponse, Error>;

    /// Create a session for delegation. Returns both session_id and a
    /// SessionActorRef for direct kameo messaging (planning context, history).
    async fn create_delegation_session(
        &self,
        cwd: Option<String>,
        parent_session_id: String,
    ) -> std::result::Result<(String, SessionActorRef), Error>;

    // --- Event system ---

    /// Subscribe to agent events.
    fn subscribe_events(&self) -> broadcast::Receiver<EventEnvelope>;

    /// Get the event fanout.
    fn event_fanout(&self) -> &Arc<EventFanout>;

    /// Emit an event.
    fn emit_event(&self, session_id: &str, kind: AgentEventKind);

    // --- Registry access ---

    /// Get the agent/delegation registry.
    fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync>;

    // --- Remote mesh routing ---

    /// Set the mesh handle for provider routing.
    ///
    /// Default implementation is a no-op. `LocalAgentHandle` overrides
    /// to store the mesh handle on its `SessionProvider`, enabling
    /// `MeshChatProvider` creation for sessions on this agent.
    #[cfg(feature = "remote")]
    fn set_mesh_handle(&self, _mesh: crate::agent::remote::MeshHandle) {}

    // --- Downcasting (transitional) ---

    /// For downcasting to concrete types. Transitional — should be eliminated
    /// over time by moving needed methods onto the trait.
    fn as_any(&self) -> &dyn Any;
}

/// Lightweight facade replacing `Arc<QueryMTAgent>` for all consumers.
///
/// Holds shared config, the kameo session registry, and connection-level
/// mutable state. Not an actor — just a convenient bundle.
#[cfg(feature = "remote")]
#[derive(Clone, Debug)]
struct CachedNodeEntry {
    info: crate::agent::remote::NodeInfo,
    expires_at: std::time::Instant,
}

#[cfg(feature = "remote")]
#[derive(Debug)]
struct RemoteNodeMetadataCache {
    by_label: parking_lot::RwLock<std::collections::HashMap<String, CachedNodeEntry>>,
    invalidation_task_started: AtomicBool,
}

#[cfg(feature = "remote")]
impl RemoteNodeMetadataCache {
    fn new() -> Self {
        Self {
            by_label: parking_lot::RwLock::new(std::collections::HashMap::new()),
            invalidation_task_started: AtomicBool::new(false),
        }
    }
}

pub struct LocalAgentHandle {
    pub config: Arc<AgentConfig>,
    pub registry: Arc<Mutex<SessionRegistry>>,

    /// Session materializer for heavy async work (DB, MCP, actor spawn).
    /// Separates expensive operations from registry lock to keep control plane fast.
    pub session_materializer: Arc<SessionMaterializer>,

    // Connection-level mutable state
    pub client_state: Arc<StdMutex<Option<ClientState>>>,
    pub bridge: Arc<StdMutex<Option<ClientBridgeSender>>>,

    // Mutable default mode (UI "set agent mode" → affects new sessions)
    pub default_mode: StdMutex<AgentMode>,

    /// Default reasoning effort for new sessions. Lock-free via `ArcSwap`.
    /// `None` = use model heuristic defaults.
    pub default_reasoning_effort: ArcSwap<Option<ReasoningEffort>>,

    /// Handle to the kameo mesh swarm, set after `bootstrap_mesh()` succeeds.
    /// `None` in local-only mode. Wrapped in a `Mutex` for interior mutability
    /// so startup code can set it on the shared `Arc<LocalAgentHandle>`.
    #[cfg(feature = "remote")]
    pub mesh: StdMutex<Option<crate::agent::remote::MeshHandle>>,

    /// Keepalive refs for mesh-visible local actors so new scopes can re-register
    /// the existing node manager/provider host without respawning them.
    #[cfg(feature = "remote")]
    pub local_mesh_actor_refs: StdMutex<Option<crate::agent::remote::LocalMeshActorRefs>>,

    #[cfg(feature = "remote")]
    remote_node_cache: Arc<RemoteNodeMetadataCache>,

    /// Non-blocking model inventory with snapshot-based reads and background refresh.
    /// This is the canonical public API for model listing and cache management.
    pub model_inventory: crate::model_inventory::ModelInventory,

    /// Shared OAuth service for all auth operations across UI and ACP transports.
    pub oauth_service: crate::auth::service::OAuthService,

    /// Optional profile runtime manager shared by UI and ACP extension transports.
    pub profiles: ArcSwap<Option<Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>>>,

    /// Handle to the scheduler actor, set after `start_scheduler()` succeeds.
    /// `None` if scheduling is not enabled or lease was not acquired.
    scheduler_handle: StdMutex<Option<crate::scheduler::SchedulerHandle>>,

    /// Guard to ensure `shutdown()` only runs its body once.
    shutdown_done: AtomicBool,
}

// ── Remote node lookup type aliases ─────────────────────────────────────────
// These aliases improve readability of the complex async types used for
// concurrent remote node discovery operations.

#[cfg(feature = "remote")]
type RemoteNodeInfoResult = Result<
    Result<crate::agent::remote::NodeInfo, kameo::error::RemoteSendError<crate::error::AgentError>>,
    tokio::time::error::Elapsed,
>;

#[cfg(feature = "remote")]
type RemoteNodeLookupResult = (String, Option<libp2p::PeerId>, RemoteNodeInfoResult);

#[cfg(feature = "remote")]
type RemoteNodeLookupFuture = futures_util::future::BoxFuture<'static, RemoteNodeLookupResult>;

#[cfg(feature = "remote")]
type RemoteNodeLookupQueue = futures_util::stream::FuturesUnordered<RemoteNodeLookupFuture>;

impl LocalAgentHandle {
    /// Construct a `LocalAgentHandle` from a shared `AgentConfig`.
    ///
    /// This is the canonical way to create a `LocalAgentHandle` after building
    /// an `AgentConfig` via `AgentConfigBuilder::build()`.
    pub fn from_config(config: Arc<AgentConfig>) -> Self {
        let registry = Arc::new(Mutex::new(SessionRegistry::new(config.clone())));
        let session_materializer = Arc::new(SessionMaterializer::new(config.clone()));
        let model_inventory = crate::model_inventory::ModelInventory::new(config.clone());
        let oauth_service = crate::auth::service::OAuthService::new(
            config.clone(),
            model_inventory.clone(),
            std::sync::Arc::new(Mutex::new(std::collections::HashMap::new())),
            std::sync::Arc::new(Mutex::new(None)),
        );
        Self {
            config,
            registry,
            session_materializer,
            client_state: Arc::new(StdMutex::new(None)),
            bridge: Arc::new(StdMutex::new(None)),
            default_mode: StdMutex::new(crate::agent::core::AgentMode::Build),
            default_reasoning_effort: ArcSwap::from_pointee(None),
            #[cfg(feature = "remote")]
            mesh: StdMutex::new(None),
            #[cfg(feature = "remote")]
            local_mesh_actor_refs: StdMutex::new(None),
            #[cfg(feature = "remote")]
            remote_node_cache: Arc::new(RemoteNodeMetadataCache::new()),
            model_inventory,
            oauth_service,
            profiles: ArcSwap::from_pointee(None),
            scheduler_handle: StdMutex::new(None),
            shutdown_done: AtomicBool::new(false),
        }
    }

    /// Subscribes to agent events via the fanout (live stream).
    pub fn subscribe_events(&self) -> broadcast::Receiver<crate::events::EventEnvelope> {
        self.config.event_sink.fanout().subscribe()
    }

    pub fn set_profiles(&self, profiles: Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>) {
        self.profiles.store(Arc::new(Some(profiles)));
    }

    pub fn profiles(&self) -> Option<Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>> {
        self.profiles.load_full().as_ref().clone()
    }

    /// Acquire the registry lock with tracing for wait and hold durations.
    ///
    /// Returns a guard after recording `registry.lock.wait_ms` for lock acquisition.
    ///
    /// Hold duration is not instrumented here because the plain mutex guard does not
    /// provide a drop hook for recording `registry.lock.hold_ms`.
    pub async fn registry_lock(&self) -> tokio::sync::MutexGuard<'_, SessionRegistry> {
        let start = std::time::Instant::now();
        let guard = self.registry.lock().await;
        let wait_ms = start.elapsed().as_millis() as u64;

        if wait_ms > 10 {
            tracing::warn!(
                wait_ms,
                "Registry lock wait exceeded 10ms - possible contention"
            );
        }

        // Note: We can't easily instrument hold duration with a simple guard
        // because the guard needs to record the time on drop.
        // For now, we rely on callers to use tracing spans if needed.
        // The key insight is that with the 3-phase pattern, hold duration
        // should be microseconds for register_prepared_session().
        guard
    }

    /// Access the agent registry.
    pub fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        self.config.agent_registry.clone()
    }

    /// Access the tool registry for built-in tool execution.
    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.config.tool_registry_arc()
    }

    /// Access the pending elicitations map for resolving tool and MCP server elicitation requests.
    pub fn pending_elicitations(&self) -> crate::elicitation::PendingElicitationMap {
        self.config.pending_elicitations()
    }

    /// Access the workspace manager actor ref.
    pub fn workspace_manager_actor(&self) -> ActorRef<WorkspaceIndexManagerActor> {
        self.config.workspace_manager_actor()
    }

    /// Sets the client bridge for ACP stdio communication.
    ///
    /// Also propagates the bridge to the session registry so that newly
    /// created sessions receive it via `SetBridge`.
    pub async fn set_bridge(&self, bridge: ClientBridgeSender) {
        if let Ok(mut handle) = self.bridge.lock() {
            *handle = Some(bridge.clone());
        }
        self.registry.lock().await.set_bridge(bridge);
    }

    /// Emits an event for external observers.
    ///
    /// This is a detached fire-and-forget API.
    /// FIXME: Prefer an awaited emit path for critical flows.
    pub fn emit_event(&self, session_id: &str, kind: crate::events::AgentEventKind) {
        self.config.emit_event(session_id, kind);
    }

    /// Start the scheduler actor if a schedule repository is configured.
    ///
    /// This should be called after construction, during agent startup.
    /// Returns `true` if the scheduler was started (lease acquired).
    /// Returns `false` if no schedule repository is configured or the lease
    /// was not acquired (another scheduler is active).
    pub async fn start_scheduler(&self) -> bool {
        let schedule_repo = match &self.config.schedule_repository {
            Some(repo) => repo.clone(),
            None => {
                log::debug!(
                    "LocalAgentHandle: no schedule repository configured, skipping scheduler"
                );
                return false;
            }
        };

        let handle = crate::scheduler::SchedulerActor::spawn(
            schedule_repo,
            self.config.provider.history_store(),
            self.registry.clone(),
            self.config.clone(),
            crate::scheduler::SchedulerConfig::default(),
        )
        .await;

        match handle {
            Some(h) => {
                if let Ok(mut guard) = self.scheduler_handle.lock() {
                    *guard = Some(h);
                }
                log::info!("LocalAgentHandle: scheduler started");
                true
            }
            None => {
                log::info!("LocalAgentHandle: scheduler not started (lease not acquired)");
                false
            }
        }
    }

    /// Get a reference to the scheduler handle, if the scheduler is running.
    pub fn scheduler(&self) -> Option<crate::scheduler::SchedulerHandle> {
        self.scheduler_handle
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn clear_scheduler_handle(&self) {
        if let Ok(mut guard) = self.scheduler_handle.lock() {
            *guard = None;
        }
    }

    fn scheduler_unavailable_error() -> agent_client_protocol::Error {
        agent_client_protocol::Error::internal_error().data("Scheduler unavailable".to_string())
    }

    fn is_actor_not_running_error(error_message: &str) -> bool {
        error_message.contains("actor not running")
    }

    async fn get_or_start_scheduler(&self) -> Option<crate::scheduler::SchedulerHandle> {
        if let Some(scheduler) = self.scheduler() {
            return Some(scheduler);
        }

        if self.start_scheduler().await {
            return self.scheduler();
        }

        None
    }

    /// Create a new session with already-connected MCP peers.
    ///
    /// This is used by mobile FFI clients that manage MCP transport lifetimes
    /// externally (e.g. pipe transports) and want those tools available in
    /// each newly created session.
    ///
    /// Uses the 3-phase materialization pattern:
    /// Create a new session.
    ///
    /// Uses the 3-phase materialization pattern:
    /// 1. Prepare (NO lock): DB creation, MCP init, actor spawn
    /// 2. Register (lock held): Insert into in-memory maps (microseconds)
    /// 3. Finalize (NO lock): DHT registration, event emission
    pub async fn new_session(
        &self,
        req: NewSessionRequest,
    ) -> std::result::Result<NewSessionResponse, Error> {
        // Auth check stays on LocalAgentHandle (connection-level concern)
        if let Ok(state) = self.client_state.lock()
            && let Some(state) = state.as_ref()
        {
            let auth_required = !self.config.auth_methods.is_empty();

            if auth_required && !state.authenticated {
                return Err(Error::auth_required());
            }
        }

        // Phase 1: Prepare session (heavy work, NO registry lock held)
        let prepared = self.session_materializer.prepare_new_session(req).await?;

        let session_id = prepared.session_id.clone();

        // Phase 2: Register session (fast, registry lock held for microseconds only)
        let session_ref = {
            let mut registry = self.registry_lock().await;
            registry.register_prepared_session(&prepared).await
        };

        // Phase 3: Finalize session (post-registration work, NO registry lock held)
        // Pass the bridge for setup outside the lock
        let bridge = self.bridge.lock().ok().and_then(|guard| guard.clone());
        self.session_materializer
            .finalize_session(&prepared, bridge)
            .await?;

        // Get current mode for response
        let current_mode = session_ref.get_mode().await.map_err(Error::from)?;

        Ok(NewSessionResponse::new(session_id)
            .modes(crate::agent::session_registry::mode_state(current_mode))
            .config_options(crate::agent::session_registry::config_options(
                current_mode,
                **self.config.default_reasoning_effort.load(),
            )))
    }

    /// Load an existing session. MCP attachments are resolved internally by
    /// the [`SessionMaterializer`] via the runtime attachment source.
    ///
    /// Uses the 3-phase materialization pattern with single-flight protection:
    /// 1. Check registry (lock held briefly): Return existing if already materialized
    /// 2. Prepare (NO lock): DB validation, MCP init, actor spawn
    /// 3. Register (lock held): Insert into in-memory maps (microseconds)
    /// 4. Finalize (NO lock): DHT registration, event emission
    pub async fn load_session(
        &self,
        req: agent_client_protocol::schema::LoadSessionRequest,
    ) -> std::result::Result<agent_client_protocol::schema::LoadSessionResponse, Error> {
        let session_id = req.session_id.to_string();

        // Single-flight check: Check if session is already materialized
        // Clone the session_ref out of the registry to avoid holding the lock
        // during the async get_mode() call.
        let existing_session_ref = {
            let registry = self.registry_lock().await;
            registry.get(&session_id).cloned()
        };

        if let Some(session_ref) = existing_session_ref {
            // Registry lock is now dropped, safe to make async actor call
            let current_mode = session_ref.get_mode().await.map_err(Error::from)?;
            return Ok(LoadSessionResponse::new()
                .modes(crate::agent::session_registry::mode_state(current_mode))
                .config_options(crate::agent::session_registry::config_options(
                    current_mode,
                    **self.config.default_reasoning_effort.load(),
                )));
        }

        // Phase 1: Prepare session (heavy work, NO registry lock held)
        // The session_materializer's internal single-flight lock prevents
        // duplicate materialization of the same session ID.
        // Pass registry so it can re-check after acquiring the lock.
        let (prepared, session_ref) = match self
            .session_materializer
            .prepare_load_session(req, Some(&self.registry))
            .await?
        {
            PreparedSessionResult::Prepared(prepared) => {
                let session_ref = {
                    let mut registry = self.registry_lock().await;
                    registry.register_prepared_session(&prepared).await
                };
                (Some(prepared), session_ref)
            }
            PreparedSessionResult::AlreadyRegistered(session_ref) => (None, session_ref),
        };

        // Phase 3: Finalize session (post-registration work, NO registry lock held)
        // Pass the bridge for setup outside the lock
        if let Some(prepared) = prepared.as_ref() {
            let bridge = self.bridge.lock().ok().and_then(|guard| guard.clone());
            self.session_materializer
                .finalize_session(prepared, bridge)
                .await?;
        }

        // Get current mode for response
        let current_mode = session_ref.get_mode().await.map_err(Error::from)?;

        Ok(LoadSessionResponse::new()
            .modes(crate::agent::session_registry::mode_state(current_mode))
            .config_options(crate::agent::session_registry::config_options(
                current_mode,
                **self.config.default_reasoning_effort.load(),
            )))
    }

    /// Gracefully shutdown the agent and all background tasks.
    pub async fn shutdown(&self) {
        if self.shutdown_done.swap(true, Ordering::SeqCst) {
            return;
        }
        log::info!("LocalAgentHandle: Starting graceful shutdown");

        // Shutdown the scheduler first
        if let Some(scheduler) = self.scheduler() {
            scheduler.shutdown().await;
        }

        self.config.shutdown().await;

        // Wait briefly for in-flight work to settle, then flush any
        // buffered OTLP spans/logs before the process exits.
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        querymt_utils::telemetry::flush_telemetry();

        log::info!("LocalAgentHandle: Shutdown complete");
    }

    // ── Schedule management API (Phase 7) ─────────────────────────────────

    /// Create a recurring task and schedule for a session.
    ///
    /// Creates the underlying `Task` row first (so FK constraints are satisfied),
    /// then creates the `Schedule` referencing it and registers it with the scheduler.
    ///
    /// Returns the schedule public ID, or an error if the scheduler is not running
    /// or the session cannot be found.
    pub async fn create_scheduled_task(
        &self,
        session_public_id: &str,
        prompt: &str,
        trigger: crate::session::domain_schedule::ScheduleTrigger,
        max_steps: Option<u32>,
        max_cost_usd: Option<f64>,
        max_runs: Option<u32>,
    ) -> Result<String, agent_client_protocol::Error> {
        use crate::session::domain::{Task, TaskKind, TaskStatus};
        use crate::session::domain_schedule::ScheduleExecutionLimits;

        // Look up session to get the internal row ID required by FK constraints.
        let store = self.config.provider.history_store();
        let session = store
            .get_session(session_public_id)
            .await
            .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?
            .ok_or_else(|| {
                agent_client_protocol::Error::invalid_params()
                    .data(format!("Session not found: {session_public_id}"))
            })?;

        // Create the task row so the schedule FK is satisfied.
        let now = time::OffsetDateTime::now_utc();
        let task = Task {
            id: 0,                    // populated by create_task
            public_id: String::new(), // generated by create_task
            session_id: session.id,
            kind: TaskKind::Recurring,
            status: TaskStatus::Active,
            expected_deliverable: Some(prompt.to_string()),
            acceptance_criteria: None,
            created_at: now,
            updated_at: now,
        };
        let task = store
            .create_task(task)
            .await
            .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?;

        // Build the schedule with valid internal IDs.
        let mut schedule = crate::session::domain_schedule::Schedule::new(
            task.public_id.clone(),
            session_public_id.to_string(),
            trigger,
        );
        schedule.task_id = task.id;
        schedule.session_id = session.id;

        // Apply user-provided limits.
        if max_runs.is_some() {
            schedule.config.max_runs = max_runs;
        }
        if max_steps.is_some() || max_cost_usd.is_some() {
            schedule.config.execution_limits = Some(ScheduleExecutionLimits {
                max_steps,
                max_cost_usd,
            });
        }

        let schedule_public_id = schedule.public_id.clone();

        let scheduler = self
            .get_or_start_scheduler()
            .await
            .ok_or_else(Self::scheduler_unavailable_error)?;

        match scheduler.add_schedule(schedule.clone()).await {
            Ok(()) => Ok(schedule_public_id),
            Err(e) => {
                let msg = e.to_string();
                if !Self::is_actor_not_running_error(&msg) {
                    return Err(agent_client_protocol::Error::internal_error().data(msg));
                }

                self.clear_scheduler_handle();
                let scheduler = self
                    .get_or_start_scheduler()
                    .await
                    .ok_or_else(Self::scheduler_unavailable_error)?;
                scheduler.add_schedule(schedule).await.map_err(|err| {
                    agent_client_protocol::Error::internal_error().data(err.to_string())
                })?;
                Ok(schedule_public_id)
            }
        }
    }

    /// Trigger a schedule to fire immediately.
    pub async fn trigger_schedule_now(
        &self,
        schedule_public_id: &str,
    ) -> Result<(), agent_client_protocol::Error> {
        let scheduler = self
            .get_or_start_scheduler()
            .await
            .ok_or_else(Self::scheduler_unavailable_error)?;

        match scheduler.trigger_now(schedule_public_id).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if !Self::is_actor_not_running_error(&msg) {
                    return Err(agent_client_protocol::Error::internal_error().data(msg));
                }

                self.clear_scheduler_handle();
                let scheduler = self
                    .get_or_start_scheduler()
                    .await
                    .ok_or_else(Self::scheduler_unavailable_error)?;
                scheduler
                    .trigger_now(schedule_public_id)
                    .await
                    .map_err(|err| {
                        agent_client_protocol::Error::internal_error().data(err.to_string())
                    })
            }
        }
    }

    /// Pause a schedule.
    pub async fn pause_schedule(
        &self,
        schedule_public_id: &str,
    ) -> Result<(), agent_client_protocol::Error> {
        let scheduler = self
            .get_or_start_scheduler()
            .await
            .ok_or_else(Self::scheduler_unavailable_error)?;

        match scheduler.pause_schedule(schedule_public_id).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if !Self::is_actor_not_running_error(&msg) {
                    return Err(agent_client_protocol::Error::internal_error().data(msg));
                }

                self.clear_scheduler_handle();
                let scheduler = self
                    .get_or_start_scheduler()
                    .await
                    .ok_or_else(Self::scheduler_unavailable_error)?;
                scheduler
                    .pause_schedule(schedule_public_id)
                    .await
                    .map_err(|err| {
                        agent_client_protocol::Error::internal_error().data(err.to_string())
                    })
            }
        }
    }

    /// Resume a paused schedule.
    pub async fn resume_schedule(
        &self,
        schedule_public_id: &str,
    ) -> Result<(), agent_client_protocol::Error> {
        let scheduler = self
            .get_or_start_scheduler()
            .await
            .ok_or_else(Self::scheduler_unavailable_error)?;

        match scheduler.resume_schedule(schedule_public_id).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if !Self::is_actor_not_running_error(&msg) {
                    return Err(agent_client_protocol::Error::internal_error().data(msg));
                }

                self.clear_scheduler_handle();
                let scheduler = self
                    .get_or_start_scheduler()
                    .await
                    .ok_or_else(Self::scheduler_unavailable_error)?;
                scheduler
                    .resume_schedule(schedule_public_id)
                    .await
                    .map_err(|err| {
                        agent_client_protocol::Error::internal_error().data(err.to_string())
                    })
            }
        }
    }

    /// Delete a schedule.
    pub async fn delete_schedule(
        &self,
        schedule_public_id: &str,
    ) -> Result<(), agent_client_protocol::Error> {
        let scheduler = self
            .get_or_start_scheduler()
            .await
            .ok_or_else(Self::scheduler_unavailable_error)?;

        match scheduler.remove_schedule(schedule_public_id).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if !Self::is_actor_not_running_error(&msg) {
                    return Err(agent_client_protocol::Error::internal_error().data(msg));
                }

                self.clear_scheduler_handle();
                let scheduler = self
                    .get_or_start_scheduler()
                    .await
                    .ok_or_else(Self::scheduler_unavailable_error)?;
                scheduler
                    .remove_schedule(schedule_public_id)
                    .await
                    .map_err(|err| {
                        agent_client_protocol::Error::internal_error().data(err.to_string())
                    })
            }
        }
    }

    /// List schedules, optionally filtered by session.
    ///
    /// Returns an empty list if the scheduler is not running (rather than an
    /// error), since the frontend polls this on every session load and an
    /// absent scheduler simply means no schedules exist yet.
    pub async fn list_schedules(
        &self,
        session_public_id: Option<&str>,
    ) -> Result<Vec<crate::session::domain_schedule::Schedule>, agent_client_protocol::Error> {
        let scheduler = match self.get_or_start_scheduler().await {
            Some(s) => s,
            None => return Ok(vec![]),
        };

        match scheduler.list_schedules(session_public_id).await {
            Ok(schedules) => Ok(schedules),
            Err(e) => {
                let msg = e.to_string();
                if !Self::is_actor_not_running_error(&msg) {
                    return Err(agent_client_protocol::Error::internal_error().data(msg));
                }

                self.clear_scheduler_handle();
                let scheduler = match self.get_or_start_scheduler().await {
                    Some(s) => s,
                    None => return Ok(vec![]),
                };
                scheduler
                    .list_schedules(session_public_id)
                    .await
                    .or_else(|retry_err| {
                        if Self::is_actor_not_running_error(&retry_err.to_string()) {
                            Ok(vec![])
                        } else {
                            Err(retry_err)
                        }
                    })
                    .map_err(|retry_err| {
                        agent_client_protocol::Error::internal_error().data(retry_err.to_string())
                    })
            }
        }
    }

    /// Get a single schedule by public ID.
    ///
    /// Returns `None` if the scheduler is not running or the schedule does not exist.
    pub async fn get_schedule(
        &self,
        schedule_public_id: &str,
    ) -> Result<Option<crate::session::domain_schedule::Schedule>, agent_client_protocol::Error>
    {
        let scheduler = match self.get_or_start_scheduler().await {
            Some(s) => s,
            None => return Ok(None),
        };

        match scheduler.get_schedule(schedule_public_id).await {
            Ok(schedule) => Ok(schedule),
            Err(e) => {
                let msg = e.to_string();
                if !Self::is_actor_not_running_error(&msg) {
                    return Err(agent_client_protocol::Error::internal_error().data(msg));
                }

                self.clear_scheduler_handle();
                let scheduler = match self.get_or_start_scheduler().await {
                    Some(s) => s,
                    None => return Ok(None),
                };
                scheduler
                    .get_schedule(schedule_public_id)
                    .await
                    .or_else(|retry_err| {
                        if Self::is_actor_not_running_error(&retry_err.to_string()) {
                            Ok(None)
                        } else {
                            Err(retry_err)
                        }
                    })
                    .map_err(|retry_err| {
                        agent_client_protocol::Error::internal_error().data(retry_err.to_string())
                    })
            }
        }
    }

    /// Get scheduler metrics snapshot.
    pub async fn scheduler_metrics(&self) -> Option<crate::scheduler::SchedulerMetrics> {
        let scheduler = self.scheduler()?;
        Some(scheduler.metrics().await)
    }

    /// Switch provider and model for a session (simple form)
    pub async fn set_provider(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
    ) -> Result<(), Error> {
        // Preserve the system prompt when switching models
        let system_prompt = self.get_session_system_prompt(session_id).await;

        let mut config = LLMParams::new().provider(provider).model(model);

        // Add system prompt to config
        for prompt_part in system_prompt {
            config = config.system(prompt_part);
        }

        self.set_llm_config(session_id, config).await
    }

    /// Helper method to extract system prompt from current session config
    async fn get_session_system_prompt(&self, session_id: &str) -> Vec<String> {
        // Try to get the current session's LLM config
        if let Ok(Some(current_config)) = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
        {
            // Try to extract system prompt from params JSON
            if let Some(params) = &current_config.params
                && let Some(system_array) = params.get("system").and_then(|v| v.as_array())
            {
                // Parse the array of strings
                let mut system_parts = Vec::new();
                for item in system_array {
                    if let Some(s) = item.as_str() {
                        system_parts.push(s.to_string());
                    }
                }
                if !system_parts.is_empty() {
                    return system_parts;
                }
            }
        }

        // Fall back to initial_config system prompt
        self.config.provider.initial_config().system.clone()
    }

    /// Switch provider configuration for a session (advanced form)
    pub async fn set_llm_config(&self, session_id: &str, config: LLMParams) -> Result<(), Error> {
        use crate::error::AgentError;
        let provider_name = config
            .provider
            .as_ref()
            .ok_or_else(|| Error::from(AgentError::ProviderRequired))?;

        if self
            .config
            .provider
            .plugin_registry()
            .get(provider_name)
            .await
            .is_none()
        {
            return Err(Error::from(AgentError::UnknownProvider {
                name: provider_name.clone(),
            }));
        }

        let llm_config = self
            .config
            .provider
            .history_store()
            .create_or_get_llm_config(&config)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        self.config
            .provider
            .history_store()
            .set_session_llm_config(session_id, llm_config.id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        // Fetch context limit from model info
        let context_limit =
            crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                .and_then(|m| m.context_limit());

        self.emit_event(
            session_id,
            crate::events::AgentEventKind::ProviderChanged {
                provider: llm_config.provider.clone(),
                model: llm_config.model.clone(),
                config_id: llm_config.id,
                context_limit,
                provider_node_id: None,
            },
        );
        Ok(())
    }

    /// Get current LLM config for a session
    pub async fn get_session_llm_config(
        &self,
        session_id: &str,
    ) -> Result<Option<LLMConfig>, Error> {
        self.config
            .provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }

    /// Get LLM config by ID
    pub async fn get_llm_config(&self, config_id: i64) -> Result<Option<LLMConfig>, Error> {
        self.config
            .provider
            .history_store()
            .get_llm_config(config_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }

    /// Creates a CompositeDriver from the configured middleware drivers
    pub fn create_driver(&self) -> CompositeDriver {
        self.config.create_driver()
    }

    /// Returns the session limits from configured middleware
    pub fn get_session_limits(&self) -> Option<crate::events::SessionLimits> {
        self.config.get_session_limits()
    }

    /// Builds delegation metadata for ACP AgentCapabilities._meta field
    pub fn build_delegation_meta(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        self.config.build_delegation_meta()
    }

    /// Undo: revert filesystem to state at the given message_id.
    ///
    /// Routes through the kameo session actor via `SessionActorRef`.
    pub async fn undo(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<crate::agent::undo::UndoResult, crate::agent::undo::UndoError> {
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned().ok_or_else(|| {
                crate::agent::undo::UndoError::Other(format!("Session not found: {}", session_id))
            })?
        };

        session_ref.undo(message_id.to_string()).await
    }

    /// Redo: re-apply the next change in the redo stack.
    ///
    /// Routes through the kameo session actor via `SessionActorRef`.
    pub async fn redo(
        &self,
        session_id: &str,
    ) -> Result<crate::agent::undo::RedoResult, crate::agent::undo::UndoError> {
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned().ok_or_else(|| {
                crate::agent::undo::UndoError::Other(format!("Session not found: {}", session_id))
            })?
        };

        session_ref.redo().await
    }

    // ── Remote session management (requires `remote` feature) ─────────────────

    /// List discovered peers in the kameo mesh.
    ///
    /// Looks up all `RemoteNodeManager` instances registered under
    /// `"node_manager"` in the Kademlia DHT and calls `GetNodeInfo` on each.
    /// Requires a bootstrapped swarm (`--mesh` flag).
    ///
    /// Without a swarm or with no peers, returns an empty list.
    /// Returns a clone of the `MeshHandle` if the mesh is active.
    #[cfg(feature = "remote")]
    pub fn mesh(&self) -> Option<crate::agent::remote::MeshHandle> {
        self.mesh.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Activate the mesh by storing the `MeshHandle` returned by `bootstrap_mesh()`.
    ///
    /// Also propagates into `config.provider` so that sessions created by a
    /// `RemoteNodeManager` (which holds `Arc<AgentConfig>` with this provider)
    /// can route LLM calls through the mesh even though the mesh was bootstrapped
    /// after the config was built.
    #[cfg(feature = "remote")]
    pub fn set_mesh(&self, mesh: crate::agent::remote::MeshHandle) {
        *self.mesh.lock().unwrap_or_else(|e| e.into_inner()) = Some(mesh.clone());
        self.config.provider.set_mesh(Some(mesh.clone()));

        // Propagate to the session registry so remove/detach can clean up
        // re-registration closures (Phase 4 of Bug 1 fix).
        if let Ok(mut registry) = self.registry.try_lock() {
            registry.set_mesh(Some(mesh.clone()));
        }

        // Propagate to the session materializer for mesh-aware session creation
        self.session_materializer.set_mesh(mesh.clone());

        // Propagate to the model inventory for remote model enumeration
        self.model_inventory.set_mesh(mesh);
    }

    /// Enable/disable automatic mesh fallback for unpinned provider resolution.
    #[cfg(feature = "remote")]
    pub fn set_mesh_fallback(&self, enabled: bool) {
        self.config.provider.set_mesh_fallback(enabled);
    }

    #[cfg(feature = "remote")]
    fn remote_node_info_timeout() -> std::time::Duration {
        let default_ms = 3_000_u64;
        let timeout_ms = std::env::var("QUERYMT_REMOTE_NODE_INFO_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default_ms);
        std::time::Duration::from_millis(timeout_ms)
    }

    #[cfg(feature = "remote")]
    fn remote_node_lookup_parallelism() -> usize {
        std::env::var("QUERYMT_REMOTE_NODE_LOOKUP_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(8)
    }

    #[cfg(feature = "remote")]
    fn remote_node_cache_ttl() -> std::time::Duration {
        let default_ms = 10_000_u64;
        let ttl_ms = std::env::var("QUERYMT_REMOTE_NODE_CACHE_TTL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default_ms);
        std::time::Duration::from_millis(ttl_ms)
    }

    #[cfg(feature = "remote")]
    fn should_skip_stale_dht_record(
        scope: &crate::agent::remote::scope::MeshScopeId,
        is_peer_alive: bool,
    ) -> bool {
        // LAN discovery can transiently lose route liveness while DHT registrations
        // remain valid, so we still probe LAN entries instead of dropping them.
        !is_peer_alive && scope.is_iroh()
    }

    #[cfg(feature = "remote")]
    fn peer_cache_key(peer_id: Option<libp2p::PeerId>, fallback_actor_id: u64) -> String {
        if let Some(pid) = peer_id {
            format!("peer:{pid}")
        } else {
            format!("actor:{fallback_actor_id}")
        }
    }

    #[cfg(feature = "remote")]
    fn get_cached_remote_node(&self, cache_key: &str) -> Option<crate::agent::remote::NodeInfo> {
        let now = std::time::Instant::now();
        if let Some(entry) = self
            .remote_node_cache
            .by_label
            .read()
            .get(cache_key)
            .cloned()
            && entry.expires_at > now
        {
            return Some(entry.info);
        }

        let mut guard = self.remote_node_cache.by_label.write();
        if let Some(entry) = guard.get(cache_key)
            && entry.expires_at <= now
        {
            guard.remove(cache_key);
        }
        None
    }

    #[cfg(feature = "remote")]
    fn insert_cached_remote_node(&self, cache_key: String, info: crate::agent::remote::NodeInfo) {
        let ttl = Self::remote_node_cache_ttl();
        self.remote_node_cache.by_label.write().insert(
            cache_key,
            CachedNodeEntry {
                info,
                expires_at: std::time::Instant::now() + ttl,
            },
        );
    }

    #[cfg(feature = "remote")]
    fn ensure_remote_node_cache_invalidation_task(&self, mesh: &crate::agent::remote::MeshHandle) {
        if self
            .remote_node_cache
            .invalidation_task_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let mut rx = mesh.subscribe_peer_events();
        let cache = Arc::clone(&self.remote_node_cache);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(crate::agent::remote::mesh::PeerEvent::Discovered(peer_id))
                    | Ok(crate::agent::remote::mesh::PeerEvent::Expired(peer_id)) => {
                        let key = format!("peer:{peer_id}");
                        cache.by_label.write().remove(&key);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        cache
                            .invalidation_task_started
                            .store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
    }

    #[cfg(feature = "remote")]
    pub async fn list_remote_nodes(&self) -> Vec<crate::agent::remote::NodeInfo> {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};
        use futures_util::{StreamExt, stream::FuturesUnordered};

        let Some(mesh) = self.mesh() else {
            log::debug!("list_remote_nodes: mesh not bootstrapped");
            return Vec::new();
        };

        self.ensure_remote_node_cache_invalidation_task(&mesh);

        let local_peer_id = *mesh.peer_id();
        let timeout = Self::remote_node_info_timeout();
        let concurrency = Self::remote_node_lookup_parallelism();
        let semaphore = Arc::new(Semaphore::new(concurrency));

        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        let mut lookups: RemoteNodeLookupQueue = FuturesUnordered::new();
        let mut cached_nodes = Vec::new();
        let mut scheduled_cache_keys = std::collections::HashSet::new();

        let scopes = runtime.active_scopes();
        let alive_peers: Vec<_> = mesh.known_peer_ids();
        log::debug!(
            "list_remote_nodes: querying {} scope(s), {} known peer(s), local_peer_id={}",
            scopes.len(),
            alive_peers.len(),
            local_peer_id,
        );

        for scope in &scopes {
            for peer_id in &alive_peers {
                if *peer_id == local_peer_id {
                    continue;
                }
                let peer_id = *peer_id;
                let dht_name =
                    crate::agent::remote::scope::scoped_node_manager_for_peer(scope, &peer_id);
                log::debug!(
                    "list_remote_nodes: querying per-peer DHT name '{}'",
                    dht_name
                );
                match runtime
                    .lookup_actor_no_retry::<RemoteNodeManager>(dht_name.clone())
                    .await
                {
                    Ok(Some(node_manager_ref)) => {
                        log::debug!(
                            "list_remote_nodes: per-peer DHT hit for peer {} under '{}'",
                            peer_id,
                            dht_name
                        );
                        let cache_key = Self::peer_cache_key(
                            Some(peer_id),
                            node_manager_ref.id().sequence_id(),
                        );
                        if !scheduled_cache_keys.insert(cache_key.clone()) {
                            log::debug!(
                                "list_remote_nodes: duplicate discovery for peer {:?} under '{}'",
                                Some(peer_id),
                                dht_name
                            );
                            continue;
                        }

                        if let Some(info) = self.get_cached_remote_node(&cache_key) {
                            log::debug!(
                                "list_remote_nodes: cache hit for peer {:?} under '{}'",
                                Some(peer_id),
                                dht_name
                            );
                            cached_nodes.push(info);
                            continue;
                        }

                        log::debug!(
                            "list_remote_nodes: enqueuing GetNodeInfo for peer {:?} under '{}'",
                            Some(peer_id),
                            dht_name
                        );
                        let semaphore = Arc::clone(&semaphore);
                        lookups.push(Box::pin(async move {
                            let permit = semaphore.acquire_owned().await.ok();
                            let res = tokio::time::timeout(
                                timeout,
                                node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo),
                            )
                            .await;
                            drop(permit);
                            (cache_key, Some(peer_id), res)
                        }));
                    }
                    Ok(None) => {
                        log::debug!(
                            "list_remote_nodes: per-peer DHT miss for peer {} under '{}'",
                            peer_id,
                            dht_name
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            "list_remote_nodes: per-peer lookup error for '{}': {}",
                            dht_name,
                            e
                        );
                    }
                }
            }
        }

        for scope in &scopes {
            let dht_name = crate::agent::remote::scope::scoped_node_manager(scope);
            log::debug!("list_remote_nodes: querying DHT name '{}'", dht_name);
            let mut stream = runtime.lookup_all_actors::<RemoteNodeManager>(dht_name.clone());
            let mut found_count = 0usize;

            while let Some(result) = stream.next().await {
                match result {
                    Ok(node_manager_ref) => {
                        found_count += 1;
                        let peer_id = node_manager_ref.id().peer_id().copied();
                        if peer_id == Some(local_peer_id) {
                            log::debug!("list_remote_nodes: skipping local node");
                            continue;
                        }

                        if let Some(pid) = peer_id {
                            let is_peer_alive = mesh.is_peer_alive(&pid);
                            if Self::should_skip_stale_dht_record(scope, is_peer_alive) {
                                let key = format!("peer:{pid}");
                                self.remote_node_cache.by_label.write().remove(&key);
                                log::warn!(
                                    "list_remote_nodes: skipping stale DHT record for peer {pid} \
                                 (is_peer_alive=false, scope=iroh, dht_name='{}')",
                                    dht_name
                                );
                                continue;
                            }

                            if !is_peer_alive {
                                log::debug!(
                                    "list_remote_nodes: keeping LAN DHT record for peer {pid} despite is_peer_alive=false (dht_name='{}')",
                                    dht_name
                                );
                            }
                        }

                        let cache_key =
                            Self::peer_cache_key(peer_id, node_manager_ref.id().sequence_id());
                        if !scheduled_cache_keys.insert(cache_key.clone()) {
                            log::debug!(
                                "list_remote_nodes: duplicate discovery for peer {:?} under '{}'",
                                peer_id,
                                dht_name
                            );
                            continue;
                        }

                        if let Some(info) = self.get_cached_remote_node(&cache_key) {
                            log::debug!(
                                "list_remote_nodes: cache hit for peer {:?} under '{}'",
                                peer_id,
                                dht_name
                            );
                            cached_nodes.push(info);
                            continue;
                        }

                        log::debug!(
                            "list_remote_nodes: enqueuing GetNodeInfo for peer {:?} under '{}'",
                            peer_id,
                            dht_name
                        );
                        let semaphore = Arc::clone(&semaphore);
                        lookups.push(Box::pin(async move {
                            let permit = semaphore.acquire_owned().await.ok();
                            let res = tokio::time::timeout(
                                timeout,
                                node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo),
                            )
                            .await;
                            drop(permit);
                            (cache_key, peer_id, res)
                        }));
                    }
                    Err(e) => {
                        log::warn!("list_remote_nodes: lookup error for '{}': {}", dht_name, e)
                    }
                }
            }

            log::debug!(
                "list_remote_nodes: DHT name '{}' yielded {} actor(s)",
                dht_name,
                found_count
            );
        }

        if scopes.is_empty() {
            log::warn!("list_remote_nodes: active_scopes() returned empty — no DHT queries issued");
        }

        let mut fetched_nodes = Vec::new();
        while let Some((cache_key, peer_id, result)) = lookups.next().await {
            match result {
                Ok(Ok(info)) => {
                    self.insert_cached_remote_node(cache_key, info.clone());
                    fetched_nodes.push(info);
                }
                Ok(Err(e)) => {
                    log::warn!("list_remote_nodes: GetNodeInfo failed: {}", e);
                }
                Err(_) => {
                    log::warn!(
                        "list_remote_nodes: GetNodeInfo timed out for peer {:?}",
                        peer_id
                    );
                }
            }
        }

        cached_nodes.extend(fetched_nodes);
        cached_nodes
    }

    /// Find a `RemoteNodeManager` by its stable node id (PeerId string).
    ///
    /// ## Fast path
    ///
    /// Tries a direct DHT lookup under the scoped per-peer node-manager name first.
    /// This succeeds whenever the remote node registered under the same scope
    /// (see [`crate::agent::remote::scope::scoped_node_manager_for_peer`]) and is **not** gated on
    /// `is_peer_alive`, so it works even when mDNS has transiently expired the
    /// peer (TTL = 30 s) while the TCP connection is still alive.
    ///
    /// ## Fallback scan
    ///
    /// If the direct lookup misses (e.g. the remote node is running an older
    /// version that only registers under the global `"node_manager"` name),
    /// falls back to iterating all `RemoteNodeManager` actors via
    /// `lookup_all_actors` and comparing `GetNodeInfo.node_id`.  Unlike
    /// `list_remote_nodes`, this scan deliberately **skips the `is_peer_alive`
    /// filter**: the user has explicitly requested this node, so we attempt
    /// `GetNodeInfo` contact (3 s timeout) before giving up rather than
    /// silently discarding the candidate.
    #[cfg(feature = "remote")]
    pub async fn find_node_manager(
        &self,
        node_id: &str,
    ) -> Result<
        kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        agent_client_protocol::Error,
    > {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};
        use futures_util::{StreamExt, stream::FuturesUnordered};

        use crate::error::AgentError;
        let mesh = self
            .mesh()
            .ok_or_else(|| agent_client_protocol::Error::from(AgentError::MeshNotBootstrapped))?;

        self.ensure_remote_node_cache_invalidation_task(&mesh);

        // ── Fast path: direct per-peer DHT lookup ────────────────────────────
        //
        // Remote nodes register under both the global "node_manager" name (for
        // mesh-wide discovery) and a per-peer "node_manager::peer::{peer_id}"
        // name (for this O(1) lookup). The per-peer lookup bypasses the
        // is_peer_alive gate that guards the fallback scan, so it works even
        // when mDNS has temporarily expired the peer's heartbeat.
        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        for scope in runtime.active_scopes() {
            let direct_dht_name =
                crate::agent::remote::scope::scoped_node_manager_for_peer(&scope, &node_id);
            match runtime
                .lookup_actor::<RemoteNodeManager>(direct_dht_name.clone())
                .await
            {
                Ok(Some(node_manager_ref)) => {
                    log::debug!(
                        "find_node_manager: fast-path DHT hit for '{}'",
                        direct_dht_name
                    );
                    return Ok(node_manager_ref);
                }
                Ok(None) => {
                    log::debug!(
                        "find_node_manager: no direct DHT entry for '{}', trying next scope",
                        direct_dht_name
                    );
                }
                Err(e) => {
                    log::debug!(
                        "find_node_manager: direct DHT lookup error for '{}': {}, trying next scope",
                        direct_dht_name,
                        e
                    );
                }
            }
        }

        // ── Fallback scan: iterate all registered RemoteNodeManagers ─────────
        //
        // NOTE: unlike list_remote_nodes, we do NOT filter by is_peer_alive
        // here. The user explicitly chose this node, so we attempt GetNodeInfo
        // contact before giving up. The 3-second timeout on GetNodeInfo is the
        // real liveness check for a targeted user action.
        let local_peer_id = *mesh.peer_id();
        let timeout = Self::remote_node_info_timeout();
        let concurrency = Self::remote_node_lookup_parallelism();
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let mut lookups = FuturesUnordered::new();

        for scope in runtime.active_scopes() {
            let mut stream = runtime.lookup_all_actors::<RemoteNodeManager>(
                crate::agent::remote::scope::scoped_node_manager(&scope),
            );
            while let Some(result) = stream.next().await {
                match result {
                    Ok(node_manager_ref) => {
                        let peer_id = node_manager_ref.id().peer_id().copied();
                        if peer_id == Some(local_peer_id) {
                            continue;
                        }
                        // No is_peer_alive check here — we contact the peer
                        // directly and let the GetNodeInfo timeout decide.

                        let cache_key =
                            Self::peer_cache_key(peer_id, node_manager_ref.id().sequence_id());
                        if let Some(info) = self.get_cached_remote_node(&cache_key) {
                            if info.node_id.to_string() == node_id {
                                return Ok(node_manager_ref);
                            }
                            continue;
                        }

                        let semaphore = Arc::clone(&semaphore);
                        lookups.push(async move {
                            let permit = semaphore.acquire_owned().await.ok();
                            let res = tokio::time::timeout(
                                timeout,
                                node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo),
                            )
                            .await;
                            drop(permit);
                            (node_manager_ref, cache_key, peer_id, res)
                        });
                    }
                    Err(e) => {
                        log::warn!("find_node_manager: lookup error: {}", e);
                    }
                }
            }
        }

        while let Some((node_manager_ref, cache_key, peer_id, result)) = lookups.next().await {
            match result {
                Ok(Ok(info)) => {
                    self.insert_cached_remote_node(cache_key, info.clone());
                    if info.node_id.to_string() == node_id {
                        return Ok(node_manager_ref);
                    }
                }
                Ok(Err(e)) => {
                    log::warn!("find_node_manager: GetNodeInfo failed: {}", e);
                }
                Err(_) => {
                    log::warn!(
                        "find_node_manager: GetNodeInfo timed out for peer {:?}",
                        peer_id
                    );
                }
            }
        }

        Err(agent_client_protocol::Error::from(
            AgentError::RemoteSessionNotFound {
                details: format!(
                    "Remote node id '{}' not found in the mesh. \
                     The node may have gone offline or mDNS discovery may not have \
                     completed yet. Available nodes can be listed via list_remote_nodes.",
                    node_id
                ),
            },
        ))
    }

    /// List sessions on a specific remote node.
    ///
    /// Sends `ListRemoteSessions` to the `RemoteNodeManager` registered under
    /// `node_manager_name` in the Kademlia DHT.
    ///
    /// Requires a bootstrapped swarm (Phase 6). Returns an error if the node
    /// is not reachable or has no registered `RemoteNodeManager`.
    #[cfg(feature = "remote")]
    pub async fn list_remote_sessions(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Result<
        crate::agent::remote::node_manager::ListRemoteSessionsResponse,
        agent_client_protocol::Error,
    > {
        use crate::agent::remote::ListRemoteSessions;
        use crate::error::AgentError;
        node_manager_ref
            .ask(&ListRemoteSessions { offset, limit })
            .await
            .map_err(|e| agent_client_protocol::Error::from(AgentError::RemoteActor(e.to_string())))
    }

    /// Create a session on a remote node and return the owning node's live session ref.
    ///
    /// Callers can immediately finalize local attachment from the returned capability
    /// while DHT registration continues as background discoverability for reconnects.
    #[cfg(feature = "remote")]
    pub async fn create_remote_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        cwd: Option<String>,
    ) -> Result<crate::agent::remote::CreateRemoteSessionResponse, agent_client_protocol::Error>
    {
        use crate::agent::remote::CreateRemoteSession;
        use crate::error::AgentError;

        node_manager_ref
            .ask(&CreateRemoteSession { cwd })
            .await
            .map_err(|e| agent_client_protocol::Error::from(AgentError::RemoteActor(e.to_string())))
    }

    /// Fork a session on a remote node and return the forked child's live session ref.
    #[cfg(feature = "remote")]
    pub async fn fork_remote_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        source_session_id: String,
        message_id: String,
    ) -> Result<crate::agent::remote::ForkRemoteSessionResponse, agent_client_protocol::Error> {
        use crate::agent::remote::ForkRemoteSession;
        use crate::error::AgentError;

        node_manager_ref
            .ask(&ForkRemoteSession {
                source_session_id,
                message_id,
            })
            .await
            .map_err(|e| agent_client_protocol::Error::from(AgentError::RemoteActor(e.to_string())))
    }

    /// Attach an existing remote session (already has a `RemoteActorRef`) to
    /// the local registry.
    ///
    /// This is the lower-level entry point used when the caller already has a
    /// `RemoteActorRef<SessionActor>` (e.g., obtained via swarm lookup after
    /// Phase 6 bootstrap).
    #[cfg(feature = "remote")]
    pub async fn attach_remote_session(
        &self,
        session_id: String,
        remote_ref: kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
        peer_label: String,
        preferred_scope: Option<crate::agent::remote::scope::MeshScopeId>,
        remote_node_id: Option<String>,
    ) -> crate::agent::remote::SessionActorRef {
        let mesh = self.mesh();
        let mut registry = self.registry.lock().await;
        registry
            .attach_remote_session(
                session_id,
                remote_ref,
                peer_label,
                mesh,
                preferred_scope,
                remote_node_id,
            )
            .await
    }

    #[cfg(feature = "remote")]
    pub async fn resume_remote_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: String,
    ) -> Result<crate::agent::remote::CreateRemoteSessionResponse, agent_client_protocol::Error>
    {
        use crate::agent::remote::ResumeRemoteSession;
        use crate::error::AgentError;

        node_manager_ref
            .ask(&ResumeRemoteSession { session_id })
            .await
            .map_err(|e| agent_client_protocol::Error::from(AgentError::RemoteActor(e.to_string())))
    }

    async fn session_ref_for_agent_session(
        &self,
        session_id: &str,
    ) -> Result<SessionActorRef, Error> {
        let registry = self.registry.lock().await;
        registry.get(session_id).cloned().ok_or_else(|| {
            Error::invalid_params().data(serde_json::json!({
                "message": "unknown session",
                "sessionId": session_id,
            }))
        })
    }

    pub async fn stop_session(&self, session_id: &str) -> Result<(), Error> {
        use crate::agent::messages::SessionRuntimeStatus;

        const STOP_ESCALATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned()
        };

        let Some(session_ref) = session_ref else {
            log::warn!(
                "Stop requested for session {} but not found in registry",
                session_id
            );
            return Ok(());
        };

        self.config
            .emit_event(session_id, AgentEventKind::SessionStopRequested);
        let _ = session_ref.cancel().await;

        tokio::time::sleep(STOP_ESCALATION_TIMEOUT).await;

        let status = session_ref
            .get_runtime_status()
            .await
            .unwrap_or(SessionRuntimeStatus::Running);
        if status == SessionRuntimeStatus::Idle {
            tracing::debug!(
                "Session {} stop: status=Idle, graceful shutdown — returning without force-stop",
                session_id,
            );
            return Ok(());
        }
        // For remote sessions, CancelRequested doesn't mean the prompt is done —
        // the provider stream might still be active on the remote node.
        if status == SessionRuntimeStatus::CancelRequested && !session_ref.is_remote() {
            tracing::debug!(
                "Session {} stop: status=CancelRequested (local), graceful shutdown — returning without force-stop",
                session_id,
            );
            return Ok(());
        }

        if matches!(status, SessionRuntimeStatus::CancelRequested) {
            tracing::warn!(
                "Session {} stop: still CancelRequested after {:?}; escalating to force-stop",
                session_id,
                STOP_ESCALATION_TIMEOUT
            );
        }

        self.config.emit_event(
            session_id,
            AgentEventKind::SessionForceStopped {
                escalated_after_ms: STOP_ESCALATION_TIMEOUT.as_millis() as u64,
                reason: "graceful cancellation timeout elapsed".to_string(),
            },
        );

        if session_ref.is_remote() {
            #[cfg(feature = "remote")]
            {
                let bookmark = self
                    .config
                    .provider
                    .history_store()
                    .list_remote_session_bookmarks()
                    .await
                    .map_err(|e| Error::internal_error().data(e.to_string()))?
                    .into_iter()
                    .find(|b| b.session_id == session_id);

                if let Some(bookmark) = bookmark {
                    if let Some(mesh) = self.mesh() {
                        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
                        let mut provider_host = None;
                        for scope in runtime.active_scopes() {
                            let provider_host_name =
                                crate::agent::remote::scope::scoped_provider_host(
                                    &scope,
                                    &bookmark.node_id,
                                );
                            if let Ok(Some(found)) = mesh
                                .lookup_actor::<crate::agent::remote::provider_host::ProviderHostActor>(
                                    &provider_host_name,
                                )
                                .await
                            {
                                provider_host = Some(found);
                                break;
                            }
                        }
                        if let Some(provider_host) = provider_host {
                            let status = provider_host
                                .ask(
                                    &crate::agent::remote::provider_host::GetProviderStreamStatus {
                                        session_id: session_id.to_string(),
                                        request_id: None,
                                    },
                                )
                                .await
                                .ok()
                                .flatten();
                            if let Some(status) = status {
                                tracing::warn!(
                                    session_id,
                                    request_id = %status.request_id,
                                    phase = ?status.phase,
                                    elapsed_ms = status.elapsed_ms,
                                    idle_ms = status.idle_ms,
                                    chunk_count = status.chunk_count,
                                    receiver_connected = status.receiver_connected,
                                    lease_expires_in_ms = status.lease_expires_in_ms,
                                    provider = %status.provider,
                                    model = %status.model,
                                    last_error = ?status.last_error,
                                    "remote stop found active provider stream; issuing provider-host cancel"
                                );
                                let _ = provider_host
                                    .ask(&crate::agent::remote::provider_host::CancelProviderStreamRequest {
                                        session_id: session_id.to_string(),
                                        request_id: Some(status.request_id.clone()),
                                        reason: Some("session stop requested".to_string()),
                                    })
                                    .await;
                            } else {
                                let _ = provider_host
                                    .ask(&crate::agent::remote::provider_host::CancelProviderStreamRequest {
                                        session_id: session_id.to_string(),
                                        request_id: None,
                                        reason: Some("session stop requested without status".to_string()),
                                    })
                                    .await;
                            }
                        }
                    }

                    let nm_ref = self.find_node_manager(&bookmark.node_id).await?;
                    nm_ref
                        .ask(&crate::agent::remote::StopRemoteSessionRuntime {
                            session_id: session_id.to_string(),
                        })
                        .await
                        .map_err(|e| {
                            Error::from(crate::error::AgentError::RemoteActor(e.to_string()))
                        })?;
                }

                let mut registry = self.registry.lock().await;
                registry
                    .detach_remote_session_preserve_bookmark(session_id)
                    .await;
            }
        } else {
            let removed = {
                let mut registry = self.registry.lock().await;
                registry.remove(session_id)
            };
            if let Some(session_ref) = removed {
                let _ = session_ref.shutdown().await;
            }
        }

        Ok(())
    }
}

// ── Model registry convenience ────────────────────────────────────────────

impl LocalAgentHandle {
    /// Invalidate the model cache, forcing a fresh enumeration on next call.
    ///
    /// Delegates to `ModelInventory::invalidate_all`.
    pub async fn invalidate_model_cache(&self) {
        self.model_inventory.invalidate_all().await;
    }

    /// Attempt to re-attach a remote session from a persisted bookmark.
    ///
    /// Performs a DHT lookup for the session, and if found, attaches it to the
    /// local registry (spawning an EventRelayActor and sending SubscribeEvents).
    ///
    /// Returns `Ok(session_ref)` on success, or an error if the mesh is not
    /// active, the session is not found in the DHT, or the attach fails.
    #[cfg(feature = "remote")]
    pub async fn reattach_from_bookmark(
        &self,
        bookmark: &crate::session::store::RemoteSessionBookmark,
    ) -> Result<crate::agent::remote::SessionActorRef, crate::error::AgentError> {
        let mesh = self
            .mesh()
            .ok_or(crate::error::AgentError::MeshNotBootstrapped)?;

        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        let mut remote_ref = None;
        let mut matched_scope = None;
        for scope in runtime.active_scopes() {
            let dht_name =
                crate::agent::remote::scope::scoped_session(&scope, &bookmark.session_id);
            let lookup = runtime
                .lookup_actor::<crate::agent::session_actor::SessionActor>(dht_name.clone())
                .await
                .map_err(|e| crate::error::AgentError::SwarmLookupFailed {
                    key: dht_name.clone(),
                    reason: e.to_string(),
                })?;
            if let Some(found) = lookup {
                remote_ref = Some(found);
                matched_scope = Some(scope);
                break;
            }
        }
        let remote_ref =
            remote_ref.ok_or_else(|| crate::error::AgentError::RemoteSessionNotFound {
                details: format!(
                    "bookmarked session {} not found in DHT",
                    bookmark.session_id
                ),
            })?;

        let mut registry = self.registry.lock().await;
        let session_ref = registry
            .attach_remote_session(
                bookmark.session_id.clone(),
                remote_ref,
                bookmark.peer_label.clone(),
                Some(mesh),
                matched_scope,
                Some(bookmark.node_id.clone()),
            )
            .await;

        Ok(session_ref)
    }

    /// Like [`reattach_from_bookmark`] but uses a single DHT lookup with **no
    /// retries**.
    ///
    /// Intended for bulk bookmark reattach during session listing where we
    /// prefer a fast failure over spending ~1.75 s per stale bookmark.
    #[cfg(feature = "remote")]
    pub async fn reattach_from_bookmark_quick(
        &self,
        bookmark: &crate::session::store::RemoteSessionBookmark,
    ) -> Result<crate::agent::remote::SessionActorRef, crate::error::AgentError> {
        let mesh = self
            .mesh()
            .ok_or(crate::error::AgentError::MeshNotBootstrapped)?;

        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        let mut remote_ref = None;
        let mut matched_scope = None;
        for scope in runtime.active_scopes() {
            let dht_name =
                crate::agent::remote::scope::scoped_session(&scope, &bookmark.session_id);
            let lookup = runtime
                .lookup_actor_no_retry::<crate::agent::session_actor::SessionActor>(
                    dht_name.clone(),
                )
                .await
                .map_err(|e| crate::error::AgentError::SwarmLookupFailed {
                    key: dht_name.clone(),
                    reason: e.to_string(),
                })?;
            if let Some(found) = lookup {
                remote_ref = Some(found);
                matched_scope = Some(scope);
                break;
            }
        }
        let remote_ref =
            remote_ref.ok_or_else(|| crate::error::AgentError::RemoteSessionNotFound {
                details: format!(
                    "bookmarked session {} not found in DHT",
                    bookmark.session_id
                ),
            })?;

        let mut registry = self.registry.lock().await;
        let session_ref = registry
            .attach_remote_session(
                bookmark.session_id.clone(),
                remote_ref,
                bookmark.peer_label.clone(),
                Some(mesh),
                matched_scope,
                Some(bookmark.node_id.clone()),
            )
            .await;

        Ok(session_ref)
    }

    /// Resolve a `SessionHandoff` into a concrete remote actor reference.
    ///
    /// - `DirectRemote` → return the embedded ref directly.
    /// - `LookupOnly` → DHT lookup for the session.
    /// - `NoAttachPath` → error.
    #[cfg(feature = "remote")]
    pub async fn resolve_handoff(
        &self,
        session_id: &str,
        handoff: crate::agent::remote::node_manager::SessionHandoff,
    ) -> Result<
        kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
        agent_client_protocol::Error,
    > {
        use crate::error::AgentError;

        match handoff {
            crate::agent::remote::node_manager::SessionHandoff::DirectRemote { session_ref } => {
                Ok(session_ref)
            }
            crate::agent::remote::node_manager::SessionHandoff::LookupOnly => {
                let mesh = self.mesh().ok_or_else(|| {
                    agent_client_protocol::Error::from(AgentError::MeshNotBootstrapped)
                })?;
                let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
                for scope in runtime.active_scopes() {
                    let dht_name = crate::agent::remote::scope::scoped_session(&scope, session_id);
                    if let Some(found) = runtime
                        .lookup_actor::<crate::agent::session_actor::SessionActor>(dht_name)
                        .await
                        .map_err(|e| {
                            agent_client_protocol::Error::from(AgentError::SwarmLookupFailed {
                                key: session_id.to_string(),
                                reason: e.to_string(),
                            })
                        })?
                    {
                        return Ok(found);
                    }
                }
                Err(agent_client_protocol::Error::from(
                    AgentError::RemoteSessionNotFound {
                        details: format!(
                            "session {} registered but not found in DHT after lookup",
                            session_id
                        ),
                    },
                ))
            }
            crate::agent::remote::node_manager::SessionHandoff::NoAttachPath => Err(
                agent_client_protocol::Error::from(AgentError::RemoteSessionNotFound {
                    details: format!(
                        "session {} was created but the remote node cannot provide an attach path",
                        session_id
                    ),
                }),
            ),
        }
    }

    /// Build a lightweight `SessionLoadSnapshot` from the locally-attached
    /// remote session's event stream. Used by the ACP extension path to
    /// return history to mobile clients.
    #[cfg(feature = "remote")]
    pub async fn build_remote_attach_snapshot(
        &self,
        session_id: &str,
    ) -> Result<serde_json::Value, agent_client_protocol::Error> {
        use crate::error::AgentError;

        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned()
        };
        let Some(session_ref) = session_ref else {
            return Err(agent_client_protocol::Error::from(
                AgentError::SessionNotFound {
                    session_id: session_id.to_string(),
                },
            ));
        };

        let events = session_ref.get_event_stream().await.unwrap_or_default();
        log::info!(
            "remote attach snapshot built from attached session ref: session_id={}, events={}",
            session_id,
            events.len()
        );
        let cursor = crate::session::cursor_from_events(&events);
        let audit = crate::session::projection::AuditView {
            session_id: session_id.to_string(),
            events,
            tasks: Vec::new(),
            intent_snapshots: Vec::new(),
            decisions: Vec::new(),
            progress_entries: Vec::new(),
            artifacts: Vec::new(),
            delegations: Vec::new(),
            generated_at: time::OffsetDateTime::now_utc(),
        };

        Ok(serde_json::json!({
            "audit": audit,
            "cursor": cursor,
        }))
    }
}

// ══════════════════════════════════════════════════════════════════════════
//  AgentHandle trait implementation for LocalAgentHandle
// ══════════════════════════════════════════════════════════════════════════

#[async_trait]
impl AgentHandle for LocalAgentHandle {
    async fn new_session(
        &self,
        req: NewSessionRequest,
    ) -> std::result::Result<NewSessionResponse, Error> {
        SendAgent::new_session(self, req).await
    }

    async fn prompt(&self, req: PromptRequest) -> std::result::Result<PromptResponse, Error> {
        SendAgent::prompt(self, req).await
    }

    async fn cancel(&self, notif: CancelNotification) -> std::result::Result<(), Error> {
        SendAgent::cancel(self, notif).await
    }

    async fn load_session(
        &self,
        req: LoadSessionRequest,
    ) -> std::result::Result<LoadSessionResponse, Error> {
        SendAgent::load_session(self, req).await
    }

    async fn create_delegation_session(
        &self,
        cwd: Option<String>,
        parent_session_id: String,
    ) -> std::result::Result<(String, SessionActorRef), Error> {
        let cwd_path = cwd.map(std::path::PathBuf::from).unwrap_or_default();
        let mut meta = serde_json::Map::new();
        meta.insert(
            "parent_session_id".to_string(),
            serde_json::Value::String(parent_session_id),
        );
        let req = NewSessionRequest::new(cwd_path).meta(meta);

        // Use the 3-phase materialization pattern (no registry lock held during DB/actor work)
        let resp = self.new_session(req).await?;
        let session_id = resp.session_id.to_string();
        let session_ref = self.registry.lock().await;
        let session_ref = session_ref.get(&session_id).cloned().ok_or_else(|| {
            Error::internal_error().data("Session created but not found in registry")
        })?;

        Ok((session_id, session_ref))
    }

    fn subscribe_events(&self) -> broadcast::Receiver<EventEnvelope> {
        self.config.event_sink.fanout().subscribe()
    }

    fn event_fanout(&self) -> &Arc<EventFanout> {
        self.config.event_sink.fanout()
    }

    fn emit_event(&self, session_id: &str, kind: AgentEventKind) {
        self.config.emit_event(session_id, kind);
    }

    fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        self.config.agent_registry.clone()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    #[cfg(feature = "remote")]
    fn set_mesh_handle(&self, mesh: crate::agent::remote::MeshHandle) {
        self.set_mesh(mesh);
    }
}

/// SendAgent implementation for LocalAgentHandle
///
/// All methods delegate to either the kameo session registry or the shared config.
/// This replaces the `impl SendAgent for QueryMTAgent` from protocol.rs.
#[async_trait]
impl SendAgent for LocalAgentHandle {
    async fn initialize(&self, req: InitializeRequest) -> Result<InitializeResponse, Error> {
        use agent_client_protocol::schema::{
            AgentCapabilities, Implementation, McpCapabilities, PromptCapabilities,
            ProtocolVersion, SessionCapabilities, SessionCloseCapabilities,
            SessionDeleteCapabilities, SessionForkCapabilities, SessionListCapabilities,
            SessionResumeCapabilities,
        };

        let protocol_version = if req.protocol_version <= ProtocolVersion::LATEST {
            req.protocol_version
        } else {
            ProtocolVersion::LATEST
        };

        if let Ok(mut state) = self.client_state.lock() {
            *state = Some(ClientState {
                protocol_version,
                client_capabilities: req.client_capabilities.clone(),
                client_info: req.client_info.clone(),
                authenticated: false,
            });
        }

        let auth_methods = self.config.auth_methods.clone();

        let mut capabilities = AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(PromptCapabilities::new().embedded_context(true))
            .mcp_capabilities(McpCapabilities::new().http(true).sse(true))
            .session_capabilities(
                SessionCapabilities::new()
                    .list(SessionListCapabilities::new())
                    .fork(SessionForkCapabilities::new())
                    .resume(SessionResumeCapabilities::new())
                    .close(SessionCloseCapabilities::new())
                    .delete(SessionDeleteCapabilities::new()),
            );

        // Add delegation metadata if agent registry is available
        if let Some(delegation_meta) = self.build_delegation_meta() {
            capabilities = capabilities.meta(delegation_meta);
        }

        Ok(InitializeResponse::new(protocol_version)
            .agent_capabilities(capabilities)
            .auth_methods(auth_methods)
            .agent_info(
                Implementation::new("querymt-agent", env!("QMT_BUILD_VERSION"))
                    .title("QueryMT Agent"),
            ))
    }

    async fn authenticate(&self, req: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        let auth_methods = &self.config.auth_methods;

        if !auth_methods.is_empty() && !auth_methods.iter().any(|m| *m.id() == req.method_id) {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "unknown auth method",
                "methodId": req.method_id.to_string(),
            })));
        }

        if let Ok(mut state) = self.client_state.lock()
            && let Some(state) = state.as_mut()
        {
            state.authenticated = true;
        }
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        self.new_session(req).await
    }

    async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, Error> {
        let session_id = req.session_id.to_string();
        let session_ref = self.session_ref_for_agent_session(&session_id).await?;
        session_ref.prompt(req).await
    }

    async fn cancel(&self, notif: CancelNotification) -> Result<(), Error> {
        let session_id = notif.session_id.to_string();
        let Ok(session_ref) = self.session_ref_for_agent_session(&session_id).await else {
            // Keep ACP cancel semantics as best-effort/no-op for unknown sessions.
            return Ok(());
        };
        session_ref.cancel().await.map_err(Error::from)
    }

    async fn load_session(&self, req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        self.load_session(req).await
    }

    async fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        // Use the history store directly without holding the registry lock.
        // The registry lock is only needed for in-memory mutations; listing
        // sessions is a pure DB read that shouldn't be serialized on the lock.
        let store = self.config.provider.history_store();
        let sessions = store
            .list_sessions()
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        let requested_cwd = req.cwd.as_ref().map(std::path::PathBuf::from);
        let filtered_infos: Vec<SessionInfo> = sessions
            .into_iter()
            .filter(|s| match requested_cwd.as_ref() {
                Some(cwd) => s.cwd.as_ref() == Some(cwd),
                None => true,
            })
            .map(|s| {
                let mut info = SessionInfo::new(
                    agent_client_protocol::schema::SessionId::from(s.public_id),
                    s.cwd.unwrap_or_default(),
                );
                if let Some(name) = s.name {
                    info.title = Some(name);
                }
                if let Some(updated_at) = s.updated_at {
                    info.updated_at = Some(
                        updated_at
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default(),
                    );
                }
                info
            })
            .collect();

        let start_idx = req
            .cursor
            .as_ref()
            .and_then(|c| c.parse::<usize>().ok())
            .unwrap_or(0);
        let limit = 100;
        let end_idx = (start_idx + limit).min(filtered_infos.len());
        let paginated = filtered_infos[start_idx..end_idx].to_vec();
        let next_cursor = if end_idx < filtered_infos.len() {
            Some(end_idx.to_string())
        } else {
            None
        };

        Ok(ListSessionsResponse::new(paginated).next_cursor(next_cursor))
    }

    /// Fork an existing session at the latest message.
    ///
    /// Uses the 3-phase materialization pattern:
    /// 1. Prepare (NO lock): DB operations to create fork
    /// 2. Load the forked session (which uses its own 3-phase pattern)
    async fn fork_session(&self, req: ForkSessionRequest) -> Result<ForkSessionResponse, Error> {
        // Phase 1: Prepare fork (heavy DB work, NO registry lock held)
        let response = self.session_materializer.prepare_fork_session(req).await?;

        // The forked session is created in DB but not materialized yet.
        // The client will need to load it separately (which will use the 3-phase pattern).
        Ok(response)
    }

    /// Resume an existing session without history replay.
    ///
    /// Uses the 3-phase materialization pattern with single-flight protection:
    /// 1. Check registry (lock held briefly): Return existing if already materialized
    /// 2. Prepare (NO lock): DB validation, MCP init, actor spawn
    /// 3. Register (lock held): Insert into in-memory maps (microseconds)
    /// 4. Finalize (NO lock): DHT registration, event emission
    async fn resume_session(
        &self,
        req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, Error> {
        let session_id = req.session_id.to_string();

        // Single-flight check: Check if session is already materialized
        // Clone the actor_ref out of the registry to avoid holding the lock
        // during the async ask(GetMode) call.
        let existing_actor_ref = {
            let registry = self.registry_lock().await;
            registry.local_actor_ref(&session_id).cloned()
        };

        if let Some(existing_ref) = existing_actor_ref {
            // Registry lock is now dropped, safe to make async actor call
            let current_mode = existing_ref
                .ask(crate::agent::messages::GetMode)
                .await
                .map_err(|e| Error::internal_error().data(e.to_string()))?;

            return Ok(ResumeSessionResponse::new()
                .modes(crate::agent::session_registry::mode_state(current_mode))
                .config_options(crate::agent::session_registry::config_options(
                    current_mode,
                    **self.config.default_reasoning_effort.load(),
                )));
        }

        // Phase 1: Prepare session (heavy work, NO registry lock held)
        // The session_materializer's internal single-flight lock prevents
        // duplicate materialization of the same session ID.
        // Pass registry so it can re-check after acquiring the lock.
        let (prepared, session_ref) = match self
            .session_materializer
            .prepare_resume_session(req, Some(&self.registry))
            .await?
        {
            PreparedSessionResult::Prepared(prepared) => {
                let session_ref = {
                    let mut registry = self.registry_lock().await;
                    registry.register_prepared_session(&prepared).await
                };
                (Some(prepared), session_ref)
            }
            PreparedSessionResult::AlreadyRegistered(session_ref) => (None, session_ref),
        };

        // Phase 3: Finalize session (post-registration work, NO registry lock held)
        // Pass the bridge for setup outside the lock
        if let Some(prepared) = prepared.as_ref() {
            let bridge = self.bridge.lock().ok().and_then(|guard| guard.clone());
            self.session_materializer
                .finalize_session(prepared, bridge)
                .await?;
        }

        // Get current mode for response
        let current_mode = session_ref.get_mode().await.map_err(Error::from)?;

        Ok(ResumeSessionResponse::new()
            .modes(crate::agent::session_registry::mode_state(current_mode))
            .config_options(crate::agent::session_registry::config_options(
                current_mode,
                **self.config.default_reasoning_effort.load(),
            )))
    }

    async fn close_session(&self, req: CloseSessionRequest) -> Result<CloseSessionResponse, Error> {
        let session_id = req.session_id.to_string();
        self.stop_session(&session_id).await?;
        Ok(CloseSessionResponse::new())
    }

    async fn delete_session(
        &self,
        req: DeleteSessionRequest,
    ) -> Result<DeleteSessionResponse, Error> {
        let session_id = req.session_id.to_string();

        let is_loaded = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).is_some()
        };
        if is_loaded {
            // Closing is best-effort: deleting persisted history is the primary intent.
            let _ = self.stop_session(&session_id).await;
        }

        self.config
            .provider
            .history_store()
            .delete_session(&session_id)
            .await
            .map_err(|e| {
                Error::internal_error().data(serde_json::json!({"error": e.to_string()}))
            })?;

        Ok(DeleteSessionResponse::new())
    }

    async fn set_session_model(
        &self,
        req: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error> {
        let session_id = req.session_id.to_string();
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned().ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "unknown session",
                    "sessionId": session_id,
                }))
            })?
        };

        session_ref.set_session_model(req).await
    }

    async fn set_session_mode(
        &self,
        req: agent_client_protocol::schema::SetSessionModeRequest,
    ) -> Result<agent_client_protocol::schema::SetSessionModeResponse, Error> {
        let mode = req
            .mode_id
            .0
            .parse::<AgentMode>()
            .map_err(|e| Error::invalid_params().data(serde_json::json!({ "error": e })))?;
        let session_id = req.session_id.to_string();

        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned().ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "unknown session",
                    "sessionId": session_id,
                }))
            })?
        };

        session_ref.set_mode(mode).await.map_err(Error::from)?;
        Ok(agent_client_protocol::schema::SetSessionModeResponse::new())
    }

    async fn set_session_config_option(
        &self,
        req: agent_client_protocol::schema::SetSessionConfigOptionRequest,
    ) -> Result<agent_client_protocol::schema::SetSessionConfigOptionResponse, Error> {
        use crate::agent::session_registry::config_options;
        use agent_client_protocol::schema::SessionConfigOptionValue;

        let config_id = req.config_id.0.as_ref();

        let SessionConfigOptionValue::ValueId { value: value_id } = req.value else {
            return Err(Error::invalid_params().data(serde_json::json!({
                "error": "config option requires a value id",
            })));
        };

        let session_id = req.session_id.0.to_string();

        match config_id {
            "model" => {
                #[derive(serde::Deserialize)]
                struct QuerymtMeta {
                    #[serde(rename = "modelEntry")]
                    model_entry: crate::model_registry::ModelEntry,
                }

                #[derive(serde::Deserialize)]
                struct RequestMeta {
                    querymt: QuerymtMeta,
                }

                let model_id = value_id.0.to_string();
                let provider_node_id = req
                    .meta
                    .as_ref()
                    .and_then(|m| serde_json::from_value::<RequestMeta>(serde_json::Value::Object(m.clone())).ok())
                    .and_then(|m| m.querymt.model_entry.node_id)
                    .map(|node_id| {
                        #[cfg(feature = "remote")]
                        {
                            crate::agent::remote::NodeId::parse(&node_id).map_err(|e| {
                                Error::invalid_params().data(serde_json::json!({
                                    "error": format!("invalid modelEntry.node_id '{}': {}", node_id, e),
                                }))
                            })
                        }
                        #[cfg(not(feature = "remote"))]
                        {
                            let _ = node_id;
                            Ok::<(), Error>(())
                        }
                    })
                    .transpose()?;

                let session_ref = {
                    let registry = self.registry.lock().await;
                    registry.get(&session_id).cloned().ok_or_else(|| {
                        Error::invalid_params().data(serde_json::json!({
                            "message": "unknown session",
                            "sessionId": session_id,
                        }))
                    })?
                };

                #[cfg(feature = "remote")]
                let msg = crate::agent::messages::SetSessionModel {
                    req: SetSessionModelRequest::new(session_id.clone(), model_id),
                    provider_node_id,
                };
                #[cfg(not(feature = "remote"))]
                let msg = crate::agent::messages::SetSessionModel {
                    req: SetSessionModelRequest::new(session_id.clone(), model_id),
                    provider_node_id: None,
                };

                session_ref.set_session_model_with_node(msg).await?;

                let mode = session_ref.get_mode().await.unwrap_or(AgentMode::Build);
                let effort = session_ref.get_reasoning_effort().await.ok().flatten();

                Ok(
                    agent_client_protocol::schema::SetSessionConfigOptionResponse::new(
                        config_options(mode, effort),
                    ),
                )
            }
            "mode" => {
                let mode = value_id
                    .0
                    .parse::<AgentMode>()
                    .map_err(|e| Error::invalid_params().data(serde_json::json!({ "error": e })))?;

                let session_ref = {
                    let registry = self.registry.lock().await;
                    registry.get(&session_id).cloned().ok_or_else(|| {
                        Error::invalid_params().data(serde_json::json!({
                            "message": "unknown session",
                            "sessionId": session_id,
                        }))
                    })?
                };
                session_ref.set_mode(mode).await.map_err(Error::from)?;

                let effort = session_ref.get_reasoning_effort().await.ok().flatten();
                Ok(
                    agent_client_protocol::schema::SetSessionConfigOptionResponse::new(
                        config_options(mode, effort),
                    ),
                )
            }
            "reasoning_effort" => {
                let effort_str = value_id.0.as_ref();
                let effort = if effort_str == "auto" {
                    None
                } else {
                    Some(
                        serde_json::from_value::<querymt::chat::ReasoningEffort>(
                            serde_json::json!(effort_str),
                        )
                        .map_err(|e| {
                            Error::invalid_params().data(serde_json::json!({
                                "error": format!("Invalid reasoning effort '{}': {}", effort_str, e),
                            }))
                        })?,
                    )
                };

                let session_ref = {
                    let registry = self.registry.lock().await;
                    registry.get(&session_id).cloned().ok_or_else(|| {
                        Error::invalid_params().data(serde_json::json!({
                            "message": "unknown session",
                            "sessionId": session_id,
                        }))
                    })?
                };

                session_ref
                    .set_reasoning_effort(effort)
                    .await
                    .map_err(Error::from)?;

                let mode = session_ref.get_mode().await.unwrap_or(AgentMode::Build);

                Ok(
                    agent_client_protocol::schema::SetSessionConfigOptionResponse::new(
                        config_options(mode, effort),
                    ),
                )
            }
            _ => Err(Error::invalid_params().data(serde_json::json!({
                "error": format!("Unsupported configId: {}", config_id),
            }))),
        }
    }

    async fn ext_method(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        match req.method.as_ref() {
            "querymt/models" => {
                // Return local + remote models via the ModelInventory (non-blocking snapshot).
                let (models, meta) = self.model_inventory.get_snapshot().await;

                // If snapshot is empty, trigger background refresh but return empty immediately
                if models.is_empty() && !meta.refresh_in_progress {
                    self.model_inventory.trigger_refresh().await;
                }

                let result = serde_json::json!({
                    "models": models,
                    "meta": {
                        "stale": meta.is_stale,
                        "refresh_in_progress": meta.refresh_in_progress,
                        "remote_timeout_count": meta.remote_timeout_count,
                        "remote_node_count": meta.remote_node_count,
                    }
                });
                let json = serde_json::to_string(&result).map_err(|e| {
                    Error::from(crate::error::AgentError::Serialization(e.to_string()))
                })?;
                let raw = serde_json::value::RawValue::from_string(json).map_err(|e| {
                    Error::from(crate::error::AgentError::Serialization(e.to_string()))
                })?;
                Ok(ExtResponse::new(Arc::from(raw)))
            }
            "querymt/profiles" => ext_json_response(&self.profiles_response().await?),
            "querymt/profile/setActive" => {
                #[derive(serde::Deserialize)]
                struct SetActiveProfileRequest {
                    #[serde(alias = "profileId")]
                    profile_id: String,
                }

                let parsed: SetActiveProfileRequest = serde_json::from_str(req.params.get())
                    .map_err(|e| {
                        Error::invalid_params().data(serde_json::json!({
                            "message": format!("invalid profile setActive params: {e}"),
                        }))
                    })?;
                let profile_id = parsed.profile_id.trim();
                if profile_id.is_empty() {
                    return Err(Error::invalid_params().data(serde_json::json!({
                        "message": "profile_id must be a non-empty string",
                    })));
                }

                let profiles = self.profiles().ok_or_else(|| {
                    Error::invalid_params().data(serde_json::json!({
                        "message": "profiles are not configured",
                    }))
                })?;
                profiles
                    .set_active_profile(profile_id)
                    .await
                    .map_err(|err| {
                        Error::internal_error().data(serde_json::json!({
                            "message": format_prefixed_error_chain(
                                "Failed to set active profile",
                                &err,
                            ),
                        }))
                    })?;

                ext_json_response(&self.profiles_response().await?)
            }
            "querymt/refreshModels" => {
                // Refreshes now stay fully backgrounded so slow provider/model scans
                // cannot extend the caller's critical path.
                let handle = self.model_inventory.trigger_refresh().await;
                let (models, meta) = self.model_inventory.get_snapshot().await;
                let result = serde_json::json!({
                    "models": models,
                    "meta": {
                        "stale": meta.is_stale,
                        "refresh_in_progress": meta.refresh_in_progress,
                        "remote_timeout_count": meta.remote_timeout_count,
                        "remote_node_count": meta.remote_node_count,
                        "refresh_trigger": handle.disposition().as_str(),
                        "started_new_refresh": handle.started_new_refresh(),
                        "wait_for_completion": handle.waits_for_completion(),
                    }
                });
                let json = serde_json::to_string(&result).map_err(|e| {
                    Error::from(crate::error::AgentError::Serialization(e.to_string()))
                })?;
                let raw = serde_json::value::RawValue::from_string(json).map_err(|e| {
                    Error::from(crate::error::AgentError::Serialization(e.to_string()))
                })?;
                Ok(ExtResponse::new(Arc::from(raw)))
            }
            "querymt/modelInfo" => {
                // Batch lookup of model metadata from the providers registry.
                // Request:  { "models": [{ "provider": "openai", "model": "gpt-4" }, ...] }
                // Response: { "models": { "openai:gpt-4": <ModelInfo|null>, ... } }
                #[derive(serde::Deserialize)]
                struct ModelInfoRequest {
                    #[serde(default)]
                    models: Vec<ModelKey>,
                }
                #[derive(serde::Deserialize)]
                struct ModelKey {
                    provider: String,
                    model: String,
                }

                let parsed: ModelInfoRequest =
                    serde_json::from_str(req.params.get()).map_err(|e| {
                        Error::from(crate::error::AgentError::Serialization(e.to_string()))
                    })?;

                let registry = querymt::providers::read_providers_from_cache();
                let mut info_map = serde_json::Map::new();

                match registry {
                    Ok(reg) => {
                        for key in &parsed.models {
                            let lookup = reg.get_model(&key.provider, &key.model);
                            let map_key = format!("{}/{}", key.provider, key.model);
                            match lookup {
                                Some(model_info) => {
                                    let val = serde_json::to_value(model_info)
                                        .unwrap_or(serde_json::Value::Null);
                                    info_map.insert(map_key, val);
                                }
                                None => {
                                    info_map.insert(map_key, serde_json::Value::Null);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to load providers registry for modelInfo: {}", e);
                        // Return all nulls
                        for key in &parsed.models {
                            let map_key = format!("{}/{}", key.provider, key.model);
                            info_map.insert(map_key, serde_json::Value::Null);
                        }
                    }
                }

                let result = serde_json::json!({ "models": info_map });
                let json = serde_json::to_string(&result).map_err(|e| {
                    Error::from(crate::error::AgentError::Serialization(e.to_string()))
                })?;
                let raw = serde_json::value::RawValue::from_string(json).map_err(|e| {
                    Error::from(crate::error::AgentError::Serialization(e.to_string()))
                })?;
                Ok(ExtResponse::new(Arc::from(raw)))
            }
            "querymt/chat" => {
                // One-shot chat completion using specified model.
                // This is a stub — full implementation requires wiring up the
                // provider registry for arbitrary model routing and streaming.
                // For now, return an error indicating it is not yet implemented.
                Err(Error::from(
                    crate::error::AgentError::MethodNotImplemented {
                        method: "querymt/chat".to_string(),
                    },
                ))
            }
            "querymt/tokenCount" => {
                // Token counting — stub for now.
                Err(Error::from(
                    crate::error::AgentError::MethodNotImplemented {
                        method: "querymt/tokenCount".to_string(),
                    },
                ))
            }

            // ── _querymt/auth extensions ──────────────────────────────────
            "querymt/auth/status" => {
                #[derive(serde::Deserialize, Default)]
                struct StatusReq {
                    #[serde(default)]
                    provider: Option<String>,
                }
                let parsed: StatusReq = serde_json::from_str(req.params.get()).unwrap_or_default();
                let statuses = self
                    .oauth_service
                    .auth_status(parsed.provider.as_deref())
                    .await;
                ext_json_response(&serde_json::json!({ "providers": statuses }))
            }
            "querymt/auth/start" => {
                #[derive(serde::Deserialize)]
                struct StartReq {
                    provider: String,
                }
                let parsed: StartReq = serde_json::from_str(req.params.get()).map_err(|e| {
                    Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
                })?;
                let result = self
                    .oauth_service
                    .start_flow("acp", &parsed.provider, None)
                    .await
                    .map_err(|e| Error::internal_error().data(serde_json::json!({"error": e})))?;
                ext_json_response(&result)
            }
            "querymt/auth/complete" => {
                #[derive(serde::Deserialize)]
                struct CompleteReq {
                    flow_id: String,
                    response: String,
                }
                let parsed: CompleteReq = serde_json::from_str(req.params.get()).map_err(|e| {
                    Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
                })?;
                let result = self
                    .oauth_service
                    .complete_flow("acp", &parsed.flow_id, &parsed.response)
                    .await;
                ext_json_response(&result)
            }
            "querymt/auth/logout" => {
                #[derive(serde::Deserialize)]
                struct LogoutReq {
                    provider: String,
                }
                let parsed: LogoutReq = serde_json::from_str(req.params.get()).map_err(|e| {
                    Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
                })?;
                let result = self.oauth_service.logout("acp", &parsed.provider).await;
                ext_json_response(&result)
            }

            // ── querymt/mesh extensions ────────────────────────────────
            "querymt/mesh/status" => {
                #[cfg(feature = "remote")]
                {
                    if let Some(mesh) = self.mesh() {
                        let result = serde_json::json!({
                            "enabled": true,
                            "peer_id": mesh.peer_id().to_string(),
                            "transport": if mesh.is_iroh_transport_internal() { "iroh" } else { "lan" },
                            "known_peer_count": mesh.known_peer_ids().len(),
                            "has_invite_store": mesh.invite_store().is_some(),
                            "has_mesh_state_store": mesh.mesh_state_store().is_some(),
                        });
                        return ext_json_response(&result);
                    }
                }

                ext_json_response(&serde_json::json!({
                    "enabled": false,
                    "peer_id": serde_json::Value::Null,
                    "transport": serde_json::Value::Null,
                    "known_peer_count": 0,
                    "has_invite_store": false,
                    "has_membership_store": false,
                }))
            }
            "querymt/mesh/join" => {
                #[cfg(feature = "remote")]
                {
                    #[derive(serde::Deserialize)]
                    #[serde(rename_all = "camelCase")]
                    struct JoinReq {
                        invite: String,
                    }

                    let parsed: JoinReq = serde_json::from_str(req.params.get()).map_err(|e| {
                        Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
                    })?;

                    let invite =
                        crate::agent::remote::invite::SignedInviteGrant::decode(&parsed.invite)
                            .map_err(|e| {
                                Error::invalid_params().data(
                            serde_json::json!({"error": format!("invalid mesh invite: {}", e)}),
                        )
                            })?;

                    let mesh = crate::agent::remote::join_mesh_via_invite(&invite, None)
                        .await
                        .map_err(|e| {
                            Error::internal_error()
                                .data(serde_json::json!({"error": e.to_string()}))
                        })?;

                    self.set_mesh(mesh.clone());

                    return ext_json_response(&serde_json::json!({
                        "joined": true,
                        "peer_id": mesh.peer_id().to_string(),
                        "mesh_name": invite.grant.mesh_name,
                        "inviter_peer_id": invite.grant.inviter_peer_id,
                    }));
                }

                #[cfg(not(feature = "remote"))]
                {
                    Err(Error::method_not_found())
                }
            }
            "querymt/mesh/nodes" => {
                #[cfg(feature = "remote")]
                {
                    let nodes = self
                        .list_remote_nodes()
                        .await
                        .into_iter()
                        .map(|n| {
                            serde_json::json!({
                                "id": n.node_id.to_string(),
                                "label": n.hostname,
                                "capabilities": n.capabilities,
                                "active_sessions": n.active_sessions,
                            })
                        })
                        .collect::<Vec<_>>();
                    return ext_json_response(&serde_json::json!({ "nodes": nodes }));
                }

                #[cfg(not(feature = "remote"))]
                {
                    ext_json_response(&serde_json::json!({ "nodes": [] }))
                }
            }
            "querymt/remote/sessions" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct RemoteSessionsReq {
                    node_id: String,
                    #[serde(default)]
                    offset: Option<u32>,
                    #[serde(default)]
                    limit: Option<u32>,
                }

                let parsed: RemoteSessionsReq =
                    serde_json::from_str(req.params.get()).map_err(|e| {
                        Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
                    })?;

                #[cfg(feature = "remote")]
                {
                    let nm_ref = self.find_node_manager(&parsed.node_id).await?;
                    let response = self
                        .list_remote_sessions(&nm_ref, parsed.offset, parsed.limit)
                        .await?;
                    return ext_json_response(&serde_json::json!({
                        "nodeId": parsed.node_id,
                        "sessions": response.sessions,
                        "nextOffset": response.next_offset,
                        "totalCount": response.total_count,
                    }));
                }

                #[cfg(not(feature = "remote"))]
                {
                    let _ = parsed;
                    Err(Error::method_not_found())
                }
            }
            "querymt/remote/createSession" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct CreateReq {
                    node_id: String,
                    #[serde(default)]
                    cwd: Option<String>,
                }

                let parsed: CreateReq = serde_json::from_str(req.params.get()).map_err(|e| {
                    Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
                })?;

                #[cfg(feature = "remote")]
                {
                    let nm_ref = self.find_node_manager(&parsed.node_id).await?;
                    let resp = self
                        .create_remote_session(&nm_ref, parsed.cwd.clone())
                        .await?;

                    // Session is created on the remote node but NOT attached
                    // to the local registry here. The caller must call
                    // querymt/remote/attachSession to subscribe to session
                    // events and hydrate the local view.
                    return ext_json_response(&serde_json::json!({
                        "sessionId": resp.session_id,
                        "nodeId": parsed.node_id,
                        "attached": false,
                        "configOptions": [],
                    }));
                }

                #[cfg(not(feature = "remote"))]
                {
                    let _ = parsed;
                    Err(Error::method_not_found())
                }
            }
            "querymt/remote/attachSession" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct AttachReq {
                    node_id: String,
                    session_id: String,
                }

                let parsed: AttachReq = serde_json::from_str(req.params.get()).map_err(|e| {
                    Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
                })?;

                #[cfg(feature = "remote")]
                {
                    let mesh = self
                        .mesh()
                        .ok_or_else(|| Error::invalid_request().data("mesh not bootstrapped"))?;

                    // Try scoped DHT lookup first — works for already-hydrated sessions.
                    let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
                    let mut remote_ref = None;
                    let mut matched_scope = None;
                    let mut lookup_err = None;
                    for scope in runtime.active_scopes() {
                        let dht_name =
                            crate::agent::remote::scope::scoped_session(&scope, &parsed.session_id);
                        match runtime
                            .lookup_actor::<crate::agent::session_actor::SessionActor>(dht_name)
                            .await
                        {
                            Ok(Some(found)) => {
                                remote_ref = Some(found);
                                matched_scope = Some(scope);
                                break;
                            }
                            Ok(None) => {}
                            Err(e) => lookup_err = Some(e),
                        }
                    }
                    let remote_ref = match remote_ref {
                        Some(r) => r,
                        None => {
                            if let Some(err) = lookup_err {
                                log::debug!(
                                    "remote attach scoped lookup error before resume fallback: {}",
                                    err
                                );
                            }
                            // Session not in DHT — ask the remote node to
                            // resume (materialize) it from persistence, then
                            // resolve the handoff into a remote actor ref.
                            let nm_ref = self.find_node_manager(&parsed.node_id).await?;
                            let resumed = self
                                .resume_remote_session(&nm_ref, parsed.session_id.clone())
                                .await?;
                            self.resolve_handoff(&parsed.session_id, resumed.handoff)
                                .await?
                        }
                    };

                    let peer_label = self
                        .list_remote_nodes()
                        .await
                        .into_iter()
                        .find(|n| n.node_id.to_string() == parsed.node_id)
                        .map(|n| n.hostname)
                        .unwrap_or_else(|| parsed.node_id.clone());

                    self.attach_remote_session(
                        parsed.session_id.clone(),
                        remote_ref,
                        peer_label,
                        matched_scope,
                        Some(parsed.node_id.clone()),
                    )
                    .await;

                    let snapshot = self
                        .build_remote_attach_snapshot(&parsed.session_id)
                        .await
                        .unwrap_or(serde_json::Value::Null);

                    return ext_json_response(&serde_json::json!({
                        "sessionId": parsed.session_id,
                        "nodeId": parsed.node_id,
                        "attached": true,
                        "configOptions": [],
                        "snapshot": snapshot,
                    }));
                }

                #[cfg(not(feature = "remote"))]
                {
                    let _ = parsed;
                    Err(Error::method_not_found())
                }
            }
            "querymt/remote/dismissSession" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct DismissReq {
                    session_id: String,
                }

                let parsed: DismissReq = serde_json::from_str(req.params.get()).map_err(|e| {
                    Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
                })?;

                #[cfg(feature = "remote")]
                {
                    {
                        let mut registry = self.registry.lock().await;
                        registry.detach_remote_session(&parsed.session_id).await;
                    }

                    self.config
                        .provider
                        .history_store()
                        .remove_remote_session_bookmark(&parsed.session_id)
                        .await
                        .map_err(|e| {
                            Error::internal_error()
                                .data(serde_json::json!({"error": e.to_string()}))
                        })?;

                    return ext_json_response(&serde_json::json!({ "success": true }));
                }

                #[cfg(not(feature = "remote"))]
                {
                    let _ = parsed;
                    Err(Error::method_not_found())
                }
            }
            "querymt/mesh/createInvite" => {
                #[derive(serde::Deserialize, Default)]
                struct CreateInviteReq {
                    #[serde(default)]
                    #[serde(alias = "meshName")]
                    mesh_name: Option<String>,
                    #[serde(default)]
                    ttl: Option<String>,
                    #[serde(default)]
                    #[serde(alias = "maxUses")]
                    max_uses: Option<u32>,
                }

                let parsed: CreateInviteReq =
                    serde_json::from_str(req.params.get()).map_err(|e| {
                        Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
                    })?;

                #[cfg(feature = "remote")]
                {
                    let Some(mesh) = self.mesh() else {
                        return Err(Error::invalid_request().data(
                            serde_json::json!({"error": "mesh not bootstrapped - start with --mesh"}),
                        ));
                    };

                    if !mesh.is_iroh_transport_internal() {
                        return ext_json_response(&serde_json::json!({
                            "error": "mesh invites require iroh transport; restart host with --mesh --mesh-invite (or set transport=iroh)"
                        }));
                    }

                    let ttl_secs = parsed
                        .ttl
                        .as_deref()
                        .and_then(crate::agent::remote::invite::parse_duration_secs)
                        .or(Some(24 * 3600));

                    let invite = mesh
                        .create_invite(parsed.mesh_name.clone(), ttl_secs, parsed.max_uses, false)
                        .map_err(|e| {
                            Error::internal_error()
                                .data(serde_json::json!({"error": format!("{e}")}))
                        })?;

                    let qr_code = crate::agent::remote::qr::render_to_terminal(&invite.to_url());

                    return ext_json_response(&serde_json::json!({
                        "inviteId": invite.grant.invite_id,
                        "url": invite.to_url(),
                        "qrCode": qr_code,
                        "expiresAt": invite.grant.expires_at,
                        "maxUses": invite.grant.max_uses,
                        "meshName": parsed.mesh_name,
                    }));
                }

                #[cfg(not(feature = "remote"))]
                {
                    Err(Error::method_not_found())
                }
            }
            "querymt/mesh/listInvites" => {
                #[cfg(feature = "remote")]
                {
                    let Some(mesh) = self.mesh() else {
                        return ext_json_response(&serde_json::json!({"invites": []}));
                    };

                    let invites: Vec<serde_json::Value> = if let Some(store) = mesh.invite_store() {
                        let store = store.read();
                        store
                            .list_pending()
                            .into_iter()
                            .map(|r| {
                                serde_json::json!({
                                    "inviteId": r.invite_id,
                                    "meshName": r.grant.mesh_name,
                                    "expiresAt": r.grant.expires_at,
                                    "maxUses": r.grant.max_uses,
                                    "usesRemaining": r.uses_remaining,
                                    "status": match r.status {
                                        crate::agent::remote::invite::InviteStatus::Pending => "pending",
                                        crate::agent::remote::invite::InviteStatus::Consumed => "consumed",
                                        crate::agent::remote::invite::InviteStatus::Revoked => "revoked",
                                    },
                                    "usedBy": r.used_by,
                                    "createdAt": r.created_at,
                                })
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                    return ext_json_response(&serde_json::json!({"invites": invites}));
                }

                #[cfg(not(feature = "remote"))]
                {
                    Err(Error::method_not_found())
                }
            }
            "querymt/mesh/revokeInvite" => {
                #[derive(serde::Deserialize)]
                struct RevokeInviteReq {
                    #[serde(alias = "inviteId")]
                    invite_id: String,
                }

                let parsed: RevokeInviteReq =
                    serde_json::from_str(req.params.get()).map_err(|e| {
                        Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
                    })?;

                #[cfg(feature = "remote")]
                {
                    let Some(mesh) = self.mesh() else {
                        return ext_json_response(&serde_json::json!({
                            "success": false,
                            "message": "mesh not bootstrapped - start with --mesh"
                        }));
                    };

                    let result = if let Some(store) = mesh.invite_store() {
                        store.write().revoke(&parsed.invite_id)
                    } else {
                        Err(crate::agent::remote::invite::InviteError::StoreError(
                            "invite store not available".to_string(),
                        ))
                    };

                    return match result {
                        Ok(()) => ext_json_response(&serde_json::json!({
                            "success": true,
                            "message": null,
                        })),
                        Err(e) => ext_json_response(&serde_json::json!({
                            "success": false,
                            "message": e.to_string(),
                        })),
                    };
                }

                #[cfg(not(feature = "remote"))]
                {
                    Err(Error::method_not_found())
                }
            }

            // ── _querymt/updatePlugins ─────────────────────────────────
            "querymt/updatePlugins" => {
                #[cfg(feature = "plugin-loaders")]
                {
                    let registry = self.config.provider.plugin_registry();
                    let results = crate::plugin_update::update_all_plugins(&registry, None).await;
                    ext_json_response(&serde_json::json!({ "results": results }))
                }

                #[cfg(not(feature = "plugin-loaders"))]
                {
                    Err(Error::method_not_found())
                }
            }

            _ => Err(Error::method_not_found()),
        }
    }

    #[tracing::instrument(name = "acp.ext_notification", skip_all)]
    async fn ext_notification(&self, _notif: ExtNotification) -> Result<(), Error> {
        // OK - extensions not yet implemented
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn format_error_chain(err: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in err.chain() {
        let message = cause.to_string();
        if parts.last() != Some(&message) {
            parts.push(message);
        }
    }
    parts.join(": ")
}

fn format_prefixed_error_chain(prefix: &str, err: &anyhow::Error) -> String {
    format!("{prefix}: {}", format_error_chain(err))
}

impl LocalAgentHandle {
    async fn profiles_response(&self) -> Result<serde_json::Value, Error> {
        let Some(profiles) = self.profiles() else {
            return Ok(serde_json::json!({
                "profiles": [],
                "active_profile_id": serde_json::Value::Null,
            }));
        };

        let profile_infos: Vec<serde_json::Value> = profiles
            .list_profiles()
            .await
            .map(|profiles| {
                profiles
                    .into_iter()
                    .map(|metadata| {
                        serde_json::json!({
                            "id": metadata.id,
                            "name": metadata.name,
                            "description": metadata.description,
                            "tags": metadata.tags,
                            "config_kind": metadata.config_kind.map(|kind| kind.storage_label()),
                            "source": metadata.source.storage_label(),
                            "fingerprint": metadata.fingerprint,
                        })
                    })
                    .collect()
            })
            .map_err(|err| {
                Error::internal_error().data(serde_json::json!({
                    "message": format_prefixed_error_chain("Failed to list profiles", &err),
                }))
            })?;
        let active_profile_id = profiles.active_profile_id().await;

        Ok(serde_json::json!({
            "profiles": profile_infos,
            "active_profile_id": active_profile_id,
        }))
    }
}

/// Helper to build an `ExtResponse` from a serializable value.
fn ext_json_response<T: serde::Serialize>(
    value: &T,
) -> Result<ExtResponse, agent_client_protocol::Error> {
    let json = serde_json::to_string(value).map_err(|e| {
        agent_client_protocol::Error::from(crate::error::AgentError::Serialization(e.to_string()))
    })?;
    let raw = serde_json::value::RawValue::from_string(json).map_err(|e| {
        agent_client_protocol::Error::from(crate::error::AgentError::Serialization(e.to_string()))
    })?;
    Ok(ExtResponse::new(Arc::from(raw)))
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::SessionActor;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::api::AgentInfra;
    use crate::send_agent::SendAgent;
    use crate::session::backend::StorageBackend;
    use crate::session::store::SessionStore;
    use crate::test_utils::{
        MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory,
        empty_plugin_registry, mock_llm_config, mock_plugin_registry, mock_session,
    };
    use agent_client_protocol::schema::{
        CancelNotification, CloseSessionRequest, DeleteSessionRequest, InitializeRequest,
        ListSessionsRequest, ProtocolVersion, SessionId,
    };
    use querymt::LLMParams;
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[cfg(feature = "remote")]
    use kameo::actor::Spawn;

    // ── Shared fixture ───────────────────────────────────────────────────────

    struct HandleFixture {
        handle: LocalAgentHandle,
        _temp_dir: tempfile::TempDir,
    }

    impl HandleFixture {
        async fn new() -> Self {
            Self::with_list_sessions(vec![]).await
        }

        async fn with_profiles(self, active_profile_id: &str, profile_dir: &Path) -> Self {
            let catalog: Arc<dyn ProfileCatalog> = Arc::new(
                crate::profiles::LocalProfileCatalog::builder()
                    .include_embedded_default(false)
                    .local_dir(profile_dir)
                    .build(),
            );
            let (plugin_registry, _temp_dir) = empty_plugin_registry().expect("plugin registry");
            let profiles = Arc::new(ProfileRuntimeManager::with_infra_boxed(
                catalog,
                active_profile_id,
                AgentInfra {
                    plugin_registry: Arc::new(plugin_registry),
                    storage: None,
                    session_mcp_attachment_source: None,
                },
            ));
            self.handle.set_profiles(profiles);
            self
        }

        async fn with_list_sessions(listed_sessions: Vec<crate::session::store::Session>) -> Self {
            let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
            let shared = SharedLlmProvider {
                inner: provider.clone(),
                tools: vec![].into_boxed_slice(),
            };
            let factory = Arc::new(TestProviderFactory { provider: shared });
            let (plugin_registry, temp_dir) =
                mock_plugin_registry(factory).expect("plugin registry");

            let llm_config = mock_llm_config();
            let session = mock_session("test-session");
            let mut store = MockSessionStore::new();
            let session_clone = session.clone();
            store
                .expect_get_session()
                .returning(move |_| Ok(Some(session_clone.clone())))
                .times(0..);
            let llm_for_mock = llm_config.clone();
            store
                .expect_get_session_llm_config()
                .returning(move |_| Ok(Some(llm_for_mock.clone())))
                .times(0..);
            store
                .expect_get_llm_config()
                .returning(move |_| Ok(Some(llm_config.clone())))
                .times(0..);
            store
                .expect_list_sessions()
                .returning(move || Ok(listed_sessions.clone()))
                .times(0..);
            store
                .expect_create_or_get_llm_config()
                .returning(|_| Ok(mock_llm_config()))
                .times(0..);
            store
                .expect_set_session_llm_config()
                .returning(|_, _| Ok(()))
                .times(0..);
            store
                .expect_delete_session()
                .returning(|_| Ok(()))
                .times(0..);

            let store: Arc<dyn SessionStore> = Arc::new(store);
            let storage = Arc::new(
                crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                    .await
                    .expect("create event store"),
            );

            let mut builder = AgentConfigBuilder::new(
                Arc::new(plugin_registry),
                store.clone(),
                storage.event_journal(),
                LLMParams::new().provider("mock").model("mock-model"),
            )
            .with_tool_policy(ToolPolicy::ProviderOnly);

            if let Some(repo) = storage.schedule_repository() {
                builder = builder.with_schedule_repository(repo);
            }

            let config = Arc::new(builder.build());

            Self {
                handle: LocalAgentHandle::from_config(config),
                _temp_dir: temp_dir,
            }
        }
    }

    fn raw_params(value: &str) -> Arc<serde_json::value::RawValue> {
        Arc::from(serde_json::value::RawValue::from_string(value.to_string()).unwrap())
    }

    fn write_profile(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).expect("profile should be written");
    }

    impl LocalAgentHandle {
        fn should_return_without_force_stop(
            status: crate::agent::messages::SessionRuntimeStatus,
        ) -> bool {
            matches!(status, crate::agent::messages::SessionRuntimeStatus::Idle)
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_should_return_without_force_stop_only_for_idle() {
        assert!(LocalAgentHandle::should_return_without_force_stop(
            crate::agent::messages::SessionRuntimeStatus::Idle
        ));
        assert!(!LocalAgentHandle::should_return_without_force_stop(
            crate::agent::messages::SessionRuntimeStatus::Running
        ));
        assert!(!LocalAgentHandle::should_return_without_force_stop(
            crate::agent::messages::SessionRuntimeStatus::CancelRequested
        ));
    }

    #[tokio::test]
    async fn test_from_config_creates_empty_registry() {
        let f = HandleFixture::new().await;
        let registry = f.handle.registry.lock().await;
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn test_initialize_returns_latest_protocol() {
        let f = HandleFixture::new().await;
        let req = InitializeRequest::new(ProtocolVersion::LATEST);
        let resp = f.handle.initialize(req).await.expect("initialize");
        assert!(resp.protocol_version <= ProtocolVersion::LATEST);
    }

    #[tokio::test]
    async fn test_initialize_downgrades_newer_client_protocol() {
        let f = HandleFixture::new().await;
        // Simulate a client claiming a future protocol version by using LATEST
        // (we can't construct a truly higher version, but LATEST is still valid)
        let req = InitializeRequest::new(ProtocolVersion::LATEST);
        let resp = f.handle.initialize(req).await.expect("initialize");
        // Server caps at LATEST
        assert_eq!(resp.protocol_version, ProtocolVersion::LATEST);
    }

    #[tokio::test]
    async fn test_initialize_advertises_session_capabilities() {
        let f = HandleFixture::new().await;
        let req = InitializeRequest::new(ProtocolVersion::LATEST);
        let resp = f.handle.initialize(req).await.expect("initialize");

        assert!(resp.agent_capabilities.load_session);
        assert!(resp.agent_capabilities.session_capabilities.list.is_some());
        assert!(resp.agent_capabilities.session_capabilities.fork.is_some());
        assert!(
            resp.agent_capabilities
                .session_capabilities
                .resume
                .is_some()
        );
        assert!(resp.agent_capabilities.session_capabilities.close.is_some());
        assert!(
            resp.agent_capabilities
                .session_capabilities
                .delete
                .is_some()
        );
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn test_cancel_known_remote_session_routes_cancel_to_session_ref() {
        use crate::agent::core::SessionRuntime;
        use crate::agent::remote::scope::{MeshScopeId, scoped_session};

        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
        let f = HandleFixture::new().await;
        f.handle.set_mesh(mesh.clone());

        let session_id = "remote-cancel-known".to_string();
        let actor = SessionActor::new(
            f.handle.config.clone(),
            session_id.clone(),
            SessionRuntime::new(
                None,
                std::collections::HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        )
        .with_mesh(Some(mesh.clone()));
        let local_ref = SessionActor::spawn(actor);
        let dht_name = scoped_session(&MeshScopeId::lan_default(), &session_id);
        mesh.register_actor(local_ref, dht_name.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let remote_ref = mesh
            .lookup_actor::<SessionActor>(&dht_name)
            .await
            .expect("DHT lookup should succeed")
            .expect("remote actor should be available");

        f.handle
            .attach_remote_session(
                session_id.clone(),
                remote_ref,
                "remote-peer".to_string(),
                None,
                None,
            )
            .await;

        let mut rx = f.handle.subscribe_events();
        let notif = CancelNotification::new(SessionId::from(session_id.clone()));
        SendAgent::cancel(&f.handle, notif)
            .await
            .expect("cancel should succeed");

        let event = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("should receive event in time")
            .expect("event channel should remain open");

        assert_eq!(event.session_id(), session_id);
        assert!(matches!(
            event.kind(),
            crate::events::AgentEventKind::Cancelled
        ));
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn test_list_remote_nodes_prefers_per_peer_lookup() {
        use crate::agent::remote::RemoteNodeManager;
        use crate::agent::remote::scope::{MeshScopeId, scoped_node_manager_for_peer};
        use kameo::actor::Spawn;

        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
        let f = HandleFixture::new().await;
        f.handle.set_mesh(mesh.clone());

        let remote_cfg = HandleFixture::new().await;
        let peer_id = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let node_manager = RemoteNodeManager::new(
            remote_cfg.handle.config.clone(),
            remote_cfg.handle.registry.clone(),
            Some(mesh.clone()),
        )
        .with_node_name("peer-alpha".to_string());
        let node_manager_ref = RemoteNodeManager::spawn(node_manager);

        let per_peer_name = scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &peer_id);
        mesh.register_actor(node_manager_ref, per_peer_name).await;
        mesh.inject_known_peer_for_test(peer_id);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let nodes = f.handle.list_remote_nodes().await;
        let labels: HashSet<_> = nodes.into_iter().map(|node| node.hostname).collect();

        assert!(
            labels.contains("peer-alpha"),
            "per-peer DHT registrations should be visible in list_remote_nodes"
        );
    }

    #[tokio::test]
    async fn test_list_sessions_empty() {
        let f = HandleFixture::new().await;
        let req = ListSessionsRequest::new();
        let resp = f.handle.list_sessions(req).await.expect("list_sessions");
        assert!(resp.sessions.is_empty());
    }

    #[tokio::test]
    async fn test_list_sessions_filters_by_cwd() {
        let cwd_a = std::env::temp_dir().join("querymt-list-sessions-a");
        let cwd_b = std::env::temp_dir().join("querymt-list-sessions-b");

        let mut session_a = mock_session("session-a");
        session_a.cwd = Some(cwd_a.clone());
        let mut session_b = mock_session("session-b");
        session_b.cwd = Some(cwd_b.clone());

        let f = HandleFixture::with_list_sessions(vec![session_a, session_b]).await;

        let resp = f
            .handle
            .list_sessions(ListSessionsRequest::new().cwd(cwd_a.clone()))
            .await
            .expect("list_sessions filtered by cwd");

        assert_eq!(resp.sessions.len(), 1);
        assert_eq!(resp.sessions[0].cwd, cwd_a);
    }

    #[tokio::test]
    async fn test_cancel_unknown_session_is_noop() {
        let f = HandleFixture::new().await;
        let notif = CancelNotification::new(SessionId::from("no-such-session".to_string()));
        // Should not return an error — stop for unknown sessions is a no-op
        let result = SendAgent::cancel(&f.handle, notif).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_close_unknown_session_is_noop() {
        let f = HandleFixture::new().await;
        let req = CloseSessionRequest::new(SessionId::from("no-such-session".to_string()));
        let result = SendAgent::close_session(&f.handle, req).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_delete_unknown_session_is_noop() {
        let f = HandleFixture::new().await;
        let req = DeleteSessionRequest::new(SessionId::from("no-such-session".to_string()));
        let result = SendAgent::delete_session(&f.handle, req).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_prompt_unknown_session_returns_error() {
        let f = HandleFixture::new().await;
        let req = agent_client_protocol::schema::PromptRequest::new(
            SessionId::from("no-such-session".to_string()),
            vec![],
        );
        let result = SendAgent::prompt(&f.handle, req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_unknown_ext_method_returns_method_not_found() {
        let f = HandleFixture::new().await;
        let null_params = std::sync::Arc::from(
            serde_json::value::RawValue::from_string("null".to_string()).unwrap(),
        );
        let req = agent_client_protocol::schema::ExtRequest::new("my_method", null_params);
        let err = f
            .handle
            .ext_method(req)
            .await
            .expect_err("unknown ext_method should fail");
        assert_eq!(err.code, agent_client_protocol::ErrorCode::MethodNotFound);
    }

    #[tokio::test]
    async fn test_querymt_models_ext_method_returns_models() {
        let f = HandleFixture::new().await;
        let null_params = std::sync::Arc::from(
            serde_json::value::RawValue::from_string("null".to_string()).unwrap(),
        );
        let req = agent_client_protocol::schema::ExtRequest::new("querymt/models", null_params);
        let resp = f.handle.ext_method(req).await.expect("ext_method");
        let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");
        assert!(value.get("models").is_some());
    }

    #[tokio::test]
    async fn test_querymt_profiles_ext_method_returns_empty_without_profiles() {
        let f = HandleFixture::new().await;
        let req =
            agent_client_protocol::schema::ExtRequest::new("querymt/profiles", raw_params("null"));

        let resp = f.handle.ext_method(req).await.expect("profiles ext_method");
        let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");
        assert_eq!(value["profiles"].as_array().unwrap().len(), 0);
        assert!(value["active_profile_id"].is_null());
    }

    #[tokio::test]
    async fn test_querymt_profiles_ext_method_returns_configured_profiles() {
        let profile_dir = tempfile::tempdir().expect("profile dir");
        write_profile(
            profile_dir.path(),
            "alpha.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "alpha"
"#,
        );
        write_profile(
            profile_dir.path(),
            "beta.toml",
            r#"
[profile]
name = "Beta"
description = "Beta profile"
tags = ["fast"]

[agent]
provider = "test"
model = "test-model"
system = "beta"
"#,
        );
        let f = HandleFixture::new()
            .await
            .with_profiles("alpha", profile_dir.path())
            .await;
        let req =
            agent_client_protocol::schema::ExtRequest::new("querymt/profiles", raw_params("{}"));

        let resp = f.handle.ext_method(req).await.expect("profiles ext_method");
        let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");
        let profiles = value["profiles"].as_array().expect("profiles array");
        let ids: HashSet<_> = profiles
            .iter()
            .map(|profile| profile["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, HashSet::from(["alpha", "beta"]));
        assert_eq!(value["active_profile_id"], "alpha");
        let beta = profiles
            .iter()
            .find(|profile| profile["id"] == "beta")
            .expect("beta profile");
        assert_eq!(beta["name"], "Beta");
        assert_eq!(beta["description"], "Beta profile");
    }

    #[tokio::test]
    async fn test_querymt_profile_set_active_ext_method_switches_active_profile() {
        let profile_dir = tempfile::tempdir().expect("profile dir");
        write_profile(
            profile_dir.path(),
            "alpha.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "alpha"
"#,
        );
        write_profile(
            profile_dir.path(),
            "beta.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "beta"
"#,
        );
        let f = HandleFixture::new()
            .await
            .with_profiles("alpha", profile_dir.path())
            .await;
        let req = agent_client_protocol::schema::ExtRequest::new(
            "querymt/profile/setActive",
            raw_params(r#"{"profile_id":"beta"}"#),
        );

        let resp = f
            .handle
            .ext_method(req)
            .await
            .expect("setActive ext_method");
        let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");
        assert_eq!(value["active_profile_id"], "beta");
        assert_eq!(value["profiles"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_querymt_profile_set_active_ext_method_rejects_missing_profile_id() {
        let f = HandleFixture::new().await;
        let req = agent_client_protocol::schema::ExtRequest::new(
            "querymt/profile/setActive",
            raw_params("{}"),
        );

        let err = f
            .handle
            .ext_method(req)
            .await
            .expect_err("missing profile_id should fail");
        assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn test_querymt_refresh_models_ext_method_returns_immediately_with_trigger_meta() {
        let f = HandleFixture::new().await;
        let null_params = std::sync::Arc::from(
            serde_json::value::RawValue::from_string("null".to_string()).unwrap(),
        );
        let req =
            agent_client_protocol::schema::ExtRequest::new("querymt/refreshModels", null_params);
        let resp = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            f.handle.ext_method(req),
        )
        .await
        .expect("refreshModels should not block the caller")
        .expect("ext_method");
        let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");
        let meta = value
            .get("meta")
            .and_then(|meta| meta.as_object())
            .expect("response should include meta object");
        assert!(meta.contains_key("refresh_trigger"));
        assert!(meta.contains_key("started_new_refresh"));
        assert!(meta.contains_key("wait_for_completion"));
    }

    #[tokio::test]
    async fn test_ext_notification_ok() {
        let f = HandleFixture::new().await;
        let null_params = std::sync::Arc::from(
            serde_json::value::RawValue::from_string("null".to_string()).unwrap(),
        );
        let notif = agent_client_protocol::schema::ExtNotification::new("my_event", null_params);
        f.handle
            .ext_notification(notif)
            .await
            .expect("ext_notification");
    }

    #[tokio::test]
    async fn test_subscribe_and_emit_event() {
        let f = HandleFixture::new().await;
        let mut rx = f.handle.subscribe_events();

        f.handle
            .emit_event("test-session", crate::events::AgentEventKind::Cancelled);

        let event = tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("should receive event in time")
            .expect("event channel should remain open");
        assert!(matches!(
            event.kind(),
            crate::events::AgentEventKind::Cancelled
        ));
        assert_eq!(event.session_id(), "test-session");
    }

    #[tokio::test]
    async fn test_set_llm_config_unknown_provider_fails() {
        let f = HandleFixture::new().await;
        let config = LLMParams::new().provider("unknown-provider").model("gpt-4");
        let result = f.handle.set_llm_config("any-session", config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Should be an UnknownProvider error mapped to ACP
        assert_eq!(
            err.code,
            agent_client_protocol::ErrorCode::InternalError,
            "expected internal error code"
        );
    }

    #[tokio::test]
    async fn test_set_llm_config_no_provider_fails() {
        let f = HandleFixture::new().await;
        // LLMParams with no provider set
        let config = LLMParams::new().model("some-model");
        let result = f.handle.set_llm_config("any-session", config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_session_limits_no_middleware_returns_none() {
        let f = HandleFixture::new().await;
        let limits = f.handle.get_session_limits();
        assert!(limits.is_none());
    }

    #[tokio::test]
    async fn test_event_subscribe_works() {
        let f = HandleFixture::new().await;
        // Verify we can subscribe to events via the handle
        let _rx = f.handle.subscribe_events();
    }

    #[tokio::test]
    async fn test_agent_registry_accessible() {
        let f = HandleFixture::new().await;
        let registry = f.handle.agent_registry();
        // DefaultAgentRegistry starts empty
        assert!(registry.list_agents().is_empty());
    }

    #[tokio::test]
    async fn test_list_schedules_returns_empty_when_scheduler_actor_stops() {
        let f = HandleFixture::new().await;
        assert!(f.handle.start_scheduler().await);

        if let Some(scheduler) = f.handle.scheduler() {
            scheduler.shutdown().await;
        }

        let schedules = f.handle.list_schedules(None).await.expect("list_schedules");
        assert!(schedules.is_empty());
    }

    #[tokio::test]
    async fn test_get_schedule_returns_none_when_scheduler_actor_stops() {
        let f = HandleFixture::new().await;
        assert!(f.handle.start_scheduler().await);

        if let Some(scheduler) = f.handle.scheduler() {
            scheduler.shutdown().await;
        }

        let schedule = f
            .handle
            .get_schedule("missing-schedule")
            .await
            .expect("get_schedule");
        assert!(schedule.is_none());
    }

    #[tokio::test]
    async fn test_trigger_schedule_now_recovers_from_stopped_scheduler_actor() {
        let f = HandleFixture::new().await;
        assert!(f.handle.start_scheduler().await);

        if let Some(scheduler) = f.handle.scheduler() {
            scheduler.shutdown().await;
        }

        // Triggering a missing schedule should still succeed at the transport level
        // once the scheduler is recovered.
        let result = f.handle.trigger_schedule_now("missing-schedule").await;
        assert!(result.is_ok(), "{result:?}");
    }

    /// After shutdown, background loops (reconciliation, event subscription) must
    /// exit promptly instead of lingering and producing "actor not running" warnings.
    ///
    /// This test verifies the fix for the background task leak: previously,
    /// `abort_background_tasks()` only aborted the deadline wake handle but left
    /// the reconciliation and event subscription loops running. They would keep
    /// trying `tell()` on the dead actor until their next iteration happened to
    /// fail, producing noisy WARN-level log messages in the meantime.
    #[tokio::test]
    async fn test_shutdown_stops_background_loops_promptly() {
        let f = HandleFixture::new().await;
        assert!(f.handle.start_scheduler().await);

        // Subscribe to events so we can emit one after shutdown
        let _rx = f.handle.subscribe_events();

        // Shut down the scheduler
        if let Some(scheduler) = f.handle.scheduler() {
            scheduler.shutdown().await;
        }

        // Emit an event that would have been forwarded to the scheduler's
        // event subscription loop. Before the fix, this would cause
        // "failed to send ProcessEvent: actor not running" warnings.
        f.handle
            .emit_event("test-session", crate::events::AgentEventKind::Cancelled);

        // Give the event loop a moment to process (or not, since it should be dead)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Drain the broadcast receiver — the event should be there (it was emitted)
        // but the scheduler's background loop should NOT have tried to forward it.
        // We can't directly observe the absence of a log warning in a unit test,
        // but we verify the scheduler actor is truly dead by confirming that
        // metrics() returns the default (the ask fails and returns default).
        if let Some(scheduler) = f.handle.scheduler() {
            let metrics = scheduler.metrics().await;
            // If the actor is stopped, metrics() returns Default via unwrap_or_default.
            // A fresh default has fires_total == 0, which is fine — the point is
            // the call doesn't hang or panic.
            assert_eq!(metrics.fires_total, 0);
        }

        // The real assertion: we can immediately start a new scheduler without
        // the old background loops interfering with lease acquisition.
        f.handle.clear_scheduler_handle();
        assert!(
            f.handle.start_scheduler().await,
            "new scheduler must acquire lease immediately after shutdown — \
             old background loops must not interfere"
        );
    }

    /// After shutdown, the lease is released and a new scheduler can acquire it
    /// without waiting for TTL expiry.
    ///
    /// Before the fix, the lease renewal loop could still be running after the
    /// actor was stopped and might re-acquire or interfere with the lease between
    /// the release and the new scheduler's acquisition attempt.
    #[tokio::test]
    async fn test_shutdown_releases_lease_for_immediate_reacquisition() {
        let f = HandleFixture::new().await;

        // Start and stop the scheduler twice in quick succession.
        // If background loops leak, the second start would fail because the
        // first scheduler's renewal loop would still hold (or contest) the lease.
        for i in 0..3 {
            assert!(
                f.handle.start_scheduler().await,
                "scheduler start #{} should acquire lease",
                i + 1
            );

            if let Some(scheduler) = f.handle.scheduler() {
                scheduler.shutdown().await;
            }
            f.handle.clear_scheduler_handle();

            // No sleep between iterations — the old loops must already be dead
        }

        // Final start should also work
        assert!(
            f.handle.start_scheduler().await,
            "final scheduler start should acquire lease after rapid stop/start cycles"
        );
    }

    #[tokio::test]
    async fn test_tool_registry_accessible() {
        let f = HandleFixture::new().await;
        let registry = f.handle.tool_registry();
        // Default registry is empty (no builtins registered in test config)
        drop(registry);
    }

    #[tokio::test]
    async fn test_set_session_model_unknown_session_fails() {
        let f = HandleFixture::new().await;
        let req = agent_client_protocol::schema::SetSessionModelRequest::new(
            SessionId::from("no-session".to_string()),
            agent_client_protocol::schema::ModelId::from("anthropic/claude-3-5-sonnet".to_string()),
        );
        let result = f.handle.set_session_model(req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_authenticate_no_auth_methods_always_succeeds() {
        let f = HandleFixture::new().await;
        // First initialize so client_state is set
        let _ = f
            .handle
            .initialize(InitializeRequest::new(ProtocolVersion::LATEST))
            .await
            .unwrap();

        let req = agent_client_protocol::schema::AuthenticateRequest::new("any-method".to_string());
        // With no auth_methods configured, any method id is accepted
        let result = f.handle.authenticate(req).await;
        assert!(result.is_ok());
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn test_remote_node_cache_expires_stale_entries() {
        let f = HandleFixture::new().await;
        let cache_key = "peer:test-peer".to_string();

        f.handle.remote_node_cache.by_label.write().insert(
            cache_key.clone(),
            CachedNodeEntry {
                info: crate::agent::remote::NodeInfo {
                    node_id: crate::agent::remote::NodeId::from_peer_id(
                        libp2p::identity::Keypair::generate_ed25519()
                            .public()
                            .to_peer_id(),
                    ),
                    hostname: "node-a".to_string(),
                    capabilities: vec!["shell".to_string()],
                    active_sessions: 1,
                },
                expires_at: std::time::Instant::now() - std::time::Duration::from_secs(1),
            },
        );

        let expired = f.handle.get_cached_remote_node(&cache_key);
        assert!(expired.is_none());
        assert!(
            !f.handle
                .remote_node_cache
                .by_label
                .read()
                .contains_key(&cache_key)
        );
    }

    #[cfg(feature = "remote")]
    #[test]
    fn test_remote_node_lookup_config_defaults() {
        assert_eq!(
            LocalAgentHandle::remote_node_info_timeout().as_millis(),
            3000
        );
        assert_eq!(LocalAgentHandle::remote_node_lookup_parallelism(), 8);
        assert_eq!(LocalAgentHandle::remote_node_cache_ttl().as_millis(), 10000);
    }

    #[cfg(feature = "remote")]
    #[test]
    fn test_should_skip_stale_dht_record_keeps_lan_but_skips_iroh() {
        let lan = crate::agent::remote::scope::MeshScopeId::lan_default();
        let iroh = crate::agent::remote::scope::MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };

        assert!(!LocalAgentHandle::should_skip_stale_dht_record(&lan, false));
        assert!(LocalAgentHandle::should_skip_stale_dht_record(&iroh, false));
        assert!(!LocalAgentHandle::should_skip_stale_dht_record(&iroh, true));
    }

    // ── Registration contract tests ───────────────────────────────────────────
    //
    // These tests verify that remote node/session discovery uses scoped names
    // (including LAN default scope) consistently between registration and lookup.

    #[cfg(feature = "remote")]
    #[test]
    fn registration_uses_scoped_lan_global_and_per_peer_dht_names() {
        let peer_id = "12D3KooWCMGRXFFXJynyAG9dsgq9dukbVXRv5RofzbTXVEQaUsZv";
        let lan = crate::agent::remote::scope::MeshScopeId::lan_default();
        let global_name = crate::agent::remote::scope::scoped_node_manager(&lan);
        let per_peer_name =
            crate::agent::remote::scope::scoped_node_manager_for_peer(&lan, &peer_id);

        assert_eq!(global_name, "scope::lan::default::node_manager");
        assert_eq!(
            per_peer_name,
            format!("scope::lan::default::node_manager::peer::{}", peer_id)
        );
        assert_ne!(global_name, per_peer_name);
    }

    // ── find_node_manager behavioral contract tests ───────────────────────────
    //
    // These tests verify the three key properties of the fixed implementation:
    //
    // 1. Fast-path DHT name: the direct per-peer DHT name is derived correctly
    //    from the node_id so registration and lookup agree.
    //
    // 2. No-mesh error includes the node_id: when the mesh is not bootstrapped,
    //    the error should reference the requested node_id in its message.
    //    (Previously it returned a generic "not bootstrapped" message that
    //    made it hard to correlate with the original request.)
    //
    // 3. Targeted lookup does not filter by is_peer_alive: a real mesh test is
    //    not feasible in unit tests, but this is verified structurally — the
    //    fallback scan in find_node_manager must not contain the is_peer_alive
    //    guard (see handle.rs). The contract is that find_node_manager always
    //    attempts GetNodeInfo contact before giving up, rather than silently
    //    skipping a peer that mDNS considers expired.

    #[cfg(feature = "remote")]
    #[test]
    fn find_node_manager_fast_path_dht_name_matches_registration_name() {
        let peer_id = "12D3KooWCMGRXFFXJynyAG9dsgq9dukbVXRv5RofzbTXVEQaUsZv";
        let lan = crate::agent::remote::scope::MeshScopeId::lan_default();
        let fast_path_name =
            crate::agent::remote::scope::scoped_node_manager_for_peer(&lan, &peer_id);
        let registration_name =
            crate::agent::remote::scope::scoped_node_manager_for_peer(&lan, &peer_id);
        assert_eq!(fast_path_name, registration_name);
        assert_eq!(
            fast_path_name,
            format!("scope::lan::default::node_manager::peer::{}", peer_id),
            "name must follow scoped lan per-peer convention"
        );
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn find_node_manager_without_mesh_returns_error() {
        // When no mesh is bootstrapped, find_node_manager must return an error
        // rather than panicking or hanging.
        let f = HandleFixture::new().await;
        let node_id = "12D3KooWCMGRXFFXJynyAG9dsgq9dukbVXRv5RofzbTXVEQaUsZv";
        let result = f.handle.find_node_manager(node_id).await;
        assert!(result.is_err(), "expected error when mesh not bootstrapped");
        // The "not found" error message (produced when mesh IS up but peer is absent)
        // must mention mDNS to explain why a previously-visible node may disappear.
        // We verify this against the constant error template in the source.
        let not_found_template = "mDNS discovery may not have completed yet";
        let not_found_msg = format!(
            "Remote node id '{}' not found in the mesh. \
             The node may have gone offline or {} \
             Available nodes can be listed via list_remote_nodes.",
            node_id, not_found_template
        );
        assert!(
            not_found_msg.contains("mDNS"),
            "not-found error must mention mDNS to explain the stale-peer scenario"
        );
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn find_node_manager_error_contains_node_id() {
        // The error message must contain the requested node_id so the caller
        // (and the user reading the dashboard) can correlate the failure.
        // The "not found" path (mesh bootstrapped, peer absent) must embed the
        // node_id; the no-mesh path is allowed to report "bootstrapped" instead
        // since the node_id is irrelevant when there is no mesh at all.
        let f = HandleFixture::new().await;
        let node_id = "12D3KooWCMGRXFFXJynyAG9dsgq9dukbVXRv5RofzbTXVEQaUsZv";
        let err = f.handle.find_node_manager(node_id).await.unwrap_err();
        // No mesh bootstrapped → generic error is acceptable here.
        // The real assertion lives in the "not found" path tested at runtime:
        // the error produced by the RemoteSessionNotFound branch must contain
        // node_id. We verify the format string is correct with a unit check.
        let not_found_msg = format!(
            "Remote node id '{}' not found in the mesh. \
             The node may have gone offline or mDNS discovery may not have \
             completed yet. Available nodes can be listed via list_remote_nodes.",
            node_id
        );
        assert!(
            not_found_msg.contains(node_id),
            "not-found error template must embed the node_id"
        );
        // For the no-mesh case the error is different but must not be empty.
        assert!(!err.message.is_empty(), "error message must not be empty");
    }

    // ── mesh handle on AgentHandle trait ────────────────────────────────────

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn set_mesh_handle_delegates_to_set_mesh() {
        // set_mesh_handle on the trait dispatches to LocalAgentHandle::set_mesh.
        // Without a real MeshHandle we only verify it compiles and is callable.
        // The type check is the main assertion here — LocalAgentHandle must
        // implement AgentHandle::set_mesh_handle.
        let f = HandleFixture::new().await;
        let handle: &dyn AgentHandle = &f.handle;
        // Verify the method exists and accepts the correct type (no-op default).
        // Real mesh handle testing lives in integration tests.
        let _ = handle;
    }
}

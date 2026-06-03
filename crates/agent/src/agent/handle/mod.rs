//! AgentHandle trait and LocalAgentHandle concrete implementation.
//!
//! `AgentHandle` is the trait that all agent handles implement — both local
//! and remote. `LocalAgentHandle` is the concrete implementation for local
//! agents, bundling shared config, the kameo session registry, and
//! connection-level mutable state.

mod agent_handle_impl;
mod core;
mod ext;
mod ext_auth;
mod ext_mesh;
mod ext_models;
mod ext_plugins;
mod ext_remote;
mod ext_schedules;
mod model_registry;
mod remote_mesh;
mod remote_nodes;
mod remote_sessions;
mod scheduler;
mod send_agent_config;
mod send_agent_impl;
mod send_agent_lifecycle;
mod session_config;
mod session_control;
mod utils;

#[cfg(test)]
mod tests;

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
    ResumeSessionRequest, ResumeSessionResponse, SessionConfigOption, SessionInfo,
    SetSessionModelRequest, SetSessionModelResponse,
};
use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use kameo::actor::ActorRef;
use parking_lot::Mutex as ParkingMutex;
use querymt::LLMParams;
use querymt::chat::ReasoningEffort;
use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
#[cfg(feature = "remote")]
use tokio::sync::OnceCell;
#[cfg(feature = "remote")]
use tokio::sync::Semaphore;
use tokio::sync::{Mutex, broadcast};

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
enum CachedNodeEntry {
    Ready {
        info: crate::agent::remote::NodeInfo,
        expires_at: std::time::Instant,
    },
    Unreachable {
        expires_at: std::time::Instant,
    },
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

    /// Handle to the active mesh runtime, set after remote mesh bootstrap succeeds.
    /// `None` in local-only mode. Wrapped in a `Mutex` for interior mutability
    /// so startup code can set it on the shared `Arc<LocalAgentHandle>`.
    #[cfg(feature = "remote")]
    pub mesh: StdMutex<Option<crate::agent::remote::MeshHandle>>,

    /// Keepalive refs for mesh-visible local actors so new scopes can re-register
    /// the existing node manager/provider host without respawning them.
    #[cfg(feature = "remote")]
    pub local_mesh_actor_refs: OnceCell<crate::agent::remote::LocalMeshActorRefs>,

    #[cfg(feature = "remote")]
    local_mesh_node_name: StdMutex<Option<String>>,

    #[cfg(feature = "remote")]
    published_mesh_scopes: StdMutex<std::collections::HashSet<crate::agent::remote::MeshScopeId>>,

    #[cfg(feature = "remote")]
    remote_node_cache: Arc<RemoteNodeMetadataCache>,

    /// Non-blocking model inventory with snapshot-based reads and background refresh.
    /// This is the canonical public API for model listing and cache management.
    pub model_inventory: crate::model_inventory::ModelInventory,

    /// Shared OAuth service for all auth operations across UI and ACP transports.
    pub oauth_service: crate::auth::service::OAuthService,

    /// Optional profile runtime manager shared by UI and ACP extension transports.
    pub profiles: ProfileRuntimeSlot,

    /// Handle to the scheduler actor, set after `start_scheduler()` succeeds.
    /// `None` if scheduling is not enabled or lease was not acquired.
    pub(crate) scheduler_handle: SchedulerHandleSlot,

    /// Guard to ensure `shutdown()` only runs its body once.
    shutdown_done: AtomicBool,
}

type SharedProfileCatalog = Arc<dyn ProfileCatalog>;
type ProfileRuntime = Arc<ProfileRuntimeManager<SharedProfileCatalog>>;
type ProfileRuntimeSlot = ArcSwap<Option<ProfileRuntime>>;
pub(crate) type SchedulerHandleSlot = Arc<ParkingMutex<Option<crate::scheduler::SchedulerHandle>>>;

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

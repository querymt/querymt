//! AgentHandle trait and LocalAgentHandle concrete implementation.
//!
//! `AgentHandle` is the trait that all agent handles implement — both local
//! and remote. `LocalAgentHandle` is the concrete implementation for local
//! agents, bundling shared config, the kameo session registry, and
//! connection-level mutable state.

mod agent_handle;
pub(crate) mod agent_handle_impl;
pub(crate) mod core;
pub(crate) mod ext;
pub(crate) mod ext_auth;
pub(crate) mod ext_mesh;
pub(crate) mod ext_models;
pub(crate) mod ext_plugins;
pub(crate) mod ext_profiles;
pub(crate) mod ext_remote;
pub(crate) mod ext_schedules;
pub(crate) mod ext_session_history;
pub(crate) mod ext_session_ops;
pub(crate) mod ext_worktree_ops;
pub(crate) mod model_registry;
#[cfg(feature = "remote")]
pub(crate) mod remote_connect;
pub(crate) mod remote_mesh;
pub(crate) mod remote_nodes;
pub(crate) mod remote_sessions;
#[doc(hidden)]
pub mod runtime_observation;
pub(crate) mod scheduler;
pub(crate) mod send_agent_config;
pub(crate) mod send_agent_impl;
pub(crate) mod send_agent_lifecycle;
pub(crate) mod session_config;
pub(crate) mod session_control;
pub(crate) mod session_lifecycle;
#[cfg(feature = "remote")]
pub(crate) mod session_operation;
pub(crate) mod utils;

use crate::acp::client_bridge::ClientBridgeSender;
use crate::acp::protocol::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, DeleteSessionRequest, DeleteSessionResponse, Error, ExtNotification,
    ExtRequest, ExtResponse, ForkSessionRequest, ForkSessionResponse, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionConfigOption, SetSessionModelRequest,
    SetSessionModelResponse,
};
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, ClientState};
pub use crate::agent::handle::agent_handle::AgentHandle;
use crate::agent::remote::SessionActorRef;
use crate::agent::session_materializer::{PreparedSessionResult, SessionMaterializer};
use crate::agent::session_registry::SessionRegistry;
use crate::delegation::AgentRegistry;
use crate::middleware::CompositeDriver;
use crate::profile_host::ProfileRuntimeHost;
use crate::send_agent::SendAgent;
use crate::session::store::LLMConfig;
use crate::tools::ToolRegistry;
use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use kameo::actor::ActorRef;
use parking_lot::Mutex as ParkingMutex;
use querymt::LLMParams;
use querymt::chat::ReasoningEffort;
use querymt_agent_core::events::{AgentEventKind, EventEnvelope};
use querymt_agent_index::WorkspaceIndexManagerActor;
use querymt_agent_services::event_fanout::EventFanout;
use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
#[cfg(feature = "remote")]
use tokio::sync::OnceCell;
#[cfg(feature = "remote")]
use tokio::sync::Semaphore;
use tokio::sync::{Mutex, broadcast};

/// Lightweight facade replacing `Arc<QueryMTAgent>` for all consumers.
///
/// Holds shared config, the kameo session registry, and connection-level
/// mutable state. Not an actor — just a convenient bundle.
#[cfg(feature = "remote")]
#[derive(Clone, Debug)]
#[doc(hidden)]
pub enum CachedNodeEntry {
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
#[doc(hidden)]
pub struct RemoteNodeMetadataCache {
    pub by_label: parking_lot::RwLock<std::collections::HashMap<String, CachedNodeEntry>>,
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
    ext_notifications: broadcast::Sender<ExtNotification>,

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
    pub remote_node_cache: Arc<RemoteNodeMetadataCache>,

    /// Per-session single-flight gates for the remote connection coordinator
    /// (`remote_connect`). Kept independent of the registry mutex so recovery
    /// never holds registry state across network work (plan §2).
    #[cfg(feature = "remote")]
    pub(crate) remote_connect_gates: remote_connect::RemoteConnectGateMap,
    #[cfg(feature = "remote")]
    pub(crate) remote_connect_cooldowns: remote_connect::RemoteConnectCooldownMap,

    /// Non-blocking model inventory with snapshot-based reads and background refresh.
    /// This is the canonical public API for model listing and cache management.
    pub model_inventory: crate::model_inventory::ModelInventory,

    /// Shared OAuth service for all auth operations across UI and ACP transports.
    pub oauth_service: crate::auth::service::OAuthService,

    /// Optional profile runtime manager shared by UI and ACP extension transports.
    pub profiles: ProfileRuntimeSlot,

    /// Handle to the scheduler actor, set after `start_scheduler()` succeeds.
    /// `None` if scheduling is not enabled or lease was not acquired.
    pub scheduler_handle: SchedulerHandleSlot,

    /// The delegation service this handle owns when it is a runtime's root:
    /// the orchestrator and its listener task. Stopped by `shutdown()`.
    delegation_service: ParkingMutex<Option<DelegationService>>,

    /// Guard to ensure `shutdown()` only runs its body once.
    shutdown_done: AtomicBool,
}

/// A running delegation service: the orchestrator and the task that feeds it.
struct DelegationService {
    orchestrator: Arc<crate::delegation::DelegationOrchestrator>,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for DelegationService {
    fn drop(&mut self) {
        // A handle dropped without `shutdown()` must not leave the listener running.
        self.listener.abort();
    }
}

#[doc(hidden)]
pub type ProfileRuntime = Arc<dyn ProfileRuntimeHost>;
pub type ProfileRuntimeSlot = Arc<ArcSwap<Option<ProfileRuntime>>>;
pub(crate) type SchedulerHandleSlot = Arc<ParkingMutex<Option<crate::scheduler::SchedulerHandle>>>;

// ── Remote node lookup type aliases ─────────────────────────────────────────
// These aliases improve readability of the complex async types used for
// concurrent remote node discovery operations.

#[cfg(feature = "remote")]
type RemoteNodeInfoResult =
    Result<crate::agent::remote::NodeInfo, kameo::error::RemoteSendError<crate::error::AgentError>>;

#[cfg(feature = "remote")]
type RemoteNodeLookupResult = (String, Option<libp2p::PeerId>, RemoteNodeInfoResult);

#[cfg(feature = "remote")]
type RemoteNodeLookupFuture = futures_util::future::BoxFuture<'static, RemoteNodeLookupResult>;

#[cfg(feature = "remote")]
type RemoteNodeLookupQueue = futures_util::stream::FuturesUnordered<RemoteNodeLookupFuture>;

//! Global mutable state for FFI agent/session handles.
//!
//! `HANDLE_STATE` is a global `Mutex<HashMap<u64, AgentRecord>>` indexed by
//! opaque agent handle. Each `AgentRecord` holds the `Agent` instance, storage,
//! mesh handle, session submaps, event callbacks, and MCP registrations.

use crate::events::EventCallbacks;
use crate::ffi_helpers::{ActiveCallTracker, new_agent_handle, new_session_handle};
use crate::types::{AgentHandle, FfiErrorCode, SessionHandle};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// Global handle state.
static HANDLE_STATE: once_cell::sync::Lazy<Mutex<HandleMap>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HandleMap::new()));

/// The inner handle map.
struct HandleMap {
    agents: HashMap<AgentHandle, AgentRecord>,
    runtime: Option<RuntimeRecord>,
}

impl HandleMap {
    fn new() -> Self {
        Self {
            agents: HashMap::new(),
            runtime: None,
        }
    }
}

struct RuntimeRecord {
    agent: Arc<querymt_agent::api::Agent>,
    storage: Arc<dyn querymt_agent::session::backend::StorageBackend>,
    attached_agents: usize,
}

/// All state tracked per-agent handle.
pub struct AgentRecord {
    /// Shared process-wide QueryMT Agent runtime.
    pub agent: Arc<querymt_agent::api::Agent>,

    /// View store shared from the process runtime storage.
    pub view_store: Option<Arc<dyn querymt_agent::session::projection::ViewStore>>,

    /// Diagnostic: the listen/discovery config used at bootstrap.
    pub mesh_listen: Option<String>,
    pub mesh_discovery: Option<String>,

    /// Track active FFI calls for this agent.
    pub call_tracker: Arc<ActiveCallTracker>,

    /// Session handle → session record.
    pub sessions: HashMap<SessionHandle, SessionRecord>,

    /// Registered event callback.
    pub event_callbacks: Option<EventCallbacks>,

    /// Whether the agent was fully shut down.
    pub shutdown: bool,
}

/// Per-session state.
#[derive(Clone)]
pub struct SessionRecord {
    pub session_id: String,
    pub is_remote: bool,
    pub node_id: Option<String>,
    /// For remote sessions, the actor_id from the remote node.
    pub remote_actor_id: Option<u64>,
}

// ─── Public API ─────────────────────────────────────────────────────────────

/// Insert a new agent record, returning its opaque handle.
pub fn attach_or_insert_runtime_agent(
    agent: querymt_agent::api::Agent,
    storage: Arc<dyn querymt_agent::session::backend::StorageBackend>,
) -> (AgentHandle, bool) {
    let handle = new_agent_handle();
    let mut state = HANDLE_STATE.lock();

    if state.runtime.is_none() {
        state.runtime = Some(RuntimeRecord {
            agent: Arc::new(agent),
            storage,
            attached_agents: 0,
        });
    }

    let (agent_arc, view_store, reused) = {
        let runtime = state.runtime.as_mut().expect("runtime initialized above");
        runtime.attached_agents = runtime.attached_agents.saturating_add(1);
        (
            Arc::clone(&runtime.agent),
            runtime.storage.view_store(),
            runtime.attached_agents > 1,
        )
    };

    state.agents.insert(
        handle,
        AgentRecord {
            agent: agent_arc,
            view_store,
            mesh_listen: None,
            mesh_discovery: None,
            call_tracker: Arc::new(ActiveCallTracker::new()),
            sessions: HashMap::new(),
            event_callbacks: None,
            shutdown: false,
        },
    );
    (handle, reused)
}

pub fn attach_existing_runtime_agent() -> Option<AgentHandle> {
    let handle = new_agent_handle();
    let mut state = HANDLE_STATE.lock();
    let (agent_arc, view_store) = {
        let runtime = state.runtime.as_mut()?;
        runtime.attached_agents = runtime.attached_agents.saturating_add(1);
        (Arc::clone(&runtime.agent), runtime.storage.view_store())
    };

    state.agents.insert(
        handle,
        AgentRecord {
            agent: agent_arc,
            view_store,
            mesh_listen: None,
            mesh_discovery: None,
            call_tracker: Arc::new(ActiveCallTracker::new()),
            sessions: HashMap::new(),
            event_callbacks: None,
            shutdown: false,
        },
    );
    Some(handle)
}

pub fn runtime_attached_agents() -> usize {
    let state = HANDLE_STATE.lock();
    state
        .runtime
        .as_ref()
        .map_or(0, |runtime| runtime.attached_agents)
}

pub fn shutdown_runtime_if_idle() -> Result<bool, FfiErrorCode> {
    let mut state = HANDLE_STATE.lock();
    let Some(runtime) = state.runtime.as_ref() else {
        return Ok(false);
    };

    if runtime.attached_agents > 0 || !state.agents.is_empty() {
        return Err(FfiErrorCode::Busy);
    }

    state.runtime = None;
    Ok(true)
}

/// Look up an agent record by handle. Returns `None` if not found or shut down.
pub fn find_agent(handle: AgentHandle) -> Option<AgentRecordGuard> {
    let state = HANDLE_STATE.lock();
    if let Some(record) = state.agents.get(&handle) {
        if record.shutdown {
            return None;
        }
        drop(state);
        Some(AgentRecordGuard { handle })
    } else {
        None
    }
}

/// Perform an operation on an agent record, holding the lock for the duration.
pub fn with_agent<F, R>(handle: AgentHandle, f: F) -> Result<R, FfiErrorCode>
where
    F: FnOnce(&mut AgentRecord) -> Result<R, FfiErrorCode>,
{
    let mut state = HANDLE_STATE.lock();
    let record = state
        .agents
        .get_mut(&handle)
        .ok_or(FfiErrorCode::NotFound)?;
    if record.shutdown {
        return Err(FfiErrorCode::NotFound);
    }
    f(record)
}

/// Perform a read-only operation on an agent record.
pub fn with_agent_read<F, R>(handle: AgentHandle, f: F) -> Result<R, FfiErrorCode>
where
    F: FnOnce(&AgentRecord) -> Result<R, FfiErrorCode>,
{
    let state = HANDLE_STATE.lock();
    let record = state.agents.get(&handle).ok_or(FfiErrorCode::NotFound)?;
    if record.shutdown {
        return Err(FfiErrorCode::NotFound);
    }
    f(record)
}

/// Remove and return an agent record by handle. Used during shutdown.
pub fn remove_agent(handle: AgentHandle) -> Result<AgentRecord, FfiErrorCode> {
    let mut state = HANDLE_STATE.lock();
    let mut record = state.agents.remove(&handle).ok_or(FfiErrorCode::NotFound)?;
    record.shutdown = true;

    if let Some(runtime) = state.runtime.as_mut() {
        runtime.attached_agents = runtime.attached_agents.saturating_sub(1);
    }

    Ok(record)
}

/// Allocate a session handle and register it under an agent.
pub fn register_session(
    agent_handle: AgentHandle,
    session_id: String,
    is_remote: bool,
    node_id: Option<String>,
    remote_actor_id: Option<u64>,
) -> Result<SessionHandle, FfiErrorCode> {
    let s_handle = new_session_handle();
    let mut state = HANDLE_STATE.lock();
    let record = state
        .agents
        .get_mut(&agent_handle)
        .ok_or(FfiErrorCode::NotFound)?;
    if record.shutdown {
        return Err(FfiErrorCode::NotFound);
    }
    record.sessions.insert(
        s_handle,
        SessionRecord {
            session_id,
            is_remote,
            node_id,
            remote_actor_id,
        },
    );
    Ok(s_handle)
}

/// Look up a session record.
pub fn with_session<F, R>(
    agent_handle: AgentHandle,
    session_handle: SessionHandle,
    f: F,
) -> Result<R, FfiErrorCode>
where
    F: FnOnce(&SessionRecord) -> Result<R, FfiErrorCode>,
{
    let state = HANDLE_STATE.lock();
    let record = state
        .agents
        .get(&agent_handle)
        .ok_or(FfiErrorCode::NotFound)?;
    if record.shutdown {
        return Err(FfiErrorCode::NotFound);
    }
    let session = record
        .sessions
        .get(&session_handle)
        .ok_or(FfiErrorCode::NotFound)?;
    f(session)
}

/// Remove a session handle, returning its record.
pub fn unregister_session(
    agent_handle: AgentHandle,
    session_handle: SessionHandle,
) -> Result<SessionRecord, FfiErrorCode> {
    let mut state = HANDLE_STATE.lock();
    let record = state
        .agents
        .get_mut(&agent_handle)
        .ok_or(FfiErrorCode::NotFound)?;
    if record.shutdown {
        return Err(FfiErrorCode::NotFound);
    }
    record
        .sessions
        .remove(&session_handle)
        .ok_or(FfiErrorCode::NotFound)
}

/// Remove all session handles for a given session_id across all agents.
pub fn unregister_sessions_by_id(session_id: &str) {
    let mut state = HANDLE_STATE.lock();
    for record in state.agents.values_mut() {
        record.sessions.retain(|_, s| s.session_id != session_id);
    }
}

/// Find a registered session handle by stable session id under one agent.
pub fn find_session_handle_by_id(
    agent_handle: AgentHandle,
    session_id: &str,
) -> Result<Option<SessionHandle>, FfiErrorCode> {
    let state = HANDLE_STATE.lock();
    let record = state
        .agents
        .get(&agent_handle)
        .ok_or(FfiErrorCode::NotFound)?;
    if record.shutdown {
        return Err(FfiErrorCode::NotFound);
    }
    Ok(record
        .sessions
        .iter()
        .find_map(|(handle, session)| (session.session_id == session_id).then_some(*handle)))
}

/// Look up session metadata by stable session id under one agent.
pub fn find_session_by_id(
    agent_handle: AgentHandle,
    session_id: &str,
) -> Result<Option<SessionRecord>, FfiErrorCode> {
    let state = HANDLE_STATE.lock();
    let record = state
        .agents
        .get(&agent_handle)
        .ok_or(FfiErrorCode::NotFound)?;
    if record.shutdown {
        return Err(FfiErrorCode::NotFound);
    }
    Ok(record
        .sessions
        .values()
        .find(|session| session.session_id == session_id)
        .cloned())
}

/// A guard that holds a reference to an agent for the duration of a blocking call.
pub struct AgentRecordGuard {
    pub handle: AgentHandle,
}

impl AgentRecordGuard {
    pub fn handle(&self) -> AgentHandle {
        self.handle
    }
}

// State integration tests require async agent construction and mock storage,
// so they live in a separate file or integration test crate. Unit tests for
// the lower-level helpers (FFI error codes, handle generation, background
// state, active call tracking) are in ffi_helpers.rs and types.rs.

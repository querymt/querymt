//! # QueryMT Mobile FFI
//!
//! Stable C ABI for use by Swift (via module map) and Android (via JNI).
//! All public functions are `extern "C"` with `#[no_mangle]`.
//!
//! ## Handle Model
//!
//! All opaque handles are process-local `uint64_t` values allocated by Rust.
//! Handles are never stable across process restarts. Session IDs are stable
//! strings persisted by QueryMT storage.
//!
//! ## Error Handling
//!
//! Every function returns `int32_t` (`FfiErrorCode`). On failure, a thread-local
//! error code and message are stored. Callers can retrieve them with
//! `qmt_ffi_last_error_code()` and `qmt_ffi_last_error_message()`.
//!
//! ## Memory
//!
//! C strings returned by Rust are owned by the caller and must be freed with
//! `qmt_ffi_free_string`.

pub mod events;
pub mod ffi_helpers;
pub mod runtime;
pub mod state;
pub mod types;

mod agent;
mod mcp;
mod storage;

use async_trait::async_trait;
use ffi_helpers::{set_last_error, take_last_error_code, take_last_error_message};
use querymt_agent::acp::protocol::Error as AcpError;
use querymt_agent::session::projection::ViewStore;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex as StdMutex};
use std::thread::{self, JoinHandle, ThreadId};
use tokio::sync::Mutex;
use types::FfiErrorCode;

pub type AcpMessageHandlerFn = unsafe extern "C" fn(
    connection_handle: u64,
    message_json: *const std::ffi::c_char,
    user_data: *mut std::ffi::c_void,
);

struct AcpConnection {
    agent_handle: u64,
    adapter: Option<querymt_agent::acp::websocket::EmbeddedAcpConnection>,
    outbox: Arc<StdMutex<VecDeque<String>>>,
    callbacks: Arc<AcpCallbackWorker>,
    output_task: Option<tokio::task::JoinHandle<()>>,
}

impl AcpConnection {
    async fn close(&mut self) {
        if let Some(mut adapter) = self.adapter.take() {
            adapter.close().await;
        }
        if let Some(task) = self.output_task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

struct CallbackState {
    handler: Option<(AcpMessageHandlerFn, usize)>,
    in_flight: usize,
    closed: bool,
    worker_id: Option<ThreadId>,
}

struct CallbackControl {
    state: StdMutex<CallbackState>,
    idle: Condvar,
}

enum CallbackMessage {
    Json(String),
    Stop,
}

struct AcpCallbackWorker {
    sender: std::sync::mpsc::Sender<CallbackMessage>,
    control: Arc<CallbackControl>,
    worker: StdMutex<Option<JoinHandle<()>>>,
}

impl AcpCallbackWorker {
    fn new(
        connection_handle: u64,
        outbox: Arc<StdMutex<VecDeque<String>>>,
    ) -> std::io::Result<Arc<Self>> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let control = Arc::new(CallbackControl {
            state: StdMutex::new(CallbackState {
                handler: None,
                in_flight: 0,
                closed: false,
                worker_id: None,
            }),
            idle: Condvar::new(),
        });
        let worker_control = control.clone();
        let worker = thread::Builder::new()
            .name(format!("qmt-acp-callback-{connection_handle}"))
            .spawn(move || {
                worker_control
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .worker_id = Some(thread::current().id());
                while let Ok(message) = receiver.recv() {
                    let CallbackMessage::Json(json) = message else {
                        break;
                    };
                    let handler = {
                        let mut state = worker_control
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if state.closed {
                            break;
                        }
                        if let Some(handler) = state.handler {
                            state.in_flight += 1;
                            Some(handler)
                        } else {
                            None
                        }
                    };
                    if let Some((handler, user_data)) = handler {
                        if let Ok(c_json) = std::ffi::CString::new(json) {
                            unsafe {
                                handler(
                                    connection_handle,
                                    c_json.as_ptr(),
                                    user_data as *mut std::ffi::c_void,
                                );
                            }
                        }
                        let mut state = worker_control
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.in_flight -= 1;
                        worker_control.idle.notify_all();
                    } else {
                        outbox
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push_back(json);
                    }
                }
            })?;
        Ok(Arc::new(Self {
            sender,
            control,
            worker: StdMutex::new(Some(worker)),
        }))
    }

    fn send(&self, json: String) -> bool {
        self.sender.send(CallbackMessage::Json(json)).is_ok()
    }

    fn set_handler(&self, handler: Option<AcpMessageHandlerFn>, user_data: usize) -> bool {
        let current_thread = thread::current().id();
        let mut state = self
            .control
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return false;
        }
        state.handler = handler.map(|handler| (handler, user_data));
        if state.worker_id != Some(current_thread) {
            while state.in_flight != 0 {
                state = self
                    .control
                    .idle
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        true
    }

    fn close(&self) {
        let current_thread = thread::current().id();
        let caller_is_worker = {
            let mut state = self
                .control
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            state.handler = None;
            let is_worker = state.worker_id == Some(current_thread);
            if !is_worker {
                while state.in_flight != 0 {
                    state = self
                        .control
                        .idle
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
            is_worker
        };
        let _ = self.sender.send(CallbackMessage::Stop);
        if !caller_is_worker
            && let Some(worker) = self
                .worker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        {
            let _ = worker.join();
        }
    }
}

struct MobileAcpSessionHooks {
    view_store: Arc<dyn ViewStore>,
}

#[async_trait]
impl querymt_agent::acp::shared::AcpSessionHooks for MobileAcpSessionHooks {
    async fn on_session_loaded(
        &self,
        agent: &querymt_agent::agent::LocalAgentHandle,
        session_id: &str,
        response: &mut serde_json::Value,
    ) -> Result<(), AcpError> {
        inject_session_load_snapshot(agent, self.view_store.clone(), session_id, response).await
    }

    async fn on_remote_session_attached(
        &self,
        agent: &querymt_agent::agent::LocalAgentHandle,
        session_id: &str,
        response: &mut serde_json::Value,
    ) -> Result<(), AcpError> {
        ensure_remote_attach_snapshot(agent, self.view_store.clone(), session_id, response).await
    }
}

/// Runtime MCP attachment source that collects the mobile device's in-process
/// MCP peers (e.g., contacts, calendar) and makes them available to every
/// session materialized on this node.
pub(crate) struct MobileSessionMcpAttachmentSource {
    pub(crate) agent_handle_cell: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait]
impl querymt_agent::agent::session_mcp::SessionMcpAttachmentSource
    for MobileSessionMcpAttachmentSource
{
    async fn attachments(
        &self,
        _context: &querymt_agent::agent::session_mcp::SessionMcpAttachmentContext,
    ) -> Result<Vec<querymt_agent::agent::session_mcp::SessionMcpAttachment>, AcpError> {
        let agent_handle = self
            .agent_handle_cell
            .load(std::sync::atomic::Ordering::Acquire);
        let peers = mcp::collect_connected_mcp_peers(agent_handle)
            .await
            .map_err(|e| {
                AcpError::internal_error().data(serde_json::json!({
                    "message": "collect connected MCP peers failed",
                    "ffiCode": e as i32,
                }))
            })?;
        Ok(peers
            .into_iter()
            .map(querymt_agent::agent::session_mcp::SessionMcpAttachment::ConnectedPeer)
            .collect())
    }
}

#[derive(Deserialize)]
struct RegisterInprocMcpPipeParams {
    server_name: String,
}

#[derive(Deserialize)]
struct UnregisterInprocMcpParams {
    server_name: String,
}

fn default_session_cwd() -> std::path::PathBuf {
    querymt_agent::acp::cwd::no_cwd_path()
}

async fn inject_session_load_snapshot(
    agent: &querymt_agent::agent::LocalAgentHandle,
    view_store: Arc<dyn ViewStore>,
    session_id: &str,
    response: &mut serde_json::Value,
) -> Result<(), AcpError> {
    if let Some(meta) = response.get("_meta")
        && meta.get("location").and_then(serde_json::Value::as_str) == Some("remote")
        && meta.get("querymt/sessionLoadSnapshot.v1").is_some()
    {
        // The remote producer already read the owner's history and runtime.
        // Relay-cache backfill is asynchronous, so it is not a replacement source.
        log::info!("ffi remote session/load owner snapshot preserved: session_id={session_id}");
        return Ok(());
    }
    let snapshot = querymt_agent::session::load_session_snapshot(agent, view_store, session_id)
        .await
        .map_err(|e| AcpError::internal_error().data(e.to_string()))?;

    let event_count = snapshot.audit.events.len();
    log::info!(
        "ffi session/load snapshot injected: session_id={}, events={}",
        session_id,
        event_count
    );

    let Some(obj) = response.as_object_mut() else {
        return Err(AcpError::internal_error().data("session/load returned non-object response"));
    };

    let meta = obj
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(meta_obj) = meta.as_object_mut() else {
        return Err(AcpError::internal_error().data("session/load returned non-object _meta"));
    };
    meta_obj.insert(
        "querymt/sessionLoadSnapshot.v1".to_string(),
        serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null),
    );
    Ok(())
}

async fn ensure_remote_attach_snapshot(
    agent: &querymt_agent::agent::LocalAgentHandle,
    view_store: Arc<dyn ViewStore>,
    session_id: &str,
    response: &mut serde_json::Value,
) -> Result<(), AcpError> {
    let Some(result_obj) = response.as_object_mut() else {
        return Err(AcpError::internal_error().data("remote attach returned non-object response"));
    };

    if result_obj.contains_key("snapshot") {
        log::info!(
            "ffi remote attach snapshot preserved: session_id={}",
            session_id
        );
        return Ok(());
    }

    let snapshot = querymt_agent::session::load_session_snapshot(agent, view_store, session_id)
        .await
        .map_err(|e| AcpError::internal_error().data(e.to_string()))?;
    let event_count = snapshot.audit.events.len();
    log::info!(
        "ffi remote attach fallback snapshot injected: session_id={}, events={}",
        session_id,
        event_count
    );
    result_obj.insert(
        "snapshot".to_string(),
        serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null),
    );
    Ok(())
}

/// Normalize `session/new` params: fill in cwd sentinel and mcpServers if missing.
fn normalize_new_session_params(mut params: serde_json::Value) -> serde_json::Value {
    if !params.is_object() {
        params = serde_json::json!({});
    }

    if let Some(obj) = params.as_object_mut() {
        let cwd_needs_default = match obj.get("cwd") {
            None => true,
            Some(serde_json::Value::String(s))
                if s.trim().is_empty() || s == "$HOME" || s == "~" =>
            {
                true
            }
            _ => false,
        };

        if cwd_needs_default {
            obj.insert(
                "cwd".to_string(),
                serde_json::Value::String(default_session_cwd().display().to_string()),
            );
        }

        if !obj.contains_key("mcpServers") {
            obj.insert(
                "mcpServers".to_string(),
                serde_json::Value::Array(Vec::new()),
            );
        }
    }

    params
}

/// Normalize `session/load` params: fill in cwd sentinel and mcpServers if missing.
fn normalize_load_session_params(mut params: serde_json::Value) -> serde_json::Value {
    if !params.is_object() {
        params = serde_json::json!({});
    }

    if let Some(obj) = params.as_object_mut() {
        let cwd_needs_default = match obj.get("cwd") {
            None => true,
            Some(serde_json::Value::String(s))
                if s.trim().is_empty() || s == "$HOME" || s == "~" =>
            {
                true
            }
            _ => false,
        };

        if cwd_needs_default {
            obj.insert(
                "cwd".to_string(),
                serde_json::Value::String(default_session_cwd().display().to_string()),
            );
        }

        if !obj.contains_key("mcpServers") {
            obj.insert(
                "mcpServers".to_string(),
                serde_json::Value::Array(Vec::new()),
            );
        }
    }

    params
}

static NEXT_ACP_CONNECTION_HANDLE: AtomicU64 = AtomicU64::new(1);
static ACP_CONNECTIONS: once_cell::sync::Lazy<Mutex<HashMap<u64, Arc<Mutex<AcpConnection>>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

async fn close_acp_connections_for_agent(agent_handle: u64) {
    let candidates = ACP_CONNECTIONS
        .lock()
        .await
        .iter()
        .map(|(handle, connection)| (*handle, connection.clone()))
        .collect::<Vec<_>>();
    let mut connections = Vec::new();
    for (handle, connection) in candidates {
        if connection.lock().await.agent_handle != agent_handle {
            continue;
        }
        let removed = ACP_CONNECTIONS.lock().await.remove(&handle);
        if removed.is_some_and(|removed| Arc::ptr_eq(&removed, &connection)) {
            connections.push(connection);
        }
    }
    for connection in connections {
        let callbacks = {
            let mut connection = connection.lock().await;
            let callbacks = connection.callbacks.clone();
            connection.close().await;
            callbacks
        };
        // A callback may reenter the connection while close drains it.
        callbacks.close();
    }
}

// ============================================================================
// Lifecycle
// ============================================================================

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_storage_register(
    callbacks: *const storage::FfiStorageCallbacks,
) -> i32 {
    match unsafe { storage::register_from_ptr(callbacks) } {
        Ok(()) => {
            ffi_helpers::clear_last_error();
            FfiErrorCode::Ok as i32
        }
        Err(code) => {
            let message = match code {
                FfiErrorCode::Busy => "Storage callbacks are in use",
                FfiErrorCode::AlreadyExists => "Storage callbacks are already registered",
                _ => "Invalid storage callback table",
            };
            set_last_error(code, message.into());
            code as i32
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_storage_unregister() -> i32 {
    match storage::unregister() {
        Ok(()) => {
            ffi_helpers::clear_last_error();
            FfiErrorCode::Ok as i32
        }
        Err(code) => {
            let message = match code {
                FfiErrorCode::Busy => "Storage callbacks are in use",
                _ => "Storage callbacks are not registered",
            };
            set_last_error(code, message.into());
            code as i32
        }
    }
}

/// Initialize Android's rustls platform verifier bridge.
///
/// `env` must be the current `JNIEnv*` and `context` an Android `Context` object.
/// This must run before any reqwest/TLS work on Android.
#[cfg(target_os = "android")]
unsafe fn qmt_internal_android_init(
    env: *mut std::ffi::c_void,
    context: *mut std::ffi::c_void,
) -> i32 {
    ffi_panic_boundary("qmt_internal_android_init", || unsafe {
        android_init_impl(env, context)
    })
}

/// Initialize the agent runtime. Call once per agent instance.
///
/// `config_toml` is an inline TOML config string, parsed by the shared
/// `querymt_agent::config::load_config` parser. Supports both single-agent
/// (`[agent]`) and multi-agent/quorum (`[quorum]`/`[planner]`) configs.
/// On success, `*out_agent` is set to an opaque handle.
///
/// Telemetry is configured explicitly via `qmt_ffi_configure_telemetry`
/// before agent init, falling back to the build-mode default when omitted.
///
/// # Safety
///
/// - `config_toml` must be a valid pointer to a null-terminated C string.
/// - `out_agent` must be a valid pointer to a `u64` that will receive the handle.
unsafe fn qmt_internal_init_agent(
    config_toml: *const std::ffi::c_char,
    out_agent: *mut u64,
) -> i32 {
    let _init_reservation = match storage::reserve_init() {
        Ok(reservation) => reservation,
        Err(code) => {
            set_last_error(
                code,
                "Storage callbacks must be registered before initialization".into(),
            );
            return code as i32;
        }
    };
    ffi_panic_boundary("qmt_internal_init_agent", || {
        log::info!("ffi.agent.init: requested attach/init");
        let config_result = agent::parse_config(config_toml);
        let config = match config_result {
            Ok(config) => config,
            Err(code) => return code as i32,
        };
        storage::install_android_default_store();

        let _rt_guard = runtime::global_runtime().enter();
        events::setup_mobile_telemetry();
        match agent::init_agent_from_config(config, out_agent) {
            Ok(()) => {
                ffi_helpers::clear_last_error();
                FfiErrorCode::Ok as i32
            }
            Err(code) => code as i32,
        }
    })
}

/// Shut down an agent and release all resources owned by the handle.
///
/// Returns `QMT_FFI_BUSY` if the agent has active FFI calls.
/// Calling shutdown twice is safe: first call succeeds, subsequent calls on the
/// same stale handle return `QMT_FFI_NOT_FOUND`.

///
/// # Safety
///
/// - `agent_handle` must be a valid handle returned by `qmt_internal_init_agent`.
unsafe fn qmt_internal_shutdown(agent_handle: u64) -> i32 {
    let _shutdown_reservation = storage::reserve_shutdown();
    log::info!("ffi.agent.shutdown: requested detach (agent_handle={agent_handle})");
    runtime::global_runtime().block_on(close_acp_connections_for_agent(agent_handle));
    let result = agent::shutdown_agent_inner(agent_handle);
    match result {
        Ok(()) => {
            ffi_helpers::clear_last_error();
            FfiErrorCode::Ok as i32
        }
        Err(code) => code as i32,
    }
}

/// Notify the runtime of app lifecycle transitions.
///
/// Mesh networking stays alive while backgrounded. Foreground-only user
/// operations return `QMT_FFI_INVALID_STATE` while backgrounded.
///
/// # Safety
///
/// No additional safety requirements beyond calling from a valid thread context.
unsafe fn qmt_internal_set_backgrounded(backgrounded: i32) -> i32 {
    let is_backgrounded = backgrounded != 0;
    ffi_helpers::set_backgrounded(is_backgrounded);
    log::info!(
        "ffi.lifecycle.state: {}",
        if is_backgrounded {
            "backgrounded"
        } else {
            "foregrounded"
        }
    );
    FfiErrorCode::Ok as i32
}

/// Configure mobile telemetry before agent init.
///
/// When `enabled` is non-zero, OTLP export is initialized on the next agent
/// startup using the provided endpoint or the shared default endpoint.
/// When disabled, Rust telemetry initialization is skipped entirely.
///
/// # Safety
///
/// - `endpoint` must be null or a valid pointer to a null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_configure_telemetry(
    enabled: i32,
    endpoint: *const std::ffi::c_char,
) -> i32 {
    ffi_panic_boundary("qmt_ffi_configure_telemetry", || {
        let endpoint = if endpoint.is_null() {
            None
        } else {
            match unsafe { std::ffi::CStr::from_ptr(endpoint).to_str() } {
                Ok(v) if !v.is_empty() => Some(v.to_string()),
                Ok(_) => None,
                Err(_) => {
                    set_last_error(
                        FfiErrorCode::InvalidArgument,
                        "telemetry endpoint is not valid UTF-8".into(),
                    );
                    return FfiErrorCode::InvalidArgument as i32;
                }
            }
        };

        log::debug!(
            "configure_telemetry: enabled={} endpoint={}",
            enabled != 0,
            endpoint.as_deref().unwrap_or("<default>")
        );

        events::configure_mobile_telemetry(events::MobileTelemetryConfig {
            enabled: enabled != 0,
            endpoint,
        });
        ffi_helpers::clear_last_error();
        FfiErrorCode::Ok as i32
    })
}

// ============================================================================
// Embedded ACP Transport
// ============================================================================

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_acp_open(agent_handle: u64, out_connection: *mut u64) -> i32 {
    if out_connection.is_null() {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "out_connection is null".into(),
        );
        return FfiErrorCode::InvalidArgument as i32;
    }
    let (acp_server, view_store) = match state::with_agent_read(agent_handle, |record| {
        let view_store = record
            .view_store
            .clone()
            .ok_or(FfiErrorCode::RuntimeError)?;
        Ok((record.acp_server.clone(), view_store))
    }) {
        Ok(result) => result,
        Err(code) => return code as i32,
    };

    let connection_handle = NEXT_ACP_CONNECTION_HANDLE.fetch_add(1, Ordering::Relaxed);
    let outbox = Arc::new(StdMutex::new(VecDeque::new()));
    let callbacks = match AcpCallbackWorker::new(connection_handle, outbox.clone()) {
        Ok(callbacks) => callbacks,
        Err(error) => {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("Failed to start ACP callback worker: {error}"),
            );
            return FfiErrorCode::RuntimeError as i32;
        }
    };
    let hooks: Arc<dyn querymt_agent::acp::shared::AcpSessionHooks> =
        Arc::new(MobileAcpSessionHooks { view_store });
    let (adapter, mut outgoing) = runtime::global_runtime().block_on(acp_server.open(Some(hooks)));
    let callback_sender = callbacks.clone();
    let output_task = runtime::global_runtime().spawn(async move {
        while let Some(json) = outgoing.recv().await {
            if !callback_sender.send(json) {
                break;
            }
        }
    });
    let connection = Arc::new(Mutex::new(AcpConnection {
        agent_handle,
        adapter: Some(adapter),
        outbox,
        callbacks,
        output_task: Some(output_task),
    }));
    runtime::global_runtime().block_on(async {
        ACP_CONNECTIONS
            .lock()
            .await
            .insert(connection_handle, connection);
    });
    unsafe { *out_connection = connection_handle };
    FfiErrorCode::Ok as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_acp_close(connection_handle: u64) -> i32 {
    let removed = runtime::global_runtime()
        .block_on(async { ACP_CONNECTIONS.lock().await.remove(&connection_handle) });
    match removed {
        Some(connection) => {
            let callbacks = runtime::global_runtime().block_on(async {
                let mut connection = connection.lock().await;
                let callbacks = connection.callbacks.clone();
                connection.close().await;
                callbacks
            });
            callbacks.close();
            FfiErrorCode::Ok as i32
        }
        None => FfiErrorCode::NotFound as i32,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_acp_send(
    connection_handle: u64,
    message_json: *const std::ffi::c_char,
) -> i32 {
    if message_json.is_null() {
        set_last_error(FfiErrorCode::InvalidArgument, "message_json is null".into());
        return FfiErrorCode::InvalidArgument as i32;
    }
    let message = match unsafe { std::ffi::CStr::from_ptr(message_json).to_str() } {
        Ok(message) => message.to_string(),
        Err(_) => {
            set_last_error(
                FfiErrorCode::InvalidArgument,
                "message_json is not valid UTF-8".into(),
            );
            return FfiErrorCode::InvalidArgument as i32;
        }
    };

    let result = runtime::block_on_ffi(async move {
        let value: serde_json::Value =
            serde_json::from_str(&message).map_err(|_| FfiErrorCode::InvalidArgument)?;
        let connection = ACP_CONNECTIONS
            .lock()
            .await
            .get(&connection_handle)
            .cloned()
            .ok_or(FfiErrorCode::NotFound)?;
        let mut connection = connection.lock().await;
        let adapter = connection.adapter.as_ref().ok_or(FfiErrorCode::NotFound)?;
        let Some(method) = value.get("method").and_then(serde_json::Value::as_str) else {
            adapter
                .receive_json(&message)
                .await
                .map_err(|_| FfiErrorCode::InvalidArgument)?;
            return Ok::<(), FfiErrorCode>(());
        };
        let params = value
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let request_id = value.get("id").cloned();
        log::debug!("ffi acp send: method={method}");

        if method == "querymt/mcp/registerInprocPipe" || method == "querymt/mcp/unregister" {
            let response = if method == "querymt/mcp/registerInprocPipe" {
                match serde_json::from_value::<RegisterInprocMcpPipeParams>(params) {
                    Ok(params) => match std::ffi::CString::new(params.server_name) {
                        Ok(server_name) => {
                            let mut read_fd = -1;
                            let mut write_fd = -1;
                            match mcp::register_inproc_mcp_pipe_inner(
                                connection.agent_handle,
                                server_name.as_ptr(),
                                &mut read_fd,
                                &mut write_fd,
                            ) {
                                Ok(()) => Ok(serde_json::json!({
                                    "read_fd": read_fd,
                                    "write_fd": write_fd
                                })),
                                Err(error) => Err(serde_json::json!({
                                    "code": -32000,
                                    "message": "querymt/mcp/registerInprocPipe failed",
                                    "data": { "ffiCode": error as i32 }
                                })),
                            }
                        }
                        Err(_) => Err(serde_json::json!({
                            "code": -32602,
                            "message": "invalid params"
                        })),
                    },
                    Err(error) => Err(serde_json::json!({
                        "code": -32602,
                        "message": "invalid params",
                        "data": { "detail": error.to_string() }
                    })),
                }
            } else {
                match serde_json::from_value::<UnregisterInprocMcpParams>(params) {
                    Ok(params) => match std::ffi::CString::new(params.server_name) {
                        Ok(server_name) => match mcp::unregister_inproc_mcp_inner(
                            connection.agent_handle,
                            server_name.as_ptr(),
                        ) {
                            Ok(()) => Ok(serde_json::Value::Null),
                            Err(error) => Err(serde_json::json!({
                                "code": -32000,
                                "message": "querymt/mcp/unregister failed",
                                "data": { "ffiCode": error as i32 }
                            })),
                        },
                        Err(_) => Err(serde_json::json!({
                            "code": -32602,
                            "message": "invalid params"
                        })),
                    },
                    Err(error) => Err(serde_json::json!({
                        "code": -32602,
                        "message": "invalid params",
                        "data": { "detail": error.to_string() }
                    })),
                }
            };
            if let Some(id) = request_id {
                let response = match response {
                    Ok(result) => querymt_agent::acp::shared::RpcResponse {
                        jsonrpc: "2.0".to_string(),
                        result: Some(result),
                        error: None,
                        id,
                    },
                    Err(error) => querymt_agent::acp::shared::RpcResponse {
                        jsonrpc: "2.0".to_string(),
                        result: None,
                        error: Some(error),
                        id,
                    },
                };
                let json =
                    serde_json::to_string(&response).map_err(|_| FfiErrorCode::RuntimeError)?;
                adapter
                    .send_json(json)
                    .await
                    .map_err(|_| FfiErrorCode::RuntimeError)?;
            }
            return Ok::<(), FfiErrorCode>(());
        }

        let params = match method {
            "session/new" => normalize_new_session_params(params),
            "session/load" => normalize_load_session_params(params),
            _ => params,
        };
        let mut request = serde_json::Map::new();
        request.insert("jsonrpc".into(), serde_json::json!("2.0"));
        request.insert("method".into(), serde_json::json!(method));
        request.insert("params".into(), params);
        if let Some(id) = request_id {
            request.insert("id".into(), id);
        }
        adapter
            .receive_json(&serde_json::Value::Object(request).to_string())
            .await
            .map_err(|_| FfiErrorCode::InvalidArgument)?;
        Ok::<(), FfiErrorCode>(())
    });

    match result {
        Ok(()) => FfiErrorCode::Ok as i32,
        Err(code) => code as i32,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_acp_next_message(
    connection_handle: u64,
    timeout_ms: i32,
    out_message_json: *mut *mut std::ffi::c_char,
) -> i32 {
    if out_message_json.is_null() {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "out_message_json is null".into(),
        );
        return FfiErrorCode::InvalidArgument as i32;
    }
    let outbox = match runtime::global_runtime().block_on(async {
        ACP_CONNECTIONS
            .lock()
            .await
            .get(&connection_handle)
            .map(|connection| connection.clone())
            .ok_or(FfiErrorCode::NotFound)
    }) {
        Ok(connection) => connection,
        Err(code) => return code as i32,
    };
    let outbox = runtime::global_runtime().block_on(async { outbox.lock().await.outbox.clone() });
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
    loop {
        let message = outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        if let Some(message) = message {
            unsafe { *out_message_json = alloc_string(&message) };
            return FfiErrorCode::Ok as i32;
        }
        if std::time::Instant::now() >= deadline {
            return FfiErrorCode::NotFound as i32;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_acp_set_message_handler(
    connection_handle: u64,
    handler: Option<AcpMessageHandlerFn>,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    let callbacks = match runtime::global_runtime().block_on(async {
        ACP_CONNECTIONS
            .lock()
            .await
            .get(&connection_handle)
            .map(|connection| connection.clone())
            .ok_or(FfiErrorCode::NotFound)
    }) {
        Ok(connection) => {
            runtime::global_runtime().block_on(async { connection.lock().await.callbacks.clone() })
        }
        Err(code) => return code as i32,
    };
    if callbacks.set_handler(handler, user_data as usize) {
        FfiErrorCode::Ok as i32
    } else {
        FfiErrorCode::NotFound as i32
    }
}

/// Set the global log handler callback.
///
/// # Safety
///
/// - If `handler` is non-null, `user_data` must remain valid for the lifetime
///   of the process or until a new handler is set.
/// - The `handler` function pointer must be safe to call from any thread.
unsafe fn qmt_internal_set_log_handler(
    handler: Option<events::LogHandlerFn>,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    match events::set_log_handler(handler, user_data) {
        Ok(()) => {
            ffi_helpers::clear_last_error();
            FfiErrorCode::Ok as i32
        }
        Err(code) => {
            set_last_error(code, "Invalid arguments for log handler".into());
            code as i32
        }
    }
}

// ============================================================================
// Canonical qmt_ffi API
// ============================================================================

/// Initialize Android JNI state required by TLS certificate verification.
///
/// Android callers must invoke this entrypoint before `qmt_ffi_init_agent` and
/// before any TLS/reqwest work. The `rustls-platform-verifier` backend needs the
/// current `JNIEnv*` and an Android `Context`; repeated calls are safe because
/// the verifier initialization is idempotent.
///
/// Returns a `QMT_FFI_*` status code and sets the thread-local last error on
/// failure.
///
/// # Safety
///
/// - `env` must be a valid `JNIEnv*` for the current thread.
/// - `context` must be a valid Android application `Context` JNI object.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_android_init(
    env: *mut std::ffi::c_void,
    context: *mut std::ffi::c_void,
) -> i32 {
    unsafe { qmt_internal_android_init(env, context) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_init_agent(
    config_toml: *const std::ffi::c_char,
    out_agent: *mut u64,
) -> i32 {
    unsafe { qmt_internal_init_agent(config_toml, out_agent) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_shutdown(agent_handle: u64) -> i32 {
    unsafe { qmt_internal_shutdown(agent_handle) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_set_lifecycle_state(_agent_handle: u64, backgrounded: i32) -> i32 {
    unsafe { qmt_internal_set_backgrounded(backgrounded) }
}

/// Explicit developer reset hook for mobile debug/dev workflows.
///
/// This does not attempt to reset kameo's global OnceLock and therefore only
/// succeeds when no logical agents are attached.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_shutdown_runtime_if_idle() -> i32 {
    let _shutdown_reservation = storage::reserve_shutdown();
    ffi_panic_boundary(
        "qmt_ffi_shutdown_runtime_if_idle",
        || match state::shutdown_runtime_if_idle() {
            Ok(true) => {
                log::info!("ffi.runtime.shutdown: process runtime released (idle)");
                ffi_helpers::clear_last_error();
                FfiErrorCode::Ok as i32
            }
            Ok(false) => {
                ffi_helpers::clear_last_error();
                FfiErrorCode::Ok as i32
            }
            Err(FfiErrorCode::Busy) => {
                set_last_error(
                    FfiErrorCode::Busy,
                    "Runtime still has attached agents".into(),
                );
                FfiErrorCode::Busy as i32
            }
            Err(code) => code as i32,
        },
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_set_log_handler(
    handler: Option<events::LogHandlerFn>,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    unsafe { qmt_internal_set_log_handler(handler, user_data) }
}

// ============================================================================
// Error Reporting & Memory
// ============================================================================

/// Return the last error code for the calling thread.
///
/// # Safety
///
/// No additional safety requirements beyond calling from a valid thread context.
unsafe fn qmt_internal_last_error_code() -> i32 {
    take_last_error_code() as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_last_error_code() -> i32 {
    unsafe { qmt_internal_last_error_code() }
}

/// Return the last error message for the calling thread.
/// Caller must free the returned string with `qmt_internal_free_string`.
///
/// # Safety
///
/// No additional safety requirements beyond calling from a valid thread context.
unsafe fn qmt_internal_last_error_message() -> *mut std::ffi::c_char {
    alloc_string(&take_last_error_message())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_last_error_message() -> *mut std::ffi::c_char {
    unsafe { qmt_internal_last_error_message() }
}

/// Free a string allocated by the FFI layer. NULL is a no-op.
///
/// # Safety
///
/// - `ptr` must be either NULL or a pointer previously returned by an FFI
///   function that allocates a C string. The pointer must not have been freed
///   already, and must not be used after this call.
unsafe fn qmt_internal_free_string(ptr: *mut std::ffi::c_char) {
    if !ptr.is_null() {
        unsafe {
            let _ = std::ffi::CString::from_raw(ptr);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_free_string(ptr: *mut std::ffi::c_char) {
    unsafe { qmt_internal_free_string(ptr) }
}

// ============================================================================
// Internal Helpers
// ============================================================================

#[cfg(target_os = "android")]
static ANDROID_CONTEXT_INIT: std::sync::Once = std::sync::Once::new();
#[cfg(target_os = "android")]
static ANDROID_CONTEXT_GLOBAL: once_cell::sync::OnceCell<
    jni::objects::Global<jni::objects::JObject<'static>>,
> = once_cell::sync::OnceCell::new();

#[cfg(target_os = "android")]
unsafe fn android_init_impl(env: *mut std::ffi::c_void, context: *mut std::ffi::c_void) -> i32 {
    if env.is_null() || context.is_null() {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "JNIEnv and Android Context must not be null".into(),
        );
        return FfiErrorCode::InvalidArgument as i32;
    }

    let mut env = unsafe { jni::EnvUnowned::from_raw(env.cast::<jni::sys::JNIEnv>()) };
    let context = context.cast::<jni::sys::_jobject>();

    match env
        .with_env_no_catch(|env| {
            let context = unsafe { jni::objects::JObject::from_raw(env, context) };
            let java_vm = env.get_java_vm()?;
            let global_context =
                ANDROID_CONTEXT_GLOBAL.get_or_try_init(|| env.new_global_ref(&context))?;

            ANDROID_CONTEXT_INIT.call_once(|| unsafe {
                ndk_context::initialize_android_context(
                    java_vm.get_raw().cast(),
                    global_context.as_obj().as_raw().cast(),
                );
            });

            rustls_platform_verifier::android::init_with_env(env, context)
        })
        .into_outcome()
    {
        jni::Outcome::Ok(()) => {
            // init_with_env stores global refs internally and is idempotent.
            ffi_helpers::clear_last_error();
            FfiErrorCode::Ok as i32
        }
        jni::Outcome::Err(err) => {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("failed to initialize rustls platform verifier: {err}"),
            );
            FfiErrorCode::RuntimeError as i32
        }
        jni::Outcome::Panic(payload) => std::panic::resume_unwind(payload),
    }
}

fn ffi_panic_boundary<F>(function_name: &'static str, f: F) -> i32
where
    F: FnOnce() -> i32,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(payload) => {
            let panic_message = panic_payload_message(payload.as_ref());
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("{function_name} panicked: {panic_message}"),
            );
            FfiErrorCode::RuntimeError as i32
        }
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<std::fmt::Arguments<'_>>() {
        message.to_string()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Allocate a C string managed by the caller.
fn alloc_string(s: &str) -> *mut std::ffi::c_char {
    std::ffi::CString::new(s)
        .unwrap_or_else(|_| std::ffi::CString::new("").unwrap())
        .into_raw()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    struct FfiTestProvider;

    #[async_trait]
    impl querymt::chat::ChatProvider for FfiTestProvider {
        async fn chat(
            &self,
            _call: &querymt::chat::ChatCall<'_>,
        ) -> Result<querymt::chat::ChatOutput, querymt::error::LLMError> {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Ok(querymt::chat::ChatOutput::from_projections(
                None,
                Some("deterministic reply".to_string()),
                None,
                None,
                None,
            ))
        }
    }

    #[async_trait]
    impl querymt::completion::CompletionProvider for FfiTestProvider {
        async fn complete(
            &self,
            _request: &querymt::completion::CompletionRequest,
        ) -> Result<querymt::completion::CompletionResponse, querymt::error::LLMError> {
            Err(querymt::error::LLMError::NotImplemented(
                "test provider".into(),
            ))
        }
    }

    #[async_trait]
    impl querymt::embedding::EmbeddingProvider for FfiTestProvider {
        async fn embed(
            &self,
            _input: Vec<String>,
        ) -> Result<Vec<Vec<f32>>, querymt::error::LLMError> {
            Err(querymt::error::LLMError::NotImplemented(
                "test provider".into(),
            ))
        }
    }

    impl querymt::LLMProvider for FfiTestProvider {}

    struct FfiTestProviderFactory;

    impl querymt::plugin::LLMProviderFactory for FfiTestProviderFactory {
        fn name(&self) -> &str {
            "mock"
        }

        fn config_schema(&self) -> String {
            "{}".to_string()
        }

        fn from_config(
            &self,
            _config: &str,
        ) -> Result<Box<dyn querymt::LLMProvider>, querymt::error::LLMError> {
            Ok(Box::new(FfiTestProvider))
        }

        fn list_models<'a>(
            &'a self,
            _config: &str,
        ) -> querymt::plugin::Fut<'a, Result<Vec<String>, querymt::error::LLMError>> {
            Box::pin(async { Ok(vec!["mock-model".to_string()]) })
        }
    }

    async fn make_ffi_test_agent() -> u64 {
        let registry = Arc::new(querymt::plugin::host::PluginRegistry::empty());
        registry.register_static(Arc::new(FfiTestProviderFactory));
        let storage = Arc::new(
            querymt_agent::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create isolated test storage"),
        );
        let infra = querymt_agent::api::AgentInfra {
            plugin_registry: registry,
            storage: Some(storage.clone()),
            session_mcp_attachment_source: None,
            event_fanout: Some(Arc::new(querymt_agent::event_fanout::EventFanout::new())),
        };
        let agent = querymt_agent::api::Agent::single()
            .provider("mock", "mock-model")
            .infra(infra)
            .build()
            .await
            .expect("build deterministic test agent");
        state::attach_or_insert_runtime_agent(agent, storage).0
    }

    async fn ffi_acp_open(agent_handle: u64) -> (i32, u64) {
        tokio::task::spawn_blocking(move || {
            let mut connection_handle = 0;
            let code = unsafe { qmt_ffi_acp_open(agent_handle, &mut connection_handle) };
            (code, connection_handle)
        })
        .await
        .expect("open FFI ACP connection")
    }

    async fn ffi_acp_send(connection_handle: u64, message: String) -> i32 {
        tokio::task::spawn_blocking(move || {
            let message = std::ffi::CString::new(message).expect("test JSON has no NUL");
            unsafe { qmt_ffi_acp_send(connection_handle, message.as_ptr()) }
        })
        .await
        .expect("send FFI ACP message")
    }

    async fn ffi_acp_poll(
        connection_handle: u64,
        timeout_ms: i32,
    ) -> (i32, Option<serde_json::Value>) {
        tokio::task::spawn_blocking(move || {
            let mut message = std::ptr::null_mut();
            let code =
                unsafe { qmt_ffi_acp_next_message(connection_handle, timeout_ms, &mut message) };
            if code != FfiErrorCode::Ok as i32 {
                return (code, None);
            }
            let json = unsafe { std::ffi::CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned();
            unsafe { qmt_ffi_free_string(message) };
            (
                code,
                Some(serde_json::from_str(&json).expect("valid ACP JSON")),
            )
        })
        .await
        .expect("poll FFI ACP message")
    }

    async fn ffi_poll_until(
        connection_handle: u64,
        timeout: std::time::Duration,
        mut predicate: impl FnMut(&serde_json::Value) -> bool,
    ) -> Vec<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut messages = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for ACP output");
            let (code, message) = ffi_acp_poll(
                connection_handle,
                remaining.as_millis().clamp(1, 100) as i32,
            )
            .await;
            if code == FfiErrorCode::Ok as i32 {
                let message = message.expect("successful poll returns message");
                let matched = predicate(&message);
                messages.push(message);
                if matched {
                    return messages;
                }
            }
        }
    }

    struct BlockingCallbackState {
        entered: std::sync::mpsc::Sender<()>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
    }

    unsafe extern "C" fn blocking_acp_handler(
        _connection_handle: u64,
        _message_json: *const std::ffi::c_char,
        user_data: *mut std::ffi::c_void,
    ) {
        let state = unsafe { &*(user_data as *const BlockingCallbackState) };
        let _ = state.entered.send(());
        let _ = state.release.lock().expect("callback release lock").recv();
    }

    unsafe extern "C" fn self_closing_acp_handler(
        connection_handle: u64,
        _message_json: *const std::ffi::c_char,
        user_data: *mut std::ffi::c_void,
    ) {
        let result = unsafe { qmt_ffi_acp_close(connection_handle) };
        let completion = unsafe { &*(user_data as *const std::sync::mpsc::Sender<i32>) };
        let _ = completion.send(result);
    }

    async fn ffi_acp_set_handler(
        connection_handle: u64,
        handler: Option<AcpMessageHandlerFn>,
        user_data: usize,
    ) -> i32 {
        tokio::task::spawn_blocking(move || unsafe {
            qmt_ffi_acp_set_message_handler(
                connection_handle,
                handler,
                user_data as *mut std::ffi::c_void,
            )
        })
        .await
        .expect("set FFI ACP callback")
    }

    async fn ffi_acp_close(connection_handle: u64) -> i32 {
        tokio::task::spawn_blocking(move || unsafe { qmt_ffi_acp_close(connection_handle) })
            .await
            .expect("close FFI ACP connection")
    }

    #[test]
    fn panic_payload_message_extracts_str_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_payload_message(payload.as_ref()), "boom");
    }

    #[test]
    fn panic_payload_message_extracts_string_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("owned boom"));
        assert_eq!(panic_payload_message(payload.as_ref()), "owned boom");
    }

    #[test]
    fn ffi_panic_boundary_converts_panic_to_runtime_error() {
        ffi_helpers::clear_last_error();

        let code = ffi_panic_boundary("test_boundary", || {
            std::panic::panic_any(String::from("boundary boom"))
        });

        assert_eq!(code, FfiErrorCode::RuntimeError as i32);
        assert_eq!(take_last_error_code(), FfiErrorCode::RuntimeError);
        assert_eq!(
            take_last_error_message(),
            "test_boundary panicked: boundary boom"
        );
    }

    #[test]
    fn ffi_panic_boundary_preserves_success_code() {
        ffi_helpers::clear_last_error();

        let code = ffi_panic_boundary("test_boundary", || FfiErrorCode::Unsupported as i32);

        assert_eq!(code, FfiErrorCode::Unsupported as i32);
        assert_eq!(take_last_error_code(), FfiErrorCode::Ok);
        assert_eq!(take_last_error_message(), "");
    }

    #[test]
    fn mesh_notification_json_shape_matches_mobile_expectations() {
        let msg =
            querymt_agent::acp::shared::mesh_nodes_changed_notification("peer-123", "discovered");
        let json = serde_json::to_value(msg).expect("serialize mesh notification");
        assert_eq!(json["jsonrpc"], serde_json::json!("2.0"));
        assert_eq!(
            json["method"],
            serde_json::json!("querymt/mesh/nodesChanged")
        );
        assert_eq!(json["params"]["peer_id"], serde_json::json!("peer-123"));
        assert_eq!(json["params"]["change"], serde_json::json!("discovered"));
    }

    /// Regression test: the FFI remote attach path must NOT overwrite a
    /// snapshot already present in the ACP response.  The server-side
    /// `querymt/remote/attachSession` handler builds a snapshot from the
    /// remote actor's event stream; replacing it with the local (empty)
    /// journal loses all history.
    #[tokio::test]
    async fn remote_attach_snapshot_preserves_existing() {
        let agent_handle = make_ffi_test_agent().await;
        let (agent, view) = state::with_agent_read(agent_handle, |record| {
            Ok((record.agent.handle(), record.view_store.clone().unwrap()))
        })
        .unwrap();
        let snapshot = serde_json::json!({
            "audit": {"session_id": "remote", "events": [{"owner_history": "preserved"}]},
            "cursor": {"local_seq": 17, "remote_seq_by_source": {"owner": 9}},
            "runtimeStatus": {"phase": "idle", "observation": {"availability": "live", "epoch": "owner", "revision": 3},
                "delegation_activity": [{"delegationId": "verifier", "activity": "live", "stage": "verifying", "sessionSeq": 11, "sourceEpoch": "workflow"}]},
            "delegationUpdates": [{"delegationId": "verifier", "activity": "live", "stage": "verifying", "sessionSeq": 11, "sourceEpoch": "workflow"}]
        });
        let mut response = serde_json::json!({"session_id": "remote", "snapshot": snapshot});
        // No viewer-local session exists: falling back would fail or lose owner history.
        let hooks = MobileAcpSessionHooks { view_store: view };
        querymt_agent::acp::shared::AcpSessionHooks::on_remote_session_attached(
            &hooks,
            agent.as_ref(),
            "remote",
            &mut response,
        )
        .await
        .unwrap();
        assert_eq!(response["snapshot"], snapshot);
        agent
            .config
            .provider
            .history_store()
            .save_remote_session_bookmark(&querymt_agent::session::store::RemoteSessionBookmark {
                session_id: "remote".into(),
                node_id: "owner".into(),
                peer_label: "owner".into(),
                cwd: None,
                created_at: 0,
                title: None,
            })
            .await
            .unwrap();
        // A newly attached viewer may have no relay history yet. The production
        // remote load response already contains the authoritative owner snapshot.
        let mut loaded = serde_json::json!({"_meta": {
            "location": "remote", "nodeId": "owner", "querymt/sessionLoadSnapshot.v1": snapshot,
        }});
        querymt_agent::acp::shared::AcpSessionHooks::on_session_loaded(
            &hooks,
            agent.as_ref(),
            "remote",
            &mut loaded,
        )
        .await
        .unwrap();
        assert_eq!(
            loaded["_meta"]["querymt/sessionLoadSnapshot.v1"], snapshot,
            "FFI remote load must not replace owner truth with an empty viewer journal"
        );
        assert_eq!(loaded["_meta"]["nodeId"], "owner");
        state::remove_agent(agent_handle).unwrap();
    }

    /// Inverse: when the server response has no snapshot key, the FFI
    /// injection path must still be able to insert one.
    #[tokio::test]
    async fn remote_attach_snapshot_allows_fallback_when_missing() {
        let agent_handle = make_ffi_test_agent().await;
        let (agent, view) = state::with_agent_read(agent_handle, |record| {
            Ok((record.agent.handle(), record.view_store.clone().unwrap()))
        })
        .unwrap();
        let created = agent
            .new_session(querymt_agent::acp::protocol::NewSessionRequest::new(
                std::env::temp_dir(),
            ))
            .await
            .unwrap();
        let id = created.session_id.to_string();
        let mut attached = serde_json::json!({"session_id": id});
        let hooks = MobileAcpSessionHooks { view_store: view };
        querymt_agent::acp::shared::AcpSessionHooks::on_remote_session_attached(
            &hooks,
            agent.as_ref(),
            &id,
            &mut attached,
        )
        .await
        .unwrap();
        assert_eq!(attached["snapshot"]["runtimeStatus"]["phase"], "idle");
        assert_eq!(
            attached["snapshot"]["runtimeStatus"]["observation"]["availability"],
            "live"
        );
        let mut loaded = serde_json::json!({"_meta": {"sentinel": true, "querymt/sessionLoadSnapshot.v1": {"old_incomplete": true}}});
        querymt_agent::acp::shared::AcpSessionHooks::on_session_loaded(
            &hooks,
            agent.as_ref(),
            &id,
            &mut loaded,
        )
        .await
        .unwrap();
        let snapshot = &loaded["_meta"]["querymt/sessionLoadSnapshot.v1"];
        assert_eq!(
            snapshot["runtimeStatus"],
            attached["snapshot"]["runtimeStatus"]
        );
        assert!(snapshot.get("old_incomplete").is_none());
        assert!(snapshot["delegationUpdates"].is_array());
        assert!(snapshot["audit"]["events"].is_array());
        assert_eq!(loaded["_meta"]["sentinel"], true);
        state::remove_agent(agent_handle).unwrap();
    }

    #[test]
    fn normalize_load_session_params_preserves_trace_meta() {
        let params = serde_json::json!({
            "sessionId": "s-1",
            "_meta": {
                "traceparent": "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
            }
        });

        let normalized = normalize_load_session_params(params);

        assert_eq!(
            normalized["_meta"]["traceparent"],
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            "_meta.traceparent must survive normalization"
        );
        assert!(normalized.get("cwd").is_some(), "cwd should be filled in");
        assert!(
            normalized.get("mcpServers").is_some(),
            "mcpServers should be filled in"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exported_acp_transport_dispatches_interactions_and_waits_for_callbacks() {
        let agent_handle = make_ffi_test_agent().await;
        let (code, connection) = ffi_acp_open(agent_handle).await;
        assert_eq!(code, FfiErrorCode::Ok as i32);

        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "initialize",
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {"elicitation": {"form": {}}},
                "clientInfo": {"name": "ffi-test", "version": "1"}
            }
        });
        assert_eq!(
            ffi_acp_send(connection, initialize.to_string()).await,
            FfiErrorCode::Ok as i32
        );
        let initialized =
            ffi_poll_until(connection, std::time::Duration::from_secs(5), |message| {
                message["id"] == "initialize"
            })
            .await;
        let initialized = initialized.last().expect("initialize response");
        assert!(initialized["error"].is_null(), "{initialized}");

        let new_session = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "new",
            "method": "session/new",
            "params": {"cwd": "/tmp", "mcpServers": []}
        });
        assert_eq!(
            ffi_acp_send(connection, new_session.to_string()).await,
            FfiErrorCode::Ok as i32
        );
        let created = ffi_poll_until(connection, std::time::Duration::from_secs(5), |message| {
            message["id"] == "new"
        })
        .await;
        let created = created.last().expect("session/new response");
        assert!(created["error"].is_null(), "{created}");
        let session_id = created["result"]["sessionId"]
            .as_str()
            .expect("new session id")
            .to_string();
        let config_options = created["result"]["configOptions"]
            .as_array()
            .expect("model/config options retained");
        assert!(config_options.iter().any(|option| option["id"] == "model"));

        let reused_handle = make_ffi_test_agent().await;
        let attached_handle =
            state::attach_existing_runtime_agent().expect("attach shared runtime");
        let server = state::with_agent_read(agent_handle, |record| Ok(record.acp_server.clone()))
            .expect("original runtime server");
        for handle in [reused_handle, attached_handle] {
            let shared = state::with_agent_read(handle, |record| Ok(record.acp_server.clone()))
                .expect("attached runtime server");
            assert!(Arc::ptr_eq(&server, &shared));
            let (code, other_connection) = ffi_acp_open(handle).await;
            assert_eq!(code, FfiErrorCode::Ok as i32);
            assert_ne!(connection, other_connection);
            assert_eq!(
                ffi_acp_send(
                    other_connection,
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": "shared-load", "method": "session/load",
                        "params": {"sessionId": session_id}
                    })
                    .to_string()
                )
                .await,
                FfiErrorCode::Ok as i32
            );
            let loaded = ffi_poll_until(
                other_connection,
                std::time::Duration::from_secs(5),
                |message| message["id"] == "shared-load",
            )
            .await;
            assert!(loaded.last().expect("shared session loaded")["error"].is_null());
            assert_eq!(
                ffi_acp_close(other_connection).await,
                FfiErrorCode::Ok as i32
            );
            assert_eq!(
                ffi_acp_close(other_connection).await,
                FfiErrorCode::NotFound as i32
            );
            assert_eq!(
                tokio::task::spawn_blocking(move || unsafe { qmt_ffi_shutdown(handle) })
                    .await
                    .expect("detach additional logical handle"),
                FfiErrorCode::Ok as i32
            );
        }
        drop(server);

        let prompt = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "prompt",
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "reply deterministically"}]
            }
        });
        assert_eq!(
            ffi_acp_send(connection, prompt.to_string()).await,
            FfiErrorCode::Ok as i32
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            ffi_acp_send(
                connection,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "cancel",
                    "method": "session/cancel",
                    "params": {"sessionId": session_id}
                })
                .to_string(),
            )
            .await,
            FfiErrorCode::Ok as i32
        );
        let mut saw_update = false;
        let mut saw_prompt_response = false;
        let mut saw_cancel_response = false;
        ffi_poll_until(connection, std::time::Duration::from_secs(10), |message| {
            saw_update |= message["method"] == "session/update";
            saw_prompt_response |= message["id"] == "prompt";
            saw_cancel_response |= message["id"] == "cancel";
            saw_prompt_response && saw_cancel_response
        })
        .await;
        assert!(
            saw_update,
            "prompt events must stream through the FFI outbox"
        );

        while ffi_acp_poll(connection, 0).await.0 == FfiErrorCode::Ok as i32 {}
        let mut notification = initialize.clone();
        notification
            .as_object_mut()
            .expect("initialize is a JSON object")
            .remove("id");
        assert_eq!(
            ffi_acp_send(connection, notification.to_string()).await,
            FfiErrorCode::Ok as i32
        );
        let notification_deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(250);
        while tokio::time::Instant::now() < notification_deadline {
            let remaining =
                notification_deadline.saturating_duration_since(tokio::time::Instant::now());
            let (poll_code, message) =
                ffi_acp_poll(connection, remaining.as_millis().clamp(1, 50) as i32).await;
            if poll_code == FfiErrorCode::NotFound as i32 {
                continue;
            }
            assert_eq!(poll_code, FfiErrorCode::Ok as i32);
            assert!(
                message.expect("successful poll returns JSON")["id"].is_null(),
                "a notification must not receive a JSON-RPC response"
            );
        }

        let inner = state::with_agent_read(agent_handle, |record| Ok(record.agent.inner().clone()))
            .expect("agent remains attached");
        let pending = inner.pending_elicitations();
        let (permission_tx, permission_rx) = tokio::sync::oneshot::channel();
        let permission_request = querymt_agent::acp::protocol::RequestPermissionRequest::new(
            querymt_agent::acp::protocol::SessionId::from(session_id.clone()),
            querymt_agent::acp::protocol::ToolCallUpdate::new(
                querymt_agent::acp::protocol::ToolCallId::from("ffi-tool-call"),
                querymt_agent::acp::protocol::ToolCallUpdateFields::new()
                    .status(querymt_agent::acp::protocol::ToolCallStatus::Pending),
            ),
            vec![querymt_agent::acp::protocol::PermissionOption::new(
                querymt_agent::acp::protocol::PermissionOptionId::from("allow_once"),
                "Allow once",
                querymt_agent::acp::protocol::PermissionOptionKind::AllowOnce,
            )],
        );
        querymt_agent::elicitation::register_pending_permission(
            &pending,
            "ffi-permission".to_string(),
            session_id.clone(),
            permission_request,
            Some("ffi-run".to_string()),
            Some("ffi-tool-call".to_string()),
            permission_tx,
        )
        .await;
        assert_eq!(
            ffi_acp_send(
                connection,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "load",
                    "method": "session/load",
                    "params": {
                        "sessionId": session_id,
                        "_meta": {"traceparent": "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"}
                    }
                })
                .to_string(),
            )
            .await,
            FfiErrorCode::Ok as i32
        );
        let permission_messages =
            ffi_poll_until(connection, std::time::Duration::from_secs(10), |message| {
                message["method"] == "session/request_permission"
            })
            .await;
        let load_response = permission_messages
            .iter()
            .find(|message| message["id"] == "load")
            .expect("session/load response precedes attached permission");
        assert!(
            load_response["result"]["_meta"]["querymt/sessionLoadSnapshot.v1"].is_object(),
            "mobile snapshot is carried in the session-load metadata"
        );
        assert!(load_response["result"]["configOptions"].is_array());
        let permission_message = permission_messages
            .last()
            .expect("outgoing permission request");
        let permission_result = serde_json::to_value(
            querymt_agent::acp::protocol::RequestPermissionResponse::new(
                querymt_agent::acp::protocol::RequestPermissionOutcome::Selected(
                    querymt_agent::acp::protocol::SelectedPermissionOutcome::new("allow_once"),
                ),
            ),
        )
        .expect("permission result serializes");
        assert_eq!(
            ffi_acp_send(
                connection,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": permission_message["id"],
                    "result": permission_result
                })
                .to_string(),
            )
            .await,
            FfiErrorCode::Ok as i32
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), permission_rx)
            .await
            .expect("permission answer reaches pending interaction")
            .expect("permission waiter remains connected");

        let (form_tx, form_rx) = tokio::sync::oneshot::channel();
        querymt_agent::elicitation::insert_pending_elicitation(
            &pending,
            "ffi-form".to_string(),
            session_id.clone(),
            form_tx,
        )
        .await;
        inner
            .config
            .event_sink
            .fanout()
            .publish(querymt_agent::events::EventEnvelope::Ephemeral(
                querymt_agent::events::EphemeralEvent {
                    session_seq: 0,
                    session_id: session_id.clone(),
                    timestamp: 1,
                    origin: querymt_agent::events::EventOrigin::Local,
                    source_node: None,
                    kind: querymt_agent::events::AgentEventKind::ElicitationRequested {
                        elicitation_id: "ffi-form".to_string(),
                        session_id: session_id.clone(),
                        message: "Choose one".to_string(),
                        requested_schema: serde_json::json!({"type": "object"}),
                        source: "builtin:question".to_string(),
                    },
                },
            ));
        let form_request =
            ffi_poll_until(connection, std::time::Duration::from_secs(5), |message| {
                message["method"] == "elicitation/create"
            })
            .await;
        assert_eq!(
            ffi_acp_send(
                connection,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "form-answer",
                    "method": "elicitation_result",
                    "params": {
                        "elicitation_id": "ffi-form",
                        "session_id": session_id,
                        "action": "decline"
                    }
                })
                .to_string(),
            )
            .await,
            FfiErrorCode::Ok as i32
        );
        ffi_poll_until(connection, std::time::Duration::from_secs(5), |message| {
            message["id"] == "form-answer"
        })
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(2), form_rx)
            .await
            .expect("form answer reaches pending interaction")
            .expect("form waiter remains connected");
        assert_eq!(form_request.last().unwrap()["method"], "elicitation/create");
        assert_eq!(ffi_acp_close(connection).await, FfiErrorCode::Ok as i32);

        let (unregister_code, unregister_connection) = ffi_acp_open(agent_handle).await;
        assert_eq!(unregister_code, FfiErrorCode::Ok as i32);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let unregister_state = Box::new(BlockingCallbackState {
            entered: entered_tx,
            release: StdMutex::new(release_rx),
        });
        let unregister_data = (&*unregister_state as *const BlockingCallbackState) as usize;
        assert_eq!(
            ffi_acp_set_handler(
                unregister_connection,
                Some(blocking_acp_handler),
                unregister_data,
            )
            .await,
            FfiErrorCode::Ok as i32
        );
        assert_eq!(
            ffi_acp_send(unregister_connection, initialize.to_string()).await,
            FfiErrorCode::Ok as i32
        );
        tokio::task::spawn_blocking(move || {
            entered_rx.recv_timeout(std::time::Duration::from_secs(5))
        })
        .await
        .expect("callback entry task")
        .expect("callback started");
        let mut unregister = tokio::task::spawn_blocking(move || unsafe {
            qmt_ffi_acp_set_message_handler(unregister_connection, None, std::ptr::null_mut())
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut unregister)
                .await
                .is_err()
        );
        release_tx
            .send(())
            .expect("release callback before userdata expires");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), unregister)
                .await
                .expect("unregister waits for callback")
                .expect("unregister task"),
            FfiErrorCode::Ok as i32
        );
        drop(unregister_state);
        assert_eq!(
            ffi_acp_close(unregister_connection).await,
            FfiErrorCode::Ok as i32
        );

        let (close_code, close_connection) = ffi_acp_open(agent_handle).await;
        assert_eq!(close_code, FfiErrorCode::Ok as i32);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let close_state = Box::new(BlockingCallbackState {
            entered: entered_tx,
            release: StdMutex::new(release_rx),
        });
        let close_data = (&*close_state as *const BlockingCallbackState) as usize;
        assert_eq!(
            ffi_acp_set_handler(close_connection, Some(blocking_acp_handler), close_data).await,
            FfiErrorCode::Ok as i32
        );
        assert_eq!(
            ffi_acp_send(close_connection, initialize.to_string()).await,
            FfiErrorCode::Ok as i32
        );
        tokio::task::spawn_blocking(move || {
            entered_rx.recv_timeout(std::time::Duration::from_secs(5))
        })
        .await
        .expect("callback entry task")
        .expect("callback started");
        let mut close =
            tokio::task::spawn_blocking(move || unsafe { qmt_ffi_acp_close(close_connection) });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut close)
                .await
                .is_err()
        );
        release_tx
            .send(())
            .expect("release callback before userdata expires");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), close)
                .await
                .expect("close waits for callback")
                .expect("close task"),
            FfiErrorCode::Ok as i32
        );
        drop(close_state);

        let (self_close_code, self_close_connection) = ffi_acp_open(agent_handle).await;
        assert_eq!(self_close_code, FfiErrorCode::Ok as i32);
        let (self_close_tx, self_close_rx) = std::sync::mpsc::channel();
        let self_close_data = Box::new(self_close_tx);
        let self_close_pointer =
            (&*self_close_data as *const std::sync::mpsc::Sender<i32>) as usize;
        assert_eq!(
            ffi_acp_set_handler(
                self_close_connection,
                Some(self_closing_acp_handler),
                self_close_pointer,
            )
            .await,
            FfiErrorCode::Ok as i32
        );
        assert_eq!(
            ffi_acp_send(self_close_connection, initialize.to_string()).await,
            FfiErrorCode::Ok as i32
        );
        assert_eq!(
            tokio::task::spawn_blocking(move || {
                self_close_rx.recv_timeout(std::time::Duration::from_secs(5))
            })
            .await
            .expect("self-close callback completion")
            .expect("self-close callback returned"),
            FfiErrorCode::Ok as i32
        );
        drop(self_close_data);

        assert_eq!(
            tokio::task::spawn_blocking(move || unsafe { qmt_ffi_shutdown(agent_handle) })
                .await
                .expect("shutdown test agent"),
            FfiErrorCode::Ok as i32
        );
        assert_eq!(
            tokio::task::spawn_blocking(move || unsafe { qmt_ffi_shutdown_runtime_if_idle() })
                .await
                .expect("shutdown idle test runtime"),
            FfiErrorCode::Ok as i32
        );
    }
}

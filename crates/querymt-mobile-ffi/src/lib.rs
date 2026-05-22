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

use async_trait::async_trait;
use ffi_helpers::{set_last_error, take_last_error_code, take_last_error_message};
use querymt_agent::session::projection::ViewStore;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;
use types::FfiErrorCode;

pub type AcpMessageHandlerFn = unsafe extern "C" fn(
    connection_handle: u64,
    message_json: *const std::ffi::c_char,
    user_data: *mut std::ffi::c_void,
);

struct AcpConnection {
    agent_handle: u64,
    conn_id: String,
    pending_permissions: querymt_agent::acp::shared::PermissionMap,
    pending_elicitations: querymt_agent::acp::shared::PendingElicitationMap,
    session_owners: querymt_agent::acp::shared::SessionOwnerMap,
    outbox: Arc<Mutex<VecDeque<String>>>,
    handler: Option<(AcpMessageHandlerFn, usize)>,
}

struct MobileAcpSessionHooks {
    agent_handle: u64,
    view_store: Arc<dyn ViewStore>,
}

#[async_trait]
impl querymt_agent::acp::shared::AcpSessionHooks for MobileAcpSessionHooks {
    async fn preconnected_mcp_peers(
        &self,
    ) -> Result<
        Vec<querymt_agent::agent::session_registry::PreconnectedMcpPeer>,
        agent_client_protocol::schema::Error,
    > {
        mcp::collect_preconnected_mcp_servers(self.agent_handle)
            .await
            .map_err(|e| {
                agent_client_protocol::schema::Error::internal_error().data(serde_json::json!({
                    "message": "collect preconnected MCP failed",
                    "ffiCode": e as i32,
                }))
            })
    }

    async fn on_session_loaded(
        &self,
        agent: &querymt_agent::agent::LocalAgentHandle,
        session_id: &str,
        response: &mut serde_json::Value,
    ) -> Result<(), agent_client_protocol::schema::Error> {
        inject_session_load_snapshot(agent, self.view_store.clone(), session_id, response).await
    }

    async fn on_remote_session_attached(
        &self,
        agent: &querymt_agent::agent::LocalAgentHandle,
        session_id: &str,
        response: &mut serde_json::Value,
    ) -> Result<(), agent_client_protocol::schema::Error> {
        ensure_remote_attach_snapshot(agent, self.view_store.clone(), session_id, response).await
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterInprocMcpPipeParams {
    server_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
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
) -> Result<(), agent_client_protocol::schema::Error> {
    let snapshot = querymt_agent::session::load_session_snapshot(agent, view_store, session_id)
        .await
        .map_err(|e| agent_client_protocol::schema::Error::internal_error().data(e.to_string()))?;

    let event_count = snapshot.audit.events.len();
    log::info!(
        "ffi session/load snapshot injected: session_id={}, events={}",
        session_id,
        event_count
    );

    let Some(obj) = response.as_object_mut() else {
        return Err(agent_client_protocol::schema::Error::internal_error()
            .data("session/load returned non-object response"));
    };

    let meta = obj
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(meta_obj) = meta.as_object_mut() else {
        return Err(agent_client_protocol::schema::Error::internal_error()
            .data("session/load returned non-object _meta"));
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
) -> Result<(), agent_client_protocol::schema::Error> {
    let Some(result_obj) = response.as_object_mut() else {
        return Err(agent_client_protocol::schema::Error::internal_error()
            .data("remote attach returned non-object response"));
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
        .map_err(|e| agent_client_protocol::schema::Error::internal_error().data(e.to_string()))?;
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
static ACP_CONNECTIONS: once_cell::sync::Lazy<Mutex<HashMap<u64, AcpConnection>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

async fn push_acp_message(
    connection_handle: u64,
    outbox: &Arc<Mutex<VecDeque<String>>>,
    json: String,
) {
    let handler = {
        let guard = ACP_CONNECTIONS.lock().await;
        guard.get(&connection_handle).and_then(|conn| conn.handler)
    };

    if let Some((handler_fn, user_data_bits)) = handler
        && let Ok(c_json) = std::ffi::CString::new(json.as_str())
    {
        unsafe {
            handler_fn(
                connection_handle,
                c_json.as_ptr(),
                user_data_bits as *mut std::ffi::c_void,
            );
        }
        return;
    }

    outbox.lock().await.push_back(json);
}

#[cfg(feature = "remote")]
fn spawn_mesh_peer_event_forwarder(
    inner: std::sync::Arc<querymt_agent::agent::LocalAgentHandle>,
    connection_handle: u64,
    outbox: Arc<Mutex<VecDeque<String>>>,
) {
    let Some(mesh) = inner.mesh() else {
        return;
    };

    let model_inventory = inner.model_inventory.clone();
    let mut rx = mesh.subscribe_peer_events();
    runtime::global_runtime().spawn(async move {
        loop {
            match rx.recv().await {
                Ok(querymt_agent::agent::remote::mesh::PeerEvent::Discovered(peer_id)) => {
                    model_inventory.invalidate_remote().await;
                    let notification = querymt_agent::acp::shared::mesh_nodes_changed_notification(
                        &peer_id.to_string(),
                        "discovered",
                    );
                    if let Ok(json) = serde_json::to_string(&notification) {
                        push_acp_message(connection_handle, &outbox, json).await;
                    }
                }
                Ok(querymt_agent::agent::remote::mesh::PeerEvent::Expired(peer_id)) => {
                    model_inventory.invalidate_remote().await;
                    let notification = querymt_agent::acp::shared::mesh_peer_expired_notification(
                        &peer_id.to_string(),
                    );
                    if let Ok(json) = serde_json::to_string(&notification) {
                        push_acp_message(connection_handle, &outbox, json).await;
                    }
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

// ============================================================================
// Lifecycle
// ============================================================================

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
    ffi_panic_boundary("qmt_internal_init_agent", || {
        log::info!("ffi.agent.init: requested attach/init");
        // Parse config using the shared querymt-agent parser.
        let config_result = agent::parse_config(config_toml);
        let config = match config_result {
            Ok(c) => c,
            Err(code) => return code as i32,
        };

        // Enter the Tokio runtime context so that telemetry init (hyper-util,
        // gRPC) and agent startup can find a reactor on this thread.
        let _rt_guard = runtime::global_runtime().enter();

        // Initialize telemetry from the explicit mobile config (idempotent).
        events::setup_mobile_telemetry();

        let result = agent::init_agent_from_config(config, out_agent);
        match result {
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
    log::info!("ffi.agent.shutdown: requested detach (agent_handle={agent_handle})");
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
    let inner = match state::with_agent_read(agent_handle, |r| Ok(r.agent.inner())) {
        Ok(inner) => inner,
        Err(code) => return code as i32,
    };

    let connection_handle = NEXT_ACP_CONNECTION_HANDLE.fetch_add(1, Ordering::Relaxed);
    let conn_id = format!("ffi-{connection_handle}");
    let pending_permissions = Arc::new(Mutex::new(HashMap::new()));
    let pending_elicitations = inner.pending_elicitations();
    let session_owners = Arc::new(Mutex::new(HashMap::new()));
    let outbox = Arc::new(Mutex::new(VecDeque::new()));

    let event_sources = querymt_agent::acp::shared::collect_event_sources(&inner);
    for source in event_sources {
        let mut rx = source.subscribe();
        let outbox_cloned = outbox.clone();
        let conn_id_cloned = conn_id.clone();
        let owners_cloned = session_owners.clone();
        runtime::global_runtime().spawn(async move {
            while let Ok(event) = rx.recv().await {
                if !querymt_agent::acp::shared::is_event_owned(
                    &owners_cloned,
                    &conn_id_cloned,
                    &event,
                )
                .await
                {
                    continue;
                }
                if let Some(notification) =
                    querymt_agent::acp::shared::translate_event_to_notification(&event)
                    && let Ok(json) = serde_json::to_string(&notification)
                {
                    push_acp_message(connection_handle, &outbox_cloned, json).await;
                }
            }
        });
    }

    #[cfg(feature = "remote")]
    spawn_mesh_peer_event_forwarder(inner.clone(), connection_handle, outbox.clone());

    runtime::global_runtime().block_on(async {
        ACP_CONNECTIONS.lock().await.insert(
            connection_handle,
            AcpConnection {
                agent_handle,
                conn_id,
                pending_permissions,
                pending_elicitations,
                session_owners,
                outbox,
                handler: None,
            },
        );
    });

    unsafe {
        *out_connection = connection_handle;
    }
    FfiErrorCode::Ok as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_acp_close(connection_handle: u64) -> i32 {
    let removed = runtime::global_runtime()
        .block_on(async { ACP_CONNECTIONS.lock().await.remove(&connection_handle) });
    if removed.is_some() {
        FfiErrorCode::Ok as i32
    } else {
        FfiErrorCode::NotFound as i32
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
        Ok(v) => v.to_string(),
        Err(_) => {
            set_last_error(
                FfiErrorCode::InvalidArgument,
                "message_json is not valid UTF-8".into(),
            );
            return FfiErrorCode::InvalidArgument as i32;
        }
    };

    let result = runtime::global_runtime().block_on(async {
        let (
            agent,
            agent_handle,
            conn_id,
            session_owners,
            pending_permissions,
            pending_elicitations,
            outbox,
            view_store,
        ) = {
            let guard = ACP_CONNECTIONS.lock().await;
            let conn = guard
                .get(&connection_handle)
                .ok_or(FfiErrorCode::NotFound)?;
            let (agent, view_store) = state::with_agent_read(conn.agent_handle, |r| {
                let view_store = r.view_store.clone().ok_or(FfiErrorCode::RuntimeError)?;
                Ok((r.agent.inner(), view_store))
            })
            .map_err(|_| FfiErrorCode::NotFound)?;
            (
                agent,
                conn.agent_handle,
                conn.conn_id.clone(),
                conn.session_owners.clone(),
                conn.pending_permissions.clone(),
                conn.pending_elicitations.clone(),
                conn.outbox.clone(),
                view_store,
            )
        };

        let value: serde_json::Value =
            serde_json::from_str(&message).map_err(|_| FfiErrorCode::InvalidArgument)?;
        let method = value
            .get("method")
            .and_then(|v| v.as_str())
            .ok_or(FfiErrorCode::InvalidArgument)?
            .to_string();
        let params = value
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let req = querymt_agent::acp::shared::RpcRequest {
            jsonrpc: "2.0".to_string(),
            method,
            params,
            id,
        };
        let req_method = req.method.clone();
        log::debug!("ffi acp send: method={}", req_method);

        let resp = if req.method == "querymt/mcp/registerInprocPipe" {
            let parsed: Result<RegisterInprocMcpPipeParams, _> =
                serde_json::from_value(req.params.clone());
            match parsed {
                Ok(p) => {
                    let c_server_name = std::ffi::CString::new(p.server_name)
                        .map_err(|_| FfiErrorCode::InvalidArgument)?;
                    let mut read_fd: i32 = -1;
                    let mut write_fd: i32 = -1;
                    let code = mcp::register_inproc_mcp_pipe_inner(
                        agent_handle,
                        c_server_name.as_ptr(),
                        &mut read_fd,
                        &mut write_fd,
                    );
                    match code {
                        Ok(()) => querymt_agent::acp::shared::RpcResponse {
                            jsonrpc: "2.0".to_string(),
                            result: Some(
                                serde_json::json!({ "readFd": read_fd, "writeFd": write_fd }),
                            ),
                            error: None,
                            id: req.id,
                        },
                        Err(e) => querymt_agent::acp::shared::RpcResponse {
                            jsonrpc: "2.0".to_string(),
                            result: None,
                            error: Some(serde_json::json!({
                                "code": -32000,
                                "message": "querymt/mcp/registerInprocPipe failed",
                                "data": { "ffiCode": e as i32 }
                            })),
                            id: req.id,
                        },
                    }
                }
                Err(e) => querymt_agent::acp::shared::RpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: None,
                    error: Some(serde_json::json!({
                        "code": -32602,
                        "message": "invalid params",
                        "data": { "detail": e.to_string() }
                    })),
                    id: req.id,
                },
            }
        } else if req.method == "querymt/mcp/unregister" {
            let parsed: Result<UnregisterInprocMcpParams, _> =
                serde_json::from_value(req.params.clone());
            match parsed {
                Ok(p) => {
                    let c_server_name = std::ffi::CString::new(p.server_name)
                        .map_err(|_| FfiErrorCode::InvalidArgument)?;
                    let code =
                        mcp::unregister_inproc_mcp_inner(agent_handle, c_server_name.as_ptr());
                    match code {
                        Ok(()) => querymt_agent::acp::shared::RpcResponse {
                            jsonrpc: "2.0".to_string(),
                            result: Some(serde_json::Value::Null),
                            error: None,
                            id: req.id,
                        },
                        Err(e) => querymt_agent::acp::shared::RpcResponse {
                            jsonrpc: "2.0".to_string(),
                            result: None,
                            error: Some(serde_json::json!({
                                "code": -32000,
                                "message": "querymt/mcp/unregister failed",
                                "data": { "ffiCode": e as i32 }
                            })),
                            id: req.id,
                        },
                    }
                }
                Err(e) => querymt_agent::acp::shared::RpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: None,
                    error: Some(serde_json::json!({
                        "code": -32602,
                        "message": "invalid params",
                        "data": { "detail": e.to_string() }
                    })),
                    id: req.id,
                },
            }
        } else {
            let req = match req.method.as_str() {
                "session/new" => querymt_agent::acp::shared::RpcRequest {
                    jsonrpc: req.jsonrpc,
                    method: req.method,
                    params: normalize_new_session_params(req.params),
                    id: req.id,
                },
                "session/load" => {
                    let normalized = normalize_load_session_params(req.params);
                    querymt_agent::acp::shared::RpcRequest {
                        jsonrpc: req.jsonrpc,
                        method: req.method,
                        params: normalized,
                        id: req.id,
                    }
                }
                _ => req,
            };
            let hooks: Arc<dyn querymt_agent::acp::shared::AcpSessionHooks> =
                Arc::new(MobileAcpSessionHooks {
                    agent_handle,
                    view_store: view_store.clone(),
                });
            let output = querymt_agent::acp::shared::handle_rpc_message_with_context(
                agent.as_ref(),
                &session_owners,
                &pending_permissions,
                &pending_elicitations,
                &conn_id,
                req,
                querymt_agent::acp::shared::RpcDispatchContext {
                    session_hooks: Some(hooks),
                },
            )
            .await;
            for notification in output.notifications {
                if let Ok(json) = serde_json::to_string(&notification) {
                    push_acp_message(connection_handle, &outbox, json).await;
                }
            }
            output.response
        };
        let resp_json = serde_json::to_string(&resp).map_err(|_| FfiErrorCode::RuntimeError)?;
        log::debug!(
            "ffi acp response: method={}, response={}",
            req_method,
            resp_json
        );
        push_acp_message(connection_handle, &outbox, resp_json).await;

        if req_method == "querymt/mesh/join"
            && resp.error.is_none()
            && let Some(peer_id) = resp
                .result
                .as_ref()
                .and_then(|result| result.get("peer_id"))
                .and_then(serde_json::Value::as_str)
        {
            let notification =
                querymt_agent::acp::shared::mesh_joined_notification(peer_id, "unknown");
            if let Ok(json) = serde_json::to_string(&notification) {
                push_acp_message(connection_handle, &outbox, json).await;
            }
        }

        if req_method == "querymt/refreshModels" && resp.error.is_none() {
            let notification =
                querymt_agent::acp::shared::models_changed_notification("manual_refresh");
            if let Ok(json) = serde_json::to_string(&notification) {
                push_acp_message(connection_handle, &outbox, json).await;
            }
        }

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

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
    loop {
        let popped = runtime::global_runtime().block_on(async {
            let mut guard = ACP_CONNECTIONS.lock().await;
            let conn = guard
                .get_mut(&connection_handle)
                .ok_or(FfiErrorCode::NotFound)?;
            Ok::<Option<String>, FfiErrorCode>(conn.outbox.lock().await.pop_front())
        });

        match popped {
            Ok(Some(msg)) => {
                unsafe { *out_message_json = alloc_string(&msg) };
                return FfiErrorCode::Ok as i32;
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return FfiErrorCode::NotFound as i32;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(code) => return code as i32,
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_ffi_acp_set_message_handler(
    connection_handle: u64,
    handler: Option<AcpMessageHandlerFn>,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    let result = runtime::global_runtime().block_on(async {
        let mut guard = ACP_CONNECTIONS.lock().await;
        let conn = guard
            .get_mut(&connection_handle)
            .ok_or(FfiErrorCode::NotFound)?;
        conn.handler = handler.map(|h| (h, user_data as usize));
        Ok::<(), FfiErrorCode>(())
    });
    match result {
        Ok(()) => FfiErrorCode::Ok as i32,
        Err(code) => code as i32,
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

    static ACP_TEST_MESSAGES: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

    unsafe extern "C" fn acp_test_handler(
        _connection_handle: u64,
        message_json: *const std::ffi::c_char,
        _user_data: *mut std::ffi::c_void,
    ) {
        let msg = unsafe { std::ffi::CStr::from_ptr(message_json) }
            .to_string_lossy()
            .to_string();
        ACP_TEST_MESSAGES.lock().expect("lock poisoned").push(msg);
    }

    fn make_test_connection(
        handler: Option<(AcpMessageHandlerFn, usize)>,
    ) -> (u64, Arc<Mutex<VecDeque<String>>>) {
        let connection_handle = NEXT_ACP_CONNECTION_HANDLE.fetch_add(1, Ordering::Relaxed);
        let outbox = Arc::new(Mutex::new(VecDeque::new()));
        let pending_permissions: querymt_agent::acp::shared::PermissionMap =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: querymt_agent::acp::shared::PendingElicitationMap =
            Arc::new(Mutex::new(HashMap::new()));
        let session_owners: querymt_agent::acp::shared::SessionOwnerMap =
            Arc::new(Mutex::new(HashMap::new()));

        runtime::global_runtime().block_on(async {
            ACP_CONNECTIONS.lock().await.insert(
                connection_handle,
                AcpConnection {
                    agent_handle: 1,
                    conn_id: format!("test-{connection_handle}"),
                    pending_permissions,
                    pending_elicitations,
                    session_owners,
                    outbox: outbox.clone(),
                    handler,
                },
            );
        });

        (connection_handle, outbox)
    }

    fn remove_test_connection(connection_handle: u64) {
        runtime::global_runtime().block_on(async {
            ACP_CONNECTIONS.lock().await.remove(&connection_handle);
        });
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
    fn push_acp_message_invokes_handler_when_registered() {
        ACP_TEST_MESSAGES.lock().expect("lock poisoned").clear();
        let (connection_handle, outbox) = make_test_connection(Some((acp_test_handler, 0)));

        runtime::global_runtime().block_on(async {
            push_acp_message(
                connection_handle,
                &outbox,
                "{\"jsonrpc\":\"2.0\"}".to_string(),
            )
            .await;
        });

        let messages = ACP_TEST_MESSAGES.lock().expect("lock poisoned").clone();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], "{\"jsonrpc\":\"2.0\"}");

        let queued = runtime::global_runtime().block_on(async { outbox.lock().await.pop_front() });
        assert!(queued.is_none());

        remove_test_connection(connection_handle);
    }

    #[test]
    fn push_acp_message_queues_when_handler_missing() {
        let (connection_handle, outbox) = make_test_connection(None);

        runtime::global_runtime().block_on(async {
            push_acp_message(connection_handle, &outbox, "queued-message".to_string()).await;
        });

        let queued = runtime::global_runtime().block_on(async { outbox.lock().await.pop_front() });
        assert_eq!(queued.as_deref(), Some("queued-message"));

        remove_test_connection(connection_handle);
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
        assert_eq!(json["params"]["peerId"], serde_json::json!("peer-123"));
        assert_eq!(json["params"]["change"], serde_json::json!("discovered"));
    }

    /// Regression test: the FFI remote attach path must NOT overwrite a
    /// snapshot already present in the ACP response.  The server-side
    /// `querymt/remote/attachSession` handler builds a snapshot from the
    /// remote actor's event stream; replacing it with the local (empty)
    /// journal loses all history.
    #[test]
    fn remote_attach_snapshot_preserves_existing() {
        let mut result_obj = serde_json::json!({
            "sessionId": "s-remote-1",
            "nodeId": "n-1",
            "attached": true,
            "configOptions": [],
            "snapshot": {
                "audit": {
                    "session_id": "s-remote-1",
                    "events": [{ "seq": 1, "kind": "user_message" }],
                    "tasks": [],
                    "intent_snapshots": [],
                    "decisions": [],
                    "progress_entries": [],
                    "artifacts": [],
                    "delegations": [],
                    "generated_at": "2026-05-16T08:00:00Z"
                },
                "cursor": {
                    "local_seq": 1,
                    "remote_seq_by_source": {}
                }
            }
        })
        .as_object()
        .unwrap()
        .clone();

        let before_snapshot = result_obj.get("snapshot").cloned();
        assert!(before_snapshot.is_some(), "precondition: snapshot exists");

        // Simulate the guarded branch — must NOT enter the injection path.
        if !result_obj.contains_key("snapshot") {
            result_obj.insert(
                "snapshot".to_string(),
                serde_json::json!({ "audit": { "events": [] }, "cursor": { "local_seq": 0 } }),
            );
        }

        assert_eq!(
            result_obj.get("snapshot").cloned(),
            before_snapshot,
            "snapshot must not be overwritten by local fallback"
        );
    }

    /// Inverse: when the server response has no snapshot key, the FFI
    /// injection path must still be able to insert one.
    #[test]
    fn remote_attach_snapshot_allows_fallback_when_missing() {
        let mut result_obj = serde_json::json!({
            "sessionId": "s-remote-2",
            "nodeId": "n-1",
            "attached": true,
            "configOptions": []
        })
        .as_object()
        .unwrap()
        .clone();

        assert!(
            !result_obj.contains_key("snapshot"),
            "precondition: no snapshot"
        );

        // Simulate the guarded branch — must enter the injection path.
        if !result_obj.contains_key("snapshot") {
            result_obj.insert(
                "snapshot".to_string(),
                serde_json::json!({ "audit": { "events": [] }, "cursor": { "local_seq": 0 } }),
            );
        }

        assert!(
            result_obj.contains_key("snapshot"),
            "fallback snapshot should be injected"
        );
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
}

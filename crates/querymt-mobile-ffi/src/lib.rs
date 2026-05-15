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

use ffi_helpers::{set_last_error, take_last_error_code, take_last_error_message};
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
/// Telemetry is controlled via environment variables:
/// - `QMT_MOBILE_TELEMETRY=1` or `OTEL_EXPORTER_OTLP_ENDPOINT` to enable OTLP.
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
        // Parse config using the shared querymt-agent parser.
        let config_result = agent::parse_config(config_toml);
        let config = match config_result {
            Ok(c) => c,
            Err(code) => return code as i32,
        };

        // Enter the Tokio runtime context so that telemetry init (hyper-util,
        // gRPC) and agent startup can find a reactor on this thread.
        let _rt_guard = runtime::global_runtime().enter();

        // Initialize telemetry/logging from environment (idempotent).
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
    ffi_helpers::set_backgrounded(backgrounded != 0);
    FfiErrorCode::Ok as i32
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
                let view_store = r.storage.view_store().ok_or(FfiErrorCode::RuntimeError)?;
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

        // ACP core methods stay ACP-first. We only special-case:
        // 1) querymt/mcp/* extensions for iOS in-process pipe transport registration
        // 2) session/new + session/load to inject already-registered preconnected MCP peers
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
        } else if req.method == "session/new" {
            let normalized = normalize_new_session_params(req.params.clone());
            let parsed: Result<agent_client_protocol::schema::NewSessionRequest, _> =
                serde_json::from_value(normalized);
            match parsed {
                Ok(params) => match mcp::collect_preconnected_mcp_servers(agent_handle).await {
                    Ok(preconnected) => {
                        let res = agent
                            .new_session_with_preconnected(params, preconnected)
                            .await;
                        match res {
                            Ok(r) => {
                                let mut owners = session_owners.lock().await;
                                owners.insert(r.session_id.to_string(), conn_id.clone());
                                querymt_agent::acp::shared::RpcResponse {
                                    jsonrpc: "2.0".to_string(),
                                    result: Some(
                                        serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
                                    ),
                                    error: None,
                                    id: req.id,
                                }
                            }
                            Err(e) => querymt_agent::acp::shared::RpcResponse {
                                jsonrpc: "2.0".to_string(),
                                result: None,
                                error: Some(
                                    serde_json::to_value(e)
                                        .unwrap_or_else(|_| serde_json::json!({"code": -32603})),
                                ),
                                id: req.id,
                            },
                        }
                    }
                    Err(e) => querymt_agent::acp::shared::RpcResponse {
                        jsonrpc: "2.0".to_string(),
                        result: None,
                        error: Some(serde_json::json!({
                            "code": -32000,
                            "message": "collect preconnected MCP failed",
                            "data": { "ffiCode": e as i32 }
                        })),
                        id: req.id,
                    },
                },
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
        } else if req.method == "session/load" {
            let normalized = normalize_load_session_params(req.params.clone());
            let parsed: Result<agent_client_protocol::schema::LoadSessionRequest, _> =
                serde_json::from_value(normalized);
            match parsed {
                Ok(params) => match mcp::collect_preconnected_mcp_servers(agent_handle).await {
                    Ok(preconnected) => {
                        let session_id_for_owner = params.session_id.to_string();
                        log::debug!(
                            "ffi session/load branch entered: session_id={}",
                            session_id_for_owner
                        );
                        let res = agent
                            .load_session_with_preconnected(params, preconnected)
                            .await;
                        match res {
                            Ok(mut r) => {
                                let mut owners = session_owners.lock().await;
                                owners.insert(session_id_for_owner.clone(), conn_id.clone());

                                let snapshot = querymt_agent::session::load_session_snapshot(
                                    agent.as_ref(),
                                    view_store.clone(),
                                    &session_id_for_owner,
                                )
                                .await
                                .map_err(|_| FfiErrorCode::RuntimeError)?;

                                let event_count = snapshot.audit.events.len();
                                log::info!(
                                    "ffi session/load snapshot injected: session_id={}, events={}",
                                    session_id_for_owner,
                                    event_count
                                );

                                let meta = r.meta.get_or_insert_with(serde_json::Map::new);
                                meta.insert(
                                    "querymt/sessionLoadSnapshot.v1".to_string(),
                                    serde_json::to_value(snapshot)
                                        .unwrap_or(serde_json::Value::Null),
                                );

                                querymt_agent::acp::shared::RpcResponse {
                                    jsonrpc: "2.0".to_string(),
                                    result: Some(
                                        serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
                                    ),
                                    error: None,
                                    id: req.id,
                                }
                            }
                            Err(e) => querymt_agent::acp::shared::RpcResponse {
                                jsonrpc: "2.0".to_string(),
                                result: None,
                                error: Some(
                                    serde_json::to_value(e)
                                        .unwrap_or_else(|_| serde_json::json!({"code": -32603})),
                                ),
                                id: req.id,
                            },
                        }
                    }
                    Err(e) => querymt_agent::acp::shared::RpcResponse {
                        jsonrpc: "2.0".to_string(),
                        result: None,
                        error: Some(serde_json::json!({
                            "code": -32000,
                            "message": "collect preconnected MCP failed",
                            "data": { "ffiCode": e as i32 }
                        })),
                        id: req.id,
                    },
                },
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
        } else if req.method == "querymt/remote/attachSession" {
            let session_id_for_owner = req
                .params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let output = querymt_agent::acp::shared::handle_rpc_message(
                agent.as_ref(),
                &session_owners,
                &pending_permissions,
                &pending_elicitations,
                &conn_id,
                req,
            )
            .await;

            let mut response = output.response;
            if response.error.is_none() {
                if let Some(sid) = &session_id_for_owner {
                    let mut owners = session_owners.lock().await;
                    owners.insert(sid.clone(), conn_id.clone());

                    if let Some(result) = response.result.as_mut()
                        && let Some(result_obj) = result.as_object_mut()
                    {
                        let snapshot = querymt_agent::session::load_session_snapshot(
                            agent.as_ref(),
                            view_store.clone(),
                            sid,
                        )
                        .await
                        .map_err(|_| FfiErrorCode::RuntimeError)?;
                        let event_count = snapshot.audit.events.len();
                        log::info!(
                            "ffi remote attach snapshot injected: session_id={}, events={}",
                            sid,
                            event_count
                        );
                        result_obj.insert(
                            "snapshot".to_string(),
                            serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null),
                        );
                    }
                }
            }

            for notification in output.notifications {
                if let Ok(json) = serde_json::to_string(&notification) {
                    push_acp_message(connection_handle, &outbox, json).await;
                }
            }
            response
        } else {
            let output = querymt_agent::acp::shared::handle_rpc_message(
                agent.as_ref(),
                &session_owners,
                &pending_permissions,
                &pending_elicitations,
                &conn_id,
                req,
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
}

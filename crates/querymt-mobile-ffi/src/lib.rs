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
//! `qmt_mobile_last_error_code()` and `qmt_mobile_last_error_message()`.
//!
//! ## Memory
//!
//! C strings returned by Rust are owned by the caller and must be freed with
//! `qmt_mobile_free_string`.

pub mod events;
pub mod ffi_helpers;
pub mod runtime;
pub mod state;
pub mod types;

mod agent;
mod mcp;
mod mesh;
mod models;
mod prompt;
mod providers;
mod session;

use ffi_helpers::{set_last_error, take_last_error_code, take_last_error_message};
use types::FfiErrorCode;

// ============================================================================
// Lifecycle
// ============================================================================

/// Initialize the agent runtime. Call once per agent instance.
///
/// `config_json` is a JSON representation of a mobile agent config.
/// On success, `*out_agent` is set to an opaque handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_init_agent(
    config_json: *const std::ffi::c_char,
    out_agent: *mut u64,
) -> i32 {
    events::ensure_logger();

    let result = agent::init_agent_inner(config_json, out_agent);
    match result {
        Ok(()) => {
            ffi_helpers::clear_last_error();
            FfiErrorCode::Ok as i32
        }
        Err(code) => code as i32,
    }
}

/// Shut down an agent and release all resources owned by the handle.
///
/// Returns `QMT_MOBILE_BUSY` if the agent has active FFI calls.
/// Is idempotent only after the first successful shutdown; later calls with the
/// same stale handle return `QMT_MOBILE_NOT_FOUND`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_shutdown(agent_handle: u64) -> i32 {
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
/// operations return `QMT_MOBILE_INVALID_STATE` while backgrounded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_set_backgrounded(backgrounded: i32) -> i32 {
    ffi_helpers::set_backgrounded(backgrounded != 0);
    FfiErrorCode::Ok as i32
}

// ============================================================================
// Sessions
// ============================================================================

/// Create a new local session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_create_session(
    agent_handle: u64,
    options_json: *const std::ffi::c_char,
    out_session: *mut u64,
) -> i32 {
    let result = session::create_session_inner(agent_handle, options_json, out_session);
    ffi_result_code(result)
}

/// Load an existing local session from persistent storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_load_session(
    agent_handle: u64,
    session_id: *const std::ffi::c_char,
    out_session: *mut u64,
) -> i32 {
    let result = session::load_session_inner(agent_handle, session_id, out_session);
    ffi_result_code(result)
}

/// List persisted local sessions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_list_sessions(
    agent_handle: u64,
    out_json: *mut *mut std::ffi::c_char,
) -> i32 {
    let result = session::list_sessions_inner(agent_handle, out_json);
    ffi_result_code(result)
}

/// Delete a local session and all associated FFI session handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_delete_session(
    agent_handle: u64,
    session_id: *const std::ffi::c_char,
) -> i32 {
    let result = session::delete_session_inner(agent_handle, session_id);
    ffi_result_code(result)
}

// ============================================================================
// Remote Mesh
// ============================================================================

/// List local and reachable remote mesh nodes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_list_nodes(
    agent_handle: u64,
    out_json: *mut *mut std::ffi::c_char,
) -> i32 {
    let result = mesh::list_nodes_inner(agent_handle, out_json);
    ffi_result_code(result)
}

/// Create a session on a specific node. Null/empty node_id creates local session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_create_session_on_node(
    agent_handle: u64,
    node_id: *const std::ffi::c_char,
    options_json: *const std::ffi::c_char,
    out_session: *mut u64,
) -> i32 {
    let result =
        mesh::create_session_on_node_inner(agent_handle, node_id, options_json, out_session);
    ffi_result_code(result)
}

/// List sessions available on a remote node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_list_remote_sessions(
    agent_handle: u64,
    node_id: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> i32 {
    let result = mesh::list_remote_sessions_inner(agent_handle, node_id, out_json);
    ffi_result_code(result)
}

/// Attach/resume an existing remote session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_attach_remote_session(
    agent_handle: u64,
    node_id: *const std::ffi::c_char,
    session_id: *const std::ffi::c_char,
    out_session: *mut u64,
) -> i32 {
    let result = mesh::attach_remote_session_inner(agent_handle, node_id, session_id, out_session);
    ffi_result_code(result)
}

/// Create an invite token.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_create_invite(
    agent_handle: u64,
    options_json: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> i32 {
    let result = mesh::create_invite_inner(agent_handle, options_json, out_json);
    ffi_result_code(result)
}

/// Join a mesh from an invite token after agent initialization.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_join_mesh(
    agent_handle: u64,
    invite_token: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> i32 {
    let result = mesh::join_mesh_inner(agent_handle, invite_token, out_json);
    ffi_result_code(result)
}

/// Return local mesh state for UI/debugging.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_mesh_status(
    agent_handle: u64,
    out_json: *mut *mut std::ffi::c_char,
) -> i32 {
    let result = mesh::mesh_status_inner(agent_handle, out_json);
    ffi_result_code(result)
}

// ============================================================================
// Prompt & Events
// ============================================================================

/// Send a user prompt to an active session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_prompt(
    agent_handle: u64,
    session_handle: u64,
    content_json: *const std::ffi::c_char,
    request_id: *const std::ffi::c_char,
) -> i32 {
    let result = prompt::prompt_inner(agent_handle, session_handle, content_json, request_id);
    ffi_result_code(result)
}

/// Cancel active execution in a session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_cancel(agent_handle: u64, session_handle: u64) -> i32 {
    let result = prompt::cancel_inner(agent_handle, session_handle);
    ffi_result_code(result)
}

/// Get persisted session history.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_get_session_history(
    agent_handle: u64,
    session_id: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> i32 {
    let result = prompt::get_session_history_inner(agent_handle, session_id, out_json);
    ffi_result_code(result)
}

/// Get durable agent events for a session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_get_session_events(
    agent_handle: u64,
    session_id: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> i32 {
    let result = prompt::get_session_events_inner(agent_handle, session_id, out_json);
    ffi_result_code(result)
}

// ============================================================================
// Models & Providers
// ============================================================================

/// List available local and mesh-routable models.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_list_models(
    agent_handle: u64,
    out_json: *mut *mut std::ffi::c_char,
    traceparent: *const std::ffi::c_char,
) -> i32 {
    let result = models::list_models_inner(agent_handle, out_json, traceparent);
    ffi_result_code(result)
}

/// Set the model/provider for a session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_set_session_model(
    agent_handle: u64,
    session_handle: u64,
    provider: *const std::ffi::c_char,
    model: *const std::ffi::c_char,
    node_id: *const std::ffi::c_char,
) -> i32 {
    let result =
        models::set_session_model_inner(agent_handle, session_handle, provider, model, node_id);
    ffi_result_code(result)
}

// ============================================================================
// MCP Server Registration
// ============================================================================

/// Register an in-process MCP server via callback functions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_register_inproc_mcp(
    agent_handle: u64,
    server_name: *const std::ffi::c_char,
    handler: Option<events::McpHandlerFn>,
    free_response: Option<events::McpFreeFn>,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    let result = mcp::register_inproc_mcp_inner(
        agent_handle,
        server_name,
        handler,
        free_response,
        user_data,
    );
    ffi_result_code(result)
}

/// Register an in-process MCP server via platform pipes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_register_inproc_mcp_pipe(
    agent_handle: u64,
    server_name: *const std::ffi::c_char,
    out_read_fd: *mut i32,
    out_write_fd: *mut i32,
) -> i32 {
    let result =
        mcp::register_inproc_mcp_pipe_inner(agent_handle, server_name, out_read_fd, out_write_fd);
    ffi_result_code(result)
}

/// Unregister a previously registered MCP server.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_unregister_inproc_mcp(
    agent_handle: u64,
    server_name: *const std::ffi::c_char,
) -> i32 {
    let result = mcp::unregister_inproc_mcp_inner(agent_handle, server_name);
    ffi_result_code(result)
}

// ============================================================================
// Callbacks
// ============================================================================

/// Set the event handler callback for an agent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_set_event_handler(
    agent_handle: u64,
    handler: Option<events::EventHandlerFn>,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    use state::with_agent;

    if handler.is_none() && !user_data.is_null() {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "Non-null user_data with null handler".into(),
        );
        return FfiErrorCode::InvalidArgument as i32;
    }

    let result = with_agent(agent_handle, |record| {
        if let Some(handler_fn) = handler {
            record.event_callbacks = Some(events::EventCallbacks {
                handler: handler_fn,
                user_data,
            });
            // Wire up the event subscription
            let mut rx = record.agent.subscribe();
            let agent_handle = agent_handle;
            let user_data_bits = user_data as usize;
            std::thread::spawn(move || {
                let user_data = user_data_bits as *mut std::ffi::c_void;
                while let Ok(envelope) = rx.blocking_recv() {
                    let session_id = envelope.session_id().to_string();
                    let session_handle =
                        state::find_session_handle_by_id(agent_handle, &session_id)
                            .ok()
                            .flatten();
                    let session_meta = state::find_session_by_id(agent_handle, &session_id)
                        .ok()
                        .flatten();
                    let wrapped = types::EventEnvelope {
                        session_id: Some(session_id),
                        session_handle,
                        is_remote: session_meta.as_ref().map(|s| s.is_remote).unwrap_or(false),
                        node_id: session_meta.and_then(|s| s.node_id),
                        request_id: None,
                        event: serde_json::to_value(&envelope).unwrap_or(serde_json::Value::Null),
                    };
                    if let Ok(json) = serde_json::to_string(&wrapped) {
                        let c_json = std::ffi::CString::new(json).unwrap_or_default();
                        unsafe {
                            handler_fn(
                                agent_handle,
                                session_handle.unwrap_or(0),
                                c_json.as_ptr(),
                                user_data,
                            )
                        };
                    }
                }
            });
        } else {
            record.event_callbacks = None;
        }
        Ok(())
    });

    match result {
        Ok(_) => {
            ffi_helpers::clear_last_error();
            FfiErrorCode::Ok as i32
        }
        Err(code) => {
            set_last_error(code, format!("Agent handle {} not found", agent_handle));
            code as i32
        }
    }
}

/// Set the global log handler callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_set_log_handler(
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
// Error Reporting & Memory
// ============================================================================

/// Return the last error code for the calling thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_last_error_code() -> i32 {
    take_last_error_code() as i32
}

/// Return a human-readable name for an error code.
/// Caller must free the returned string with `qmt_mobile_free_string`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_error_name(error_code: i32) -> *mut std::ffi::c_char {
    match error_code {
        0 => alloc_string("QMT_MOBILE_OK"),
        1 => alloc_string("QMT_MOBILE_INVALID_ARGUMENT"),
        2 => alloc_string("QMT_MOBILE_NOT_FOUND"),
        3 => alloc_string("QMT_MOBILE_RUNTIME_ERROR"),
        4 => alloc_string("QMT_MOBILE_UNSUPPORTED"),
        5 => alloc_string("QMT_MOBILE_ALREADY_EXISTS"),
        6 => alloc_string("QMT_MOBILE_BUSY"),
        7 => alloc_string("QMT_MOBILE_INVALID_STATE"),
        _ => alloc_string("QMT_MOBILE_UNKNOWN"),
    }
}

/// Return the last error message for the calling thread.
/// Caller must free the returned string with `qmt_mobile_free_string`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_last_error_message() -> *mut std::ffi::c_char {
    alloc_string(&take_last_error_message())
}

/// Free a string allocated by the FFI layer. NULL is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qmt_mobile_free_string(ptr: *mut std::ffi::c_char) {
    if !ptr.is_null() {
        unsafe {
            let _ = std::ffi::CString::from_raw(ptr);
        }
    }
}

// ============================================================================
// Internal Helpers
// ============================================================================

/// Convert a `Result<(), FfiErrorCode>` to an i32 return code.
fn ffi_result_code(result: Result<(), FfiErrorCode>) -> i32 {
    match result {
        Ok(()) => {
            ffi_helpers::clear_last_error();
            FfiErrorCode::Ok as i32
        }
        Err(code) => code as i32,
    }
}

/// Allocate a C string managed by the caller.
fn alloc_string(s: &str) -> *mut std::ffi::c_char {
    std::ffi::CString::new(s)
        .unwrap_or_else(|_| std::ffi::CString::new("").unwrap())
        .into_raw()
}

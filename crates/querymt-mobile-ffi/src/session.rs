//! Session management: create, load, list, delete.

use crate::ffi_helpers::{check_not_backgrounded, set_last_error};
use crate::mcp;
use crate::runtime::global_runtime;
use crate::state;
use crate::types::{FfiErrorCode, SessionListResponse, SessionOptions, SessionSummary};
use agent_client_protocol::schema::{LoadSessionRequest, NewSessionRequest};
use querymt_agent::send_agent::SendAgent;
use std::ffi::CStr;

pub fn create_session_inner(
    agent_handle: u64,
    options_json: *const std::ffi::c_char,
    out_session: *mut u64,
) -> Result<(), FfiErrorCode> {
    create_session_with_id_inner(
        agent_handle,
        options_json,
        out_session,
        std::ptr::null_mut(),
    )
}

/// Create a new local session, optionally returning the real session ID.
///
/// When `out_session_id` is non-null, the caller must free the returned string
/// with `qmt_mobile_free_string`.
pub fn create_session_with_id_inner(
    agent_handle: u64,
    options_json: *const std::ffi::c_char,
    out_session: *mut u64,
    out_session_id: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if out_session.is_null() {
        set_last_error(FfiErrorCode::InvalidArgument, "out_session is null".into());
        return Err(FfiErrorCode::InvalidArgument);
    }

    let options = parse_session_options(options_json)?;

    let runtime = global_runtime();
    runtime.block_on(async {
        let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

        let mcp_servers = mcp::registered_mcp_servers(agent_handle);
        let req = match &options.cwd {
            Some(cwd) => {
                NewSessionRequest::new(std::path::PathBuf::from(cwd)).mcp_servers(mcp_servers)
            }
            None => NewSessionRequest::new(std::path::PathBuf::new()).mcp_servers(mcp_servers),
        };

        let agent_trait: &dyn querymt_agent::agent::handle::AgentHandle = agent.as_ref();
        let response = agent_trait.new_session(req).await.map_err(|e| {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("Failed to create session: {e}"),
            );
            FfiErrorCode::RuntimeError
        })?;

        let session_id = response.session_id.to_string();

        // Apply provider/model overrides if provided
        if let (Some(provider), Some(model)) = (&options.provider, &options.model) {
            if let Err(e) = agent.set_provider(&session_id, provider, model).await {
                log::warn!("Failed to set session provider/model: {e}");
            }
        }

        let s_handle =
            state::register_session(agent_handle, session_id.clone(), false, None, None)?;
        unsafe {
            *out_session = s_handle;
            if !out_session_id.is_null() {
                *out_session_id = alloc_cstr(&session_id);
            }
        }
        Ok(())
    })
}

pub fn load_session_inner(
    agent_handle: u64,
    session_id: *const std::ffi::c_char,
    out_session: *mut u64,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if session_id.is_null() || out_session.is_null() {
        return Err(invalid_arg("Null pointer"));
    }

    let sid = cstr_to_string(session_id)?;
    let runtime = global_runtime();
    runtime.block_on(async {
        let store = state::with_agent_read(agent_handle, |r| Ok(r.storage.session_store()))?;
        let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

        let session = store.get_session(&sid).await.map_err(|e| {
            set_last_error(FfiErrorCode::RuntimeError, format!("Storage error: {e}"));
            FfiErrorCode::RuntimeError
        })?;

        match session {
            Some(_) => {
                // Pass registered MCP servers to the load request so they get attached
                let mcp_servers = mcp::registered_mcp_servers(agent_handle);
                let sid_for_req = sid.clone();
                let load_req = LoadSessionRequest::new(sid_for_req, std::path::PathBuf::new())
                    .mcp_servers(mcp_servers);
                let _ = agent.load_session(load_req).await.map_err(|e| {
                    log::warn!("Failed to attach MCP servers to loaded session: {e}");
                });

                let s_handle = state::register_session(agent_handle, sid, false, None, None)?;
                unsafe {
                    *out_session = s_handle;
                }
                Ok(())
            }
            None => {
                set_last_error(FfiErrorCode::NotFound, format!("Session not found: {sid}"));
                Err(FfiErrorCode::NotFound)
            }
        }
    })
}

pub fn list_sessions_inner(
    agent_handle: u64,
    out_json: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    if out_json.is_null() {
        return Err(invalid_arg("out_json is null"));
    }

    let runtime = global_runtime();
    runtime.block_on(async {
        let store = state::with_agent_read(agent_handle, |r| Ok(r.storage.session_store()))?;

        let sessions = store.list_sessions().await.map_err(|e| {
            set_last_error(FfiErrorCode::RuntimeError, format!("Storage error: {e}"));
            FfiErrorCode::RuntimeError
        })?;

        let summaries: Vec<SessionSummary> = sessions
            .into_iter()
            .map(|s| SessionSummary {
                session_id: s.public_id,
                title: s.name.unwrap_or_default(),
                created_at: s.created_at.map(|t| t.unix_timestamp()).unwrap_or(0),
                updated_at: s.updated_at.map(|t| t.unix_timestamp()).unwrap_or(0),
                runtime_state: "idle".to_string(),
                is_remote: false,
                node_id: None,
            })
            .collect();

        let json = serde_json::to_string(&SessionListResponse {
            sessions: summaries,
            next_cursor: None,
        })
        .map_err(|e| serde_err(e))?;
        unsafe {
            *out_json = alloc_cstr(&json);
        }
        Ok(())
    })
}

pub fn delete_session_inner(
    agent_handle: u64,
    session_id: *const std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if session_id.is_null() {
        return Err(invalid_arg("session_id is null"));
    }

    let sid = cstr_to_string(session_id)?;
    let runtime = global_runtime();
    runtime.block_on(async {
        let store = state::with_agent_read(agent_handle, |r| Ok(r.storage.session_store()))?;
        store.delete_session(&sid).await.map_err(|e| {
            set_last_error(FfiErrorCode::RuntimeError, format!("Storage error: {e}"));
            FfiErrorCode::RuntimeError
        })?;
        state::unregister_sessions_by_id(&sid);
        Ok(())
    })
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn invalid_arg(msg: &str) -> FfiErrorCode {
    set_last_error(FfiErrorCode::InvalidArgument, msg.into());
    FfiErrorCode::InvalidArgument
}

fn serde_err(e: serde_json::Error) -> FfiErrorCode {
    set_last_error(
        FfiErrorCode::RuntimeError,
        format!("Serialization error: {e}"),
    );
    FfiErrorCode::RuntimeError
}

fn alloc_cstr(s: &str) -> *mut std::ffi::c_char {
    std::ffi::CString::new(s).unwrap_or_default().into_raw()
}

fn cstr_to_string(ptr: *const std::ffi::c_char) -> Result<String, FfiErrorCode> {
    unsafe { CStr::from_ptr(ptr).to_str().map(|s| s.to_string()) }
        .map_err(|_| invalid_arg("Invalid UTF-8"))
}

fn parse_session_options(
    options_json: *const std::ffi::c_char,
) -> Result<SessionOptions, FfiErrorCode> {
    if options_json.is_null() {
        return Ok(SessionOptions {
            cwd: None,
            provider: None,
            model: None,
        });
    }
    let s = unsafe {
        CStr::from_ptr(options_json)
            .to_str()
            .map_err(|_| invalid_arg("options_json not valid UTF-8"))?
    };
    serde_json::from_str(s).map_err(|e| {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            format!("Failed to parse options: {e}"),
        );
        FfiErrorCode::InvalidArgument
    })
}

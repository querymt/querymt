//! Model listing and session model/provider assignment.

use crate::ffi_helpers::{check_not_backgrounded, set_last_error};
use crate::runtime::global_runtime;
use crate::state;
use crate::types::FfiErrorCode;
use agent_client_protocol::schema::{SetSessionConfigOptionRequest, SetSessionModelRequest};
use querymt_agent::send_agent::SendAgent;
use std::ffi::CStr;

pub fn list_models_inner(
    agent_handle: u64,
    out_json: *mut *mut std::ffi::c_char,
    _traceparent: *const std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    if out_json.is_null() {
        return Err(invalid_arg("out_json is null"));
    }

    let runtime = global_runtime();
    runtime.block_on(async {
        let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

        // Use the shared ModelRegistry — same path as the desktop UI.
        // Returns Vec<ModelEntry> with id, label, source, provider, model,
        // node_id, node_label, family, quant — all the fields the generated
        // TS ModelEntry type expects.
        #[cfg(feature = "remote")]
        let models = agent
            .model_registry
            .get_all_models(&agent.config, agent.mesh().as_ref())
            .await;

        #[cfg(not(feature = "remote"))]
        let models = agent.model_registry.get_all_models(&agent.config).await;

        let json = serde_json::to_string(&serde_json::json!({ "models": models }))
            .map_err(|e| serde_err(e))?;
        unsafe {
            *out_json = alloc_cstr(&json);
        }
        Ok(())
    })
}

pub fn set_session_model_inner(
    agent_handle: u64,
    session_handle: u64,
    provider: *const std::ffi::c_char,
    model: *const std::ffi::c_char,
    node_id: *const std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if provider.is_null() || model.is_null() {
        return Err(invalid_arg("provider or model is null"));
    }

    let provider_str = cstr_to_string(provider)?;
    let model_str = cstr_to_string(model)?;
    let node_id_str: Option<String> = ptr_to_opt_string(node_id);

    // Combine into "provider/model" so parse_transport_model_id() in the
    // session actor can split them correctly.  The desktop UI sends the
    // same format; bare model names fall back to the session's current
    // provider which is wrong for remote models.
    let model_id = format!("{}/{}", provider_str, model_str);

    #[cfg(not(feature = "remote"))]
    if let Some(ref nid) = node_id_str {
        if !nid.is_empty() {
            return Err(invalid_arg("Remote node routing requires 'remote' feature"));
        }
    }

    let runtime = global_runtime();
    runtime.block_on(async {
        let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;
        let session_id =
            state::with_session(agent_handle, session_handle, |s| Ok(s.session_id.clone()))?;

        // Use the combined "provider/model" identifier so the session actor
        // resolves the correct provider — matching the desktop UI path.
        let req = SetSessionModelRequest::new(session_id.clone(), model_id);

        // Route through the session ref so remote sessions and provider_node_id work.
        let session_ref = {
            let registry = agent.registry.lock().await;
            registry.get(&session_id).cloned()
        }
        .ok_or_else(|| {
            set_last_error(
                FfiErrorCode::NotFound,
                format!("Session not found: {}", session_id),
            );
            FfiErrorCode::NotFound
        })?;

        #[cfg(feature = "remote")]
        let provider_node_id = match node_id_str.as_deref() {
            Some("") | None => None,
            Some(node) => Some(
                querymt_agent::agent::remote::NodeId::parse(node).map_err(|e| {
                    set_last_error(
                        FfiErrorCode::InvalidArgument,
                        format!("invalid node_id '{}': {}", node, e),
                    );
                    FfiErrorCode::InvalidArgument
                })?,
            ),
        };
        #[cfg(not(feature = "remote"))]
        let provider_node_id = None;

        let msg = querymt_agent::agent::messages::SetSessionModel {
            req,
            provider_node_id,
        };
        session_ref
            .set_session_model_with_node(msg)
            .await
            .map_err(|e| {
                set_last_error(
                    FfiErrorCode::RuntimeError,
                    format!("Set session model failed: {e}"),
                );
                FfiErrorCode::RuntimeError
            })?;
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

fn ptr_to_opt_string(ptr: *const std::ffi::c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(ptr).to_str().ok().map(|s| s.to_string()) }
    }
}

// ─── Session Config Options ──────────────────────────────────────────────────

pub fn set_session_config_option_inner(
    agent_handle: u64,
    session_handle: u64,
    request_json: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if request_json.is_null() || out_json.is_null() {
        return Err(invalid_arg("request_json or out_json is null"));
    }

    let req_str = cstr_to_string(request_json)?;
    let req: SetSessionConfigOptionRequest = serde_json::from_str(&req_str).map_err(|e| {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            format!("Failed to parse SetSessionConfigOptionRequest: {e}"),
        );
        FfiErrorCode::InvalidArgument
    })?;

    let runtime = global_runtime();
    runtime.block_on(async {
        let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;
        let _session_id =
            state::with_session(agent_handle, session_handle, |s| Ok(s.session_id.clone()))?;

        let response = agent.set_session_config_option(req).await.map_err(|e| {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("Set session config option failed: {e}"),
            );
            FfiErrorCode::RuntimeError
        })?;

        let json = serde_json::to_string(&response).map_err(|e| serde_err(e))?;
        unsafe {
            *out_json = alloc_cstr(&json);
        }
        Ok(())
    })
}

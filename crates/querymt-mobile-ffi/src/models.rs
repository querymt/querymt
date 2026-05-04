//! Model listing and session model/provider assignment.

use crate::ffi_helpers::{check_not_backgrounded, set_last_error};
use crate::runtime::global_runtime;
use crate::state;
use crate::types::{FfiErrorCode, ModelInfo, ModelListResponse};
use agent_client_protocol::schema::SetSessionModelRequest;
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
        let (registry, agent) = state::with_agent_read(agent_handle, |r| {
            Ok((r.plugin_registry.clone(), r.agent.handle()))
        })?;

        let factories = registry.list();
        let mut models = Vec::new();

        for factory in &factories {
            let provider_name = factory.name().to_string();
            // Try to list models via the factory using the default (empty) config
            match factory.list_models("{}").await {
                Ok(model_ids) => {
                    for model_id in model_ids {
                        models.push(ModelInfo {
                            provider: provider_name.clone(),
                            model: model_id.clone(),
                            display_name: format!("{} ({})", model_id, provider_name),
                            node_id: None,
                            is_local: true,
                        });
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Failed to list models for provider '{}': {}",
                        provider_name,
                        e
                    );
                }
            }
        }

        #[cfg(feature = "remote")]
        {
            if agent.mesh().is_some() {
                let nodes = agent.list_remote_nodes().await;
                for node in &nodes {
                    let node_id_str = node.node_id.to_string();
                    if let Ok(nm_ref) = agent.find_node_manager(&node_id_str).await {
                        use querymt_agent::agent::remote::ListAvailableModels;
                        let resp = nm_ref.ask(&ListAvailableModels).await;
                        let available = match resp {
                            Ok(models) => models,
                            _ => continue,
                        };
                        for am in available {
                            models.push(ModelInfo {
                                provider: am.provider.clone(),
                                model: am.model.clone(),
                                display_name: format!("{} on {}", am.model, node_id_str),
                                node_id: Some(node_id_str.clone()),
                                is_local: false,
                            });
                        }
                    }
                }
            }
        }

        let json =
            serde_json::to_string(&ModelListResponse { models }).map_err(|e| serde_err(e))?;
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

        // Model IDs are ACP-level identifiers; for now encode provider/model as `provider:model`.
        let model_id = format!("{}:{}", provider_str, model_str);
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

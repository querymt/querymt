//! Prompt, cancel, and history retrieval.

use crate::ffi_helpers::{check_not_backgrounded, set_last_error, set_last_error_from_anyhow};
use crate::runtime::global_runtime;
use crate::state;
use crate::types::{FfiErrorCode, SessionHistoryResponse, SessionMessage};
use agent_client_protocol::schema::{CancelNotification, ContentBlock, PromptRequest, TextContent};
use querymt_agent::agent::handle::AgentHandle;
use std::ffi::CStr;

pub fn prompt_inner(
    agent_handle: u64,
    session_handle: u64,
    content_json: *const std::ffi::c_char,
    request_id: *const std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if content_json.is_null() {
        return Err(invalid_arg("content_json is null"));
    }

    let content_str = unsafe {
        CStr::from_ptr(content_json)
            .to_str()
            .map_err(|_| invalid_arg("content_json not valid UTF-8"))?
    };
    let _rid: Option<String> = ptr_to_opt_string(request_id);

    let blocks: Vec<ContentBlock> = match serde_json::from_str(content_str) {
        Ok(parsed) => parsed,
        Err(_) => vec![ContentBlock::Text(TextContent::new(content_str))],
    };

    let runtime = global_runtime();
    runtime.block_on(async {
        let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;
        let session_id =
            state::with_session(agent_handle, session_handle, |s| Ok(s.session_id.clone()))?;

        let req = PromptRequest::new(session_id, blocks);
        agent.prompt(req).await.map_err(|e| {
            set_last_error_from_anyhow(FfiErrorCode::RuntimeError, e.into());
            FfiErrorCode::RuntimeError
        })?;
        Ok(())
    })
}

pub fn cancel_inner(agent_handle: u64, session_handle: u64) -> Result<(), FfiErrorCode> {
    let runtime = global_runtime();
    runtime.block_on(async {
        let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;
        let session_id =
            state::with_session(agent_handle, session_handle, |s| Ok(s.session_id.clone()))?;

        let notif = CancelNotification::new(session_id);
        agent.cancel(notif).await.map_err(|e| {
            set_last_error_from_anyhow(FfiErrorCode::RuntimeError, e.into());
            FfiErrorCode::RuntimeError
        })
    })
}

pub fn get_session_history_inner(
    agent_handle: u64,
    session_id: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    if session_id.is_null() || out_json.is_null() {
        return Err(invalid_arg("Null pointer"));
    }

    let sid = cstr_to_string(session_id)?;
    let runtime = global_runtime();
    runtime.block_on(async {
        let store = state::with_agent_read(agent_handle, |r| Ok(r.storage.session_store()))?;

        let history = store.get_history(&sid).await.map_err(|e| {
            set_last_error(FfiErrorCode::RuntimeError, format!("Storage error: {e}"));
            FfiErrorCode::RuntimeError
        })?;

        let messages: Vec<SessionMessage> = history
            .into_iter()
            .map(|msg| {
                let role = match msg.role {
                    querymt::chat::ChatRole::User => "user",
                    querymt::chat::ChatRole::Assistant => "assistant",
                }
                .to_string();
                let parts: Vec<serde_json::Value> = msg
                    .parts
                    .iter()
                    .map(|part| serde_json::to_value(part).unwrap_or_default())
                    .collect();
                SessionMessage {
                    role,
                    parts,
                    created_at: msg.created_at,
                    message_id: msg.id,
                }
            })
            .collect();

        let json = serde_json::to_string(&SessionHistoryResponse { messages })
            .map_err(|e| serde_err(e))?;
        unsafe {
            *out_json = alloc_cstr(&json);
        }
        Ok(())
    })
}

pub fn get_session_events_inner(
    agent_handle: u64,
    session_id: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    if session_id.is_null() || out_json.is_null() {
        return Err(invalid_arg("Null pointer"));
    }

    let sid = cstr_to_string(session_id)?;
    let runtime = global_runtime();
    runtime.block_on(async {
        let journal = state::with_agent_read(agent_handle, |r| Ok(r.storage.event_journal()))?;

        let events = journal
            .load_session_stream(&sid, None, None)
            .await
            .map_err(|e| {
                set_last_error(
                    FfiErrorCode::RuntimeError,
                    format!("Event journal error: {e}"),
                );
                FfiErrorCode::RuntimeError
            })?;

        let json = serde_json::to_string(&events).map_err(|e| serde_err(e))?;
        unsafe {
            *out_json = alloc_cstr(&json);
        }
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

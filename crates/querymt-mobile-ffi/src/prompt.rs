//! Prompt, cancel, and history retrieval.

use crate::ffi_helpers::{check_not_backgrounded, set_last_error, set_last_error_from_anyhow};
use crate::runtime::global_runtime;
use crate::state;
use crate::types::{FfiErrorCode, SessionHistoryResponse, SessionMessage};
use agent_client_protocol::schema::{CancelNotification, ContentBlock, PromptRequest, TextContent};
use querymt_agent::agent::handle::AgentHandle;
use querymt_agent::agent::remote::actor_ref::SessionActorRef;
use querymt_agent::model::MessagePart;
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
                    .filter_map(message_part_to_acp_block)
                    .filter_map(|block| serde_json::to_value(block).ok())
                    .collect();
                SessionMessage {
                    role,
                    parts,
                    created_at: msg.created_at,
                    message_id: msg.id,
                }
            })
            .collect();

        let json =
            serde_json::to_string(&SessionHistoryResponse { messages }).map_err(serde_err)?;
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

        let json = serde_json::to_string(&events).map_err(serde_err)?;
        unsafe {
            *out_json = alloc_cstr(&json);
        }
        Ok(())
    })
}

/// Get the full durable event stream for a session from the attached session
/// actor (works for both local and remote sessions).
///
/// This is the mobile equivalent of `session_ref.get_event_stream()` used by
/// the desktop attach handler — it queries the session actor via the registry
/// rather than reading a local storage backend. For remote sessions, the
/// request is forwarded over the mesh to the remote session actor.
pub fn get_remote_session_events_inner(
    agent_handle: u64,
    session_id: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if session_id.is_null() || out_json.is_null() {
        return Err(invalid_arg("Null pointer"));
    }

    let sid = cstr_to_string(session_id)?;
    let runtime = global_runtime();
    runtime.block_on(async {
        let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

        // Look up the session in the kameo session registry
        let session_ref = {
            let registry = agent.registry.lock().await;
            registry.get(&sid).cloned().ok_or_else(|| {
                set_last_error(
                    FfiErrorCode::NotFound,
                    format!("Session {sid} not found in registry"),
                );
                FfiErrorCode::NotFound
            })
        }?;

        // Get the full event stream from the session actor
        let events = session_ref.get_event_stream().await.map_err(|e| {
            set_last_error(
                FfiErrorCode::RuntimeError,
                format!("Failed to get event stream: {e}"),
            );
            FfiErrorCode::RuntimeError
        })?;

        log::info!(
            "[get_remote_session_events] session={sid} returned {} events",
            events.len()
        );

        let json = serde_json::to_string(&events).map_err(serde_err)?;
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

/// Convert a stored `MessagePart` into an ACP `ContentBlock` for mobile history.
///
/// Only displayable parts are converted; tool-use, snapshot, and patch parts
/// are dropped since the mobile chat UI does not render them from history.
fn message_part_to_acp_block(part: &MessagePart) -> Option<ContentBlock> {
    match part {
        MessagePart::Text { content } => {
            Some(ContentBlock::Text(TextContent::new(content.clone())))
        }
        MessagePart::Prompt { blocks } => {
            // Prompt blocks are already ACP ContentBlock values; return the
            // first text block (the user-visible prompt text).
            blocks.first().cloned()
        }
        MessagePart::Reasoning { content, .. } => {
            // Skip reasoning for mobile history chat bubbles.
            let _ = content;
            None
        }
        // Tool calls, results, patches, snapshots, compaction — not shown in
        // mobile chat bubble history.
        _ => None,
    }
}

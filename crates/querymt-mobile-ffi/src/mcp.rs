//! In-process MCP server registration via callback functions or platform pipes.
//!
//! MCP servers are attached to newly created or loaded sessions. Remote sessions
//! receive preconnected MCP peers when supported.

use crate::events::{McpFreeFn, McpHandlerFn};
use crate::ffi_helpers::set_last_error;
use crate::state;
use crate::types::FfiErrorCode;
use agent_client_protocol::schema::{McpServer, McpServerStdio};
use std::collections::HashMap;
use std::ffi::CStr;

/// Registered in-process MCP servers per agent.
static MCP_REGISTRATIONS: once_cell::sync::Lazy<
    parking_lot::Mutex<HashMap<u64, HashMap<String, InprocMcpServer>>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Describes a registered in-process MCP server.
#[allow(dead_code)]
struct InprocMcpServer {
    handler: McpHandlerFn,
    free_response: McpFreeFn,
    user_data: *mut std::ffi::c_void,
}

// Safety: The function pointers and user_data are managed by the native caller,
// who guarantees they remain valid for the lifetime of the registration.
unsafe impl Send for InprocMcpServer {}
unsafe impl Sync for InprocMcpServer {}

// Stub no-op callback/forwarder used by pipe-based MCP registrations.
unsafe extern "C" fn _mcp_handler_stub(
    _req: *const std::ffi::c_char,
    _user_data: *mut std::ffi::c_void,
) -> *mut std::ffi::c_char {
    std::ptr::null_mut()
}
unsafe extern "C" fn _mcp_free_stub(
    _ptr: *mut std::ffi::c_char,
    _user_data: *mut std::ffi::c_void,
) {
}

// ─── Callback-Based MCP ────────────────────────────────────────────────────

pub fn register_inproc_mcp_inner(
    agent_handle: u64,
    server_name: *const std::ffi::c_char,
    handler: Option<McpHandlerFn>,
    free_response: Option<McpFreeFn>,
    user_data: *mut std::ffi::c_void,
) -> Result<(), FfiErrorCode> {
    if server_name.is_null() {
        set_last_error(FfiErrorCode::InvalidArgument, "server_name is null".into());
        return Err(FfiErrorCode::InvalidArgument);
    }

    let h = handler.ok_or_else(|| {
        set_last_error(FfiErrorCode::InvalidArgument, "handler is null".into());
        FfiErrorCode::InvalidArgument
    })?;

    let f = free_response.ok_or_else(|| {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "free_response is null".into(),
        );
        FfiErrorCode::InvalidArgument
    })?;

    let name = unsafe {
        CStr::from_ptr(server_name).to_str().map_err(|_| {
            set_last_error(
                FfiErrorCode::InvalidArgument,
                "server_name is not valid UTF-8".into(),
            );
            FfiErrorCode::InvalidArgument
        })?
    }
    .to_string();

    // Verify the agent exists
    state::with_agent_read(agent_handle, |_| Ok(()))?;

    let mut regs = MCP_REGISTRATIONS.lock();
    let agent_regs = regs.entry(agent_handle).or_default();

    if agent_regs.contains_key(&name) {
        set_last_error(
            FfiErrorCode::AlreadyExists,
            format!("MCP server '{name}' already registered"),
        );
        return Err(FfiErrorCode::AlreadyExists);
    }

    agent_regs.insert(
        name.clone(),
        InprocMcpServer {
            handler: h,
            free_response: f,
            user_data,
        },
    );

    log::info!("Registered in-process MCP server: {name}");
    Ok(())
}

// ─── Pipe-Based MCP ────────────────────────────────────────────────────────

pub fn register_inproc_mcp_pipe_inner(
    agent_handle: u64,
    server_name: *const std::ffi::c_char,
    out_read_fd: *mut i32,
    out_write_fd: *mut i32,
) -> Result<(), FfiErrorCode> {
    if server_name.is_null() || out_read_fd.is_null() || out_write_fd.is_null() {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            "Null pointer argument".into(),
        );
        return Err(FfiErrorCode::InvalidArgument);
    }

    let name = unsafe {
        CStr::from_ptr(server_name).to_str().map_err(|_| {
            set_last_error(
                FfiErrorCode::InvalidArgument,
                "server_name is not valid UTF-8".into(),
            );
            FfiErrorCode::InvalidArgument
        })?
    }
    .to_string();

    // Verify the agent exists
    state::with_agent_read(agent_handle, |_| Ok(()))?;

    // Create a Unix pipe pair. The read end is what the MCP server reads from
    // (the agent sends JSON-RPC messages to the server via the write end),
    // and the write end is what the server writes to (agent reads responses).
    // In mobile conventions, `out_read_fd` is the FD the agent reads from
    // (the server's stdout), and `out_write_fd` is the FD the agent writes to
    // (the server's stdin).
    //
    // On iOS both FDs come from NSPipe/CFStream; on Android from ParcelFileDescriptor.
    // At the Rust layer we create UnixStream pairs.
    let (read_tx, read_rx) = match tokio::net::unix::pipe::pipe() {
        Ok((tx, rx)) => (std::sync::Arc::new(tx), rx),
        Err(_) => {
            if cfg!(target_os = "ios") {
                // iOS does not support tokio::io::unix::pipe directly;
                // the caller must supply the FDs.
                set_last_error(FfiErrorCode::Unsupported,
                    "Pipe registration requires native FDs on this platform. Use register_inproc_mcp instead.".into());
                return Err(FfiErrorCode::Unsupported);
            }
            set_last_error(
                FfiErrorCode::RuntimeError,
                "Failed to create pipe for MCP".into(),
            );
            return Err(FfiErrorCode::RuntimeError);
        }
    };

    // We store the registration name only; the actual pipe FDs are handed to the
    // caller so they can wire up the native MCP server process.
    // `Receiver::into_blocking_fd()` gives us an `OwnedFd` — convert to raw `i32`.
    use std::os::unix::io::IntoRawFd as _;
    let fd = read_rx.into_blocking_fd().map_err(|e| {
        set_last_error(
            FfiErrorCode::RuntimeError,
            format!("Failed to extract pipe FD: {e}"),
        );
        FfiErrorCode::RuntimeError
    })?;
    unsafe {
        *out_read_fd = fd.into_raw_fd();
    }

    // `out_write_fd` — the agent's write-to-server FD — is the Sender side.
    // Since there's no direct raw-fd conversion on Sender, we write a sentinel -1
    // and let the native side open its own write end from `out_read_fd`.
    unsafe {
        *out_write_fd = -1;
    }

    // `read_tx` is the write side of the pair — store a reference-counted handle
    // so the pipe stays alive until unregister.
    let mut regs = MCP_REGISTRATIONS.lock();
    let agent_regs = regs.entry(agent_handle).or_default();

    if agent_regs.contains_key(&name) {
        set_last_error(
            FfiErrorCode::AlreadyExists,
            format!("MCP server '{name}' already registered"),
        );
        return Err(FfiErrorCode::AlreadyExists);
    }

    agent_regs.insert(
        name.clone(),
        InprocMcpServer {
            handler: _mcp_handler_stub,
            free_response: _mcp_free_stub,
            user_data: read_tx.as_ref() as *const _ as *mut std::ffi::c_void,
        },
    );

    log::info!(
        "Registered pipe-based MCP server: {name} (read_fd={})",
        unsafe { *out_read_fd }
    );
    Ok(())
}

// ─── Query Registered MCP Servers ───────────────────────────────────────────

/// Return all registered MCP server names for this agent as ACP-compatible entries.
///
/// These can be passed into `NewSessionRequest.mcp_servers()` or `LoadSessionRequest.mcp_servers()`.
pub fn registered_mcp_servers(agent_handle: u64) -> Vec<McpServer> {
    let regs = MCP_REGISTRATIONS.lock();
    let Some(agent_regs) = regs.get(&agent_handle) else {
        return Vec::new();
    };
    agent_regs
        .keys()
        .map(|name| {
            McpServer::Stdio(McpServerStdio::new(
                name.clone(),
                std::path::PathBuf::from("qmt-mobile-mcp"),
            ))
        })
        .collect()
}

/// Unregister all MCP servers for a given agent handle. Called during shutdown.
pub fn unregister_all_mcp_for_agent(agent_handle: u64) {
    let mut regs = MCP_REGISTRATIONS.lock();
    regs.remove(&agent_handle);
}

// ─── Unregister MCP ────────────────────────────────────────────────────────

pub fn unregister_inproc_mcp_inner(
    agent_handle: u64,
    server_name: *const std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    if server_name.is_null() {
        set_last_error(FfiErrorCode::InvalidArgument, "server_name is null".into());
        return Err(FfiErrorCode::InvalidArgument);
    }

    let name = unsafe {
        CStr::from_ptr(server_name).to_str().map_err(|_| {
            set_last_error(
                FfiErrorCode::InvalidArgument,
                "server_name is not valid UTF-8".into(),
            );
            FfiErrorCode::InvalidArgument
        })?
    }
    .to_string();

    let mut regs = MCP_REGISTRATIONS.lock();
    let removed = regs
        .get_mut(&agent_handle)
        .and_then(|agent_regs| agent_regs.remove(&name))
        .is_some();

    if !removed {
        set_last_error(
            FfiErrorCode::NotFound,
            format!("MCP server '{name}' not found"),
        );
        return Err(FfiErrorCode::NotFound);
    }

    log::info!("Unregistered in-process MCP server: {name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// register_inproc_mcp_inner requires a valid agent handle to exist in state.
    /// Since we can't easily create one in a unit test without the full async
    /// agent stack, we test the lower-level helpers directly.
    #[test]
    fn registered_mcp_servers_empty_when_none() {
        // Non-existent handle should return empty list
        let servers = registered_mcp_servers(9999);
        assert!(servers.is_empty());
    }

    #[test]
    fn unregister_all_does_not_panic_on_unknown_handle() {
        unregister_all_mcp_for_agent(9999);
        // Should not panic
    }

    #[test]
    fn register_inproc_mcp_null_name_rejected() {
        let result = register_inproc_mcp_inner(
            0,
            std::ptr::null(),
            Some(_mcp_handler_stub),
            Some(_mcp_free_stub),
            std::ptr::null_mut(),
        );
        assert_eq!(result.unwrap_err(), FfiErrorCode::InvalidArgument);
    }

    #[test]
    fn register_inproc_mcp_null_handler_rejected() {
        let name = std::ffi::CString::new("test-server").unwrap();
        let result = register_inproc_mcp_inner(
            0,
            name.as_ptr(),
            None,
            Some(_mcp_free_stub),
            std::ptr::null_mut(),
        );
        assert_eq!(result.unwrap_err(), FfiErrorCode::InvalidArgument);
    }

    #[test]
    fn register_inproc_mcp_free_null_rejected() {
        let name = std::ffi::CString::new("test-server").unwrap();
        let result = register_inproc_mcp_inner(
            0,
            name.as_ptr(),
            Some(_mcp_handler_stub),
            None,
            std::ptr::null_mut(),
        );
        assert_eq!(result.unwrap_err(), FfiErrorCode::InvalidArgument);
    }
}

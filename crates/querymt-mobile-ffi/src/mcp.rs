//! MCP server registration and lifecycle.
//!
//! MCP servers are attached to newly created or loaded sessions. Remote sessions
//! receive preconnected MCP peers when supported.

use crate::ffi_helpers::set_last_error;
use crate::state;
use crate::types::FfiErrorCode;
use querymt_agent::agent::session_registry::PreconnectedMcpPeer;
use rmcp::RoleClient;
use rmcp::service::{Peer, RunningService, serve_client};
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::io::RawFd;
use std::sync::Arc;
use tokio::net::unix::pipe;

/// Registered in-process MCP servers per agent.
static MCP_REGISTRATIONS: once_cell::sync::Lazy<
    parking_lot::Mutex<HashMap<u64, HashMap<String, InprocMcpServer>>>,
> = once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Describes a registered in-process MCP server.
enum InprocMcpServer {
    /// Pipe-based — FD pair to an existing Unix pipe. The Rust side lazily
    /// connects once (via `serve_client`) and reuses the peer across sessions.
    Pipe(InprocMcpPipeServer),
}

/// In-process MCP server using Unix pipe transport.
///
/// The Swift side receives `swift_*` FDs and runs an MCP server over them.
/// The Rust side lazily connects to `rust_*` FDs once and then reuses the
/// connected peer across sessions without re-initializing.
struct InprocMcpPipeServer {
    /// Rust-side read FD (reads from Swift's write end).
    /// Set to -1 after connection (ownership transferred to tokio pipe wrapper).
    rust_read_fd: RawFd,
    /// Rust-side write FD (writes to Swift's read end).
    /// Set to -1 after connection (ownership transferred to tokio pipe wrapper).
    rust_write_fd: RawFd,
    /// The connected peer, populated exactly once by `connect_pipe_server_if_needed`.
    connected: Option<ConnectedInprocMcpPipe>,
}

/// Holds the connected MCP peer for a pipe-based server.
///
/// `_running` must be kept alive for the duration of the connection since it
/// owns the transport. `peer` is used to list/call tools.
struct ConnectedInprocMcpPipe {
    _running: RunningService<RoleClient, querymt_agent::elicitation::McpClientHandler>,
    peer: Peer<RoleClient>,
}

// Safety: Pipe-based MCP server uses only Send/Sync-safe types.
unsafe impl Send for InprocMcpServer {}
unsafe impl Sync for InprocMcpServer {}

impl Drop for InprocMcpServer {
    fn drop(&mut self) {
        let InprocMcpServer::Pipe(pipe_server) = self;
        // If the pipe was never connected, close the raw FDs.
        if pipe_server.connected.is_none() {
            if pipe_server.rust_read_fd >= 0 {
                unsafe { libc::close(pipe_server.rust_read_fd) };
            }
            if pipe_server.rust_write_fd >= 0 {
                unsafe { libc::close(pipe_server.rust_write_fd) };
            }
        }
        // If connected, FDs were already transferred to tokio pipe wrappers
        // and will be closed when `_running` is dropped.
    }
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

    // Create TWO Unix pipe pairs:
    //   pipe_a: Rust writes (agent→server), Swift reads
    //   pipe_b: Swift writes (server→agent), Rust reads
    //
    // After creation:
    //   rust_to_swift_fds[1] → Rust writes
    //   rust_to_swift_fds[0] → Swift reads  (returned as out_read_fd)
    //   swift_to_rust_fds[0] → Rust reads
    //   swift_to_rust_fds[1] → Swift writes (returned as out_write_fd)

    let mut rust_to_swift: [libc::c_int; 2] = [-1, -1];
    let mut swift_to_rust: [libc::c_int; 2] = [-1, -1];

    if unsafe { libc::pipe(rust_to_swift.as_mut_ptr()) } != 0 {
        set_last_error(
            FfiErrorCode::RuntimeError,
            "Failed to create rust→swift pipe".into(),
        );
        return Err(FfiErrorCode::RuntimeError);
    }

    if unsafe { libc::pipe(swift_to_rust.as_mut_ptr()) } != 0 {
        // Clean up rust_to_swift pipe
        unsafe {
            libc::close(rust_to_swift[0]);
            libc::close(rust_to_swift[1]);
        }
        set_last_error(
            FfiErrorCode::RuntimeError,
            "Failed to create swift→rust pipe".into(),
        );
        return Err(FfiErrorCode::RuntimeError);
    }

    // Swift-side FDs (returned to caller)
    let swift_read_fd = rust_to_swift[0]; // Swift reads from this
    let swift_write_fd = swift_to_rust[1]; // Swift writes to this

    // Rust-side FDs (retained in registration)
    let rust_write_fd = rust_to_swift[1]; // Rust writes to this
    let rust_read_fd = swift_to_rust[0]; // Rust reads from this

    // Verify uniqueness and register BEFORE returning FDs, so if it fails
    // we close both pipes and the caller gets an error.
    if let Err(e) = ensure_not_duplicate(&name, agent_handle) {
        unsafe {
            libc::close(rust_write_fd);
            libc::close(rust_read_fd);
            libc::close(swift_read_fd);
            libc::close(swift_write_fd);
        }
        return Err(e);
    }

    {
        let mut regs = MCP_REGISTRATIONS.lock();
        let agent_regs = regs.entry(agent_handle).or_default();
        agent_regs.insert(
            name.clone(),
            InprocMcpServer::Pipe(InprocMcpPipeServer {
                rust_read_fd,
                rust_write_fd,
                connected: None,
            }),
        );
    }

    // Return Swift-side FDs to caller
    unsafe {
        *out_read_fd = swift_read_fd;
        *out_write_fd = swift_write_fd;
    }

    log::info!(
        "Registered pipe-based MCP server: {name} (swift_read_fd={}, swift_write_fd={})",
        swift_read_fd,
        swift_write_fd,
    );
    Ok(())
}

/// Check if a server with this name is already registered for this agent.
fn ensure_not_duplicate(name: &str, agent_handle: u64) -> Result<(), FfiErrorCode> {
    let regs = MCP_REGISTRATIONS.lock();
    if let Some(agent_regs) = regs.get(&agent_handle)
        && agent_regs.contains_key(name)
    {
        set_last_error(
            FfiErrorCode::AlreadyExists,
            format!("MCP server '{name}' already registered"),
        );
        return Err(FfiErrorCode::AlreadyExists);
    }
    Ok(())
}

// ─── Lazy Connection ────────────────────────────────────────────────────────

/// Info extracted from an unconnected pipe server, needed to perform the
/// async MCP handshake outside the MCP_REGISTRATIONS lock.
struct PendingPipeConnect {
    server_name: String,
    read_fd: RawFd,
    write_fd: RawFd,
}

/// Connect a single pipe-based MCP server asynchronously.
///
/// This must be called **outside** any `MCP_REGISTRATIONS` lock because it
/// `.await`s `serve_client`. The caller passes ownership of the raw FDs via
/// `info`; on success the caller stores the resulting `ConnectedInprocMcpPipe`.
async fn connect_one_pipe_server(
    info: PendingPipeConnect,
    agent_handle: u64,
    pending_elicitations: querymt_agent::elicitation::PendingElicitationMap,
    event_sink: Arc<querymt_agent::event_sink::EventSink>,
) -> Result<(String, ConnectedInprocMcpPipe), FfiErrorCode> {
    let server_name = &info.server_name;

    // SAFETY: these are valid FDs created by libc::pipe and are consumed exactly once.
    let read_owned = unsafe { OwnedFd::from_raw_fd(info.read_fd) };
    let write_owned = unsafe { OwnedFd::from_raw_fd(info.write_fd) };

    let receiver = pipe::Receiver::from_owned_fd(read_owned).map_err(|e| {
        set_last_error(
            FfiErrorCode::RuntimeError,
            format!("failed to wrap pipe read fd for {server_name}: {e}"),
        );
        FfiErrorCode::RuntimeError
    })?;
    let sender = pipe::Sender::from_owned_fd(write_owned).map_err(|e| {
        set_last_error(
            FfiErrorCode::RuntimeError,
            format!("failed to wrap pipe write fd for {server_name}: {e}"),
        );
        FfiErrorCode::RuntimeError
    })?;

    let transport = rmcp::transport::async_rw::AsyncRwTransport::new_client(receiver, sender);
    let handler = querymt_agent::elicitation::McpClientHandler::new(
        pending_elicitations,
        event_sink,
        server_name.clone(),
        format!("mobile-inproc-{agent_handle}"),
        querymt_agent::agent::mcp::agent_implementation(),
        querymt_agent::agent::core::McpToolState::empty(),
    );

    let running = serve_client(handler, transport).await.map_err(|e| {
        set_last_error(
            FfiErrorCode::RuntimeError,
            format!("failed to connect in-process MCP pipe {server_name}: {e}"),
        );
        FfiErrorCode::RuntimeError
    })?;

    let peer = running.peer().clone();
    log::info!("Connected in-process MCP pipe server: {server_name}");

    Ok((
        info.server_name.clone(),
        ConnectedInprocMcpPipe {
            _running: running,
            peer,
        },
    ))
}

// ─── Collect Preconnected Peers ─────────────────────────────────────────────

/// Collect all preconnected MCP pipe peers for a given agent.
///
/// Lazily connects any pipe servers that have not yet been connected.
/// Returns `Vec<PreconnectedMcpPeer>` where each peer is already initialized
/// and can be reused across sessions without re-initializing.
///
/// This is `async` because it may need to perform MCP handshake over the pipe.
/// It must be called from within a tokio runtime context (e.g. inside
/// `runtime.block_on(async { ... })`).
pub async fn collect_preconnected_mcp_servers(
    agent_handle: u64,
) -> Result<Vec<PreconnectedMcpPeer>, FfiErrorCode> {
    // Extract agent dependencies before acquiring MCP_REGISTRATIONS lock,
    // to avoid holding two locks simultaneously.
    let (pending_elicitations, event_sink) = state::with_agent_read(agent_handle, |r| {
        let inner = r.agent.inner();
        Ok((
            inner.pending_elicitations(),
            inner.config.event_sink.clone(),
        ))
    })?;

    // Phase 1: collect already-connected peers and extract FDs for
    // unconnected servers — all under a single short-lived lock.
    let mut pending_connects: Vec<PendingPipeConnect> = Vec::new();
    let mut preconnected: Vec<PreconnectedMcpPeer> = Vec::new();

    {
        let mut regs = MCP_REGISTRATIONS.lock();
        let Some(agent_regs) = regs.get_mut(&agent_handle) else {
            return Ok(Vec::new());
        };

        for (name, server) in agent_regs.iter_mut() {
            let InprocMcpServer::Pipe(pipe_server) = server;
            if let Some(connected) = &pipe_server.connected {
                // Already connected — just clone the peer.
                preconnected.push((name.clone(), connected.peer.clone()));
            } else if pipe_server.rust_read_fd >= 0 && pipe_server.rust_write_fd >= 0 {
                // Not yet connected and FDs are valid — take ownership.
                pending_connects.push(PendingPipeConnect {
                    server_name: name.clone(),
                    read_fd: std::mem::replace(&mut pipe_server.rust_read_fd, -1),
                    write_fd: std::mem::replace(&mut pipe_server.rust_write_fd, -1),
                });
            }
        }
    } // Lock dropped here — safe to .await.

    // Phase 2: connect any pending servers without holding the lock.
    for info in pending_connects {
        let server_name_log = info.server_name.clone();
        match connect_one_pipe_server(
            info,
            agent_handle,
            pending_elicitations.clone(),
            event_sink.clone(),
        )
        .await
        {
            Ok((name, connected)) => {
                let peer = connected.peer.clone();
                preconnected.push((name.clone(), peer));

                // Phase 3: re-lock and store the connected peer.
                let mut regs = MCP_REGISTRATIONS.lock();
                if let Some(agent_regs) = regs.get_mut(&agent_handle)
                    && let Some(InprocMcpServer::Pipe(pipe_server)) = agent_regs.get_mut(&name)
                {
                    pipe_server.connected = Some(connected);
                }
            }
            Err(e) => {
                log::warn!(
                    "Failed to connect pipe MCP server '{}': {:?}",
                    server_name_log,
                    e
                );
                // Continue — session will open without this server's tools.
            }
        }
    }

    Ok(preconnected)
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

    #[test]
    fn unregister_all_does_not_panic_on_unknown_handle() {
        unregister_all_mcp_for_agent(9999);
        // Should not panic
    }
}

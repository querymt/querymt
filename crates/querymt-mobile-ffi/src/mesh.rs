//! Remote mesh operations: node listing, remote sessions, invites, status.
//!
//! All remote operations require `feature = "remote"` (or `remote-internet`).
//! Without the feature, every function returns `QMT_MOBILE_UNSUPPORTED`.

use crate::ffi_helpers::{check_not_backgrounded, set_last_error, set_last_error_from_anyhow};
use crate::runtime::global_runtime;
use crate::state;
use crate::types::{
    FfiErrorCode, InviteCreateResponse, InviteOptions, JoinMeshResponse, MeshStatusResponse,
    NodeInfo, NodeListResponse, RemoteSessionListResponse, RemoteSessionSummary,
};
use std::ffi::CStr;

#[cfg(not(feature = "remote"))]
fn unavailable() -> Result<(), FfiErrorCode> {
    set_last_error(
        FfiErrorCode::Unsupported,
        "Mesh support not compiled in".into(),
    );
    Err(FfiErrorCode::Unsupported)
}

// ─── List Nodes ─────────────────────────────────────────────────────────────

pub fn list_nodes_inner(
    agent_handle: u64,
    out_json: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    if out_json.is_null() {
        return Err(invalid_arg("out_json is null"));
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = (agent_handle, out_json);
        return unavailable();
    }

    #[cfg(feature = "remote")]
    {
        let runtime = global_runtime();
        runtime.block_on(async {
            let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

            let mesh = agent.mesh().ok_or_else(|| {
                set_last_error(FfiErrorCode::InvalidState, "Mesh is not enabled".into());
                FfiErrorCode::InvalidState
            })?;

            let local_node_id = mesh.peer_id().to_string();
            let nodes = agent.list_remote_nodes().await;

            let mut node_infos = Vec::with_capacity(nodes.len() + 1);
            node_infos.push(NodeInfo {
                node_id: local_node_id.clone(),
                label: "Local".to_string(),
                hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string()),
                capabilities: vec![],
                active_sessions: 0,
                is_local: true,
                is_reachable: true,
            });

            node_infos.extend(nodes.into_iter().map(|n| NodeInfo {
                node_id: n.node_id.to_string(),
                label: n.hostname.clone(),
                hostname: n.hostname,
                capabilities: n.capabilities,
                active_sessions: n.active_sessions as u32,
                is_local: false,
                is_reachable: true,
            }));

            let response = NodeListResponse {
                enabled: true,
                local_node_id,
                nodes: node_infos,
            };
            let json = serde_json::to_string(&response).map_err(serde_err)?;
            unsafe {
                *out_json = alloc_cstr(&json);
            }
            Ok(())
        })
    }
}

// ─── Create Session on Node ─────────────────────────────────────────────────

pub fn create_session_on_node_inner(
    agent_handle: u64,
    node_id: *const std::ffi::c_char,
    options_json: *const std::ffi::c_char,
    out_session: *mut u64,
) -> Result<(), FfiErrorCode> {
    create_session_on_node_with_id_inner(
        agent_handle,
        node_id,
        options_json,
        out_session,
        std::ptr::null_mut(),
    )
}

/// Create a session on a specific node, optionally returning the real session ID.
///
/// When `out_session_id` is non-null, the caller must free the returned string
/// with `qmt_mobile_free_string`.
pub fn create_session_on_node_with_id_inner(
    agent_handle: u64,
    node_id: *const std::ffi::c_char,
    options_json: *const std::ffi::c_char,
    out_session: *mut u64,
    out_session_id: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if out_session.is_null() {
        return Err(invalid_arg("out_session is null"));
    }

    let node_id_str: Option<String> = ptr_to_opt_string(node_id);

    // NULL/empty → local session
    if node_id_str.as_ref().is_none_or(|s| s.is_empty()) {
        return crate::session::create_session_with_id_inner(
            agent_handle,
            options_json,
            out_session,
            out_session_id,
        );
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = (options_json, node_id_str);
        return unavailable();
    }

    #[cfg(feature = "remote")]
    {
        let options = parse_session_options(options_json)?;
        let node_id_val = node_id_str.unwrap();

        let runtime = global_runtime();
        runtime.block_on(async {
            let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

            // Find remote node manager
            let nm_ref = agent.find_node_manager(&node_id_val).await.map_err(|e| {
                set_last_error(
                    FfiErrorCode::NotFound,
                    format!("Remote node not found: {node_id_val}: {e}"),
                );
                FfiErrorCode::NotFound
            })?;

            // Create a remote session
            use querymt_agent::agent::remote::CreateRemoteSession;
            let create_req = CreateRemoteSession {
                cwd: options.cwd.clone(),
            };
            let create_resp = nm_ref.ask(&create_req).await.map_err(|e| {
                set_last_error(
                    FfiErrorCode::RuntimeError,
                    format!("Remote call failed: {e}"),
                );
                FfiErrorCode::RuntimeError
            })?;

            let session_id = create_resp.session_id;

            // Attach locally
            use querymt_agent::agent::remote::node_manager::SessionHandoff;
            match create_resp.handoff {
                SessionHandoff::DirectRemote { session_ref } => {
                    let _ = agent
                        .attach_remote_session(
                            session_id.clone(),
                            session_ref,
                            node_id_val.clone(),
                            Some(node_id_val.clone()),
                        )
                        .await;
                }
                _ => {
                    log::warn!("Remote session created but no direct attach path");
                }
            }

            let s_handle = state::register_session(
                agent_handle,
                session_id.clone(),
                true,
                Some(node_id_val),
                None,
            )?;
            unsafe {
                *out_session = s_handle;
                if !out_session_id.is_null() {
                    *out_session_id = alloc_cstr(&session_id);
                }
            }
            Ok(())
        })
    }
}

// ─── List Remote Sessions ──────────────────────────────────────────────────

pub fn list_remote_sessions_inner(
    agent_handle: u64,
    node_id: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if node_id.is_null() || out_json.is_null() {
        return Err(invalid_arg("Null pointer"));
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = (agent_handle, node_id, out_json);
        return unavailable();
    }

    #[cfg(feature = "remote")]
    {
        let node_id_str = cstr_to_string(node_id)?;

        let runtime = global_runtime();
        runtime.block_on(async {
            let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

            let nm_ref = agent.find_node_manager(&node_id_str).await.map_err(|e| {
                set_last_error(
                    FfiErrorCode::NotFound,
                    format!("Remote node not found: {node_id_str}: {e}"),
                );
                FfiErrorCode::NotFound
            })?;

            let remote_sessions = agent.list_remote_sessions(&nm_ref).await.map_err(|e| {
                set_last_error_from_anyhow(FfiErrorCode::RuntimeError, e.into());
                FfiErrorCode::RuntimeError
            })?;

            let summaries: Vec<RemoteSessionSummary> = remote_sessions
                .iter()
                .map(|s| RemoteSessionSummary {
                    session_id: s.session_id.clone(),
                    actor_id: s.actor_id,
                    cwd: s.cwd.clone(),
                    created_at: s.created_at,
                    title: s.title.clone().unwrap_or_default(),
                    peer_label: s.peer_label.clone(),
                    runtime_state: s.runtime_state.clone().unwrap_or_else(|| "idle".into()),
                })
                .collect();

            let response = RemoteSessionListResponse {
                node_id: node_id_str,
                sessions: summaries,
            };
            let json = serde_json::to_string(&response).map_err(serde_err)?;
            unsafe {
                *out_json = alloc_cstr(&json);
            }
            Ok(())
        })
    }
}

// ─── Attach Remote Session ──────────────────────────────────────────────────

pub fn attach_remote_session_inner(
    agent_handle: u64,
    node_id: *const std::ffi::c_char,
    session_id: *const std::ffi::c_char,
    out_session: *mut u64,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if node_id.is_null() || session_id.is_null() || out_session.is_null() {
        return Err(invalid_arg("Null pointer"));
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = (agent_handle, node_id, session_id, out_session);
        return unavailable();
    }

    #[cfg(feature = "remote")]
    {
        let node_id_str = cstr_to_string(node_id)?;
        let sid = cstr_to_string(session_id)?;

        let runtime = global_runtime();
        runtime.block_on(async {
            let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

            let nm_ref = agent.find_node_manager(&node_id_str).await.map_err(|e| {
                set_last_error(
                    FfiErrorCode::NotFound,
                    format!("Remote node not found: {node_id_str}: {e}"),
                );
                FfiErrorCode::NotFound
            })?;

            use querymt_agent::agent::remote::ResumeRemoteSession;
            use querymt_agent::agent::remote::node_manager::SessionHandoff;
            let resume_req = ResumeRemoteSession {
                session_id: sid.clone(),
            };
            let resume_resp = nm_ref.ask(&resume_req).await.map_err(|e| {
                set_last_error(
                    FfiErrorCode::RuntimeError,
                    format!("Remote call failed: {e}"),
                );
                FfiErrorCode::RuntimeError
            })?;

            match resume_resp.handoff {
                SessionHandoff::DirectRemote { session_ref } => {
                    let _ = agent
                        .attach_remote_session(
                            sid.clone(),
                            session_ref,
                            node_id_str.clone(),
                            Some(node_id_str.clone()),
                        )
                        .await;
                }
                _ => {
                    // Try bookmark reattachment
                    let store =
                        state::with_agent_read(agent_handle, |r| Ok(r.storage.session_store()))?;
                    let bookmarks = store
                        .list_remote_session_bookmarks()
                        .await
                        .unwrap_or_default();
                    for bk in &bookmarks {
                        if bk.session_id == sid {
                            let _ = agent.reattach_from_bookmark(bk).await;
                            break;
                        }
                    }
                }
            }

            let s_handle =
                state::register_session(agent_handle, sid, true, Some(node_id_str), None)?;
            unsafe {
                *out_session = s_handle;
            }
            Ok(())
        })
    }
}

// ─── Create Invite ──────────────────────────────────────────────────────────

pub fn create_invite_inner(
    agent_handle: u64,
    options_json: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    if out_json.is_null() {
        return Err(invalid_arg("out_json is null"));
    }

    #[cfg(not(feature = "remote-internet"))]
    {
        let _ = (agent_handle, options_json, out_json);
        set_last_error(
            FfiErrorCode::Unsupported,
            "Invite creation requires 'remote-internet' feature".into(),
        );
        return Err(FfiErrorCode::Unsupported);
    }

    #[cfg(feature = "remote-internet")]
    {
        use querymt_agent::agent::remote::invite::InvitePermissions;

        let options = parse_invite_options(options_json)?;

        let runtime = global_runtime();
        runtime.block_on(async {
            let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

            let mesh = agent.mesh().ok_or_else(|| {
                set_last_error(FfiErrorCode::InvalidState, "Mesh is not enabled".into());
                FfiErrorCode::InvalidState
            })?;

            let invite_store = mesh.invite_store().ok_or_else(|| {
                set_last_error(
                    FfiErrorCode::InvalidState,
                    "Mesh has no invite store".into(),
                );
                FfiErrorCode::InvalidState
            })?;

            let ttl_secs = if options.expires_at > 0 {
                Some(options.expires_at)
            } else {
                None
            };
            let max_uses = options.max_uses.unwrap_or(1);
            let keypair = mesh.keypair();
            let peer_id_str = mesh.peer_id().to_string();

            let mut store = invite_store.write();
            let signed = store
                .create_invite(
                    keypair,
                    &peer_id_str,
                    options.mesh_name.clone(),
                    ttl_secs,
                    max_uses,
                    InvitePermissions {
                        can_invite: options.can_invite,
                        role: "member".into(),
                    },
                )
                .map_err(|e| {
                    set_last_error(
                        FfiErrorCode::RuntimeError,
                        format!("Failed to create invite: {e}"),
                    );
                    FfiErrorCode::RuntimeError
                })?;

            let token = signed.encode();

            let response = InviteCreateResponse {
                token,
                invite_id: signed.grant.invite_id.clone(),
                inviter_peer_id: signed.grant.inviter_peer_id.clone(),
                mesh_name: signed.grant.mesh_name.clone(),
                expires_at: signed.grant.expires_at,
                max_uses: signed.grant.max_uses,
                can_invite: signed.grant.permissions.can_invite,
            };

            let json = serde_json::to_string(&response).map_err(serde_err)?;
            unsafe {
                *out_json = alloc_cstr(&json);
            }
            Ok(())
        })
    }
}

// ─── Join Mesh ──────────────────────────────────────────────────────────────

pub fn join_mesh_inner(
    agent_handle: u64,
    invite_token: *const std::ffi::c_char,
    out_json: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    check_not_backgrounded()?;
    if invite_token.is_null() || out_json.is_null() {
        return Err(invalid_arg("Null pointer"));
    }

    #[cfg(not(feature = "remote-internet"))]
    {
        let _ = (agent_handle, invite_token, out_json);
        set_last_error(
            FfiErrorCode::Unsupported,
            "Joining mesh via invite requires 'remote-internet' feature".into(),
        );
        return Err(FfiErrorCode::Unsupported);
    }

    #[cfg(feature = "remote-internet")]
    {
        use querymt_agent::agent::remote::invite::SignedInviteGrant;
        use querymt_agent::agent::remote::mesh::join_mesh_via_invite;

        let token = cstr_to_string(invite_token)?;

        let runtime = global_runtime();
        runtime.block_on(async {
            let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

            let grant = SignedInviteGrant::decode(&token).map_err(|e| {
                set_last_error(
                    FfiErrorCode::InvalidArgument,
                    format!("Invalid invite token: {e}"),
                );
                FfiErrorCode::InvalidArgument
            })?;

            let new_mesh = join_mesh_via_invite(&grant, None).await.map_err(|e| {
                set_last_error_from_anyhow(FfiErrorCode::RuntimeError, e.into());
                FfiErrorCode::RuntimeError
            })?;

            agent.set_mesh(new_mesh.clone());

            let refs = querymt_agent::agent::remote::spawn_and_register_local_mesh_actors(
                agent.as_ref(),
                &new_mesh,
            )
            .await;
            state::set_local_mesh_actors(agent_handle, refs)?;

            let response = JoinMeshResponse {
                joined: true,
                peer_id: new_mesh.peer_id().to_string(),
                mesh_name: grant.grant.mesh_name.clone(),
                inviter_peer_id: grant.grant.inviter_peer_id.to_string(),
            };

            let json = serde_json::to_string(&response).map_err(serde_err)?;
            unsafe {
                *out_json = alloc_cstr(&json);
            }
            Ok(())
        })
    }
}

// ─── Mesh Status ────────────────────────────────────────────────────────────

pub fn mesh_status_inner(
    agent_handle: u64,
    out_json: *mut *mut std::ffi::c_char,
) -> Result<(), FfiErrorCode> {
    if out_json.is_null() {
        return Err(invalid_arg("out_json is null"));
    }

    let runtime = global_runtime();
    runtime.block_on(async {
        let agent = state::with_agent_read(agent_handle, |r| Ok(r.agent.handle()))?;

        // Read diagnostic config from the agent record.
        let (mesh_listen, mesh_discovery) = state::with_agent(agent_handle, |r| {
            Ok((r.mesh_listen.clone(), r.mesh_discovery.clone()))
        })?;

        #[cfg(feature = "remote")]
        if let Some(mesh) = agent.mesh() {
            let known_peer_count = mesh.known_peer_ids().len();

            let status = MeshStatusResponse {
                enabled: true,
                peer_id: Some(mesh.peer_id().to_string()),
                transport: match mesh.transport_mode() {
                    querymt_agent::agent::remote::mesh::MeshTransportMode::Lan => "lan",
                    querymt_agent::agent::remote::mesh::MeshTransportMode::Iroh => "iroh",
                }
                .to_string(),
                backgrounded: crate::ffi_helpers::is_backgrounded(),
                known_peer_count,
                has_invite_store: mesh.invite_store().is_some(),
                has_membership_store: mesh.membership_store().is_some(),
                listen: mesh_listen,
                discovery: mesh_discovery,
                telemetry_endpoint: crate::events::active_otlp_endpoint(),
            };
            let json = serde_json::to_string(&status).map_err(serde_err)?;
            unsafe {
                *out_json = alloc_cstr(&json);
            }
            return Ok(());
        }

        // Default: mesh not enabled
        let status = MeshStatusResponse {
            enabled: false,
            peer_id: None,
            transport: "none".to_string(),
            backgrounded: crate::ffi_helpers::is_backgrounded(),
            known_peer_count: 0,
            has_invite_store: false,
            has_membership_store: false,
            listen: mesh_listen,
            discovery: mesh_discovery,
            telemetry_endpoint: crate::events::active_otlp_endpoint(),
        };
        let json = serde_json::to_string(&status).map_err(serde_err)?;
        unsafe {
            *out_json = alloc_cstr(&json);
        }
        Ok(())
    })
}

// ─── Internal Helpers ───────────────────────────────────────────────────────

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
    unsafe { CStr::from_ptr(ptr).to_str() }
        .map(|s| s.to_string())
        .map_err(|_| invalid_arg("Invalid UTF-8"))
}

fn ptr_to_opt_string(ptr: *const std::ffi::c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(ptr).to_str().ok().map(|s| s.to_string()) }
    }
}

fn parse_session_options(
    options_json: *const std::ffi::c_char,
) -> Result<crate::types::SessionOptions, FfiErrorCode> {
    if options_json.is_null() {
        return Ok(crate::types::SessionOptions {
            cwd: None,
            provider: None,
            model: None,
        });
    }
    let s = cstr_to_string(options_json)?;
    serde_json::from_str(&s).map_err(|e| {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            format!("Failed to parse options: {e}"),
        );
        FfiErrorCode::InvalidArgument
    })
}

fn parse_invite_options(
    options_json: *const std::ffi::c_char,
) -> Result<InviteOptions, FfiErrorCode> {
    if options_json.is_null() {
        return Ok(InviteOptions {
            mesh_name: None,
            expires_at: 0,
            max_uses: Some(1),
            can_invite: false,
        });
    }
    let s = cstr_to_string(options_json)?;
    serde_json::from_str(&s).map_err(|e| {
        set_last_error(
            FfiErrorCode::InvalidArgument,
            format!("Failed to parse options: {e}"),
        );
        FfiErrorCode::InvalidArgument
    })
}

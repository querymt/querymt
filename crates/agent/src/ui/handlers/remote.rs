//! Remote node and session handlers.
//!
//! Handlers for discovering and interacting with remote nodes in the kameo mesh:
//! - `handle_list_remote_nodes`
//! - `handle_list_remote_sessions`
//! - `handle_create_remote_session`
//! - `handle_attach_remote_session`
//!
//! All handlers degrade gracefully when the `remote` feature is disabled,
//! returning empty lists or an error message as appropriate.

use super::super::ServerState;
use super::super::connection::{send_error, send_message};
use super::super::messages::{MeshInviteInfo, UiServerMessage};
use super::session_ops::handle_list_sessions;
#[cfg(feature = "remote")]
use crate::agent::remote::node_manager::SessionHandoff;
#[cfg(feature = "remote")]
use crate::agent::utils::u32_from_usize;
use crate::session::projection::SessionScope;
#[cfg(feature = "remote")]
use kameo::actor::RemoteActorRef;
#[cfg(feature = "remote")]
use std::path::PathBuf;
use tokio::sync::mpsc;

/// List remote nodes discovered in the kameo mesh.
pub async fn handle_list_remote_nodes(state: &ServerState, tx: &mpsc::Sender<String>) {
    #[cfg(feature = "remote")]
    {
        let nodes = state.agent.list_remote_nodes().await;
        let _ = send_message(
            tx,
            UiServerMessage::RemoteNodes {
                nodes: nodes
                    .into_iter()
                    .map(|n| super::super::messages::RemoteNodeInfo {
                        id: n.node_id.to_string(),
                        label: n.hostname,
                        capabilities: n.capabilities,
                        active_sessions: u32_from_usize(n.active_sessions, "active_sessions", None),
                    })
                    .collect(),
            },
        )
        .await;
    }
    #[cfg(not(feature = "remote"))]
    {
        let _ = send_message(tx, UiServerMessage::RemoteNodes { nodes: Vec::new() }).await;
    }
}

/// List sessions on a specific remote node.
pub async fn handle_list_remote_sessions(
    state: &ServerState,
    node_id: &str,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "remote")]
    {
        let node_manager_ref = match state.agent.find_node_manager(node_id).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("handle_list_remote_sessions: {}", e.message);
                let _ = send_message(
                    tx,
                    UiServerMessage::RemoteSessions {
                        node_id: node_id.to_string(),
                        sessions: Vec::new(),
                    },
                )
                .await;
                return;
            }
        };

        match state.agent.list_remote_sessions(&node_manager_ref).await {
            Ok(sessions) => {
                let _ = send_message(
                    tx,
                    UiServerMessage::RemoteSessions {
                        node_id: node_id.to_string(),
                        sessions,
                    },
                )
                .await;
            }
            Err(e) => {
                log::warn!("handle_list_remote_sessions: {}", e.message);
                let _ =
                    send_error(tx, format!("Failed to list remote sessions: {}", e.message)).await;
            }
        }
    }
    #[cfg(not(feature = "remote"))]
    {
        let _ = send_message(
            tx,
            UiServerMessage::RemoteSessions {
                node_id: node_id.to_string(),
                sessions: Vec::new(),
            },
        )
        .await;
    }
}

/// Create a new session on a remote node and attach it to the local registry.
pub async fn handle_create_remote_session(
    state: &ServerState,
    conn_id: &str,
    node_id: &str,
    cwd: Option<&str>,
    request_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "remote")]
    {
        let node_manager_ref = match state.agent.find_node_manager(node_id).await {
            Ok(r) => r,
            Err(e) => {
                let _ = send_error(tx, e.message.clone()).await;
                return;
            }
        };

        match state
            .agent
            .create_remote_session(&node_manager_ref, cwd.map(|s| s.to_string()))
            .await
        {
            Ok(resp) => {
                let session_id = resp.session_id.clone();
                let cwd_path = resp.cwd.as_ref().map(PathBuf::from);
                if let Err(err) = finalize_remote_session_attach(
                    state,
                    conn_id,
                    node_id,
                    &session_id,
                    resp.handoff,
                    cwd_path,
                    tx,
                )
                .await
                {
                    let _ = send_error(
                        tx,
                        format!("Failed to finalize remote session attach: {err}"),
                    )
                    .await;
                    return;
                }

                let _ = send_message(
                    tx,
                    UiServerMessage::SessionCreated {
                        agent_id: super::super::session::PRIMARY_AGENT_ID.to_string(),
                        session_id: session_id.clone(),
                        request_id: request_id.map(|s| s.to_string()),
                    },
                )
                .await;

                log::info!(
                    "handle_create_remote_session: created session {} on node_id '{}'",
                    session_id,
                    node_id
                );
            }
            Err(e) => {
                let _ = send_error(
                    tx,
                    format!(
                        "Failed to create remote session on '{}': {}",
                        node_id, e.message
                    ),
                )
                .await;
            }
        }
    }
    #[cfg(not(feature = "remote"))]
    {
        let _ = send_error(
            tx,
            format!(
                "create_remote_session on node_id '{}' requires the 'remote' feature",
                node_id
            ),
        )
        .await;
    }
}

#[cfg(feature = "remote")]
pub(crate) async fn finalize_remote_session_attach(
    state: &ServerState,
    conn_id: &str,
    node_id: &str,
    session_id: &str,
    handoff: SessionHandoff,
    cwd: Option<PathBuf>,
    tx: &mpsc::Sender<String>,
) -> Result<(), String> {
    let peer_label = state
        .agent
        .list_remote_nodes()
        .await
        .into_iter()
        .find(|n| n.node_id.to_string() == node_id)
        .map(|n| n.hostname)
        .unwrap_or_else(|| node_id.to_string());

    let remote_ref = match handoff {
        SessionHandoff::DirectRemote { session_ref } => session_ref,
        SessionHandoff::LookupOnly => {
            lookup_remote_session_actor(state, node_id, session_id).await?
        }
        SessionHandoff::NoAttachPath => {
            return Err(format!(
                "Remote session '{}' was created on node '{}' but that node cannot provide a direct or lookup attach path",
                session_id, node_id
            ));
        }
    };

    let _session_actor_ref = state
        .agent
        .attach_remote_session(
            session_id.to_string(),
            remote_ref,
            peer_label,
            Some(node_id.to_string()),
        )
        .await;

    let attached_session_ref = {
        let registry = state.agent.registry.lock().await;
        registry.get(session_id).cloned()
    };
    let Some(attached_session_ref) = attached_session_ref else {
        return Err(format!(
            "Attached remote session '{}' but it is missing from local registry",
            session_id
        ));
    };
    if let Err(e) = attached_session_ref.get_mode().await {
        return Err(format!(
            "Remote session '{}' attached but failed health check on node '{}': {}",
            session_id, node_id, e
        ));
    }

    let agent_id = super::super::session::PRIMARY_AGENT_ID.to_string();

    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.sessions
                .insert(agent_id.clone(), session_id.to_string());
            conn.subscribed_sessions.insert(session_id.to_string());
        }
    }

    {
        let mut agents = state.session_agents.lock().await;
        agents.insert(session_id.to_string(), agent_id.clone());
    }

    if let Some(cwd_path) = cwd {
        let mut cwds = state.session_cwds.lock().await;
        cwds.insert(session_id.to_string(), cwd_path);
    }

    let remote_events = {
        let session_ref = {
            let registry = state.agent.registry.lock().await;
            registry.get(session_id).cloned()
        };
        if let Some(ref session_ref) = session_ref {
            match session_ref.get_event_stream().await {
                Ok(events) => {
                    log::info!(
                        "handle_attach_remote_session: fetched {} events from remote session {}",
                        events.len(),
                        session_id
                    );
                    events
                }
                Err(e) => {
                    log::warn!(
                        "handle_attach_remote_session: failed to fetch remote event stream for {}: {}",
                        session_id,
                        e
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        }
    };

    let cursor = super::super::cursor_from_events(&remote_events);
    let audit = crate::session::projection::AuditView {
        session_id: session_id.to_string(),
        events: remote_events,
        tasks: Vec::new(),
        intent_snapshots: Vec::new(),
        decisions: Vec::new(),
        progress_entries: Vec::new(),
        artifacts: Vec::new(),
        delegations: Vec::new(),
        generated_at: time::OffsetDateTime::now_utc(),
    };

    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.session_cursors
                .insert(session_id.to_string(), cursor.clone());
        }
    }

    let _ = send_message(
        tx,
        UiServerMessage::SessionLoaded {
            session_id: session_id.to_string(),
            agent_id,
            audit,
            undo_stack: Vec::new(),
            cursor,
        },
    )
    .await;

    let _ = send_message(
        tx,
        UiServerMessage::WorkspaceIndexStatus {
            session_id: session_id.to_string(),
            status: "building".to_string(),
            message: None,
        },
    )
    .await;

    super::super::connection::send_state(state, conn_id, tx).await;
    handle_list_sessions(
        state,
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(SessionScope::Root),
    )
    .await;

    log::info!(
        "handle_attach_remote_session: attached session {} from node_id '{}'",
        session_id,
        node_id
    );

    Ok(())
}

#[cfg(feature = "remote")]
async fn lookup_remote_session_actor(
    state: &ServerState,
    node_id: &str,
    session_id: &str,
) -> Result<RemoteActorRef<crate::agent::session_actor::SessionActor>, String> {
    use std::time::Duration;

    use crate::agent::session_actor::SessionActor;

    let mesh = state
        .agent
        .mesh()
        .ok_or_else(|| "mesh not bootstrapped — start with --mesh".to_string())?;

    let dht_name = crate::agent::remote::dht_name::session(session_id);
    let lookup_backoff_ms: [u64; 4] = [0, 120, 300, 700];
    let mut last_lookup_error = None;

    for (attempt_idx, delay_ms) in lookup_backoff_ms.iter().enumerate() {
        if *delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
        }

        match mesh
            .lookup_actor_no_retry::<SessionActor>(dht_name.clone())
            .await
        {
            Ok(Some(r)) => {
                if attempt_idx > 0 {
                    log::info!(
                        "handle_attach_remote_session: DHT lookup for {} succeeded on retry {}",
                        session_id,
                        attempt_idx + 1
                    );
                }
                return Ok(r);
            }
            Ok(None) => {
                log::debug!(
                    "handle_attach_remote_session: DHT lookup miss for {} under '{}' (attempt {}/{})",
                    session_id,
                    dht_name,
                    attempt_idx + 1,
                    lookup_backoff_ms.len()
                );
            }
            Err(e) => {
                last_lookup_error = Some(e.to_string());
                log::warn!(
                    "handle_attach_remote_session: DHT lookup error for {} under '{}' (attempt {}/{}): {}",
                    session_id,
                    dht_name,
                    attempt_idx + 1,
                    lookup_backoff_ms.len(),
                    e
                );
            }
        }
    }

    let detail = last_lookup_error
        .map(|e| format!("last error: {e}"))
        .unwrap_or_else(|| "session was not visible in DHT yet".to_string());
    Err(format!(
        "Session '{}' not attachable from node '{}': looked up '{}' {} times, {}",
        session_id,
        node_id,
        dht_name,
        lookup_backoff_ms.len(),
        detail
    ))
}

#[cfg(feature = "remote")]
pub(crate) async fn attach_remote_session_via_lookup(
    state: &ServerState,
    conn_id: &str,
    node_id: &str,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) -> Result<(), String> {
    match finalize_remote_session_attach(
        state,
        conn_id,
        node_id,
        session_id,
        SessionHandoff::LookupOnly,
        None,
        tx,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(lookup_err) => {
            let nm_ref = state
                .agent
                .find_node_manager(node_id)
                .await
                .map_err(|e| e.to_string())?;
            let resumed = state
                .agent
                .resume_remote_session(&nm_ref, session_id.to_string())
                .await
                .map_err(|e| e.to_string())?;

            finalize_remote_session_attach(
                state,
                conn_id,
                node_id,
                session_id,
                resumed.handoff,
                resumed.cwd.map(PathBuf::from),
                tx,
            )
            .await
            .map_err(|resume_err| format!("{lookup_err}; resume failed: {resume_err}"))
        }
    }
}

/// Attach an existing remote session to the local registry.
pub async fn handle_attach_remote_session(
    state: &ServerState,
    conn_id: &str,
    node_id: &str,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "remote")]
    {
        if let Err(err) =
            attach_remote_session_via_lookup(state, conn_id, node_id, session_id, tx).await
        {
            let _ = send_error(tx, err).await;
        }
    }
    #[cfg(not(feature = "remote"))]
    {
        let _ = send_error(
            tx,
            format!(
                "attach_remote_session '{}' on node_id '{}' requires the 'remote' feature",
                session_id, node_id
            ),
        )
        .await;
    }
}

/// Dismiss (remove) a persisted remote session bookmark and detach if currently attached.
pub async fn handle_dismiss_remote_session(
    state: &ServerState,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) {
    // 1. Detach if currently attached
    #[cfg(feature = "remote")]
    {
        let mut registry = state.agent.registry.lock().await;
        if registry.get(session_id).is_some_and(|r| r.is_remote()) {
            registry.detach_remote_session(session_id).await;
        }
    }

    // 2. Remove the bookmark (detach_remote_session already spawns this,
    //    but call it explicitly in case the session wasn't in the registry).
    if let Err(e) = state
        .session_store
        .remove_remote_session_bookmark(session_id)
        .await
    {
        log::warn!(
            "handle_dismiss_remote_session: failed to remove bookmark {}: {}",
            session_id,
            e
        );
    }

    // 3. Refresh session list
    handle_list_sessions(
        state,
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(SessionScope::Root),
    )
    .await;
}

/// Create a mesh invite token on the local node.
pub async fn handle_create_mesh_invite(
    state: &ServerState,
    mesh_name: Option<String>,
    ttl: Option<String>,
    max_uses: Option<u32>,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "remote")]
    {
        let Some(mesh) = state.agent.mesh() else {
            let _ = send_error(tx, "mesh not bootstrapped — start with --mesh".to_string()).await;
            return;
        };

        if !mesh.is_iroh_transport() {
            let _ = send_error(
                tx,
                "Mesh invites require iroh transport. Restart host with --mesh --mesh-invite (or set transport=iroh).".to_string(),
            )
            .await;
            return;
        }

        let ttl_secs = ttl
            .as_deref()
            .and_then(crate::agent::remote::invite::parse_duration_secs)
            .or(Some(24 * 3600));

        match mesh.create_invite(mesh_name.clone(), ttl_secs, max_uses, false) {
            Ok(invite) => {
                let url = invite.to_url();
                #[cfg(feature = "remote-internet")]
                let qr_code = crate::agent::remote::qr::render_to_terminal(&url);
                #[cfg(not(feature = "remote-internet"))]
                let qr_code: Option<String> = None;
                let _ = send_message(
                    tx,
                    UiServerMessage::MeshInviteCreated {
                        invite_id: invite.grant.invite_id,
                        url,
                        qr_code,
                        expires_at: invite.grant.expires_at,
                        max_uses: invite.grant.max_uses,
                        mesh_name,
                    },
                )
                .await;
            }
            Err(e) => {
                let _ = send_error(tx, format!("Failed to create mesh invite: {e}")).await;
            }
        }
    }
    #[cfg(not(feature = "remote"))]
    {
        let _ = send_error(
            tx,
            "create_mesh_invite requires the 'remote' feature".to_string(),
        )
        .await;
    }
}

/// List mesh invites from the local invite store.
pub async fn handle_list_mesh_invites(state: &ServerState, tx: &mpsc::Sender<String>) {
    #[cfg(feature = "remote")]
    {
        let Some(mesh) = state.agent.mesh() else {
            let _ = send_message(
                tx,
                UiServerMessage::MeshInviteList {
                    invites: Vec::new(),
                },
            )
            .await;
            return;
        };

        let invites = if let Some(store) = mesh.invite_store() {
            let store = store.read();
            store
                .list_pending()
                .into_iter()
                .map(|r| MeshInviteInfo {
                    invite_id: r.invite_id.clone(),
                    mesh_name: r.grant.mesh_name.clone(),
                    expires_at: r.grant.expires_at,
                    max_uses: r.grant.max_uses,
                    uses_remaining: r.uses_remaining,
                    status: match r.status {
                        crate::agent::remote::invite::InviteStatus::Pending => {
                            "pending".to_string()
                        }
                        crate::agent::remote::invite::InviteStatus::Consumed => {
                            "consumed".to_string()
                        }
                        crate::agent::remote::invite::InviteStatus::Revoked => {
                            "revoked".to_string()
                        }
                    },
                    used_by: r.used_by.clone(),
                    created_at: r.created_at,
                })
                .collect()
        } else {
            Vec::new()
        };

        let _ = send_message(tx, UiServerMessage::MeshInviteList { invites }).await;
    }
    #[cfg(not(feature = "remote"))]
    {
        let _ = send_message(
            tx,
            UiServerMessage::MeshInviteList {
                invites: Vec::new(),
            },
        )
        .await;
    }
}

/// Revoke a mesh invite by ID.
pub async fn handle_revoke_mesh_invite(
    state: &ServerState,
    invite_id: &str,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "remote")]
    {
        let Some(mesh) = state.agent.mesh() else {
            let _ = send_message(
                tx,
                UiServerMessage::MeshInviteRevoked {
                    invite_id: invite_id.to_string(),
                    success: false,
                    message: Some("mesh not bootstrapped — start with --mesh".to_string()),
                },
            )
            .await;
            return;
        };

        let result = if let Some(store) = mesh.invite_store() {
            store.write().revoke(invite_id)
        } else {
            Err(crate::agent::remote::invite::InviteError::StoreError(
                "invite store not available".to_string(),
            ))
        };

        match result {
            Ok(()) => {
                let _ = send_message(
                    tx,
                    UiServerMessage::MeshInviteRevoked {
                        invite_id: invite_id.to_string(),
                        success: true,
                        message: None,
                    },
                )
                .await;
            }
            Err(e) => {
                let _ = send_message(
                    tx,
                    UiServerMessage::MeshInviteRevoked {
                        invite_id: invite_id.to_string(),
                        success: false,
                        message: Some(e.to_string()),
                    },
                )
                .await;
            }
        }
    }
    #[cfg(not(feature = "remote"))]
    {
        let _ = send_message(
            tx,
            UiServerMessage::MeshInviteRevoked {
                invite_id: invite_id.to_string(),
                success: false,
                message: Some("revoke_mesh_invite requires the 'remote' feature".to_string()),
            },
        )
        .await;
    }
}

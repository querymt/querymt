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
use super::super::messages::UiServerMessage;
#[cfg(feature = "remote")]
use super::session_ops::{handle_get_file_index, handle_list_sessions};
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
                        label: n.hostname,
                        capabilities: n.capabilities,
                        active_sessions: n.active_sessions,
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
    node: &str,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "remote")]
    {
        let node_manager_ref = match state.agent.find_node_manager(node).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("handle_list_remote_sessions: {}", e.message);
                let _ = send_message(
                    tx,
                    UiServerMessage::RemoteSessions {
                        node: node.to_string(),
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
                        node: node.to_string(),
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
                node: node.to_string(),
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
    node: &str,
    cwd: Option<&str>,
    request_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "remote")]
    {
        let node_manager_ref = match state.agent.find_node_manager(node).await {
            Ok(r) => r,
            Err(e) => {
                let _ = send_error(tx, e.message.clone()).await;
                return;
            }
        };

        match state
            .agent
            .create_remote_session(
                &node_manager_ref,
                node.to_string(),
                cwd.map(|s| s.to_string()),
            )
            .await
        {
            Ok((session_id, _session_actor_ref)) => {
                let agent_id = super::super::session::PRIMARY_AGENT_ID.to_string();

                {
                    let mut connections = state.connections.lock().await;
                    if let Some(conn) = connections.get_mut(conn_id) {
                        conn.sessions.insert(agent_id.clone(), session_id.clone());
                        conn.subscribed_sessions.insert(session_id.clone());
                    }
                }

                {
                    let mut agents = state.session_agents.lock().await;
                    agents.insert(session_id.clone(), agent_id.clone());
                }

                if let Some(cwd_str) = cwd {
                    let mut cwds = state.session_cwds.lock().await;
                    cwds.insert(session_id.clone(), std::path::PathBuf::from(cwd_str));
                }

                let _ = send_message(
                    tx,
                    UiServerMessage::SessionCreated {
                        agent_id,
                        session_id: session_id.clone(),
                        request_id: request_id.map(|s| s.to_string()),
                    },
                )
                .await;

                handle_list_sessions(state, tx).await;

                // Fetch the initial file index from the remote node so the UI
                // has file listings immediately (remote sessions don't get a
                // local workspace subscription).
                //
                // Spawn in the background with a small delay: the remote node's
                // workspace index may still be initializing right after session
                // creation.
                {
                    let state_fi = state.clone();
                    let conn_id_fi = conn_id.to_string();
                    let tx_fi = tx.clone();
                    tokio::spawn(async move {
                        // Give the remote workspace index time to build
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        handle_get_file_index(&state_fi, &conn_id_fi, &tx_fi).await;
                    });
                }

                log::info!(
                    "handle_create_remote_session: created session {} on node '{}'",
                    session_id,
                    node
                );
            }
            Err(e) => {
                let _ = send_error(
                    tx,
                    format!(
                        "Failed to create remote session on '{}': {}",
                        node, e.message
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
                "create_remote_session on node '{}' requires the 'remote' feature",
                node
            ),
        )
        .await;
    }
}

/// Attach an existing remote session to the local registry.
pub async fn handle_attach_remote_session(
    state: &ServerState,
    conn_id: &str,
    node: &str,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "remote")]
    {
        use crate::agent::session_actor::SessionActor;

        let mesh = match state.agent.mesh() {
            Some(m) => m,
            None => {
                let _ =
                    send_error(tx, "mesh not bootstrapped — start with --mesh".to_string()).await;
                return;
            }
        };

        let dht_name = format!("session::{}", session_id);
        let remote_ref = match mesh.lookup_actor::<SessionActor>(dht_name.clone()).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                let _ = send_error(
                    tx,
                    format!(
                        "Session '{}' not found in DHT under '{}'. \
                         The remote node may not have registered it.",
                        session_id, dht_name
                    ),
                )
                .await;
                return;
            }
            Err(e) => {
                let _ =
                    send_error(tx, format!("DHT lookup failed for '{}': {}", dht_name, e)).await;
                return;
            }
        };

        let _session_actor_ref = state
            .agent
            .attach_remote_session(session_id.to_string(), remote_ref, node.to_string())
            .await;

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

        if let Some(cwd_path) = state.default_cwd.clone() {
            let mut cwds = state.session_cwds.lock().await;
            cwds.insert(session_id.to_string(), cwd_path);
        }

        let _ = send_message(
            tx,
            UiServerMessage::SessionCreated {
                agent_id,
                session_id: session_id.to_string(),
                request_id: None,
            },
        )
        .await;

        handle_list_sessions(state, tx).await;

        // Fetch the initial file index from the remote node (same rationale
        // as handle_create_remote_session — no local workspace subscription).
        {
            let state_fi = state.clone();
            let conn_id_fi = conn_id.to_string();
            let tx_fi = tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                handle_get_file_index(&state_fi, &conn_id_fi, &tx_fi).await;
            });
        }

        log::info!(
            "handle_attach_remote_session: attached session {} from node '{}'",
            session_id,
            node
        );
    }
    #[cfg(not(feature = "remote"))]
    {
        let _ = send_error(
            tx,
            format!(
                "attach_remote_session '{}' on node '{}' requires the 'remote' feature",
                session_id, node
            ),
        )
        .await;
    }
}

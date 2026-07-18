// Core ACP infrastructure
pub mod shutdown;
pub mod transport;
pub mod websocket;

// Internal modules
pub(crate) mod client_bridge;
pub mod cwd;
pub mod protocol;
pub mod shared;
pub(crate) mod stdio;
pub(crate) mod trace_context;

#[cfg(test)]
mod session_load_snapshot_tests;
#[cfg(test)]
mod websocket_tests;

// Public re-exports
pub use transport::AcpTransport;
pub use websocket::serve_websocket;

pub use stdio::serve_stdio;

// Existing manual JSON-RPC implementation (for dashboard compatibility)
use crate::acp::shared::{
    AcpLiveEventTranslator, PendingElicitationMap, PermissionMap, RpcMessage, SessionOwnerMap,
    collect_event_sources, dispatch_rpc_message, is_event_owned,
};
use crate::event_fanout::EventFanout;

use axum::Router;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};

pub struct AcpServer {
    agent: Arc<crate::agent::LocalAgentHandle>,
    pending_permissions: PermissionMap,
    pending_elicitations: PendingElicitationMap,
    event_sources: Vec<Arc<EventFanout>>,
    session_owners: SessionOwnerMap,
}

#[derive(Clone)]
struct ServerState {
    agent: Arc<crate::agent::LocalAgentHandle>,
    pending_permissions: PermissionMap,
    pending_elicitations: PendingElicitationMap,
    event_sources: Vec<Arc<EventFanout>>,
    session_owners: SessionOwnerMap,
}

fn spawn_event_forwarders(state: ServerState, conn_id: String, tx: mpsc::Sender<String>) {
    for event_source in &state.event_sources {
        let mut events = event_source.subscribe();
        let tx_events = tx.clone();
        let conn_id_events = conn_id.clone();
        let session_owners = state.session_owners.clone();
        tokio::spawn(async move {
            let mut translator = AcpLiveEventTranslator::new();
            while let Ok(event) = events.recv().await {
                if !is_event_owned(&session_owners, &conn_id_events, &event).await {
                    continue;
                }
                if let Some(notification) = translator.translate_notification(&event) {
                    let json = serde_json::to_string(&notification).unwrap_or_default();
                    if tx_events.send(json).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
}

impl AcpServer {
    pub fn new(agent: Arc<crate::agent::LocalAgentHandle>) -> Self {
        let event_sources = collect_event_sources(&agent);
        let pending_permissions = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations = agent.pending_elicitations();
        let session_owners = Arc::new(Mutex::new(HashMap::new()));

        Self {
            agent,
            pending_permissions,
            pending_elicitations,
            event_sources,
            session_owners,
        }
    }

    pub fn router(self) -> Router {
        websocket::router(self.agent)
    }

    pub async fn run_stdio(self) -> anyhow::Result<()> {
        let state = ServerState {
            agent: self.agent,
            pending_permissions: self.pending_permissions,
            pending_elicitations: self.pending_elicitations,
            event_sources: self.event_sources,
            session_owners: self.session_owners,
        };

        let conn_id = "stdio".to_string();
        let (tx, mut rx) = mpsc::channel::<String>(100);

        spawn_event_forwarders(state.clone(), conn_id.clone(), tx.clone());

        let send_task = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(msg) = rx.recv().await {
                if stdout.write_all(msg.as_bytes()).await.is_err() {
                    break;
                }
                if stdout.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = stdout.flush().await;
            }
        });

        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<RpcMessage>(&line) {
                Ok(request) => {
                    tokio::spawn(dispatch_rpc_message(
                        state.agent.clone(),
                        state.session_owners.clone(),
                        state.pending_permissions.clone(),
                        state.pending_elicitations.clone(),
                        conn_id.clone(),
                        request,
                        tx.clone(),
                    ));
                }
                Err(e) => {
                    log::error!("Failed to parse stdio message: {}", e);
                }
            }
        }

        send_task.abort();

        Ok(())
    }
}

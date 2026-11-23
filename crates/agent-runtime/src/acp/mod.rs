// Core ACP infrastructure
pub mod shutdown;
pub mod transport;

// Internal modules
#[doc(hidden)]
pub mod client_bridge;
pub mod cwd;
#[doc(hidden)]
pub mod delivery;
pub use querymt_agent_core::protocol;
pub mod shared;
#[doc(hidden)]
pub mod stdio;
#[doc(hidden)]
pub mod trace_context;

#[cfg(test)]
mod session_core_conformance;
#[cfg(test)]
mod session_load_snapshot_tests;

// Public re-exports
pub use transport::AcpTransport;

pub use stdio::{AcpSessionLoadOutcome, load_session_with_replay, serve_stdio};

// Existing manual JSON-RPC implementation (for dashboard compatibility)
use crate::acp::shared::{
    AcpLiveEventTranslator, PendingElicitationMap, RpcMessage, SessionOwnerMap,
    dispatch_rpc_message, is_event_owned,
};
use querymt_agent_services::event_fanout::EventFanout;

use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

pub struct AcpServer {
    agent: Arc<crate::agent::LocalAgentHandle>,
    pending_elicitations: PendingElicitationMap,
    event_source: Arc<EventFanout>,
    session_owners: SessionOwnerMap,
}

#[derive(Clone)]
struct ServerState {
    agent: Arc<crate::agent::LocalAgentHandle>,
    pending_elicitations: PendingElicitationMap,
    event_source: Arc<EventFanout>,
    session_owners: SessionOwnerMap,
}

fn spawn_event_forwarders(state: ServerState, conn_id: String, tx: mpsc::Sender<String>) {
    let translator = Arc::new(StdMutex::new(AcpLiveEventTranslator::new()));
    let mut events = state.event_source.subscribe();
    let session_owners = state.session_owners.clone();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if !is_event_owned(&session_owners, &conn_id, &event).await {
                continue;
            }
            let notifications = translator
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .translate_notifications(&event);
            for notification in notifications {
                let json = serde_json::to_string(&notification).unwrap_or_default();
                if tx.send(json).await.is_err() {
                    break;
                }
            }
        }
    });
}

impl AcpServer {
    pub fn new(agent: Arc<crate::agent::LocalAgentHandle>) -> Self {
        let event_source = agent.config.event_sink.fanout().clone();
        let pending_elicitations = agent.pending_elicitations();
        let session_owners = SessionOwnerMap::default();

        Self {
            agent,
            pending_elicitations,
            event_source,
            session_owners,
        }
    }

    pub async fn run_stdio(self) -> anyhow::Result<()> {
        let state = ServerState {
            agent: self.agent,
            pending_elicitations: self.pending_elicitations,
            event_source: self.event_source,
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

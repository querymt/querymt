//! SDK-based stdio server with bidirectional client bridge.
//!
//! This module implements the full bidirectional communication pattern:
//! - Client → Agent: via `AgentSideConnection` calling `Agent` trait methods
//! - Agent → Client: via channel bridge forwarding to `AgentSideConnection` as `Client`
//!
//! ## Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────────┐
//! │                              LocalSet                                         │
//! │                                                                               │
//! │  ┌─────────────────────┐         ┌─────────────────────────────────────┐     │
//! │  │ AgentSideConnection │◄────────│      Bridge Task                    │     │
//! │  │  (impl Client)      │         │  - Receives ClientBridgeMessage     │     │
//! │  │  (impl Agent via    │         │  - Calls connection.notify(...)     │     │
//! │  │   adapter)          │         │  - Calls connection.request_perm()  │     │
//! │  └──────────┬──────────┘         └──────────────────▲──────────────────┘     │
//! │             │                                        │                        │
//! │             │ calls Agent                            │ mpsc::channel          │
//! │             ▼                                        │ (Send types)           │
//! │  ┌─────────────────────┐                            │                        │
//! │  │  ApcAgentAdapter    │                            │                        │
//! │  └──────────┬──────────┘                            │                        │
//! │             │                                        │                        │
//! │             │ forwards                               │                        │
//! │             ▼                                        │                        │
//! │  ┌───────────────────────────────────────────────────┴──────────────────┐    │
//! │  │                     T: SendAgent                                      │    │
//! │  │  - Holds ClientBridgeSender (Send + Sync)                            │    │
//! │  │  - Can spawn tokio::spawn() tasks for parallel work                  │    │
//! │  │  - Calls bridge.notify() / bridge.request_permission()               │    │
//! │  └──────────────────────────────────────────────────────────────────────┘    │
//! │                                                                               │
//! └───────────────────────────────────────────────────────────────────────────────┘
//! ```

use crate::acp::client_bridge::{ClientBridgeMessage, ClientBridgeSender};
use crate::acp::shutdown;
use crate::event_fanout::EventFanout;
use crate::send_agent::ApcAgentAdapter;
use agent_client_protocol::{AgentSideConnection, Client, SessionId, SessionNotification};
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{info_span, instrument};

/// Run the client bridge task inside LocalSet.
///
/// This task receives messages from the agent (Send world via mpsc channel)
/// and forwards them to the AgentSideConnection (LocalSet world, !Send).
///
/// # Message Handling
///
/// - **Notification**: Fire-and-forget, just call `connection.session_notification()`
/// - **RequestPermission**: Request-response, send result back via oneshot channel
#[instrument(name = "acp.bridge_task", skip(rx, _connection))]
async fn run_bridge_task(
    mut rx: mpsc::Receiver<ClientBridgeMessage>,
    _connection: Rc<AgentSideConnection>,
) {
    log::info!("Client bridge task started");

    while let Some(msg) = rx.recv().await {
        match msg {
            ClientBridgeMessage::Notification(notif) => {
                let _span = info_span!("acp.notification").entered();
                log::debug!("Bridge: forwarding session notification");
                if let Err(e) = _connection.session_notification(notif).await {
                    log::error!("Bridge: session_notification failed: {:?}", e);
                }
            }
            ClientBridgeMessage::RequestPermission {
                request,
                response_tx,
            } => {
                let _span = info_span!("acp.permission_request").entered();
                log::debug!("Bridge: forwarding permission request");
                let result = _connection.request_permission(request).await;

                if response_tx.send(result).is_err() {
                    log::error!("Bridge: failed to send permission response (receiver dropped)");
                }
            }
            ClientBridgeMessage::Elicit {
                elicitation_id: _,
                message: _,
                requested_schema: _,
                source: _,
                response_tx,
            } => {
                // Elicitations are not supported over stdio transport yet
                // They should be handled through events and elicitation_result RPC
                log::warn!("Elicit not implemented for stdio transport");
                let _ = response_tx.send(crate::elicitation::ElicitationResponse {
                    action: crate::elicitation::ElicitationAction::Decline,
                    content: None,
                });
            }
        }
    }

    log::info!("Client bridge task ended (channel closed)");
}

/// Forwards events from EventFanout to the client bridge.
///
/// This task subscribes to the EventFanout and forwards all events to the ACP client
/// via the bridge. Unlike the WebSocket server, this does not filter events by
/// session ownership because the SDK stdio server serves a single client (Zed).
///
/// The forwarder automatically stops when:
/// - The EventFanout is shut down (recv returns error)
/// - The bridge channel closes (notify returns error)
#[instrument(name = "acp.event_forwarder", skip(event_fanout, bridge, shutdown_tx))]
fn spawn_event_bridge_forwarder(
    event_fanout: Arc<EventFanout>,
    bridge: ClientBridgeSender,
    shutdown_tx: tokio::sync::mpsc::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        log::info!("Event bridge forwarder started");
        let mut events = event_fanout.subscribe();

        let mut forwarded_count = 0u64;

        loop {
            match events.recv().await {
                Ok(event) => {
                    // Translate event to SessionUpdate
                    if let Some(update) = crate::acp::shared::translate_event_to_update(&event) {
                        let notification = SessionNotification::new(
                            SessionId::from(event.session_id().to_owned()),
                            update,
                        );

                        // Forward via bridge
                        if let Err(e) = bridge.notify(notification).await {
                            log::info!(
                                "Bridge closed after forwarding {} events, stopping forwarder: {}",
                                forwarded_count,
                                e
                            );
                            break;
                        }

                        forwarded_count += 1;
                        log::trace!(
                            "Forwarded event {} for session {}",
                            forwarded_count,
                            event.session_id()
                        );
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::info!(
                        "EventFanout closed after forwarding {} events, stopping forwarder",
                        forwarded_count
                    );
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    log::warn!(
                        "Event forwarder lagged, skipped {} events (total forwarded: {})",
                        skipped,
                        forwarded_count
                    );
                    // Continue receiving - broadcast channel drops old messages
                }
            }
        }

        // Signal that this forwarder has stopped (for graceful shutdown coordination)
        let _ = shutdown_tx.send(()).await;
        log::info!(
            "Event bridge forwarder stopped (forwarded {} total events)",
            forwarded_count
        );
    })
}

/// Run an ACP stdio server with a QueryMTAgent.
///
/// This is a convenience function for running QueryMTAgent over stdio with the ACP protocol.
/// It sets up the bidirectional bridge, configures the agent, and handles graceful shutdown.
///
/// # Example
///
/// ```rust,no_run
/// use querymt_agent::simple::Agent;
/// use querymt_agent::acp::serve_stdio;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let agent = Agent::single()
///         .provider("anthropic", "claude-sonnet-4-20250514")
///         .cwd("/tmp")
///         .build()
///         .await?;
///     
///     serve_stdio(agent.inner()).await
/// }
/// ```
///
/// # Graceful Shutdown
///
/// The server handles SIGTERM and SIGINT (Ctrl+C) for graceful shutdown.
/// Current operations are allowed to complete before exit.
#[instrument(name = "acp.serve_stdio", skip(agent))]
pub async fn serve_stdio(agent: Arc<crate::agent::AgentHandle>) -> anyhow::Result<()> {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async move {
            log::info!("Starting ACP stdio server for QueryMTAgent");

            // 1. Create the channel for agent→client communication
            let (tx, rx) = mpsc::channel::<ClientBridgeMessage>(100);
            let bridge_sender = ClientBridgeSender::new(tx);
            log::debug!("Created bridge channel");

            // 2. Set the bridge on the agent
            agent.set_bridge(bridge_sender.clone());
            log::debug!("Set bridge on agent");

            // 3. Collect EventFanouts from agent and delegates
            let event_sources = crate::acp::shared::collect_event_sources(&agent);
            log::debug!("Collected {} event source(s) for forwarding", event_sources.len());

            // 4. Create shutdown coordination channel
            // Used by forwarders to signal when they've stopped
            let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(event_sources.len());

            // 5. Spawn event bridge forwarders (one per EventFanout)
            let mut forwarder_handles = Vec::new();
            for (idx, event_fanout) in event_sources.into_iter().enumerate() {
                let handle = spawn_event_bridge_forwarder(
                    event_fanout,
                    bridge_sender.clone(),
                    shutdown_tx.clone(),
                );
                forwarder_handles.push(handle);
                log::debug!("Spawned event forwarder #{}", idx);
            }
            log::info!("Spawned {} event forwarder(s)", forwarder_handles.len());

            // Drop the shutdown_tx so the channel closes when all forwarders stop
            drop(shutdown_tx);

            // 6. Create adapter for SDK Agent trait (clone Arc so we can use agent later for shutdown)
            let adapter = ApcAgentAdapter::new(agent.clone());

            // 7. Set up stdio streams
            let stdin = tokio::io::stdin().compat();
            let stdout = tokio::io::stdout().compat_write();

            // 8. Create the SDK connection
            let (connection, io_task) = AgentSideConnection::new(adapter, stdout, stdin, |fut| {
                tokio::task::spawn_local(fut);
            });
            log::debug!("Created AgentSideConnection");

            // 9. Wrap connection in Rc for LocalSet sharing
            let connection = Rc::new(connection);

            // 10. Spawn bridge task
            let bridge_task = tokio::task::spawn_local(run_bridge_task(rx, connection.clone()));
            log::info!("Bridge task spawned");

            // 11. Spawn IO task
            let mut io_handle = tokio::task::spawn_local(io_task);
            log::info!("ACP stdio server ready, listening on stdin...");

            // 12. Wait for completion or shutdown signal
            let shutdown_triggered = tokio::select! {
                result = &mut io_handle => {
                    match result {
                        Ok(Ok(())) => log::info!("IO task completed successfully (likely due to stdin close)"),
                        Ok(Err(e)) => log::error!("IO task error: {}", e),
                        Err(e) => log::error!("IO task panicked: {}", e),
                    }
                    // Treat IO task completion as shutdown too (stdin closed)
                    true
                }
                _ = shutdown::signal() => {
                    log::info!("Shutdown signal received, stopping server...");
                    true
                }
            };

            // 13. Clean up - graceful shutdown sequence
            if shutdown_triggered {
                log::info!("Initiating graceful shutdown...");

                // Step 1: Shutdown agent (stops event emission)
                log::info!("Shutting down agent...");
                agent.shutdown().await;
                log::info!("Agent shutdown complete");

                // Step 2: Wait for forwarders to finish processing remaining events
                log::info!("Waiting for event forwarders to stop...");
                let forwarder_wait = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    async {
                        // Wait for all forwarders to signal completion
                        while shutdown_rx.recv().await.is_some() {
                            log::debug!("Event forwarder stopped");
                        }
                    }
                );

                match forwarder_wait.await {
                    Ok(_) => log::info!("All event forwarders stopped gracefully"),
                    Err(_) => {
                        log::warn!("Event forwarders did not stop within 5s, aborting...");
                        for (idx, handle) in forwarder_handles.iter().enumerate() {
                            handle.abort();
                            log::debug!("Aborted event forwarder #{}", idx);
                        }
                    }
                }

                // Step 3: Abort IO task
                log::info!("Aborting I/O task...");
                io_handle.abort();
            }

            // Step 4: Abort bridge task (always, even if not shutdown_triggered)
            log::info!("Aborting bridge task...");
            bridge_task.abort();
            log::info!("Bridge task aborted");
            log::info!("ACP stdio server shutdown complete");
            log::info!("Exiting LocalSet...");
            Ok(())
        })
        .await
}

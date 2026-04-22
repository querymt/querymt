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
use crate::send_agent::SendAgent;
use agent_client_protocol::schema::{
    AuthenticateRequest, CancelNotification, ExtRequest, ForkSessionRequest, InitializeRequest,
    ListSessionsRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    ResumeSessionRequest, SessionId, SessionNotification, SetSessionConfigOptionRequest,
    SetSessionModeRequest, SetSessionModelRequest,
};
use agent_client_protocol::{self as acp, ByteStreams, ConnectionTo};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::info_span;

/// Run the client bridge task inside LocalSet.
///
/// This task receives messages from the agent (Send world via mpsc channel)
/// and forwards them to the AgentSideConnection (LocalSet world, !Send).
///
/// # Message Handling
///
/// - **Notification**: Fire-and-forget, just call `connection.session_notification()`
/// - **RequestPermission**: Request-response, send result back via oneshot channel
async fn run_bridge_task(
    mut rx: mpsc::Receiver<ClientBridgeMessage>,
    connection: ConnectionTo<acp::Client>,
) {
    log::info!("Client bridge task started");

    while let Some(msg) = rx.recv().await {
        match msg {
            ClientBridgeMessage::Notification(notif) => {
                let _span = info_span!("acp.notification").entered();
                log::debug!("Bridge: forwarding session notification");
                if let Err(e) = connection.send_notification(notif) {
                    log::error!("Bridge: session_notification failed: {:?}", e);
                }
            }
            ClientBridgeMessage::RequestPermission {
                request,
                response_tx,
            } => {
                let _span = info_span!("acp.permission_request").entered();
                log::debug!("Bridge: forwarding permission request");
                let result = connection.send_request(request).block_task().await;

                if response_tx.send(result).is_err() {
                    log::error!("Bridge: failed to send permission response (receiver dropped)");
                }
            }
            ClientBridgeMessage::Elicit {
                elicitation_id,
                message,
                requested_schema,
                source,
                response_tx,
            } => {
                let _span = info_span!("acp.elicit").entered();
                log::debug!(
                    "Bridge: forwarding elicitation via ext_method: {}",
                    elicitation_id
                );

                // Serialize elicitation as an ext_method request to the client
                let params = serde_json::json!({
                    "elicitationId": elicitation_id,
                    "message": message,
                    "requestedSchema": requested_schema,
                    "source": source,
                });
                let params_json = serde_json::to_string(&params).unwrap();
                let raw_value = serde_json::value::RawValue::from_string(params_json)
                    .expect("valid JSON from serde_json::to_string");

                let ext_req = ExtRequest::new("querymt/elicit", std::sync::Arc::from(raw_value));
                let result = match acp::UntypedMessage::new(&ext_req.method, &ext_req.params) {
                    Ok(outbound) => connection.send_request(outbound).block_task().await,
                    Err(err) => Err(err),
                };

                let parsed = match result {
                    Ok(ext_resp) => {
                        #[derive(serde::Deserialize)]
                        struct ElicitResponse {
                            action: String,
                            content: Option<serde_json::Value>,
                        }
                        match serde_json::from_value::<ElicitResponse>(ext_resp) {
                            Ok(resp) => {
                                let action = match resp.action.as_str() {
                                    "accept" => crate::elicitation::ElicitationAction::Accept,
                                    "cancel" => crate::elicitation::ElicitationAction::Cancel,
                                    _ => crate::elicitation::ElicitationAction::Decline,
                                };
                                crate::elicitation::ElicitationResponse {
                                    action,
                                    content: resp.content,
                                }
                            }
                            Err(e) => {
                                log::error!("Failed to parse elicitation response: {}", e);
                                crate::elicitation::ElicitationResponse {
                                    action: crate::elicitation::ElicitationAction::Decline,
                                    content: None,
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("Elicitation ext_method failed: {:?}", e);
                        crate::elicitation::ElicitationResponse {
                            action: crate::elicitation::ElicitationAction::Decline,
                            content: None,
                        }
                    }
                };

                if response_tx.send(parsed).is_err() {
                    log::error!("Bridge: failed to send elicitation response (receiver dropped)");
                }
            }
            ClientBridgeMessage::WorkspaceQuery { query, response_tx } => {
                let _span = info_span!("acp.workspace_query").entered();
                log::debug!("Bridge: forwarding workspace query: {:?}", query);

                // Serialize the query as raw JSON for ExtRequest
                let params_json = serde_json::to_string(&query).unwrap();
                let raw_value = serde_json::value::RawValue::from_string(params_json)
                    .expect("valid JSON from serde_json::to_string");

                let ext_req = ExtRequest::new("workspace/query", Arc::from(raw_value));

                // Send via ext request which becomes "_workspace/query" on the wire
                let result = match acp::UntypedMessage::new(&ext_req.method, &ext_req.params) {
                    Ok(outbound) => connection.send_request(outbound).block_task().await,
                    Err(err) => Err(err),
                };

                let parsed = match result {
                    Ok(ext_resp) => serde_json::from_value(ext_resp).map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to parse workspace query response: {}", e))
                    }),
                    Err(e) => Err(e),
                };

                if response_tx.send(parsed).is_err() {
                    log::error!(
                        "Bridge: failed to send workspace query response (receiver dropped)"
                    );
                }
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
/// use querymt_agent::api::Agent;
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
pub async fn serve_stdio(agent: Arc<crate::agent::LocalAgentHandle>) -> anyhow::Result<()> {
    log::info!("Starting ACP stdio server for QueryMTAgent");

    // 1. Create the channel for agent→client communication
    let (tx, rx) = mpsc::channel::<ClientBridgeMessage>(100);
    let bridge_sender = ClientBridgeSender::new(tx);
    log::debug!("Created bridge channel");

    // 2. Set the bridge on the agent
    agent.set_bridge(bridge_sender.clone()).await;
    log::debug!("Set bridge on agent");

    // 3. Collect EventFanouts from agent and delegates
    let event_sources = crate::acp::shared::collect_event_sources(&agent);
    log::debug!(
        "Collected {} event source(s) for forwarding",
        event_sources.len()
    );

    // 4. Create shutdown coordination channel
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(event_sources.len());

    // 5. Spawn event bridge forwarders (one per EventFanout)
    let mut forwarder_handles = Vec::new();
    for (idx, event_fanout) in event_sources.into_iter().enumerate() {
        let handle =
            spawn_event_bridge_forwarder(event_fanout, bridge_sender.clone(), shutdown_tx.clone());
        forwarder_handles.push(handle);
        log::debug!("Spawned event forwarder #{}", idx);
    }
    log::info!("Spawned {} event forwarder(s)", forwarder_handles.len());

    drop(shutdown_tx);

    // 6. Set up stdio streams and connect ACP agent role.
    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();

    let serve_fut = acp::Agent
        .builder()
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: InitializeRequest, responder, _cx| {
                    responder.respond_with_result(agent.initialize(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: AuthenticateRequest, responder, _cx| {
                    responder.respond_with_result(agent.authenticate(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: NewSessionRequest, responder, _cx| {
                    responder.respond_with_result(agent.new_session(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: PromptRequest, responder, _cx| {
                    responder.respond_with_result(agent.prompt(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let agent = agent.clone();
                async move |notif: CancelNotification, _cx| agent.cancel(notif).await
            },
            acp::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: LoadSessionRequest, responder, _cx| {
                    responder.respond_with_result(agent.load_session(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: ListSessionsRequest, responder, _cx| {
                    responder.respond_with_result(agent.list_sessions(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: ForkSessionRequest, responder, _cx| {
                    responder.respond_with_result(agent.fork_session(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: ResumeSessionRequest, responder, _cx| {
                    responder.respond_with_result(agent.resume_session(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: SetSessionModelRequest, responder, _cx| {
                    responder.respond_with_result(agent.set_session_model(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: SetSessionModeRequest, responder, _cx| {
                    responder.respond_with_result(agent.set_session_mode(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: SetSessionConfigOptionRequest, responder, _cx| {
                    responder.respond_with_result(agent.set_session_config_option(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_dispatch(
            {
                let agent = agent.clone();
                async move |dispatch: acp::Dispatch<acp::UntypedMessage, acp::UntypedMessage>,
                            _cx| {
                    let dispatch = match dispatch.into_request::<acp::UntypedMessage>() {
                        Ok(Ok((request, responder))) => {
                            if request.method.starts_with("_querymt/")
                                || request.method.starts_with("querymt/")
                            {
                                let params_json = serde_json::to_string(&request.params)
                                    .unwrap_or_else(|_| "null".to_string());
                                let raw = serde_json::value::RawValue::from_string(params_json)
                                    .unwrap_or_else(|_| {
                                        serde_json::value::RawValue::from_string("null".to_string())
                                            .unwrap()
                                    });
                                let ext_req =
                                    ExtRequest::new(request.method, std::sync::Arc::from(raw));
                                let ext_resp = agent.ext_method(ext_req).await;
                                let ext_value = ext_resp.and_then(|resp| {
                                    serde_json::from_str(resp.0.get()).map_err(|e| {
                                        acp::Error::internal_error().data(e.to_string())
                                    })
                                });
                                responder.respond_with_result(ext_value)?;
                                return Ok(acp::Handled::Yes);
                            }
                            acp::Dispatch::Request(request, responder)
                        }
                        Ok(Err(dispatch)) => dispatch,
                        Err(err) => return Err(err),
                    };

                    let dispatch = match dispatch.into_notification::<acp::UntypedMessage>() {
                        Ok(Ok(notification)) => {
                            if notification.method.starts_with("_querymt/")
                                || notification.method.starts_with("querymt/")
                            {
                                let params_json = serde_json::to_string(&notification.params)
                                    .unwrap_or_else(|_| "null".to_string());
                                let raw = serde_json::value::RawValue::from_string(params_json)
                                    .unwrap_or_else(|_| {
                                        serde_json::value::RawValue::from_string("null".to_string())
                                            .unwrap()
                                    });
                                let ext_notif = agent_client_protocol::schema::ExtNotification::new(
                                    notification.method,
                                    std::sync::Arc::from(raw),
                                );
                                agent.ext_notification(ext_notif).await?;
                                return Ok(acp::Handled::Yes);
                            }
                            acp::Dispatch::Notification(notification)
                        }
                        Ok(Err(dispatch)) => dispatch,
                        Err(err) => return Err(err),
                    };

                    Ok(acp::Handled::No {
                        message: dispatch,
                        retry: false,
                    })
                }
            },
            acp::on_receive_dispatch!(),
        )
        .connect_with(ByteStreams::new(stdout, stdin), async move |cx| {
            run_bridge_task(rx, cx).await;
            Ok(())
        });

    log::info!("ACP stdio server ready, listening on stdin...");

    let shutdown_triggered = tokio::select! {
        result = serve_fut => {
            match result {
                Ok(()) => log::info!("ACP stdio connection finished (likely due to stdin close)"),
                Err(e) => log::error!("ACP stdio connection error: {}", e),
            }
            true
        }
        _ = shutdown::signal() => {
            log::info!("Shutdown signal received, stopping server...");
            true
        }
    };

    if shutdown_triggered {
        log::info!("Initiating graceful shutdown...");
        log::info!("Shutting down agent...");
        agent.shutdown().await;
        log::info!("Agent shutdown complete");

        log::info!("Waiting for event forwarders to stop...");
        let forwarder_wait = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while shutdown_rx.recv().await.is_some() {
                log::debug!("Event forwarder stopped");
            }
        });

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
    }

    log::info!("ACP stdio server shutdown complete");
    Ok(())
}

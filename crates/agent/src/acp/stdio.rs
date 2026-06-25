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
use crate::acp::protocol::{
    AgentRequest, AuthenticateRequest, CancelNotification, ClientNotification, ClientRequest,
    CloseSessionRequest, DeleteSessionRequest, ExtRequest, ForkSessionRequest, InitializeRequest,
    ListSessionsRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    ResumeSessionRequest, SessionId, SessionNotification, SetSessionConfigOptionRequest,
    SetSessionModeRequest, SetSessionModelRequest,
};
use crate::acp::shared::{AcpLiveEventTranslator, replay_agent_events_to_session_notifications};
use crate::acp::shutdown;
use crate::event_fanout::EventFanout;
use crate::send_agent::SendAgent;
use agent_client_protocol::{self as acp, ByteStreams, ConnectionTo};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::info_span;

fn agent_ext_request(
    method: &'static str,
    params: impl serde::Serialize,
) -> Result<AgentRequest, acp::Error> {
    let params_json = serde_json::to_string(&params).map_err(acp::Error::into_internal_error)?;
    let raw_value = serde_json::value::RawValue::from_string(params_json)
        .map_err(acp::Error::into_internal_error)?;
    Ok(AgentRequest::ExtMethodRequest(ExtRequest::new(
        method,
        Arc::from(raw_value),
    )))
}

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
            ClientBridgeMessage::ExtNotification(notif) => {
                let _span = info_span!("acp.ext_notification").entered();
                log::debug!("Bridge: forwarding ext notification");
                if let Err(e) =
                    connection.send_notification(ClientNotification::ExtNotification(notif))
                {
                    log::error!("Bridge: ext_notification failed: {:?}", e);
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
                    "Bridge: forwarding elicitation via ACP extension request: {}",
                    elicitation_id
                );

                let params = serde_json::json!({
                    "elicitationId": elicitation_id,
                    "message": message,
                    "requestedSchema": requested_schema,
                    "source": source,
                });
                let result = match agent_ext_request("_querymt/elicit", params) {
                    Ok(ext_req) => connection.send_request(ext_req).block_task().await,
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

                // Send a wire-form ACP extension request to the client.
                let result = match agent_ext_request("_workspace/query", &query) {
                    Ok(ext_req) => connection.send_request(ext_req).block_task().await,
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
        let mut translator = AcpLiveEventTranslator::new();

        loop {
            match events.recv().await {
                Ok(event) => {
                    // Translate event to a live ACP SessionUpdate
                    if let Some(update) = translator.translate_update(&event) {
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
                async move |req: PromptRequest, responder, cx| {
                    // Prompt execution can run for a long time, so spawn it and
                    // return immediately to keep ACP able to process cancel.
                    let agent = agent.clone();
                    cx.spawn(
                        async move { responder.respond_with_result(agent.prompt(req).await) },
                    )?;
                    Ok(())
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
                let bridge_sender = bridge_sender.clone();
                async move |req: LoadSessionRequest, responder, _cx| {
                    let session_id = req.session_id.to_string();
                    let response = agent.load_session(req).await;
                    if response.is_ok() {
                        let session_ref = {
                            let registry = agent.registry.lock().await;
                            registry.get(&session_id).cloned()
                        };
                        match session_ref {
                            Some(session_ref) => match session_ref.get_event_stream().await {
                                Ok(events) => {
                                    for notification in replay_agent_events_to_session_notifications(&session_id, events) {
                                        if let Err(err) = bridge_sender.notify(notification).await {
                                            log::warn!(
                                                "Failed to send session/load replay notification for {}: {}",
                                                session_id,
                                                err
                                            );
                                            break;
                                        }
                                    }
                                }
                                Err(err) => {
                                    log::warn!(
                                        "Failed to load session/load replay events for {}: {}",
                                        session_id,
                                        err
                                    );
                                }
                            },
                            None => {
                                log::warn!(
                                    "Loaded session {} but no runtime session ref was available for replay",
                                    session_id
                                );
                            }
                        }
                    }
                    responder.respond_with_result(response)
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
                async move |req: CloseSessionRequest, responder, _cx| {
                    responder.respond_with_result(agent.close_session(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: DeleteSessionRequest, responder, _cx| {
                    responder.respond_with_result(agent.delete_session(req).await)
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
                async move |dispatch: acp::Dispatch<ClientRequest, ClientNotification>, _cx| {
                    match dispatch {
                        acp::Dispatch::Request(ClientRequest::ExtMethodRequest(req), responder) => {
                            let ext_value = agent.ext_method(req).await.and_then(|resp| {
                                serde_json::from_str(resp.0.get())
                                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))
                            });
                            responder.respond_with_result(ext_value)?;
                            Ok(acp::Handled::Yes)
                        }
                        acp::Dispatch::Notification(ClientNotification::ExtNotification(notif)) => {
                            agent.ext_notification(notif).await?;
                            Ok(acp::Handled::Yes)
                        }
                        acp::Dispatch::Request(request, responder) => Ok(acp::Handled::No {
                            message: acp::Dispatch::Request(request, responder),
                            retry: false,
                        }),
                        acp::Dispatch::Notification(notification) => Ok(acp::Handled::No {
                            message: acp::Dispatch::Notification(notification),
                            retry: false,
                        }),
                        acp::Dispatch::Response(result, router) => Ok(acp::Handled::No {
                            message: acp::Dispatch::Response(result, router),
                            retry: false,
                        }),
                    }
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

#[cfg(test)]
mod stdio_tests {
    use super::{CancelNotification, PromptRequest, agent_ext_request};
    use crate::acp::protocol::{AgentRequest, PromptResponse, StopReason};
    use agent_client_protocol::{Agent, ByteStreams, JsonRpcMessage};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, split};
    use tokio::sync::{Mutex, Notify};
    use tokio::time::{Duration, timeout};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    #[test]
    fn outbound_workspace_query_preserves_wire_extension_method() {
        let request = agent_ext_request(
            "_workspace/query",
            serde_json::json!({ "sessionId": "s-1" }),
        )
        .expect("agent ext request");

        assert_eq!(request.method(), "_workspace/query");
        match &request {
            AgentRequest::ExtMethodRequest(ext) => {
                assert_eq!(ext.method.as_ref(), "_workspace/query");
            }
            other => panic!("expected ext request, got {other:?}"),
        }

        let untyped = request.to_untyped_message().expect("untyped ext request");
        assert_eq!(untyped.method, "_workspace/query");
    }

    #[tokio::test]
    async fn spawned_prompt_handler_keeps_cancel_unblocked() {
        let prompt_started = Arc::new(Notify::new());
        let release_prompt = Arc::new(Notify::new());
        let cancel_seen = Arc::new(Notify::new());
        let cancel_flag = Arc::new(Mutex::new(false));

        let prompt_started_for_handler = prompt_started.clone();
        let release_prompt_for_handler = release_prompt.clone();
        let cancel_seen_for_handler = cancel_seen.clone();
        let cancel_flag_for_handler = cancel_flag.clone();

        let (client, server) = tokio::io::duplex(4096);
        let (client_read, mut client_write) = split(client);
        let (server_read, server_write) = split(server);

        let server_task = tokio::spawn(async move {
            Agent
                .builder()
                .on_receive_request(
                    async move |_req: PromptRequest, responder, cx| {
                        let prompt_started = prompt_started_for_handler.clone();
                        let release_prompt = release_prompt_for_handler.clone();
                        cx.spawn(async move {
                            prompt_started.notify_one();
                            release_prompt.notified().await;
                            responder.respond(PromptResponse::new(StopReason::Cancelled))
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |_notif: CancelNotification, _cx| {
                        *cancel_flag_for_handler.lock().await = true;
                        cancel_seen_for_handler.notify_one();
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_to(ByteStreams::new(
                    server_write.compat_write(),
                    server_read.compat(),
                ))
                .await
                .expect("server should run")
        });

        let mut reader = BufReader::new(client_read);

        client_write
            .write_all(
                br#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s-1","prompt":[{"type":"text","text":"long task"}]}}
{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s-1"}}
"#,
            )
            .await
            .expect("requests should write");

        timeout(Duration::from_secs(2), prompt_started.notified())
            .await
            .expect("prompt handler should start");
        timeout(Duration::from_secs(2), cancel_seen.notified())
            .await
            .expect("cancel should be processed while prompt is blocked");
        assert!(
            *cancel_flag.lock().await,
            "cancel should be processed before the prompt is released"
        );

        release_prompt.notify_one();

        let mut response = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut response))
            .await
            .expect("response should arrive")
            .expect("response should read");
        assert!(response.contains("\"stopReason\":\"cancelled\""));

        drop(reader);
        drop(client_write);
        server_task.abort();
        let _ = server_task.await;
    }
}

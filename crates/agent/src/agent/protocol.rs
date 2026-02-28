//! Agent Client Protocol helper functions for MCP server management
use crate::agent::core::McpToolState;
use crate::error::AgentError;
use agent_client_protocol::{Error, McpServer, McpServerHttp, McpServerStdio};
use log::warn;
use querymt::tool_decorator::CallFunctionTool;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::{
    RoleClient,
    model::Implementation,
    service::{RunningService, serve_client},
    transport::{
        StreamableHttpClientTransport, child_process::TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use std::collections::HashMap;
use std::sync::Arc;

/// Builds MCP state by connecting to configured servers.
///
/// Populates `tool_state` with the initial tool list fetched from each server.
/// Each [`McpClientHandler`][crate::elicitation::McpClientHandler] created here
/// holds a clone of `tool_state` so it can refresh the maps live when the server
/// sends a `tools/list_changed` notification.
///
/// Returns the `RunningService` map (must be kept alive to maintain connections).
pub(crate) async fn build_mcp_state(
    servers: &[McpServer],
    pending_elicitations: crate::elicitation::PendingElicitationMap,
    event_sink: Arc<crate::event_sink::EventSink>,
    session_id: String,
    client_impl: &Implementation,
    tool_state: Arc<McpToolState>,
) -> Result<HashMap<String, RunningService<RoleClient, crate::elicitation::McpClientHandler>>, Error>
{
    let mut clients = HashMap::new();
    let mut tools = HashMap::new();
    let mut tool_defs = Vec::new();

    for server in servers {
        let (server_name, running): (
            String,
            RunningService<RoleClient, crate::elicitation::McpClientHandler>,
        ) = start_mcp_server(
            server,
            pending_elicitations.clone(),
            event_sink.clone(),
            session_id.clone(),
            client_impl,
            tool_state.clone(),
        )
        .await?;
        let peer = running.peer().clone();
        let tool_list = peer
            .list_all_tools()
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        for tool in tool_list {
            let adapter = querymt::mcp::adapter::McpToolAdapter::try_new(
                tool,
                peer.clone(),
                server_name.clone(),
            )
            .map_err(|e| Error::internal_error().data(e.to_string()))?;
            let name = adapter.descriptor().function.name.clone();
            if tools.contains_key(&name) {
                warn!("Duplicate MCP tool '{}', keeping first instance", name);
                continue;
            }
            tool_defs.push(adapter.descriptor());
            tools.insert(name, Arc::new(adapter));
        }
        clients.insert(server_name, running);
    }

    // Populate the shared tool state with the initial tool list.
    *tool_state.tools.write().unwrap() = tools;
    *tool_state.tool_defs.write().unwrap() = tool_defs;

    Ok(clients)
}

/// Starts an MCP server based on its configuration.
async fn start_mcp_server(
    server: &McpServer,
    pending_elicitations: crate::elicitation::PendingElicitationMap,
    event_sink: Arc<crate::event_sink::EventSink>,
    session_id: String,
    client_impl: &Implementation,
    tool_state: Arc<McpToolState>,
) -> Result<
    (
        String,
        RunningService<RoleClient, crate::elicitation::McpClientHandler>,
    ),
    Error,
> {
    match server {
        McpServer::Stdio(stdio) => {
            let McpServerStdio {
                name,
                command,
                args,
                env,
                ..
            } = stdio.clone();
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args)
                .envs(env.iter().map(|item| (&item.name, &item.value)))
                .stderr(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::piped())
                .stdin(std::process::Stdio::piped());
            let transport = TokioChildProcess::new(cmd)
                .map_err(|e| Error::internal_error().data(e.to_string()))?;
            let handler = crate::elicitation::McpClientHandler::new(
                pending_elicitations.clone(),
                event_sink.clone(),
                name.clone(),
                session_id.clone(),
                client_impl.clone(),
                tool_state.clone(),
            );
            let running: RunningService<RoleClient, crate::elicitation::McpClientHandler> =
                serve_client(handler, transport).await.map_err(|e| {
                    Error::from(AgentError::McpServerFailed {
                        transport: "stdio".to_string(),
                        reason: e.to_string(),
                    })
                })?;
            Ok((name, running))
        }
        McpServer::Http(http) => {
            let McpServerHttp {
                name, url, headers, ..
            } = http.clone();
            let client = reqwest::ClientBuilder::new()
                .default_headers(headers_to_map(&headers)?)
                .build()
                .map_err(|e| Error::internal_error().data(e.to_string()))?;
            let transport = StreamableHttpClientTransport::with_client(
                client,
                StreamableHttpClientTransportConfig::with_uri(url),
            );
            let handler = crate::elicitation::McpClientHandler::new(
                pending_elicitations.clone(),
                event_sink.clone(),
                name.clone(),
                session_id.clone(),
                client_impl.clone(),
                tool_state.clone(),
            );
            let running = serve_client(handler, transport).await.map_err(|e| {
                Error::from(AgentError::McpServerFailed {
                    transport: "http".to_string(),
                    reason: e.to_string(),
                })
            })?;
            Ok((name, running))
        }
        _ => Err(Error::invalid_params().data(serde_json::json!({
            "message": "unsupported MCP server configuration",
        }))),
    }
}

/// Converts HTTP headers from protocol format to reqwest format
fn headers_to_map(headers: &[agent_client_protocol::HttpHeader]) -> Result<HeaderMap, Error> {
    let mut map = HeaderMap::new();
    for header in headers {
        let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({
                "message": "invalid header name",
                "name": header.name,
                "error": e.to_string(),
            }))
        })?;
        let value = HeaderValue::from_str(&header.value).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({
                "message": "invalid header value",
                "name": header.name,
                "error": e.to_string(),
            }))
        })?;
        map.insert(name, value);
    }
    Ok(map)
}

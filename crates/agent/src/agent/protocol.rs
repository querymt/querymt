//! Agent Client Protocol helper functions for MCP server management
use crate::acp::protocol::{Error, McpServer, McpServerHttp, McpServerStdio};
use crate::agent::core::McpToolState;
use crate::agent::session_mcp::ConnectedMcpPeer;
use crate::error::AgentError;
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
    cwd: Option<&std::path::Path>,
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
            cwd,
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
    tool_state.store(crate::agent::core::McpToolSnapshot {
        tools,
        tool_defs,
        tools_hash: None,
    });

    Ok(clients)
}

/// Merge tools from already-connected MCP peers into an existing tool state.
///
/// The peers are already initialized and can be reused across sessions without
/// re-initializing.
pub(crate) async fn merge_connected_mcp_peers(
    tool_state: Arc<McpToolState>,
    connected_peers: &[ConnectedMcpPeer],
) -> Result<(), Error> {
    if connected_peers.is_empty() {
        return Ok(());
    }

    let current = tool_state.load();
    let mut tools = current.tools.clone();
    let mut tool_defs = current.tool_defs.clone();

    for connected in connected_peers {
        let tool_list = connected
            .peer
            .list_all_tools()
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        for tool in tool_list {
            let adapter = querymt::mcp::adapter::McpToolAdapter::try_new(
                tool,
                connected.peer.clone(),
                connected.server_name.clone(),
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
    }

    tool_state.store(crate::agent::core::McpToolSnapshot {
        tools,
        tool_defs,
        tools_hash: None,
    });

    Ok(())
}

/// Starts an MCP server based on its configuration.
/// Build the stdio child-process command for an MCP server, running inside
/// the session's effective workspace when one is set so workspace-relative
/// server entries resolve against the bound checkout rather than the agent
/// process's working directory.
fn stdio_command(stdio: &McpServerStdio, cwd: Option<&std::path::Path>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&stdio.command);
    cmd.args(stdio.args.iter())
        .envs(stdio.env.iter().map(|item| (&item.name, &item.value)))
        .stderr(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd
}

async fn start_mcp_server(
    server: &McpServer,
    pending_elicitations: crate::elicitation::PendingElicitationMap,
    event_sink: Arc<crate::event_sink::EventSink>,
    session_id: String,
    client_impl: &Implementation,
    tool_state: Arc<McpToolState>,
    cwd: Option<&std::path::Path>,
) -> Result<
    (
        String,
        RunningService<RoleClient, crate::elicitation::McpClientHandler>,
    ),
    Error,
> {
    match server {
        McpServer::Stdio(stdio) => {
            let McpServerStdio { name, .. } = stdio;
            let cmd = stdio_command(stdio, cwd);
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
            Ok((name.clone(), running))
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
fn headers_to_map(headers: &[crate::acp::protocol::HttpHeader]) -> Result<HeaderMap, Error> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_command_runs_inside_the_session_workspace() {
        let server = McpServerStdio::new("test", "/bin/true").args(vec!["--x".to_string()]);
        let dir = tempfile::TempDir::new().unwrap();

        // Workspace-bound sessions spawn MCP servers inside the bound
        // checkout so workspace-relative entries resolve against it.
        let cmd = stdio_command(&server, Some(dir.path()));
        assert_eq!(cmd.as_std().get_current_dir(), Some(dir.path()));

        // Local sessions keep the agent process working directory.
        let cmd = stdio_command(&server, None);
        assert_eq!(cmd.as_std().get_current_dir(), None);
    }
}

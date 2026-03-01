//! MCP (Model Context Protocol) integration utilities

use agent_client_protocol::{Error, HttpHeader, McpServer};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// Converts HTTP headers from protocol format to reqwest format
pub fn headers_to_map(headers: &[HttpHeader]) -> Result<HeaderMap, Error> {
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

/// Gets the name of an MCP server configuration
pub fn get_mcp_server_name(server: &McpServer) -> &str {
    match server {
        McpServer::Stdio(stdio) => &stdio.name,
        McpServer::Http(http) => &http.name,
        _ => "unknown",
    }
}

/// Gets the URL from an MCP server configuration if available
pub fn get_mcp_server_url(server: &McpServer) -> Option<&str> {
    match server {
        McpServer::Http(http) => Some(&http.url),
        _ => None,
    }
}

/// Gets the headers from an MCP server configuration if available
pub fn get_mcp_server_headers(server: &McpServer) -> &[HttpHeader] {
    match server {
        McpServer::Http(http) => &http.headers,
        _ => &[],
    }
}

/// Checks if an MCP server configuration requires network access
pub fn mcp_server_requires_network(server: &McpServer) -> bool {
    matches!(server, McpServer::Http(_))
}

/// Returns the standard `Implementation` descriptor for the querymt-agent.
pub fn agent_implementation() -> rmcp::model::Implementation {
    rmcp::model::Implementation {
        name: "querymt-agent".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        description: None,
        title: None,
        icons: None,
        website_url: None,
    }
}

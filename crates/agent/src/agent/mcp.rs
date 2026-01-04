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
        McpServer::Sse(sse) => &sse.name,
        _ => "unknown",
    }
}

/// Gets the URL from an MCP server configuration if available
pub fn get_mcp_server_url(server: &McpServer) -> Option<&str> {
    match server {
        McpServer::Http(http) => Some(&http.url),
        McpServer::Sse(sse) => Some(&sse.url),
        _ => None,
    }
}

/// Gets the headers from an MCP server configuration if available
pub fn get_mcp_server_headers(server: &McpServer) -> &[HttpHeader] {
    match server {
        McpServer::Http(http) => &http.headers,
        McpServer::Sse(sse) => &sse.headers,
        _ => &[],
    }
}

/// Checks if an MCP server configuration requires network access
pub fn mcp_server_requires_network(server: &McpServer) -> bool {
    matches!(server, McpServer::Http(_) | McpServer::Sse(_))
}

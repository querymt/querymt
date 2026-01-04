//! Transport types for ACP server

use std::fmt;

/// Transport type for the ACP server
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpTransport {
    /// Stdio transport - communicates via stdin/stdout
    /// Logging is automatically redirected to stderr
    Stdio,

    /// WebSocket transport on the specified address
    /// Starts a standalone WebSocket server (no dashboard UI)
    WebSocket(String),
}

impl AcpTransport {
    /// Check if this transport is stdio
    pub fn is_stdio(&self) -> bool {
        matches!(self, AcpTransport::Stdio)
    }

    /// Check if this transport is websocket
    pub fn is_websocket(&self) -> bool {
        matches!(self, AcpTransport::WebSocket(_))
    }

    /// Get the websocket address if this is a WebSocket transport
    pub fn websocket_addr(&self) -> Option<&str> {
        match self {
            AcpTransport::WebSocket(addr) => Some(addr),
            _ => None,
        }
    }
}

impl fmt::Display for AcpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AcpTransport::Stdio => write!(f, "stdio"),
            AcpTransport::WebSocket(addr) => write!(f, "ws://{}", addr),
        }
    }
}

/// Parse from string for convenience
impl TryFrom<&str> for AcpTransport {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s.to_lowercase().as_str() {
            "stdio" => Ok(AcpTransport::Stdio),
            s if s.starts_with("ws://") => {
                let addr = s.strip_prefix("ws://").unwrap();
                if addr.is_empty() {
                    return Err("WebSocket address cannot be empty".to_string());
                }
                Ok(AcpTransport::WebSocket(addr.to_string()))
            }
            s if s.starts_with("wss://") => {
                Err("WSS (secure WebSocket) not yet supported. Use ws:// for now.".to_string())
            }
            _ => Err(format!(
                "Invalid transport '{}'. Expected 'stdio' or 'ws://ADDRESS'",
                s
            )),
        }
    }
}

impl TryFrom<String> for AcpTransport {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.as_str().try_into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stdio() {
        assert_eq!(
            AcpTransport::try_from("stdio").unwrap(),
            AcpTransport::Stdio
        );
        assert_eq!(
            AcpTransport::try_from("STDIO").unwrap(),
            AcpTransport::Stdio
        );
    }

    #[test]
    fn test_parse_websocket() {
        assert_eq!(
            AcpTransport::try_from("ws://127.0.0.1:3030").unwrap(),
            AcpTransport::WebSocket("127.0.0.1:3030".to_string())
        );
        assert_eq!(
            AcpTransport::try_from("ws://localhost:8080").unwrap(),
            AcpTransport::WebSocket("localhost:8080".to_string())
        );
    }

    #[test]
    fn test_parse_empty_websocket() {
        assert!(AcpTransport::try_from("ws://").is_err());
    }

    #[test]
    fn test_parse_wss_not_supported() {
        let result = AcpTransport::try_from("wss://example.com");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not yet supported"));
    }

    #[test]
    fn test_parse_invalid() {
        assert!(AcpTransport::try_from("http://example.com").is_err());
        assert!(AcpTransport::try_from("invalid").is_err());
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", AcpTransport::Stdio), "stdio");
        assert_eq!(
            format!("{}", AcpTransport::WebSocket("127.0.0.1:3030".to_string())),
            "ws://127.0.0.1:3030"
        );
    }

    #[test]
    fn test_is_stdio() {
        assert!(AcpTransport::Stdio.is_stdio());
        assert!(!AcpTransport::WebSocket("addr".to_string()).is_stdio());
    }

    #[test]
    fn test_is_websocket() {
        assert!(!AcpTransport::Stdio.is_websocket());
        assert!(AcpTransport::WebSocket("addr".to_string()).is_websocket());
    }

    #[test]
    fn test_websocket_addr() {
        assert_eq!(AcpTransport::Stdio.websocket_addr(), None);
        assert_eq!(
            AcpTransport::WebSocket("127.0.0.1:3030".to_string()).websocket_addr(),
            Some("127.0.0.1:3030")
        );
    }
}

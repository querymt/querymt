//! Structured error type for the agent crate.
//!
//! Replaces 77 occurrences of raw `Error::new(-32xxx, ...)` scattered across
//! 12 files.  Every variant carries typed context and maps to a specific ACP
//! error code via the `From<AgentError> for AcpError` impl.

use agent_client_protocol::Error as AcpError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Structured, serializable error type for the agent crate.
///
/// Every variant carries typed context and maps to a specific ACP
/// error code via the `From<AgentError> for AcpError` impl.
#[derive(Debug, Error, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AgentError {
    // --- Configuration / Setup ---
    #[error("provider is required in config")]
    ProviderRequired,

    #[error("unknown provider: {name}")]
    UnknownProvider { name: String },

    #[error("mesh not bootstrapped -- start with --mesh")]
    MeshNotBootstrapped,

    // --- Session lifecycle ---
    #[error("session not found: {session_id}")]
    SessionNotFound { session_id: String },

    #[error("cannot fork empty session")]
    EmptySessionFork,

    #[error("session semaphore closed")]
    SessionSemaphoreClosed,

    #[error("session execution timeout: {details}")]
    SessionTimeout { details: String },

    // --- MCP / Protocol ---
    #[error("MCP {transport} server failed: {reason}")]
    McpServerFailed { transport: String, reason: String },

    #[error("method not implemented: {method}")]
    MethodNotImplemented { method: String },

    // --- Provider / LLM ---
    #[error("provider error: {0}")]
    Provider(String),

    #[error("provider chat failed ({operation}): {reason}")]
    ProviderChat { operation: String, reason: String },

    // --- Client bridge ---
    #[error("client bridge closed")]
    ClientBridgeClosed,

    #[error("permission request cancelled")]
    PermissionCancelled,

    #[error("permission response channel dropped")]
    PermissionChannelDropped,

    // --- Remote / Mesh ---
    #[error("remote actor error: {0}")]
    RemoteActor(String),

    #[error("swarm lookup failed for '{key}': {reason}")]
    SwarmLookupFailed { key: String, reason: String },

    #[error("remote session not found: {details}")]
    RemoteSessionNotFound { details: String },

    // --- Serialization ---
    #[error("serialization error: {0}")]
    Serialization(String),

    // --- Generic internal ---
    #[error("internal error: {0}")]
    Internal(String),
}

/// Map each `AgentError` variant to the appropriate ACP error code.
///
/// | Code    | ACP meaning        | Used for                                          |
/// |---------|--------------------|---------------------------------------------------|
/// | -32601  | MethodNotFound     | `MethodNotImplemented`                            |
/// | -32002  | ResourceNotFound   | `SessionNotFound`, `RemoteSessionNotFound`        |
/// | -32603  | InternalError      | everything else (replaces the old -32000 catch-all) |
impl From<AgentError> for AcpError {
    fn from(e: AgentError) -> Self {
        let code: i32 = match &e {
            AgentError::MethodNotImplemented { .. } => -32601, // MethodNotFound
            AgentError::SessionNotFound { .. } | AgentError::RemoteSessionNotFound { .. } => -32002, // ResourceNotFound
            _ => -32603, // InternalError
        };
        AcpError::new(code, e.to_string())
    }
}

impl From<anyhow::Error> for AgentError {
    fn from(e: anyhow::Error) -> Self {
        AgentError::Internal(e.to_string())
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(e: serde_json::Error) -> Self {
        AgentError::Serialization(e.to_string())
    }
}

impl From<crate::session::error::SessionError> for AgentError {
    fn from(e: crate::session::error::SessionError) -> Self {
        use crate::session::error::SessionError;
        match e {
            SessionError::SessionNotFound(id) => AgentError::SessionNotFound { session_id: id },
            other => AgentError::Internal(other.to_string()),
        }
    }
}

impl From<crate::middleware::error::MiddlewareError> for AgentError {
    fn from(e: crate::middleware::error::MiddlewareError) -> Self {
        AgentError::Internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::ErrorCode;

    // ── From<AgentError> for AcpError ──────────────────────────────────────

    #[test]
    fn provider_required_maps_to_internal_error() {
        let acp: AcpError = AgentError::ProviderRequired.into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("provider is required"));
    }

    #[test]
    fn unknown_provider_maps_to_internal_error() {
        let acp: AcpError = AgentError::UnknownProvider {
            name: "bad-llm".to_string(),
        }
        .into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("bad-llm"));
    }

    #[test]
    fn method_not_implemented_maps_to_method_not_found() {
        let acp: AcpError = AgentError::MethodNotImplemented {
            method: "session/set_mode".to_string(),
        }
        .into();
        assert_eq!(acp.code, ErrorCode::MethodNotFound);
        assert!(acp.message.contains("session/set_mode"));
    }

    #[test]
    fn session_not_found_maps_to_resource_not_found() {
        let acp: AcpError = AgentError::SessionNotFound {
            session_id: "abc-123".to_string(),
        }
        .into();
        assert_eq!(acp.code, ErrorCode::ResourceNotFound);
        assert!(acp.message.contains("abc-123"));
    }

    #[test]
    fn remote_session_not_found_maps_to_resource_not_found() {
        let acp: AcpError = AgentError::RemoteSessionNotFound {
            details: "DHT lookup missed".to_string(),
        }
        .into();
        assert_eq!(acp.code, ErrorCode::ResourceNotFound);
        assert!(acp.message.contains("DHT lookup missed"));
    }

    #[test]
    fn session_timeout_maps_to_internal_error() {
        let acp: AcpError = AgentError::SessionTimeout {
            details: "exceeded 30s".to_string(),
        }
        .into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("exceeded 30s"));
    }

    #[test]
    fn mcp_server_failed_maps_to_internal_error() {
        let acp: AcpError = AgentError::McpServerFailed {
            transport: "stdio".to_string(),
            reason: "process exited".to_string(),
        }
        .into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("stdio"));
        assert!(acp.message.contains("process exited"));
    }

    #[test]
    fn provider_chat_maps_to_internal_error() {
        let acp: AcpError = AgentError::ProviderChat {
            operation: "chat_with_tools".to_string(),
            reason: "rate limit".to_string(),
        }
        .into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("chat_with_tools"));
        assert!(acp.message.contains("rate limit"));
    }

    #[test]
    fn client_bridge_closed_maps_to_internal_error() {
        let acp: AcpError = AgentError::ClientBridgeClosed.into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("client bridge closed"));
    }

    #[test]
    fn permission_cancelled_maps_to_internal_error() {
        let acp: AcpError = AgentError::PermissionCancelled.into();
        assert_eq!(acp.code, ErrorCode::InternalError);
    }

    #[test]
    fn permission_channel_dropped_maps_to_internal_error() {
        let acp: AcpError = AgentError::PermissionChannelDropped.into();
        assert_eq!(acp.code, ErrorCode::InternalError);
    }

    #[test]
    fn remote_actor_maps_to_internal_error() {
        let acp: AcpError = AgentError::RemoteActor("actor dead".to_string()).into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("actor dead"));
    }

    #[test]
    fn swarm_lookup_failed_maps_to_internal_error() {
        let acp: AcpError = AgentError::SwarmLookupFailed {
            key: "session::abc".to_string(),
            reason: "timeout".to_string(),
        }
        .into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("session::abc"));
    }

    #[test]
    fn empty_session_fork_maps_to_internal_error() {
        let acp: AcpError = AgentError::EmptySessionFork.into();
        assert_eq!(acp.code, ErrorCode::InternalError);
    }

    #[test]
    fn mesh_not_bootstrapped_maps_to_internal_error() {
        let acp: AcpError = AgentError::MeshNotBootstrapped.into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("--mesh"));
    }

    #[test]
    fn internal_maps_to_internal_error() {
        let acp: AcpError = AgentError::Internal("oops".to_string()).into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("oops"));
    }

    // ── From conversions ───────────────────────────────────────────────────

    #[test]
    fn from_anyhow_error() {
        let anyhow_err = anyhow::anyhow!("something went wrong");
        let agent_err: AgentError = anyhow_err.into();
        assert!(matches!(agent_err, AgentError::Internal(_)));
        assert!(agent_err.to_string().contains("something went wrong"));
    }

    #[test]
    fn from_serde_json_error() {
        let json_err: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("{ bad json").unwrap_err();
        let agent_err: AgentError = json_err.into();
        assert!(matches!(agent_err, AgentError::Serialization(_)));
    }

    #[test]
    fn from_session_error_session_not_found() {
        let session_err = crate::session::error::SessionError::SessionNotFound("xyz".to_string());
        let agent_err: AgentError = session_err.into();
        assert!(matches!(
            agent_err,
            AgentError::SessionNotFound { session_id } if session_id == "xyz"
        ));
    }

    #[test]
    fn from_session_error_other_wraps_as_internal() {
        let session_err = crate::session::error::SessionError::TaskNotFound("t-1".to_string());
        let agent_err: AgentError = session_err.into();
        assert!(matches!(agent_err, AgentError::Internal(_)));
    }

    #[test]
    fn from_middleware_error_wraps_as_internal() {
        let mw_err = crate::middleware::error::MiddlewareError::Transition("bad state".to_string());
        let agent_err: AgentError = mw_err.into();
        assert!(matches!(agent_err, AgentError::Internal(_)));
    }

    // ── Serde round-trip ───────────────────────────────────────────────────

    #[test]
    fn agent_error_serde_round_trip() {
        let original = AgentError::SessionNotFound {
            session_id: "sess-999".to_string(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: AgentError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original.to_string(), restored.to_string());
    }

    #[test]
    fn agent_error_provider_chat_serde_round_trip() {
        let original = AgentError::ProviderChat {
            operation: "stream".to_string(),
            reason: "context too long".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: AgentError = serde_json::from_str(&json).unwrap();
        assert_eq!(original.to_string(), restored.to_string());
    }

    // ── Display messages ───────────────────────────────────────────────────

    #[test]
    fn display_messages_are_human_readable() {
        assert_eq!(
            AgentError::ProviderRequired.to_string(),
            "provider is required in config"
        );
        assert_eq!(
            AgentError::MeshNotBootstrapped.to_string(),
            "mesh not bootstrapped -- start with --mesh"
        );
        assert_eq!(
            AgentError::EmptySessionFork.to_string(),
            "cannot fork empty session"
        );
        assert_eq!(
            AgentError::SessionSemaphoreClosed.to_string(),
            "session semaphore closed"
        );
        assert_eq!(
            AgentError::ClientBridgeClosed.to_string(),
            "client bridge closed"
        );
        assert_eq!(
            AgentError::PermissionCancelled.to_string(),
            "permission request cancelled"
        );
        assert_eq!(
            AgentError::PermissionChannelDropped.to_string(),
            "permission response channel dropped"
        );
    }
}

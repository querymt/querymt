//! Structured error type for the agent crate.
//!
//! Replaces 77 occurrences of raw `Error::new(-32xxx, ...)` scattered across
//! 12 files.  Every variant carries typed context and maps to a specific ACP
//! error code via the `From<AgentError> for AcpError` impl.

use agent_client_protocol::Error as AcpError;
use querymt::error::LLMErrorPayload;
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

    #[error("schedule not found: {schedule_public_id}")]
    ScheduleNotFound { schedule_public_id: String },

    // --- MCP / Protocol ---
    #[error("MCP {transport} server failed: {reason}")]
    McpServerFailed { transport: String, reason: String },

    #[error("method not implemented: {method}")]
    MethodNotImplemented { method: String },

    // --- Provider / LLM ---
    #[error("provider error: {0}")]
    Provider(String),

    #[error("provider chat failed ({operation}): {reason}")]
    ProviderChat {
        operation: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_error: Option<LLMErrorPayload>,
    },

    // --- Client bridge ---
    #[error("client bridge closed")]
    ClientBridgeClosed,

    #[error("permission request cancelled")]
    PermissionCancelled,

    #[error("permission response channel dropped")]
    PermissionChannelDropped,

    #[error("workspace query response channel dropped")]
    WorkspaceQueryChannelDropped,

    // --- Remote / Mesh ---
    #[error("remote actor error: {0}")]
    RemoteActor(String),

    #[error("swarm lookup failed for '{key}': {reason}")]
    SwarmLookupFailed { key: String, reason: String },

    #[error("remote session not found: {details}")]
    RemoteSessionNotFound { details: String },

    #[error("mesh admission rejected: {reason}")]
    AdmissionRejected { reason: String },

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
/// | -32002  | ResourceNotFound   | `SessionNotFound`, `RemoteSessionNotFound`, `ScheduleNotFound` |
/// | -32603  | InternalError      | everything else (replaces the old -32000 catch-all) |
impl From<AgentError> for AcpError {
    fn from(e: AgentError) -> Self {
        if let AgentError::ProviderChat {
            operation,
            llm_error,
            ..
        } = &e
        {
            let message = if provider_error_is_transient(llm_error.as_ref()) {
                "LLM provider temporarily unavailable"
            } else {
                "LLM provider request failed"
            };
            return AcpError::new(-32603, message).data(serde_json::json!({
                "schema": "querymt.error.v1",
                "kind": "llm",
                "operation": operation,
                "llm_error": llm_error,
            }));
        }

        let code: i32 = match &e {
            AgentError::MethodNotImplemented { .. } => -32601, // MethodNotFound
            AgentError::SessionNotFound { .. }
            | AgentError::RemoteSessionNotFound { .. }
            | AgentError::ScheduleNotFound { .. } => -32002, // ResourceNotFound
            _ => -32603,                                       // InternalError
        };
        AcpError::new(code, e.to_string())
    }
}

fn provider_error_is_transient(payload: Option<&LLMErrorPayload>) -> bool {
    payload.is_some_and(LLMErrorPayload::is_retryable)
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
    use querymt::error::ProviderErrorContext;

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
    fn provider_chat_maps_to_structured_internal_error() {
        let llm_error = LLMErrorPayload::ProviderError {
            message: "capacity exhausted".to_string(),
            context: Some(ProviderErrorContext {
                provider: "openai".to_string(),
                kind: None,
                code: Some("server_error".to_string()),
                error_type: Some("api_error".to_string()),
                request_id: Some("req-123".to_string()),
                retry_after_secs: Some(2),
                transient: false,
            }),
        };
        let acp: AcpError = AgentError::ProviderChat {
            operation: "chat_stream".to_string(),
            reason: "LLM streaming error: LLM Provider Error: capacity exhausted".to_string(),
            llm_error: Some(llm_error),
        }
        .into();

        assert_eq!(acp.code, ErrorCode::InternalError);
        assert_eq!(acp.message, "LLM provider request failed");
        assert_eq!(
            acp.data,
            Some(serde_json::json!({
                "schema": "querymt.error.v1",
                "kind": "llm",
                "operation": "chat_stream",
                "llm_error": {
                    "type": "provider_error",
                    "message": "capacity exhausted",
                    "context": {
                        "provider": "openai",
                        "code": "server_error",
                        "error_type": "api_error",
                        "request_id": "req-123",
                        "retry_after_secs": 2,
                        "transient": false
                    }
                }
            }))
        );
    }

    #[test]
    fn provider_chat_exports_raw_response_exactly_once() {
        let raw_response = "FULL_PROVIDER_BODY".repeat(100);
        let acp: AcpError = AgentError::ProviderChat {
            operation: "chat".to_string(),
            reason: format!("response failed: {raw_response}"),
            llm_error: Some(LLMErrorPayload::ResponseFormatError {
                message: "invalid provider response".to_string(),
                raw_response: raw_response.clone(),
            }),
        }
        .into();

        let data = acp.data.expect("provider error data");
        assert_eq!(
            data.pointer("/llm_error/raw_response"),
            Some(&serde_json::Value::String(raw_response.clone()))
        );
        assert_eq!(data.to_string().matches(&raw_response).count(), 1);
        assert!(data.get("reason").is_none());
    }

    #[test]
    fn provider_chat_legacy_payload_maps_llm_error_to_null() {
        let acp: AcpError = AgentError::ProviderChat {
            operation: "chat_stream".to_string(),
            reason: "legacy failure".to_string(),
            llm_error: None,
        }
        .into();

        assert_eq!(acp.code, ErrorCode::InternalError);
        assert_eq!(acp.message, "LLM provider request failed");
        assert_eq!(
            acp.data,
            Some(serde_json::json!({
                "schema": "querymt.error.v1",
                "kind": "llm",
                "operation": "chat_stream",
                "llm_error": null
            }))
        );
    }

    #[test]
    fn transient_provider_chat_has_temporary_unavailable_message() {
        let acp: AcpError = AgentError::ProviderChat {
            operation: "chat_stream".to_string(),
            reason: "provider overloaded".to_string(),
            llm_error: Some(
                querymt::error::LLMError::ServerOverloaded {
                    message: "overloaded".to_string(),
                    context: Box::new(ProviderErrorContext {
                        provider: "codex".to_string(),
                        kind: Some(querymt::error::ProviderErrorKind::ServerOverloaded),
                        code: Some("server_is_overloaded".to_string()),
                        error_type: None,
                        request_id: Some("req-1".to_string()),
                        retry_after_secs: None,
                        transient: true,
                    }),
                }
                .to_payload(),
            ),
        }
        .into();

        assert_eq!(acp.code, ErrorCode::InternalError);
        assert_eq!(acp.message, "LLM provider temporarily unavailable");
    }

    #[test]
    fn rate_limited_and_retryable_http_status_are_temporary() {
        let rate_limited: AcpError = AgentError::ProviderChat {
            operation: "chat".to_string(),
            reason: "rate limited".to_string(),
            llm_error: Some(LLMErrorPayload::RateLimited {
                message: "slow down".to_string(),
                retry_after_secs: Some(5),
            }),
        }
        .into();
        assert_eq!(rate_limited.message, "LLM provider temporarily unavailable");

        let http_503: AcpError = AgentError::ProviderChat {
            operation: "chat_stream".to_string(),
            reason: "upstream 503".to_string(),
            llm_error: Some(LLMErrorPayload::HttpStatus {
                status_code: 503,
                message: "unavailable".to_string(),
                retry_after_secs: None,
            }),
        }
        .into();
        assert_eq!(http_503.message, "LLM provider temporarily unavailable");

        let transport: AcpError = AgentError::ProviderChat {
            operation: "chat".to_string(),
            reason: "reset".to_string(),
            llm_error: Some(LLMErrorPayload::Transport {
                kind: querymt::error::TransportErrorKind::ConnectionReset,
                message: "connection reset".to_string(),
            }),
        }
        .into();
        assert_eq!(transport.message, "LLM provider temporarily unavailable");
    }

    #[test]
    fn lossy_payload_variants_keep_stable_acp_retryability() {
        for llm_error in [
            LLMErrorPayload::JsonError {
                message: "invalid JSON".to_string(),
            },
            LLMErrorPayload::InvalidUrl {
                message: "invalid URL".to_string(),
            },
        ] {
            let acp: AcpError = AgentError::ProviderChat {
                operation: "chat".to_string(),
                reason: "provider response failed".to_string(),
                llm_error: Some(llm_error),
            }
            .into();
            assert_eq!(acp.message, "LLM provider request failed");
        }

        let acp: AcpError = AgentError::ProviderChat {
            operation: "chat".to_string(),
            reason: "I/O failed".to_string(),
            llm_error: Some(LLMErrorPayload::IoError {
                message: "connection reset".to_string(),
            }),
        }
        .into();
        assert_eq!(acp.message, "LLM provider temporarily unavailable");
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
            operation: "chat_stream".to_string(),
            reason: "context too long".to_string(),
            llm_error: Some(LLMErrorPayload::InvalidRequest {
                message: "maximum context length exceeded".to_string(),
            }),
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: AgentError = serde_json::from_str(&json).unwrap();

        assert_eq!(original.to_string(), restored.to_string());
        assert!(matches!(
            restored,
            AgentError::ProviderChat {
                operation,
                reason,
                llm_error: Some(LLMErrorPayload::InvalidRequest { message }),
            } if operation == "chat_stream"
                && reason == "context too long"
                && message == "maximum context length exceeded"
        ));
    }

    #[test]
    fn agent_error_provider_chat_deserializes_legacy_payload() {
        let json = r#"{"ProviderChat":{"operation":"chat_stream","reason":"legacy"}}"#;
        let restored: AgentError = serde_json::from_str(json).unwrap();

        assert!(matches!(
            restored,
            AgentError::ProviderChat {
                operation,
                reason,
                llm_error: None,
            } if operation == "chat_stream" && reason == "legacy"
        ));
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

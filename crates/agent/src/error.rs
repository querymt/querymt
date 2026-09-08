//! Structured error type for the agent crate.
//!
//! Replaces 77 occurrences of raw `Error::new(-32xxx, ...)` scattered across
//! 12 files.  Every variant carries typed context and maps to a specific ACP
//! error code via the `From<AgentError> for AcpError` impl.

use agent_client_protocol::Error as AcpError;
use querymt::error::{LLMErrorPayload, ProviderErrorKind};
use querymt_remote::RemoteTransportFailure;
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
    ProviderChat { operation: String, reason: String },

    #[error("provider request failed: {message}")]
    ProviderFailure {
        message: String,
        provider: Option<String>,
        model: Option<String>,
        retryable: bool,
        error: Box<LLMErrorPayload>,
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

    /// A remote transport failure with typed classification.
    ///
    /// Unlike [`AgentError::RemoteActor`], this variant stays machine-readable
    /// so the operation layer can make safe retry decisions (failure kind,
    /// delivery certainty) without parsing strings. Handler and provider
    /// errors are never represented as this variant.
    #[error("{0}")]
    RemoteTransport(RemoteTransportFailure),

    #[error("swarm lookup failed for '{key}': {reason}")]
    SwarmLookupFailed { key: String, reason: String },

    #[error("remote session not found: {details}")]
    RemoteSessionNotFound { details: String },

    #[error("mesh admission rejected: {reason}")]
    AdmissionRejected { reason: String },

    #[error("turn control error: {message}")]
    TurnControl { kind: String, message: String },

    #[error("invalid prompt content: {message}")]
    InvalidPromptContent { message: String },

    #[error("session control revision conflict: expected {expected}, found {found}")]
    SessionControlRevisionConflict { expected: u64, found: u64 },

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
        if let AgentError::ProviderFailure {
            message,
            provider,
            model,
            retryable,
            error,
        } = &e
        {
            let (kind, provider_message) = provider_error_metadata(error);
            return AcpError::new(-32010, "Provider request failed").data(serde_json::json!({
                "category": "provider",
                "kind": kind,
                "message": provider_message.unwrap_or_else(|| message.clone()),
                "provider": provider,
                "model": model,
                "retryable": retryable,
                "error": error,
            }));
        }
        if let AgentError::TurnControl { kind, message } = &e {
            return AcpError::new(-32602, message.clone()).data(serde_json::json!({
                "category": "turn_control",
                "kind": kind,
                "message": message,
            }));
        }
        if let AgentError::InvalidPromptContent { message } = &e {
            return AcpError::new(-32602, message.clone()).data(serde_json::json!({
                "category": "prompt_content",
                "message": message,
            }));
        }
        if let AgentError::SessionControlRevisionConflict { expected, found } = &e {
            return AcpError::new(-32603, e.to_string()).data(serde_json::json!({
                "category": "session_control",
                "kind": "revision_conflict",
                "expected": expected,
                "found": found,
            }));
        }
        if let AgentError::RemoteTransport(failure) = &e {
            return AcpError::new(-32603, e.to_string()).data(serde_json::json!({
                "category": "remote_transport",
                "kind": failure.kind,
                "delivery": failure.delivery,
                "message": failure.message,
            }));
        }

        let code: i32 = match &e {
            AgentError::MethodNotImplemented { .. } => -32601,
            AgentError::SessionNotFound { .. }
            | AgentError::RemoteSessionNotFound { .. }
            | AgentError::ScheduleNotFound { .. } => -32002,
            _ => -32603,
        };
        AcpError::new(code, e.to_string())
    }
}

fn provider_error_metadata(error: &LLMErrorPayload) -> (Option<ProviderErrorKind>, Option<String>) {
    match error {
        LLMErrorPayload::ProviderError { message, kind, .. } => (*kind, Some(message.clone())),
        LLMErrorPayload::RateLimited { message, .. } => {
            (Some(ProviderErrorKind::RateLimited), Some(message.clone()))
        }
        LLMErrorPayload::AuthError { message } => (
            Some(ProviderErrorKind::Authentication),
            Some(message.clone()),
        ),
        LLMErrorPayload::InvalidRequest { message } => (
            Some(ProviderErrorKind::InvalidRequest),
            Some(message.clone()),
        ),
        LLMErrorPayload::NotImplemented { message } => (
            Some(ProviderErrorKind::UnsupportedOperation),
            Some(message.clone()),
        ),
        _ => (None, None),
    }
}

impl AgentError {
    /// Wrap a classified remote transport failure.
    pub fn from_transport_failure(failure: RemoteTransportFailure) -> Self {
        Self::RemoteTransport(failure)
    }

    /// Returns the typed transport failure when this error is one, so callers
    /// can decide about recovery/retry without parsing strings.
    pub fn transport_failure(&self) -> Option<&RemoteTransportFailure> {
        match self {
            Self::RemoteTransport(failure) => Some(failure),
            _ => None,
        }
    }
}

impl From<crate::agent::turn_control::TurnControlError> for AgentError {
    fn from(error: crate::agent::turn_control::TurnControlError) -> Self {
        Self::TurnControl {
            kind: error.kind().to_string(),
            message: error.to_string(),
        }
    }
}

impl From<crate::model::PromptContentError> for AgentError {
    fn from(error: crate::model::PromptContentError) -> Self {
        Self::InvalidPromptContent {
            message: error.to_string(),
        }
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

impl AgentError {
    pub(crate) fn is_session_control_revision_conflict(&self) -> bool {
        matches!(self, Self::SessionControlRevisionConflict { .. })
    }
}

impl From<crate::session::error::SessionError> for AgentError {
    fn from(e: crate::session::error::SessionError) -> Self {
        use crate::session::error::SessionError;
        match e {
            SessionError::SessionNotFound(id) => AgentError::SessionNotFound { session_id: id },
            SessionError::SessionControlRevisionConflict { expected, found } => {
                AgentError::SessionControlRevisionConflict { expected, found }
            }
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
    use querymt_remote::{DeliveryCertainty, RemoteTransportFailureKind};

    // ── From<AgentError> for AcpError ──────────────────────────────────────

    #[test]
    fn invalid_prompt_content_maps_to_invalid_params() {
        let error: AcpError = AgentError::InvalidPromptContent {
            message: "invalid base64 attachment data".to_string(),
        }
        .into();
        assert_eq!(error.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn session_control_revision_conflict_maps_to_internal_error() {
        let error: AcpError = AgentError::SessionControlRevisionConflict {
            expected: 30,
            found: 31,
        }
        .into();
        assert_eq!(error.code, ErrorCode::InternalError);
        assert!(error.message.contains("expected 30"));
        assert!(error.message.contains("found 31"));
    }

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
    fn structured_quota_failure_maps_to_provider_acp_error() {
        let acp: AcpError = AgentError::ProviderFailure {
            message: "LLM streaming error".to_string(),
            provider: Some("codex".to_string()),
            model: Some("gpt-5.6-sol".to_string()),
            retryable: false,
            error: Box::new(LLMErrorPayload::ProviderError {
                message: "The usage limit has been reached".to_string(),
                kind: Some(ProviderErrorKind::QuotaExceeded),
                code: Some("usage_limit_reached".to_string()),
                error_type: Some("usage_limit_reached".to_string()),
                request_id: None,
                retry_after_secs: None,
            }),
        }
        .into();

        assert_eq!(acp.code, ErrorCode::Other(-32010));
        assert_eq!(acp.message, "Provider request failed");
        assert_eq!(
            acp.data,
            Some(serde_json::json!({
                "category": "provider",
                "kind": "quota_exceeded",
                "message": "The usage limit has been reached",
                "provider": "codex",
                "model": "gpt-5.6-sol",
                "retryable": false,
                "error": {
                    "type": "provider_error",
                    "message": "The usage limit has been reached",
                    "kind": "quota_exceeded",
                    "code": "usage_limit_reached",
                    "error_type": "usage_limit_reached"
                }
            }))
        );
    }

    #[test]
    fn structured_unsupported_operation_maps_to_provider_acp_error() {
        let acp: AcpError = AgentError::ProviderFailure {
            message: "LLM streaming error".to_string(),
            provider: Some("openrouter".to_string()),
            model: Some("qwen/qwen3.5-122b-a10b".to_string()),
            retryable: false,
            error: Box::new(LLMErrorPayload::NotImplemented {
                message: "Streaming request construction not supported by this HTTP provider"
                    .to_string(),
            }),
        }
        .into();

        assert_eq!(acp.code, ErrorCode::Other(-32010));
        assert_eq!(
            acp.data,
            Some(serde_json::json!({
                "category": "provider",
                "kind": "unsupported_operation",
                "message": "Streaming request construction not supported by this HTTP provider",
                "provider": "openrouter",
                "model": "qwen/qwen3.5-122b-a10b",
                "retryable": false,
                "error": {
                    "type": "not_implemented",
                    "message": "Streaming request construction not supported by this HTTP provider"
                }
            }))
        );
    }

    #[test]
    fn structured_auth_failure_maps_to_provider_acp_error() {
        let acp: AcpError = AgentError::ProviderFailure {
            message: "LLM provider initialization error".to_string(),
            provider: Some("groq".to_string()),
            model: Some("openai/gpt-oss-20b".to_string()),
            retryable: false,
            error: Box::new(LLMErrorPayload::AuthError {
                message: "No API key found for provider 'groq'. Set GROQ_API_KEY or run 'qmt auth login groq'"
                    .to_string(),
            }),
        }
        .into();

        assert_eq!(acp.code, ErrorCode::Other(-32010));
        assert_eq!(
            acp.data,
            Some(serde_json::json!({
                "category": "provider",
                "kind": "authentication",
                "message": "No API key found for provider 'groq'. Set GROQ_API_KEY or run 'qmt auth login groq'",
                "provider": "groq",
                "model": "openai/gpt-oss-20b",
                "retryable": false,
                "error": {
                    "type": "auth_error",
                    "message": "No API key found for provider 'groq'. Set GROQ_API_KEY or run 'qmt auth login groq'"
                }
            }))
        );
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
    fn remote_transport_maps_to_structured_acp_error() {
        let acp: AcpError = AgentError::from_transport_failure(RemoteTransportFailure::new(
            RemoteTransportFailureKind::ConnectionClosed,
            DeliveryCertainty::Unknown,
            "connection closed",
        ))
        .into();
        assert_eq!(acp.code, ErrorCode::InternalError);
        assert!(acp.message.contains("connection closed"));
        assert!(acp.message.contains("kind=connection_closed"));
        assert_eq!(
            acp.data,
            Some(serde_json::json!({
                "category": "remote_transport",
                "kind": "connection_closed",
                "delivery": "unknown",
                "message": "connection closed",
            }))
        );
    }

    #[test]
    fn remote_transport_failure_accessor_roundtrips() {
        let failure = RemoteTransportFailure::new(
            RemoteTransportFailureKind::ReplyTimeout,
            DeliveryCertainty::Unknown,
            "SubmitInput timed out on remote session",
        );
        let error = AgentError::from_transport_failure(failure.clone());
        assert_eq!(error.transport_failure(), Some(&failure));
        assert!(
            AgentError::RemoteActor("x".to_string())
                .transport_failure()
                .is_none()
        );
    }

    #[test]
    fn remote_transport_serde_roundtrip() {
        let error = AgentError::from_transport_failure(RemoteTransportFailure::new(
            RemoteTransportFailureKind::ProtocolMismatch,
            DeliveryCertainty::NotDelivered,
            "bad actor type",
        ));
        let json = serde_json::to_string(&error).unwrap();
        let parsed: AgentError = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.transport_failure().map(|f| (f.kind, f.delivery)),
            Some((
                RemoteTransportFailureKind::ProtocolMismatch,
                DeliveryCertainty::NotDelivered
            ))
        );
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
    fn from_session_error_revision_conflict_is_typed() {
        let session_err = crate::session::error::SessionError::SessionControlRevisionConflict {
            expected: 1,
            found: 31,
        };
        let agent_err: AgentError = session_err.into();
        assert!(matches!(
            agent_err,
            AgentError::SessionControlRevisionConflict {
                expected: 1,
                found: 31
            }
        ));
    }

    #[test]
    fn invalid_operation_conflict_text_is_not_a_typed_conflict() {
        let session_err = crate::session::error::SessionError::InvalidOperation(
            "session control revision conflict".to_string(),
        );
        let agent_err: AgentError = session_err.into();
        assert!(matches!(agent_err, AgentError::Internal(_)));
        assert!(!agent_err.is_session_control_revision_conflict());
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

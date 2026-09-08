use super::SessionActorRef;
use crate::agent::messages;
use crate::error::AgentError;
use agent_client_protocol::ErrorCode;
use querymt::error::LLMErrorPayload;
use querymt_remote::{DeliveryCertainty, RemoteTransportFailureKind};

#[test]
fn local_prompt_handler_error_preserves_structured_provider_data() {
    let error = kameo::error::SendError::HandlerError(AgentError::ProviderFailure {
        message: "LLM streaming error".to_string(),
        provider: Some("openrouter".to_string()),
        model: Some("qwen/qwen3.5-122b-a10b".to_string()),
        retryable: false,
        error: Box::new(LLMErrorPayload::NotImplemented {
            message: "Streaming request construction not supported by this HTTP provider"
                .to_string(),
        }),
    });

    let acp = SessionActorRef::map_local_prompt_send_error(error);

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
fn local_prompt_transport_error_remains_internal() {
    let error = kameo::error::SendError::<messages::Prompt, AgentError>::ActorStopped;

    let acp = SessionActorRef::map_local_prompt_send_error(error);

    assert_eq!(acp.code, ErrorCode::InternalError);
    assert!(acp.message.contains("actor stopped"));
}

// ── Typed remote transport failures (Phase 2) ──────────────────────────────

#[test]
fn remote_reply_timeout_is_typed_ambiguous_transport_failure() {
    let mapped = SessionActorRef::map_agent_timeout_remote_send_error(
        kameo::error::RemoteSendError::<AgentError>::ReplyTimeout,
        "SubmitInput timed out on remote session",
    );

    let failure = mapped
        .transport_failure()
        .expect("reply timeout must surface as a typed transport failure");
    assert_eq!(failure.kind, RemoteTransportFailureKind::ReplyTimeout);
    assert_eq!(failure.delivery, DeliveryCertainty::Unknown);
    assert_eq!(failure.message, "SubmitInput timed out on remote session");
}

#[test]
fn remote_handler_errors_pass_through_unwrapped() {
    let mapped = SessionActorRef::map_agent_timeout_remote_send_error(
        kameo::error::RemoteSendError::HandlerError(AgentError::SessionTimeout {
            details: "handler-reported timeout".to_string(),
        }),
        "caller timeout message must not be used",
    );

    match mapped {
        AgentError::SessionTimeout { details } => {
            assert_eq!(details, "handler-reported timeout")
        }
        other => panic!("handler errors must pass through unchanged, got {other}"),
    }
}

#[test]
fn remote_actor_unavailable_is_proven_not_delivered() {
    let mapped = SessionActorRef::map_infallible_remote_send_error(
        kameo::error::RemoteSendError::<kameo::error::Infallible>::ActorNotRunning,
    );

    let failure = mapped.transport_failure().expect("typed transport failure");
    assert_eq!(failure.kind, RemoteTransportFailureKind::ActorUnavailable);
    assert!(failure.proven_not_delivered());
    assert!(failure.is_retryable_kind());
}

#[test]
fn remote_connection_closed_is_ambiguous_delivery() {
    let mapped = SessionActorRef::map_infallible_remote_send_error(
        kameo::error::RemoteSendError::<kameo::error::Infallible>::ConnectionClosed,
    );

    let failure = mapped.transport_failure().expect("typed transport failure");
    assert_eq!(failure.kind, RemoteTransportFailureKind::ConnectionClosed);
    assert!(!failure.proven_not_delivered());
}

#[test]
fn remote_protocol_mismatch_is_never_retryable() {
    let mapped = SessionActorRef::map_agent_timeout_remote_send_error(
        kameo::error::RemoteSendError::<AgentError>::BadActorType,
        "ignored",
    );

    let failure = mapped.transport_failure().expect("typed transport failure");
    assert_eq!(failure.kind, RemoteTransportFailureKind::ProtocolMismatch);
    assert!(!failure.is_retryable_kind());
}

#[test]
fn timeout_message_override_applies_only_to_reply_timeout() {
    let mapped = SessionActorRef::map_infallible_timeout_remote_send_error(
        kameo::error::RemoteSendError::<kameo::error::Infallible>::ConnectionClosed,
        "GetMode timed out on remote session",
    );

    let failure = mapped.transport_failure().expect("typed transport failure");
    assert_eq!(failure.kind, RemoteTransportFailureKind::ConnectionClosed);
    // Non-timeout failures keep the original kameo message.
    assert_eq!(failure.message, "connection closed");
}

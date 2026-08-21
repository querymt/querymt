use super::SessionActorRef;
use crate::agent::messages;
use crate::error::AgentError;
use agent_client_protocol::ErrorCode;
use querymt::error::LLMErrorPayload;

#[test]
fn local_prompt_handler_error_preserves_structured_provider_data() {
    let error = kameo::error::SendError::HandlerError(AgentError::ProviderFailure {
        message: "LLM streaming error".to_string(),
        provider: Some("openrouter".to_string()),
        model: Some("qwen/qwen3.5-122b-a10b".to_string()),
        retryable: false,
        error: LLMErrorPayload::NotImplemented {
            message: "Streaming request construction not supported by this HTTP provider"
                .to_string(),
        },
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

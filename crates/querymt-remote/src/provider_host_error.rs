use querymt::error::{LLMError, LLMErrorPayload};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoteProviderHostError {
    #[error("provider chat failed ({operation}): {reason}")]
    ProviderChat {
        operation: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<LLMErrorPayload>,
    },

    #[error("provider host internal error: {0}")]
    Internal(String),
}

impl RemoteProviderHostError {
    pub fn to_payload(&self) -> LLMErrorPayload {
        match self {
            Self::ProviderChat { reason, error, .. } => error
                .clone()
                .or_else(|| serde_json::from_str::<LLMErrorPayload>(reason).ok())
                .unwrap_or_else(|| LLMError::ProviderError(reason.clone()).to_payload()),
            Self::Internal(message) => LLMError::ProviderError(message.clone()).to_payload(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::error::{ProviderErrorContext, ProviderErrorKind};

    #[test]
    fn provider_init_preserves_typed_payload() {
        let payload = LLMError::ProviderResponseError {
            message: "busy".into(),
            context: Box::new(
                ProviderErrorContext::new("openai", ProviderErrorKind::ServerOverloaded)
                    .with_code(Some("server_is_overloaded".into()))
                    .with_request_id(Some("req-remote".into()))
                    .with_retry_after_secs(Some(2)),
            ),
        }
        .to_payload();
        let error = RemoteProviderHostError::ProviderChat {
            operation: "provider_init".into(),
            reason: "provider init failed".into(),
            error: Some(payload.clone()),
        };

        assert_eq!(error.to_payload(), payload);

        let encoded = serde_json::to_value(&error).expect("serialize remote error");
        assert_eq!(
            encoded["ProviderChat"]["operation"],
            serde_json::json!("provider_init")
        );
        let decoded: RemoteProviderHostError =
            serde_json::from_value(encoded).expect("deserialize remote error");
        assert_eq!(decoded.to_payload(), payload);
    }

    #[test]
    fn legacy_provider_chat_without_payload_still_deserializes() {
        let error: RemoteProviderHostError = serde_json::from_value(serde_json::json!({
            "ProviderChat": {
                "operation": "chat_stream_with_tools",
                "reason": "legacy failure"
            }
        }))
        .expect("legacy remote error should deserialize");

        assert_eq!(
            error.to_payload(),
            LLMError::ProviderError("legacy failure".into()).to_payload()
        );
    }
}

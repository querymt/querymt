use querymt::error::{LLMError, LLMErrorPayload};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Structured host-side failure report sent back to the requesting peer.
///
/// [`Self::ProviderChat`] carries the original [`LLMErrorPayload`] by value so
/// the payload does not need an unnecessary JSON-to-string-to-JSON round trip.
/// Wire-codec failures remain transport errors and are classified separately;
/// they must not be mistaken for errors returned by the provider itself.
#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoteProviderHostError {
    #[error("provider chat failed ({operation})")]
    ProviderChat {
        operation: String,
        /// Boxed so the error enum stays small on the hot path.
        payload: Box<LLMErrorPayload>,
    },

    #[error("provider host internal error: {0}")]
    Internal(String),
}

impl RemoteProviderHostError {
    /// Build a provider failure that preserves the original structured payload.
    pub fn provider_chat(operation: impl Into<String>, error: &LLMError) -> Self {
        Self::ProviderChat {
            operation: operation.into(),
            payload: Box::new(error.to_payload()),
        }
    }

    /// Build a provider failure for a condition with no originating [`LLMError`].
    pub fn provider_chat_message(operation: impl Into<String>, message: impl Into<String>) -> Self {
        Self::ProviderChat {
            operation: operation.into(),
            payload: Box::new(LLMError::InvalidRequest(message.into()).to_payload()),
        }
    }

    /// Recover the structured payload without any parsing step, so the original
    /// provider error can never be discarded or replaced by a serde complaint.
    pub fn to_payload(&self) -> LLMErrorPayload {
        match self {
            Self::ProviderChat { payload, .. } => payload.as_ref().clone(),
            Self::Internal(message) => LLMError::ProviderError(message.clone()).to_payload(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::error::{ProviderErrorKind, ProviderFailure};

    #[test]
    fn provider_chat_preserves_original_payload_verbatim() {
        let original = LLMError::from(
            ProviderFailure::new(ProviderErrorKind::ServerOverloaded, "upstream exploded")
                .with_code(Some("server_is_overloaded".into()))
                .with_request_id(Some("req-42".into())),
        );
        let host_error =
            RemoteProviderHostError::provider_chat("chat_stream_with_tools", &original);

        let recovered = LLMError::from_payload(host_error.to_payload());
        match recovered {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.message(), "upstream exploded");
                assert_eq!(failure.kind(), ProviderErrorKind::ServerOverloaded);
                assert_eq!(failure.code(), Some("server_is_overloaded"));
                assert_eq!(failure.request_id(), Some("req-42"));
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    #[test]
    fn provider_chat_message_is_not_reported_as_a_serde_failure() {
        let host_error = RemoteProviderHostError::provider_chat_message(
            "contract_check",
            "item-aware history requires contract version 1",
        );

        let recovered = LLMError::from_payload(host_error.to_payload());
        match recovered {
            LLMError::InvalidRequest(message) => {
                assert!(message.contains("item-aware history requires contract version 1"));
            }
            other => panic!("expected InvalidRequest, got {other}"),
        }
    }

    #[test]
    fn payload_survives_the_mesh_wire_round_trip() {
        let original = LLMError::ProviderError("boom".to_string());
        let host_error = RemoteProviderHostError::provider_chat("chat_with_tools", &original);

        // Mirror kameo's handler-error codec exactly.
        let encoded = rmp_serde::to_vec_named(&host_error).expect("host error serializes");
        let decoded: RemoteProviderHostError =
            rmp_serde::decode::from_slice(&encoded).expect("host error deserializes");

        let recovered = LLMError::from_payload(decoded.to_payload());
        assert!(matches!(recovered, LLMError::ProviderError(message) if message == "boom"));
    }
}

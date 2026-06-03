use querymt::error::{LLMError, LLMErrorPayload};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum RemoteProviderHostError {
    #[error("provider chat failed ({operation}): {reason}")]
    ProviderChat { operation: String, reason: String },

    #[error("provider host internal error: {0}")]
    Internal(String),
}

impl RemoteProviderHostError {
    pub fn to_payload(&self) -> LLMErrorPayload {
        match self {
            Self::ProviderChat { reason, .. } => serde_json::from_str::<LLMErrorPayload>(reason)
                .unwrap_or_else(|_| LLMError::ProviderError(reason.clone()).to_payload()),
            Self::Internal(message) => LLMError::ProviderError(message.clone()).to_payload(),
        }
    }
}

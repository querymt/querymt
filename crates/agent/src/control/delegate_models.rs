use serde::{Deserialize, Deserializer, Serialize};
use typeshare::typeshare;

use crate::delegation::DelegateModelOverride;

pub const DELEGATE_MODELS_VERSION: u32 = 1;
/// Distinct from ACP `InvalidParams` (`-32602`) so clients can refresh-and-retry.
pub const DELEGATE_ASSIGNMENT_CONFLICT_ACP_CODE: i32 = -32020;

/// JSON null serializes as `null`. Unlike `Option`, a missing field is invalid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RequiredNullable<T>(pub Option<T>);

impl<'de, T> Deserialize<'de> for RequiredNullable<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Do not deserialize through `Option`: serde treats a missing field as
        // `None` whenever the type calls `deserialize_option`. `Value` uses
        // `deserialize_any`, so absence is `InvalidParams` and JSON null clears.
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::Null => Ok(Self(None)),
            value => T::deserialize(value)
                .map(|parsed| Self(Some(parsed)))
                .map_err(serde::de::Error::custom),
        }
    }
}

impl<T> From<Option<T>> for RequiredNullable<T> {
    fn from(value: Option<T>) -> Self {
        Self(value)
    }
}

#[typeshare]
#[derive(Debug, Clone, Deserialize)]
pub struct DelegateModelsRequest {
    #[serde(alias = "sessionId")]
    pub session_id: String,
}

#[typeshare]
#[derive(Debug, Clone, Deserialize)]
pub struct SetDelegateModelRequest {
    #[serde(alias = "sessionId")]
    pub session_id: String,
    #[serde(alias = "agentId")]
    pub agent_id: String,
    /// Present and null clears the override. Omitted is invalid, not a wipe.
    #[serde(alias = "modelId")]
    #[typeshare(typescript(type = "string | null"))]
    pub model_id: RequiredNullable<String>,
    #[serde(default, alias = "nodeId")]
    #[typeshare(typescript(type = "string | null"))]
    pub node_id: Option<String>,
    #[serde(default, alias = "expectedRevision")]
    #[typeshare(serialized_as = "Option<number>")]
    #[typeshare(typescript(type = "number | null"))]
    pub expected_revision: Option<u64>,
}

#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegateAssignmentSource {
    Override,
    ProfileDefault,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct DelegateAssignmentInfo {
    pub agent_id: String,
    pub name: String,
    pub description: String,
    #[typeshare(typescript(type = "DelegateModelOverride | null"))]
    pub model: RequiredNullable<DelegateModelOverride>,
    pub source: DelegateAssignmentSource,
    #[typeshare(typescript(type = "string | null"))]
    pub configured_default_model_id: RequiredNullable<String>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct OrphanedDelegateAssignment {
    pub agent_id: String,
    pub model: DelegateModelOverride,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct DelegateAssignmentsInfo {
    pub version: u32,
    pub session_id: String,
    pub profile_id: String,
    #[typeshare(serialized_as = "number")]
    #[typeshare(typescript(type = "number | null"))]
    pub revision: RequiredNullable<u64>,
    pub durable: bool,
    pub editable: bool,
    pub assignments: Vec<DelegateAssignmentInfo>,
    pub orphaned_overrides: Vec<OrphanedDelegateAssignment>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct SetDelegateModelResponse {
    pub version: u32,
    pub session_id: String,
    pub agent_id: String,
    #[typeshare(typescript(type = "DelegateModelOverride | null"))]
    pub model: RequiredNullable<DelegateModelOverride>,
    #[typeshare(serialized_as = "number")]
    #[typeshare(typescript(type = "number | null"))]
    pub revision: RequiredNullable<u64>,
    pub durable: bool,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct DelegateModelsChangedNotification {
    pub version: u32,
    pub session_id: String,
    #[typeshare(serialized_as = "number")]
    #[typeshare(typescript(type = "number | null"))]
    pub revision: RequiredNullable<u64>,
}

#[cfg(test)]
mod tests {
    use super::SetDelegateModelRequest;

    #[test]
    fn omitted_model_id_is_rejected() {
        let error = serde_json::from_str::<SetDelegateModelRequest>(
            r#"{"session_id":"s","agent_id":"coder"}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("model_id"));
    }

    #[test]
    fn explicit_null_model_id_clears() {
        let parsed: SetDelegateModelRequest =
            serde_json::from_str(r#"{"session_id":"s","agent_id":"coder","model_id":null}"#)
                .unwrap();
        assert!(parsed.model_id.0.is_none());
        assert!(parsed.node_id.is_none());
    }

    #[test]
    fn camel_case_null_model_id_clears() {
        let parsed: SetDelegateModelRequest =
            serde_json::from_str(r#"{"sessionId":"s","agentId":"coder","modelId":null}"#).unwrap();
        assert!(parsed.model_id.0.is_none());
    }

    #[test]
    fn present_model_id_is_some() {
        let parsed: SetDelegateModelRequest = serde_json::from_str(
            r#"{"session_id":"s","agent_id":"coder","model_id":"test/test-model"}"#,
        )
        .unwrap();
        assert_eq!(parsed.model_id.0.as_deref(), Some("test/test-model"));
    }
}

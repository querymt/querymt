use serde::{Deserialize, Deserializer, Serialize};
use typeshare::typeshare;

use crate::delegation::{DelegateModelOverride, DelegateReasoningEffort};

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

/// An additive request field where omission preserves state and null clears it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionalNullable<T> {
    Missing,
    Null,
    Value(T),
}

impl<T> Default for OptionalNullable<T> {
    fn default() -> Self {
        Self::Missing
    }
}

impl<'de, T> Deserialize<'de> for OptionalNullable<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

impl<T> OptionalNullable<T> {
    pub fn as_update(&self) -> Option<Option<T>>
    where
        T: Clone,
    {
        match self {
            Self::Missing => None,
            Self::Null => Some(None),
            Self::Value(value) => Some(Some(value.clone())),
        }
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
    /// Omitted preserves the current setting; null restores parent-session inheritance.
    #[serde(default, alias = "reasoningEffort")]
    #[typeshare(typescript(type = "DelegateReasoningEffort | null"))]
    pub reasoning_effort: OptionalNullable<DelegateReasoningEffort>,
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
    #[typeshare(typescript(type = "DelegateReasoningEffort | null"))]
    pub reasoning_effort: RequiredNullable<DelegateReasoningEffort>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct OrphanedDelegateAssignment {
    pub agent_id: String,
    #[typeshare(typescript(type = "DelegateModelOverride | null"))]
    pub model: RequiredNullable<DelegateModelOverride>,
    #[typeshare(typescript(type = "DelegateReasoningEffort | null"))]
    pub reasoning_effort: RequiredNullable<DelegateReasoningEffort>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct DelegateAssignmentsInfo {
    pub version: u32,
    pub reasoning_effort_supported: bool,
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
    pub reasoning_effort_supported: bool,
    pub session_id: String,
    pub agent_id: String,
    #[typeshare(typescript(type = "DelegateModelOverride | null"))]
    pub model: RequiredNullable<DelegateModelOverride>,
    #[typeshare(typescript(type = "DelegateReasoningEffort | null"))]
    pub reasoning_effort: RequiredNullable<DelegateReasoningEffort>,
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
    use super::{OptionalNullable, SetDelegateModelRequest};
    use crate::delegation::DelegateReasoningEffort;

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
        assert_eq!(parsed.reasoning_effort, OptionalNullable::Missing);
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

    #[test]
    fn reasoning_effort_distinguishes_missing_null_and_value() {
        let missing: SetDelegateModelRequest =
            serde_json::from_str(r#"{"session_id":"s","agent_id":"coder","model_id":null}"#)
                .unwrap();
        let clear: SetDelegateModelRequest = serde_json::from_str(
            r#"{"session_id":"s","agent_id":"coder","model_id":null,"reasoning_effort":null}"#,
        )
        .unwrap();
        let high: SetDelegateModelRequest = serde_json::from_str(
            r#"{"session_id":"s","agent_id":"coder","model_id":null,"reasoningEffort":"high"}"#,
        )
        .unwrap();
        assert_eq!(missing.reasoning_effort, OptionalNullable::Missing);
        assert_eq!(clear.reasoning_effort, OptionalNullable::Null);
        assert_eq!(
            high.reasoning_effort,
            OptionalNullable::Value(DelegateReasoningEffort::High)
        );
    }
}

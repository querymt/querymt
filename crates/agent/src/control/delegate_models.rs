use serde::{Deserialize, Serialize};
use typeshare::typeshare;

use crate::delegation::DelegateModelOverride;

pub const DELEGATE_MODELS_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequiredNullable<T>(pub Option<T>);

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
    #[serde(default, alias = "modelId")]
    #[typeshare(typescript(type = "string | null"))]
    pub model_id: Option<String>,
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

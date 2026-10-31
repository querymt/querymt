use serde::{Deserialize, Deserializer, Serialize};

use crate::session::error::{SessionError, SessionResult};

/// Persisted lifecycle state for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLifecycle {
    pub session_id: String,
    pub resolved: bool,
    pub archived: bool,
    /// RFC 3339 timestamp for the current resolved state, if resolved.
    pub resolved_at: Option<String>,
    /// RFC 3339 timestamp for the current archived state, if archived.
    pub archived_at: Option<String>,
}

/// Desired-state changes to the independent resolution and archive flags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLifecyclePatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_patch_bool"
    )]
    pub resolved: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_patch_bool"
    )]
    pub archived: Option<bool>,
}

impl SessionLifecyclePatch {
    /// Reject empty patches so every accepted update is explicit.
    pub fn validate(&self) -> SessionResult<()> {
        if self.resolved.is_none() && self.archived.is_none() {
            return Err(SessionError::InvalidOperation(
                "session lifecycle patch must set resolved or archived".into(),
            ));
        }
        Ok(())
    }
}

fn deserialize_patch_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    bool::deserialize(deserializer).map(Some)
}

/// Resolution-state selector for lifecycle-aware session queries.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionFilter {
    #[default]
    Any,
    Open,
    Resolved,
}

/// Archive-state selector for lifecycle-aware session queries.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFilter {
    #[default]
    Any,
    Excluded,
    Only,
}

/// Independent resolution and archive filters for session queries.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionLifecycleFilter {
    pub resolution: ResolutionFilter,
    pub archive: ArchiveFilter,
}

impl SessionLifecycleFilter {
    /// Whether either lifecycle dimension restricts the query.
    pub fn is_filtered(&self) -> bool {
        self.resolution != ResolutionFilter::Any || self.archive != ArchiveFilter::Any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_requires_a_field_and_rejects_null_or_invalid_supplied_values() {
        assert!(SessionLifecyclePatch::default().validate().is_err());
        assert!(serde_json::from_str::<SessionLifecyclePatch>(r#"{"resolved":null}"#).is_err());
        assert!(serde_json::from_str::<SessionLifecyclePatch>(r#"{"archived":"yes"}"#).is_err());
        let valid_patch: SessionLifecyclePatch =
            serde_json::from_str(r#"{"resolved":false}"#).expect("valid patch");
        assert_eq!(valid_patch.resolved, Some(false));
        let serialized = serde_json::to_string(&valid_patch).expect("serialize patch");
        assert_eq!(serialized, r#"{"resolved":false}"#);
        assert_eq!(
            serde_json::from_str::<SessionLifecyclePatch>(&serialized).expect("patch round trip"),
            valid_patch
        );
    }

    #[test]
    fn lifecycle_filter_defaults_to_any_and_uses_snake_case() {
        assert!(!SessionLifecycleFilter::default().is_filtered());
        let filter: SessionLifecycleFilter =
            serde_json::from_str(r#"{"resolution":"open"}"#).expect("filter values");
        assert!(filter.is_filtered());
        assert_eq!(filter.archive, ArchiveFilter::Any);
    }
}

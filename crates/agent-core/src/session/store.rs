//! Defines the generic storage interface for session persistence.

use crate::config::execution::{
    CompactionConfig, PruningConfig, RateLimitConfig, ToolOutputConfig,
};
use crate::session::domain::{ForkOrigin, ForkPointType};
use crate::session::error::SessionError;
pub use crate::session::session_store::SessionStore;
use querymt::LLMParams;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPin {
    pub session_id: String,
    pub pinned: bool,
    pub pinned_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntimeBinding {
    pub profile_id: String,
    pub agent_id: Option<String>,
    pub profile_fingerprint: Option<String>,
    pub provider_lock_digest: Option<String>,
    pub provider_locks_json: Option<String>,
}

/// A bookmark for a remote session — enough metadata to re-discover and
/// re-attach it after a server restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSessionBookmark {
    pub session_id: String,
    /// PeerId string of the remote node that owns the session.
    pub node_id: String,
    /// Human-readable hostname of the remote node.
    pub peer_label: String,
    /// Working directory on the remote machine (if known).
    pub cwd: Option<String>,
    /// Unix timestamp when the bookmark was created.
    pub created_at: i64,
    /// Session title/name (if known).
    pub title: Option<String>,
}

/// Incremental metadata from a confirmed reattach or resume. Fields left as
/// `None` mean "no new confirmed value" and must never erase existing
/// bookmark metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteSessionBookmarkUpdate {
    pub node_id: Option<String>,
    pub peer_label: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

impl RemoteSessionBookmark {
    /// Merge a confirmed reattach/resume result into this bookmark.
    ///
    /// Identity fields are never rewritten: `session_id` is fixed and the
    /// original `created_at` is preserved. Fresh `node_id`/`peer_label` come
    /// only from confirmed node info, and fresh `cwd`/`title` win only when
    /// the update actually carries them.
    pub fn merge_confirmed(self, update: RemoteSessionBookmarkUpdate) -> Self {
        RemoteSessionBookmark {
            session_id: self.session_id,
            node_id: update.node_id.unwrap_or(self.node_id),
            peer_label: update.peer_label.unwrap_or(self.peer_label),
            cwd: update.cwd.or(self.cwd),
            created_at: self.created_at,
            title: update.title.or(self.title),
        }
    }
}

/// Durable startup barrier for a copied conversation awaiting isolation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingForkIsolation {
    pub source_session_id: String,
    pub source_cwd: std::path::PathBuf,
    pub options: crate::worktree::domain::WorkspaceCreationOptions,
    /// Set when the barrier belongs to an ownership handoff: the successor
    /// must adopt this existing workspace (completing the transfer from
    /// `source_session_id`) instead of allocating a fresh one on recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_workspace_id: Option<String>,
}

/// Represents the metadata for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(skip)]
    pub id: i64,
    pub public_id: String,
    pub name: Option<String>,
    pub cwd: Option<std::path::PathBuf>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub created_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    /// Reference to current intent snapshot (internal ID)
    #[serde(skip)]
    pub current_intent_snapshot_id: Option<i64>,
    /// Active task for this session (internal ID)
    #[serde(skip)]
    pub active_task_id: Option<i64>,
    /// Current LLM configuration for this session (internal ID)
    #[serde(skip)]
    pub llm_config_id: Option<i64>,
    /// Parent session if this is a fork (internal ID)
    #[serde(skip)]
    pub parent_session_id: Option<i64>,
    /// Fork origin (user or delegation)
    pub fork_origin: Option<ForkOrigin>,
    /// Session kind for specialized workflows (e.g. "recurring")
    pub session_kind: Option<String>,
    /// Fork point type (message_index or progress_entry)
    pub fork_point_type: Option<ForkPointType>,
    /// Fork point reference (message ID or progress entry ID)
    pub fork_point_ref: Option<String>,
    /// Instructions provided when forking
    pub fork_instructions: Option<String>,
}

/// Stored LLM configuration (internal only, no public_id needed)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMConfig {
    #[serde(skip)]
    pub id: i64,
    pub name: Option<String>,
    pub provider: String,
    pub model: String,
    /// Wire protocol the provider speaks (e.g. `responses`, `chat_completions`).
    ///
    /// Part of the native-replay target identity: provider-opaque state may
    /// only be replayed when the protocol matches too.
    #[serde(default)]
    pub protocol: String,
    /// Normalized endpoint the provider targets.
    ///
    /// Part of the native-replay target identity: encrypted reasoning must not
    /// be forwarded to a different `base_url` that shares a model name.
    #[serde(default)]
    pub endpoint: String,
    pub params: Option<serde_json::Value>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub created_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    /// Mesh node label that owns this provider. `None` = local node.
    /// Stored as `"_node"` key in the `params` JSON blob so no schema migration is needed.
    /// The leading underscore signals it is an internal routing field, not a provider param.
    #[serde(skip)]
    pub provider_node_id: Option<String>,
}

/// User-managed custom model metadata persisted in storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomModel {
    pub provider: String,
    pub model_id: String,
    pub display_name: String,
    pub config_json: serde_json::Value,
    pub source_type: String,
    pub source_ref: Option<String>,
    pub family: Option<String>,
    pub quant: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub created_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPatch {
    pub expected_deliverable: Option<String>,
    pub acceptance_criteria: Option<String>,
    pub status: Option<crate::session::domain::TaskStatus>,
    pub cancellation_reason: Option<String>,
}

/// Authoritative delegate overrides for one parent session. Empty means inheritance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateAssignments {
    pub revision: u64,
    pub overrides: std::collections::BTreeMap<
        String,
        crate::delegation::model_overrides::DelegateModelOverride,
    >,
    pub reasoning_overrides: std::collections::BTreeMap<
        String,
        crate::delegation::model_overrides::DelegateReasoningEffort,
    >,
}

/// Atomic assignment-write result, including whether the transaction changed state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateAssignmentWrite {
    pub assignments: DelegateAssignments,
    pub changed: bool,
}

/// Stored session-scoped execution configuration snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionExecutionConfig {
    pub max_steps: Option<usize>,
    pub max_prompt_bytes: Option<usize>,
    pub execution_timeout_secs: u64,
    pub snapshot_policy: String,
    pub tool_output_config: ToolOutputConfig,
    pub pruning_config: PruningConfig,
    pub compaction_config: CompactionConfig,
    pub rate_limit_config: RateLimitConfig,
}

impl Default for SessionExecutionConfig {
    fn default() -> Self {
        Self {
            max_steps: None,
            max_prompt_bytes: None,
            execution_timeout_secs: 300,
            snapshot_policy: "none".to_string(),
            tool_output_config: ToolOutputConfig::default(),
            pruning_config: PruningConfig::default(),
            compaction_config: CompactionConfig::default(),
            rate_limit_config: RateLimitConfig::default(),
        }
    }
}

/// Helper to extract config values from LLMParams for storage
#[doc(hidden)]
pub fn extract_llm_config_values(
    params: &LLMParams,
) -> Result<(String, String, Option<Value>), SessionError> {
    let provider = params
        .provider
        .as_ref()
        .ok_or_else(|| SessionError::InvalidOperation("Provider is required".to_string()))?
        .clone();

    let model = params
        .model
        .as_ref()
        .ok_or_else(|| SessionError::InvalidOperation("Model is required".to_string()))?
        .clone();

    // Serialize to JSON for storage, but exclude provider, model, and name
    // since those are stored separately in dedicated columns
    let params_json = serde_json::to_value(params).map_err(|e| {
        SessionError::InvalidOperation(format!("Failed to serialize params: {}", e))
    })?;

    // Filter out fields that are stored separately or should not be persisted
    // - provider, model, name: stored in dedicated columns
    // - api_key: sensitive credential, should come from env vars at runtime
    let filtered_params = if let Some(obj) = params_json.as_object() {
        let filtered: serde_json::Map<String, Value> = obj
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "provider" | "model" | "name" | "api_key"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        if filtered.is_empty() {
            None
        } else {
            Some(Value::Object(filtered))
        }
    } else {
        None
    };

    Ok((provider, model, filtered_params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_llm_config_values_filters_meta_fields() {
        let params = LLMParams::new()
            .provider("ollama")
            .model("test-model")
            .name("my-config")
            .api_key("secret-key-123")
            .system("test system")
            .temperature(0.7)
            .parameter("num_ctx", 32768);

        let (provider, model, params_json) = extract_llm_config_values(&params).unwrap();

        // Verify provider and model are extracted
        assert_eq!(provider, "ollama");
        assert_eq!(model, "test-model");

        // Verify params JSON excludes provider, model, name, and api_key
        let params_obj = params_json.unwrap();
        let obj = params_obj.as_object().unwrap();

        assert!(
            !obj.contains_key("provider"),
            "provider should be filtered out"
        );
        assert!(!obj.contains_key("model"), "model should be filtered out");
        assert!(!obj.contains_key("name"), "name should be filtered out");
        assert!(
            !obj.contains_key("api_key"),
            "api_key should be filtered out (security!)"
        );

        // Verify other params are included
        assert_eq!(
            obj.get("system").and_then(|v| v.as_array()),
            Some(&vec![Value::String("test system".to_string())])
        );
        // Check temperature with tolerance for f32 -> f64 conversion
        let temp = obj.get("temperature").and_then(|v| v.as_f64()).unwrap();
        assert!(
            (temp - 0.7).abs() < 0.001,
            "temperature should be approximately 0.7"
        );
        assert_eq!(obj.get("num_ctx").and_then(|v| v.as_i64()), Some(32768));
    }

    #[test]
    fn merge_confirmed_preserves_identity_and_backfills() {
        let original = RemoteSessionBookmark {
            session_id: "s1".to_string(),
            node_id: "node-a".to_string(),
            peer_label: "peer-a".to_string(),
            cwd: Some("/work".to_string()),
            created_at: 111,
            title: Some("Original".to_string()),
        };
        // An update supplying no new values must not erase anything.
        let merged = original.clone().merge_confirmed(Default::default());
        assert_eq!(merged, original);
    }

    #[test]
    fn merge_confirmed_applies_fresh_values_but_keeps_created_at() {
        let original = RemoteSessionBookmark {
            session_id: "s1".to_string(),
            node_id: "node-a".to_string(),
            peer_label: "peer-a".to_string(),
            cwd: Some("/work".to_string()),
            created_at: 111,
            title: None,
        };
        let merged = original.merge_confirmed(RemoteSessionBookmarkUpdate {
            node_id: Some("node-b".to_string()),
            peer_label: None,
            cwd: Some("/other".to_string()),
            title: Some("Fresh title".to_string()),
        });
        assert_eq!(merged.session_id, "s1");
        assert_eq!(merged.created_at, 111, "created_at is identity metadata");
        assert_eq!(merged.node_id, "node-b");
        assert_eq!(merged.peer_label, "peer-a");
        assert_eq!(merged.cwd.as_deref(), Some("/other"));
        assert_eq!(merged.title.as_deref(), Some("Fresh title"));
    }
}

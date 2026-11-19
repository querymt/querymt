//! Session views that appear in load snapshots and events.

use crate::events::AgentEvent;
use crate::session::domain::{Artifact, Decision, Delegation, IntentSnapshot, ProgressEntry, Task};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use typeshare::typeshare;

/// Full audit view with all details for internal/compliance use
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditView {
    pub session_id: String,
    pub events: Vec<AgentEvent>,
    pub tasks: Vec<Task>,
    pub intent_snapshots: Vec<IntentSnapshot>,
    pub decisions: Vec<Decision>,
    pub progress_entries: Vec<ProgressEntry>,
    pub artifacts: Vec<Artifact>,
    pub delegations: Vec<Delegation>,
    #[serde(with = "time::serde::rfc3339")]
    #[typeshare(serialized_as = "string")]
    pub generated_at: OffsetDateTime,
}

/// Durable managed-workspace provenance; execution cwd stays on the session item.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWorkspaceView {
    pub workspace_id: String,
    pub repository_id: String,
    /// Project grouping key: the cwd this workspace was created from.
    /// Stable across archive and removal.
    #[serde(default)]
    pub source_cwd: String,
    pub project_name: String,
    pub checkout_path: String,
    pub branch: String,
    pub baseline_mode: String,
    pub baseline_commit: Option<String>,
    pub lifecycle: String,
    pub last_error: Option<String>,
    /// Session that currently owns (writes in) the checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    /// Session ownership was handed off to, once handoff exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handed_off_to: Option<String>,
}

impl SessionWorkspaceView {
    /// The `_meta.querymt.workspace` object disclosed on session listings.
    pub fn to_meta(&self) -> serde_json::Value {
        serde_json::json!({
            "workspaceId": self.workspace_id,
            "branch": self.branch,
            "checkoutPath": self.checkout_path,
            "sourceCwd": self.source_cwd,
            "baselineMode": self.baseline_mode,
            "lifecycle": self.lifecycle,
            "ownerSessionId": self.owner_session_id,
            "handedOffTo": self.handed_off_to,
        })
    }
}

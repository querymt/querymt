//! Protocol task identity, ownership, trust-decision records and the
//! storage contract reconciliation needs, without any SQLite detail.
//!
//! The SQLite implementation lives in `sqlite_staging::repo_dotagents_tasks`;
//! planning and trust evaluation stay in `dotagents::tasks`.

use crate::dotagents::layer::DotagentsLayer;
use crate::session::error::SessionResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

const KEY_VERSION: &str = "v1";

/// Normalize a raw protocol entry ID into its stable comparison key.
///
/// IDs are trimmed and lowercased so `Review` and `review` collide
/// deterministically instead of producing two entries on case-sensitive
/// filesystems and one on case-insensitive filesystems.
pub fn normalize_id(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Stable protocol identity used to own one task and schedule pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DotagentsTaskIdentity {
    pub layer: DotagentsLayer,
    pub canonical_workspace: Option<PathBuf>,
    pub task_id: String,
}

impl DotagentsTaskIdentity {
    /// Build an identity from a resolved task and canonical workspace.
    pub fn new(
        layer: DotagentsLayer,
        canonical_workspace: Option<impl Into<PathBuf>>,
        task_id: impl AsRef<str>,
    ) -> Self {
        Self {
            layer,
            canonical_workspace: match layer {
                DotagentsLayer::Global => None,
                DotagentsLayer::Workspace => canonical_workspace.map(Into::into),
            },
            task_id: normalize_id(task_id.as_ref()),
        }
    }

    /// Stable source key independent of the automation session binding.
    pub fn source_key(&self) -> String {
        let workspace = self
            .canonical_workspace
            .as_deref()
            .map(path_identity)
            .unwrap_or_else(|| "global".to_string());
        format!(
            "dotagents:{KEY_VERSION}:task:{}:{workspace}:{}",
            self.layer.as_str(),
            self.task_id
        )
    }

    /// Task creation key, scoped by the existing task repository to a session.
    pub fn task_creation_key(&self) -> String {
        format!("{}:record", self.source_key())
    }

    /// Stable schedule ownership key for repository-level reconciliation.
    pub fn schedule_creation_key(&self) -> String {
        format!("{}:schedule", self.source_key())
    }
}

fn path_identity(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_os_str().as_encoded_bytes());
    hex::encode(hasher.finalize())
}

/// Persisted link between a protocol source and its QueryMT records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsTaskOwnership {
    pub source_key: String,
    pub task_creation_key: String,
    pub schedule_creation_key: String,
    pub layer: DotagentsLayer,
    pub canonical_workspace: Option<PathBuf>,
    pub task_id: String,
    pub fingerprint: String,
    pub task_public_id: Option<String>,
    pub schedule_public_id: Option<String>,
}

impl DotagentsTaskOwnership {
    pub fn pending(identity: &DotagentsTaskIdentity, fingerprint: impl Into<String>) -> Self {
        Self {
            source_key: identity.source_key(),
            task_creation_key: identity.task_creation_key(),
            schedule_creation_key: identity.schedule_creation_key(),
            layer: identity.layer,
            canonical_workspace: identity.canonical_workspace.clone(),
            task_id: identity.task_id.clone(),
            fingerprint: fingerprint.into(),
            task_public_id: None,
            schedule_public_id: None,
        }
    }
}

/// A persisted fingerprint-bound workspace trust decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsTaskApproval {
    pub canonical_workspace: PathBuf,
    pub task_id: String,
    pub fingerprint: String,
    pub decision: DotagentsTaskApprovalDecision,
    pub decided_at: OffsetDateTime,
}

/// Storage needed by task reconciliation without exposing SQLite details.
#[async_trait]
pub trait DotagentsTaskStateRepository: Send + Sync {
    async fn upsert_ownership(&self, ownership: DotagentsTaskOwnership) -> SessionResult<()>;
    async fn get_ownership(
        &self,
        source_key: &str,
    ) -> SessionResult<Option<DotagentsTaskOwnership>>;
    /// List every persisted protocol ownership record.
    ///
    /// Reconciliation needs this to find protocol-owned records whose source
    /// files were removed or disabled, so their schedules can be paused without
    /// touching user-created schedules.
    async fn list_ownerships(&self) -> SessionResult<Vec<DotagentsTaskOwnership>>;
    async fn delete_ownership(&self, source_key: &str) -> SessionResult<()>;
    async fn record_approval(&self, approval: DotagentsTaskApproval) -> SessionResult<()>;
    async fn get_approval(
        &self,
        canonical_workspace: &Path,
        task_id: &str,
        fingerprint: &str,
    ) -> SessionResult<Option<DotagentsTaskApproval>>;
}

/// A host's explicit answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DotagentsTaskApprovalDecision {
    Approve,
    Deny,
}

impl DotagentsTaskApprovalDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "approve" => Some(Self::Approve),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// The designated automation session and profile a protocol task binds to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsAutomationBinding {
    /// Public ID of the persistent automation session that owns the records.
    pub session_public_id: String,
    /// Profile ID the task runs under when it does not name its own.
    pub default_profile_id: Option<String>,
}

impl DotagentsAutomationBinding {
    /// Create a binding from an automation session and default profile.
    pub fn new(
        session_public_id: impl Into<String>,
        default_profile_id: Option<impl Into<String>>,
    ) -> Self {
        Self {
            session_public_id: session_public_id.into(),
            default_profile_id: default_profile_id.map(Into::into),
        }
    }
}

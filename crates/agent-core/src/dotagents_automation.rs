//! Protocol-owned automation identity and the repository contract that
//! provisions the durable session each scope binds to (SQLite implementation:
//! `sqlite_staging::repo_dotagents_automation`).

use super::dotagents_tasks::{DotagentsAutomationBinding, normalize_id};
use crate::dotagents::layer::DotagentsLayer;
use crate::session::error::SessionResult;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// The `sessions.session_kind` value used for protocol automation sessions.
pub const AUTOMATION_SESSION_KIND: &str = "dotagents_automation";

/// Version tag embedded in automation binding keys.
const BINDING_KEY_VERSION: &str = "v1";

/// The identity of a protocol automation scope.
///
/// Two protocol trees that share a layer, canonical workspace, and target
/// profile resolve to the same identity, and therefore to the same durable
/// automation session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DotagentsAutomationIdentity {
    /// The protocol layer the tasks came from.
    pub layer: DotagentsLayer,
    /// The canonical workspace for workspace-layer tasks.
    pub canonical_workspace: Option<PathBuf>,
    /// The target profile that will execute the tasks, when profile-scoped.
    pub profile_id: Option<String>,
}

impl DotagentsAutomationIdentity {
    /// Create an automation identity.
    pub fn new(
        layer: DotagentsLayer,
        canonical_workspace: Option<impl Into<PathBuf>>,
        profile_id: Option<impl Into<String>>,
    ) -> Self {
        Self {
            layer,
            canonical_workspace: canonical_workspace.map(Into::into),
            profile_id: profile_id.map(Into::into),
        }
    }

    /// The deterministic, filesystem-independent binding key.
    ///
    /// Contains no secret material; it is derived only from the protocol layer,
    /// a hash of the canonical workspace path, and the target profile ID.
    pub fn binding_key(&self) -> String {
        let workspace = match &self.canonical_workspace {
            Some(path) => hash_path(path),
            None => "none".to_string(),
        };
        let profile = match &self.profile_id {
            Some(profile_id) => normalize_id(profile_id),
            None => "default".to_string(),
        };
        format!(
            "dotagents:{BINDING_KEY_VERSION}:automation:{}:{workspace}:{profile}",
            self.layer.as_str()
        )
    }

    /// A human-readable session name for the automation session.
    pub fn session_name(&self) -> String {
        match (&self.canonical_workspace, &self.profile_id) {
            (Some(workspace), Some(profile_id)) => format!(
                "dotagents automation ({}, {profile_id})",
                workspace.display()
            ),
            (Some(workspace), None) => {
                format!("dotagents automation ({})", workspace.display())
            }
            (None, Some(profile_id)) => format!("dotagents automation (global, {profile_id})"),
            (None, None) => "dotagents automation (global)".to_string(),
        }
    }
}

/// Hash a workspace path into a stable, path-agnostic binding component.
fn hash_path(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    // A short prefix is plenty for disambiguation and keeps keys readable.
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Creates or reuses the durable session owned by a protocol automation scope.
///
/// Implementations must be idempotent: calling `ensure_automation_session`
/// repeatedly with the same identity returns the same session and never creates
/// a duplicate, so reconciliation after a restart cannot accumulate sessions.
#[async_trait]
pub trait DotagentsAutomationRepository: Send + Sync {
    /// Return the existing automation session for `identity`, creating one if
    /// this scope has never been reconciled.
    async fn ensure_automation_session(
        &self,
        identity: &DotagentsAutomationIdentity,
    ) -> SessionResult<DotagentsAutomationBinding>;

    /// Look up an existing automation binding without creating one.
    async fn find_automation_session(
        &self,
        identity: &DotagentsAutomationIdentity,
    ) -> SessionResult<Option<DotagentsAutomationBinding>>;
}

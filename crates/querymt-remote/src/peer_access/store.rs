//! Persistence boundary for host-owned sharing grants.

use async_trait::async_trait;
use thiserror::Error;
use uuid::Uuid;

use super::{InvalidPeerGrant, PeerGrant, PeerGrantKey};
use crate::NodeId;

/// Last committed policy and the revision to supply to the next owner write.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PeerGrantState {
    /// The nil revision represents an absent row, not a persisted edit.
    pub revision: Uuid,
    /// The effective stored policy, or No access when no policy exists.
    pub grant: PeerGrant,
}

/// A failed grant read or replacement. Callers must not treat failures as grants.
#[derive(Debug, Error)]
pub enum GrantStoreError {
    #[error(transparent)]
    Invalid(#[from] InvalidPeerGrant),
    #[error("grant revision changed")]
    Conflict { expected: Uuid, actual: Uuid },
    #[error("mesh grant store unavailable")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// A committed policy change that can invalidate admitted work.
#[derive(Debug, Clone)]
pub struct PolicyChange {
    pub key: PeerGrantKey,
    pub effect: PolicyEffect,
}

#[derive(Debug, Clone)]
pub enum PolicyEffect {
    Grant(PeerGrant),
}

/// Durable owner policy reads and compare-and-swap edits.
#[async_trait]
pub trait GrantStore: Send + Sync {
    /// Return the last committed state or default No access for a missing grant.
    async fn grant(&self, key: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError>;

    /// Replace the policy iff the revision matches. Return only after commit.
    async fn replace_grant(
        &self,
        key: &PeerGrantKey,
        expected: Uuid,
        grant: PeerGrant,
    ) -> Result<PeerGrantState, GrantStoreError>;

    /// Subscribe to committed permission changes.
    /// Stores with out-of-process writers should return `None` unless they observe those writers.
    fn subscribe_changes(&self) -> Option<tokio::sync::broadcast::Receiver<PolicyChange>> {
        None
    }

    /// Subscribe to one peer's committed policy changes, if supported. Stores
    /// with only a shared history still fail closed if that history is lost.
    fn subscribe_peer_changes(
        &self,
        _key: &PeerGrantKey,
    ) -> Option<tokio::sync::broadcast::Receiver<PolicyChange>> {
        self.subscribe_changes()
    }

    /// List persisted peers, including those not currently connected.
    async fn managed_peers(&self, host: &NodeId) -> Result<Vec<NodeId>, GrantStoreError>;
}

/// Observe policy effects since admission, checking the live grant at use time.
/// A subsequent regrant cannot hide a revocation already delivered to the watcher.
pub async fn wait_for_policy_loss<E>(
    store: &dyn GrantStore,
    key: &PeerGrantKey,
    mut changes: Option<tokio::sync::broadcast::Receiver<PolicyChange>>,
    check: impl Fn(&PeerGrant) -> Result<(), E>,
    unavailable: impl Fn() -> E,
) -> E {
    use tokio::sync::broadcast::error::RecvError;

    loop {
        match store.grant(key).await {
            Ok(state) => {
                if let Err(error) = check(&state.grant) {
                    return error;
                }
            }
            Err(_) => return unavailable(),
        }
        if let Some(rx) = &mut changes {
            loop {
                match rx.recv().await {
                    Ok(change) if &change.key != key => continue,
                    Ok(PolicyChange {
                        effect: PolicyEffect::Grant(grant),
                        ..
                    }) => {
                        if let Err(error) = check(&grant) {
                            return error;
                        }
                        break;
                    }
                    Err(RecvError::Lagged(_) | RecvError::Closed) => return unavailable(),
                }
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

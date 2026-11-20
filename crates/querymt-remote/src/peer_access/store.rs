//! Persistence boundary for host-owned sharing grants.

use async_trait::async_trait;
use thiserror::Error;
use uuid::Uuid;

use super::{InvalidPeerGrant, PeerGrant, PeerGrantKey};
use crate::NodeId;

/// Last committed policy and the revision to supply to the next owner write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

    /// List persisted peers, including those not currently connected.
    async fn managed_peers(&self, host: &NodeId) -> Result<Vec<NodeId>, GrantStoreError>;
}

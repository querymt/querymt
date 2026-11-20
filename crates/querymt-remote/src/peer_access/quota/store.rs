//! Storage operations for atomic quota admission and settlement.

use async_trait::async_trait;
use uuid::Uuid;

use super::{
    AttemptUsage, QuotaAttempt, QuotaEdit, QuotaError, QuotaScope, QuotaSettlement, QuotaStatus,
    ReservationId,
};
use crate::{
    NodeId,
    peer_access::{GrantStore, PeerGrantKey},
};

/// Durable quota operations. All implementations must serialize admission and edits.
#[async_trait]
pub trait QuotaStore: GrantStore {
    /// Read current accounting information for a scope.
    async fn quota(
        &self,
        key: &PeerGrantKey,
        scope: &QuotaScope,
        now: i64,
    ) -> Result<QuotaStatus, QuotaError>;

    /// Edit a quota and advance the same revision used for grant edits.
    async fn edit_quota(
        &self,
        key: &PeerGrantKey,
        expected: Uuid,
        scope: &QuotaScope,
        edit: QuotaEdit,
        now: i64,
    ) -> Result<Uuid, QuotaError>;

    /// Check the current grant and atomically reserve every applicable finite scope.
    async fn reserve(&self, attempt: QuotaAttempt) -> Result<ReservationId, QuotaError>;

    /// Mark the attempt may-have-dispatched before issuing any upstream request.
    async fn mark_dispatched(&self, attempt: ReservationId) -> Result<(), QuotaError>;

    /// Release work proven not to have dispatched.
    async fn release_unused(&self, attempt: ReservationId) -> Result<(), QuotaError>;

    /// Settle a dispatched attempt into the original windows at most once.
    async fn settle(
        &self,
        attempt: ReservationId,
        usage: AttemptUsage,
    ) -> Result<QuotaSettlement, QuotaError>;

    /// Reconcile unfinished attempts only during exclusive startup for this host.
    /// The caller must ensure no other process is serving this host identity.
    async fn reconcile_startup(&self, host: &NodeId) -> Result<(), QuotaError>;
}

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
    /// Receive a refresh hint when committed usage changes, without advancing the grant revision.
    fn subscribe_usage_changes(&self) -> Option<tokio::sync::broadcast::Receiver<PeerGrantKey>> {
        None
    }

    /// Read the grant and all stored quota scopes from one snapshot.
    async fn access_snapshot(
        &self,
        key: &PeerGrantKey,
        _now: i64,
    ) -> Result<(crate::peer_access::PeerGrantState, Vec<QuotaStatus>), QuotaError> {
        Ok((self.grant(key).await?, Vec::new()))
    }

    /// Edit a quota and advance the same revision used for grant edits.
    async fn edit_quota(
        &self,
        key: &PeerGrantKey,
        expected: Uuid,
        scope: &QuotaScope,
        edit: QuotaEdit,
        now: i64,
    ) -> Result<Uuid, QuotaError>;

    /// Require the initiating grant revision and atomically reserve all applicable finite scopes.
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

/// Owns an admitted attempt that cannot yet invoke the provider.
/// Dropping it releases the reservation without charging.
#[must_use]
pub struct ReservedAttempt(AttemptLease);

/// Owns an attempt that may have reached the provider.
/// Dropping it without authoritative usage settles conservatively.
#[must_use]
pub struct DispatchedAttempt(AttemptLease);

struct AttemptLease {
    store: std::sync::Arc<dyn QuotaStore>,
    id: ReservationId,
    state: Option<AttemptPhase>,
}

#[derive(Clone, Copy)]
enum AttemptPhase {
    Reserved,
    Dispatched,
}

impl AttemptLease {
    async fn release(&mut self) -> Result<(), QuotaError> {
        self.store.release_unused(self.id).await?;
        self.state = None;
        Ok(())
    }

    async fn settle(&mut self, usage: AttemptUsage) -> Result<QuotaSettlement, QuotaError> {
        let result = self.store.settle(self.id, usage).await?;
        self.state = None;
        Ok(result)
    }
}

impl Drop for AttemptLease {
    fn drop(&mut self) {
        let Some(phase) = self.state.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(?self.id, "mesh quota attempt requires startup reconciliation");
            return;
        };
        let mut cleanup = AttemptLease {
            store: self.store.clone(),
            id: self.id,
            state: None,
        };
        runtime.spawn(async move {
            let result = match phase {
                AttemptPhase::Reserved => cleanup.release().await,
                AttemptPhase::Dispatched => cleanup.settle(AttemptUsage::Unknown).await.map(|_| ()),
            };
            if let Err(error) = result {
                tracing::warn!(?cleanup.id, %error, "mesh quota attempt requires reconciliation");
            }
        });
    }
}

fn task_failed(error: tokio::task::JoinError) -> QuotaError {
    crate::peer_access::GrantStoreError::Backend(Box::new(error)).into()
}

impl ReservedAttempt {
    /// Reserve under the initiating grant. Cancellation cannot orphan the committed reservation.
    pub async fn reserve(
        store: std::sync::Arc<dyn QuotaStore>,
        attempt: QuotaAttempt,
    ) -> Result<Self, QuotaError> {
        // SQLite may commit even when the waiting future is dropped. Keep ownership
        // inside the task until a guard can release the committed reservation.
        tokio::spawn(async move {
            let id = store.reserve(attempt).await?;
            Ok(Self(AttemptLease {
                store,
                id,
                state: Some(AttemptPhase::Reserved),
            }))
        })
        .await
        .map_err(task_failed)?
    }

    /// Persist dispatch before sending the upstream request.
    pub async fn begin(self) -> Result<DispatchedAttempt, QuotaError> {
        let Self(mut lease) = self;
        tokio::spawn(async move {
            lease.store.mark_dispatched(lease.id).await?;
            lease.state = Some(AttemptPhase::Dispatched);
            Ok(DispatchedAttempt(lease))
        })
        .await
        .map_err(task_failed)?
    }

    /// Await explicit release before returning a known pre-dispatch failure.
    pub async fn release(self) -> Result<(), QuotaError> {
        let Self(mut lease) = self;
        tokio::spawn(async move { lease.release().await })
            .await
            .map_err(task_failed)?
    }
}

impl DispatchedAttempt {
    /// Account against the original windows before delivering the terminal result.
    pub async fn settle(self, usage: AttemptUsage) -> Result<QuotaSettlement, QuotaError> {
        let Self(mut lease) = self;
        tokio::spawn(async move { lease.settle(usage).await })
            .await
            .map_err(task_failed)?
    }
}

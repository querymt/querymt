//! Storage operations for atomic quota admission and settlement.

use std::time::Duration;

use async_trait::async_trait;
use tokio::runtime::Handle;
use uuid::Uuid;

use super::{
    AttemptUsage, QuotaAttempt, QuotaEdit, QuotaError, QuotaScope, QuotaSettlement, QuotaStatus,
    ReservationId,
};
use crate::{
    NodeId,
    peer_access::{GrantStore, GrantStoreError, PeerGrant, PeerGrantKey, PeerGrantState, PolicyChange},
};

/// An owner edit guarded by the shared grant/quota revision.
pub enum AccessEdit {
    Grant(PeerGrant),
    Quota(QuotaScope, QuotaEdit),
}

/// Durable quota operations. All implementations must serialize admission and edits.
#[async_trait]
pub trait QuotaStore: GrantStore {
    /// Receive a refresh hint when committed usage or reservations change, without advancing the grant revision.
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

    /// Apply an owner edit and build its grant and quota snapshot in the same transaction.
    async fn edit_access(
        &self,
        key: &PeerGrantKey,
        expected: Uuid,
        edit: AccessEdit,
        now: i64,
    ) -> Result<(PeerGrantState, Vec<QuotaStatus>), QuotaError>;

    /// Edit a quota and advance the same revision used for grant edits.
    async fn edit_quota(
        &self,
        key: &PeerGrantKey,
        expected: Uuid,
        scope: &QuotaScope,
        edit: QuotaEdit,
        now: i64,
    ) -> Result<Uuid, QuotaError> {
        let (grant, _) = self
            .edit_access(key, expected, AccessEdit::Quota(scope.clone(), edit), now)
            .await?;
        Ok(grant.revision)
    }

    /// Check current policy and atomically reserve all applicable finite scopes.
    async fn reserve(&self, attempt: QuotaAttempt) -> Result<ReservationId, QuotaError>;

    /// Reserve for deferred work, checking admission-time history atomically
    /// with quota admission and retaining it until dispatch.
    async fn reserve_with_policy(
        &self,
        _attempt: QuotaAttempt,
        _changes: tokio::sync::broadcast::Receiver<PolicyChange>,
        _require_sessions: bool,
    ) -> Result<ReservationId, QuotaError> {
        Err(QuotaError::Denied)
    }

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

    /// Advance quota windows and prune expired history at startup for this host.
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
    runtime: Handle,
    state: Option<AttemptPhase>,
    completion: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Clone, Copy)]
enum AttemptPhase {
    Reserved,
    Dispatched(AttemptUsage),
}

impl AttemptLease {
    async fn release(&mut self) -> Result<(), QuotaError> {
        self.store.release_unused(self.id).await?;
        self.state = None;
        if let Some(done) = self.completion.take() {
            let _ = done.send(());
        }
        Ok(())
    }

    async fn settle(&mut self, usage: AttemptUsage) -> Result<QuotaSettlement, QuotaError> {
        if matches!(usage, AttemptUsage::Observed(tokens) if tokens < 0) {
            return Err(QuotaError::InvalidTokens);
        }
        self.state = Some(AttemptPhase::Dispatched(usage));
        let result = self.store.settle(self.id, usage).await?;
        self.state = None;
        if let Some(done) = self.completion.take() {
            let _ = done.send(());
        }
        Ok(result)
    }
}

impl Drop for AttemptLease {
    fn drop(&mut self) {
        let Some(phase) = self.state.take() else {
            return;
        };
        let store = self.store.clone();
        let id = self.id;
        let completion = self.completion.take();
        self.runtime.spawn(async move {
            let initial_delay = Duration::from_millis(50);
            let mut delay = initial_delay;
            loop {
                let result = match phase {
                    AttemptPhase::Reserved => store.release_unused(id).await,
                    AttemptPhase::Dispatched(usage) => store.settle(id, usage).await.map(|_| ()),
                };
                match result {
                    Ok(()) => {
                        if let Some(done) = completion {
                            let _ = done.send(());
                        }
                        break;
                    }
                    Err(error @ QuotaError::Store(GrantStoreError::Backend(_))) => {
                        if delay == initial_delay {
                            tracing::warn!(?id, %error, "retrying mesh quota cleanup");
                        }
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(5));
                    }
                    Err(error) => {
                        tracing::warn!(?id, %error, "mesh quota cleanup failed permanently");
                        break;
                    }
                }
            }
        });
    }
}

fn task_failed(error: tokio::task::JoinError) -> QuotaError {
    crate::peer_access::GrantStoreError::Backend(Box::new(error)).into()
}

impl ReservedAttempt {
    // A relay can drop a blocked upstream read and still wait for accounting
    // before reporting the terminal failure.
    #[cfg(any(feature = "kameo-mesh", test))]
    pub(crate) fn completion(&mut self) -> tokio::sync::oneshot::Receiver<()> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.0.completion = Some(sender);
        receiver
    }

    /// Reserve under the current grant. Cancellation cannot orphan the committed reservation.
    pub async fn reserve(
        store: std::sync::Arc<dyn QuotaStore>,
        attempt: QuotaAttempt,
    ) -> Result<Self, QuotaError> {
        Self::reserve_inner(store, attempt, None).await
    }

    /// Reserve under an admission-time policy watch. The watch must come from
    /// the same store and predate the initiating authorization.
    pub async fn reserve_with_policy(
        store: std::sync::Arc<dyn QuotaStore>,
        attempt: QuotaAttempt,
        changes: tokio::sync::broadcast::Receiver<PolicyChange>,
        require_sessions: bool,
    ) -> Result<Self, QuotaError> {
        Self::reserve_inner(store, attempt, Some((changes, require_sessions))).await
    }

    async fn reserve_inner(
        store: std::sync::Arc<dyn QuotaStore>,
        attempt: QuotaAttempt,
        policy: Option<(tokio::sync::broadcast::Receiver<PolicyChange>, bool)>,
    ) -> Result<Self, QuotaError> {
        // The store may commit even when the waiting future is dropped. Keep ownership
        // inside the task until a guard can release the committed reservation.
        tokio::spawn(async move {
            let id = match policy {
                Some((changes, require_sessions)) => {
                    store
                        .reserve_with_policy(attempt, changes, require_sessions)
                        .await?
                }
                None => store.reserve(attempt).await?,
            };
            Ok(Self(AttemptLease {
                store,
                id,
                runtime: Handle::current(),
                state: Some(AttemptPhase::Reserved),
                completion: None,
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
            lease.state = Some(AttemptPhase::Dispatched(AttemptUsage::Unknown));
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

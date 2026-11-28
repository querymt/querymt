//! Delegate sessions whose delegation is still working in their checkout.
//!
//! After a delegate's prompt ends its actor is idle, but the delegation is
//! not done: the orchestrator still verifies in the delegate's checkout,
//! reads the delegate's history through its actor and publishes the
//! worktree's result. Until then the session must not be unloaded or its
//! workspace handed off, archived or merged from a snapshot.
//!
//! A held session reports itself busy to [`LocalAgentHandle::session_is_busy`]
//! and [`LocalAgentHandle::unload_session_if_idle`], so every worktree
//! operation refuses it with `owner_active` without unloading it. The hold
//! also keeps a shared checkout activity lease, so an exclusive lease (taken by
//! archive, removal and handoff, also from another process) is refused while
//! the delegation runs even when the actor itself is gone.
//!
//! [`LocalAgentHandle::session_is_busy`]: super::LocalAgentHandle::session_is_busy
//! [`LocalAgentHandle::unload_session_if_idle`]: super::LocalAgentHandle::unload_session_if_idle

use crate::worktree::gate::CheckoutActivityLease;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

type Held = Arc<Mutex<HashMap<String, Option<CheckoutActivityLease>>>>;

/// The delegate sessions held by a running delegation, by session id.
#[derive(Default)]
pub(crate) struct DelegationHolds {
    held: Held,
}

/// A hold on one delegate session. Dropping it releases the hold (and its
/// lease) unless [`DelegationHolds::release`] already did.
pub(crate) struct DelegationHold {
    session_id: String,
    held: Held,
}

impl DelegationHolds {
    /// Holds `session_id` until the returned guard is dropped or the hold is
    /// released. `lease` is the shared checkout activity lease kept with it.
    ///
    /// A session is the child of exactly one delegation, so one hold per
    /// session exists at a time.
    pub(crate) fn hold(
        &self,
        session_id: &str,
        lease: Option<CheckoutActivityLease>,
    ) -> DelegationHold {
        self.held.lock().insert(session_id.to_string(), lease);
        DelegationHold {
            session_id: session_id.to_string(),
            held: Arc::clone(&self.held),
        }
    }

    /// Whether a delegation still holds `session_id`.
    pub(crate) fn is_held(&self, session_id: &str) -> bool {
        self.held.lock().contains_key(session_id)
    }

    /// Releases the hold on `session_id` (and its lease), before the
    /// delegation reports its terminal state, so the delegating session can
    /// hand the worktree on as soon as it learns the delegation ended.
    pub(crate) fn release(&self, session_id: &str) {
        let lease = self.held.lock().remove(session_id);
        drop(lease);
    }
}

impl Drop for DelegationHold {
    fn drop(&mut self) {
        let lease = self.held.lock().remove(&self.session_id);
        drop(lease);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hold_ends_with_its_guard_or_an_early_release() {
        let holds = DelegationHolds::default();
        let guard = holds.hold("a", None);
        let other = holds.hold("b", None);
        assert!(holds.is_held("a") && holds.is_held("b"));

        holds.release("a");
        assert!(!holds.is_held("a"));
        // Dropping a released guard is a no-op.
        drop(guard);
        assert!(holds.is_held("b"));

        drop(other);
        assert!(!holds.is_held("b"));
    }

    #[test]
    fn the_lease_is_kept_until_the_hold_ends() {
        let storage = tempfile::TempDir::new().unwrap();
        let checkout = tempfile::TempDir::new().unwrap();
        let shared =
            CheckoutActivityLease::acquire(storage.path(), checkout.path(), false).unwrap();
        let holds = DelegationHolds::default();
        let _guard = holds.hold("a", Some(shared));
        assert!(
            CheckoutActivityLease::acquire(storage.path(), checkout.path(), true).is_err(),
            "the held shared lease refuses an exclusive one"
        );
        holds.release("a");
        assert!(CheckoutActivityLease::acquire(storage.path(), checkout.path(), true).is_ok());
    }
}

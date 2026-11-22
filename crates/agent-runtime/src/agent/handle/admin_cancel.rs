//! Cancellation of session administration that runs without a session actor.
//!
//! A worktree handoff unloads the owner's actor before it summarizes the
//! conversation, so a `session/cancel` for that session finds no actor to
//! stop. An administration step that can run for a long time registers a token
//! here for the session it works on, and `cancel` reaches it through
//! [`AdminCancels::cancel`].

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

type Active = Arc<Mutex<HashMap<String, (u64, CancellationToken)>>>;

/// The cancellable administration steps in progress, by session id.
#[derive(Default)]
pub(crate) struct AdminCancels {
    active: Active,
    next_id: AtomicU64,
}

/// An administration step registered for a session. Dropping it deregisters
/// the step.
pub(crate) struct AdminCancelGuard {
    session_id: String,
    id: u64,
    token: CancellationToken,
    active: Active,
}

impl AdminCancels {
    /// Registers a step working on `session_id`.
    ///
    /// Two steps never run for one session at once: they hold the checkout's
    /// exclusive activity lease. A second registration would replace the
    /// first, which is then no longer reachable.
    pub(crate) fn begin(&self, session_id: &str) -> AdminCancelGuard {
        let token = CancellationToken::new();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.active
            .lock()
            .insert(session_id.to_string(), (id, token.clone()));
        AdminCancelGuard {
            session_id: session_id.to_string(),
            id,
            token,
            active: Arc::clone(&self.active),
        }
    }

    /// Cancels the step registered for `session_id`. Returns whether there was
    /// one.
    pub(crate) fn cancel(&self, session_id: &str) -> bool {
        match self.active.lock().get(session_id) {
            Some((_, token)) => {
                token.cancel();
                true
            }
            None => false,
        }
    }
}

impl AdminCancelGuard {
    /// The token the step watches.
    pub(crate) fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Drop for AdminCancelGuard {
    fn drop(&mut self) {
        let mut active = self.active.lock();
        // Only the registration this guard made: a later one for the same
        // session is not ours to remove.
        if active
            .get(&self.session_id)
            .is_some_and(|(id, _)| *id == self.id)
        {
            active.remove(&self.session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_reaches_the_registered_step_only() {
        let cancels = AdminCancels::default();
        let step = cancels.begin("a");
        let other = cancels.begin("b");

        assert!(cancels.cancel("a"));

        assert!(step.token().is_cancelled());
        assert!(!other.token().is_cancelled());
        assert!(!cancels.cancel("unknown"));
    }

    #[test]
    fn a_finished_step_is_no_longer_cancellable() {
        let cancels = AdminCancels::default();
        let step = cancels.begin("a");
        drop(step);

        assert!(!cancels.cancel("a"));
    }

    #[test]
    fn a_finished_step_does_not_deregister_a_later_one() {
        let cancels = AdminCancels::default();
        let first = cancels.begin("a");
        let second = cancels.begin("a");
        drop(first);

        assert!(cancels.cancel("a"), "the later step is still registered");
        assert!(second.token().is_cancelled());
    }
}

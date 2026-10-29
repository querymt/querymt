//! Per-connection delivery position of a session's event stream.
//!
//! Every client connection (WebSocket, stdio) owns one [`ConnectionDelivery`].
//! Its event forwarders pass each live event through [`ConnectionDelivery::admit`]
//! before handling it, which:
//!
//! - drops lagged copies of events that were already recovered from the journal,
//! - detects a gap in the session's `session_seq` and fills it from the
//!   journal (durable events only; missed ephemeral events are not recoverable
//!   and are superseded by the recovered durable events),
//! - hands out a [`DeliveredGuard`] that advances the *delivered* position
//!   when the forwarder is done with the event (also for filtered events and
//!   on early exits), so the position never runs ahead of what was sent.
//!
//! The dispatcher of `session/prompt` uses [`ConnectionDelivery::wait_for_tip`]
//! to hold the response until the connection has delivered every update the
//! prompt produced, and releases it (with a recorded reason) when delivery is
//! lost, so a response is never withheld indefinitely.

use crate::acp::protocol::{Meta, PromptResponse};
use crate::events::{AgentEventKind, EventEnvelope};
use crate::session::projection::EventJournal;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Longest a prompt response waits for its session updates.
pub(crate) const PROMPT_WAIT_LIMIT: Duration = Duration::from_secs(30);

/// `_meta.querymt.eventTip` key: the last `session_seq` a prompt run produced.
/// It travels in the prompt reply between nodes and is consumed (removed) by
/// the transport before the response reaches the client.
pub(crate) const EVENT_TIP_META_KEY: &str = "eventTip";

/// Attach the run's event tip to a prompt reply. A tip of `0` (no events) is
/// not attached.
pub(crate) fn with_event_tip(mut response: PromptResponse, tip: i64) -> PromptResponse {
    if tip > 0 {
        let meta = response.meta.get_or_insert_with(Meta::new);
        let namespace = meta
            .entry(crate::acp::shared::QUERYMT_META_NAMESPACE.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(namespace) = namespace.as_object_mut() {
            namespace.insert(EVENT_TIP_META_KEY.to_string(), serde_json::json!(tip));
        }
    }
    response
}

/// Remove and return the event tip of a prompt reply. A missing (or malformed)
/// tip means "no watermark": the response is not held back.
pub(crate) fn take_event_tip(response: &mut PromptResponse) -> Option<i64> {
    let meta = response.meta.as_mut()?;
    let namespace = meta
        .get_mut(crate::acp::shared::QUERYMT_META_NAMESPACE)?
        .as_object_mut()?;
    let tip = namespace.remove(EVENT_TIP_META_KEY)?.as_i64();
    if namespace.is_empty() {
        meta.remove(crate::acp::shared::QUERYMT_META_NAMESPACE);
    }
    if meta.is_empty() {
        response.meta = None;
    }
    tip.filter(|tip| *tip > 0)
}

/// Why a waiting response was released without its updates being confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseReason {
    /// The connection was closed.
    ConnectionClosed,
    /// The connection lagged behind the live stream and could not fully
    /// catch up (missed ephemeral events are not recoverable).
    Lagged,
    /// Recovering missed events from the journal failed.
    RecoveryFailed,
    /// The relay of a remote-hosted session disconnected.
    RelayDisconnected,
    /// The bounded safety limit elapsed.
    TimedOut,
}

/// Result of waiting for a delivery watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    /// The connection delivered everything up to the watermark.
    Delivered,
    /// Delivery could not be confirmed; ordering is not guaranteed.
    Released(ReleaseReason),
}

#[derive(Debug, Clone, Copy, Default)]
struct Position {
    /// Highest `session_seq` handed to a forwarder (gap baseline).
    admitted: i64,
    /// Highest `session_seq` a forwarder finished handling.
    delivered: i64,
    /// Whether `admitted` came from a real event.
    known: bool,
    /// Incremented whenever waiters of the session must be released.
    release_epoch: u64,
    /// Why the most recent release happened.
    release_reason: Option<ReleaseReason>,
    /// A journal recovery for this session failed during the current lag.
    recovery_failed: bool,
    /// `(low, high]` range recovered from the journal after a lag; lagged
    /// copies of these events still sitting in the channel are duplicates.
    recovered: Option<(i64, i64)>,
}

type SessionState = Arc<watch::Sender<Position>>;

fn release(state: &SessionState, reason: ReleaseReason) {
    state.send_modify(|p| {
        p.release_epoch += 1;
        p.release_reason = Some(reason);
    });
}

/// An event cleared for handling, with the guard that records its delivery.
pub(crate) struct Admitted {
    pub(crate) event: EventEnvelope,
    pub(crate) guard: Option<DeliveredGuard>,
}

/// Advances the delivered position of its session when dropped, and releases
/// waiters when the handled event announces lost delivery.
pub(crate) struct DeliveredGuard {
    state: SessionState,
    seq: i64,
    release: Option<ReleaseReason>,
}

impl Drop for DeliveredGuard {
    fn drop(&mut self) {
        let seq = self.seq;
        if seq > 0 {
            self.state.send_if_modified(|position| {
                if seq > position.delivered {
                    position.delivered = seq;
                    true
                } else {
                    false
                }
            });
        }
        if let Some(reason) = self.release {
            release(&self.state, reason);
        }
    }
}

/// Delivery positions of one client connection.
pub(crate) struct ConnectionDelivery {
    journal: Option<Arc<dyn EventJournal>>,
    sessions: Mutex<HashMap<String, SessionState>>,
    closed: CancellationToken,
}

impl ConnectionDelivery {
    /// `journal` is used to recover missed durable events; without one, lag
    /// and gaps are logged and skipped. `closed` is the connection's
    /// cancellation token and releases every waiter.
    pub(crate) fn new(
        journal: Option<Arc<dyn EventJournal>>,
        closed: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            journal,
            sessions: Mutex::new(HashMap::new()),
            closed,
        })
    }

    fn state(&self, session_id: &str) -> SessionState {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(state) = sessions.get(session_id) {
            return state.clone();
        }
        let (tx, _rx) = watch::channel(Position::default());
        let state = Arc::new(tx);
        sessions.insert(session_id.to_string(), state.clone());
        state
    }

    /// Delivered position of a session (`0` when nothing was delivered).
    #[cfg(test)]
    pub(crate) fn delivered(&self, session_id: &str) -> i64 {
        self.state(session_id).borrow().delivered
    }

    /// Clear one live event for handling.
    ///
    /// Returns the events to handle, in order: normally the event itself;
    /// after a gap, the recovered durable events followed by the event (unless
    /// the journal already supplied it); nothing for a lagged copy of an
    /// event that was already recovered.
    pub(crate) async fn admit(&self, event: EventEnvelope) -> Vec<Admitted> {
        let seq = event.session_seq();
        if seq <= 0 {
            // Unsequenced events are not tracked, except that the loss of a
            // remote session's relay ends the wait once it was delivered.
            let release = matches!(
                event.kind(),
                AgentEventKind::RemoteSessionDisconnected { .. }
            )
            .then_some(ReleaseReason::RelayDisconnected);
            let guard = release.map(|reason| DeliveredGuard {
                state: self.state(event.session_id()),
                seq: 0,
                release: Some(reason),
            });
            return vec![Admitted { event, guard }];
        }
        let session_id = event.session_id().to_string();
        let state = self.state(&session_id);
        let position = *state.borrow();

        if !position.known {
            state.send_modify(|p| {
                p.known = true;
                p.admitted = seq;
            });
            return vec![Self::admitted(&state, event)];
        }
        if seq <= position.admitted {
            if position
                .recovered
                .is_some_and(|(low, high)| seq > low && seq <= high)
            {
                return Vec::new();
            }
            // Older than the live position but not a recovered copy (for
            // example a backfilled event): handle it, it advances nothing.
            return vec![Self::admitted(&state, event)];
        }
        // Everything still queued from before a recovery is behind us now.
        if position.recovered.is_some() {
            state.send_modify(|p| p.recovered = None);
        }
        if seq == position.admitted + 1 {
            state.send_modify(|p| p.admitted = seq);
            return vec![Self::admitted(&state, event)];
        }

        // Gap: events (admitted, seq) were missed.
        let recovered = match self
            .recover(&session_id, position.admitted, Some(seq))
            .await
        {
            Ok(recovered) => recovered,
            Err(()) => {
                release(&state, ReleaseReason::RecoveryFailed);
                Vec::new()
            }
        };
        let mut out: Vec<Admitted> = Vec::with_capacity(recovered.len() + 1);
        let mut supplied = false;
        for durable in recovered {
            supplied |= durable.session_seq == seq;
            out.push(Self::admitted(&state, EventEnvelope::Durable(durable)));
        }
        if !supplied {
            out.push(Self::admitted(&state, event));
        }
        state.send_modify(|p| p.admitted = seq);
        out
    }

    pub(crate) fn observed_sessions(&self) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Recover every durable event a lagged connection missed, for every
    /// session it has seen events of. Call [`Self::finish_lag_recovery`] after
    /// the returned events were handled.
    pub(crate) async fn recover_after_lag(&self) -> Vec<Admitted> {
        let tracked: Vec<(String, SessionState, i64)> = {
            let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            sessions
                .iter()
                .filter_map(|(id, state)| {
                    let position = *state.borrow();
                    position
                        .known
                        .then(|| (id.clone(), state.clone(), position.admitted))
                })
                .collect()
        };

        let mut out = Vec::new();
        for (session_id, state, admitted) in tracked {
            match self.recover(&session_id, admitted, None).await {
                Ok(recovered) => {
                    let mut highest = admitted;
                    for durable in recovered {
                        highest = highest.max(durable.session_seq);
                        out.push(Self::admitted(&state, EventEnvelope::Durable(durable)));
                    }
                    if highest > admitted {
                        state.send_modify(|p| {
                            p.admitted = p.admitted.max(highest);
                            p.recovered = Some(match p.recovered {
                                Some((low, high)) => (low.min(admitted), high.max(highest)),
                                None => (admitted, highest),
                            });
                        });
                    }
                }
                Err(()) => state.send_modify(|p| p.recovery_failed = true),
            }
        }
        out
    }

    /// Record that lag handling completed: waiters whose watermark is still
    /// ahead of the delivered position are released, because the remainder
    /// may be ephemeral events that cannot be recovered.
    pub(crate) fn finish_lag_recovery(&self) {
        let sessions: Vec<SessionState> = {
            let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            sessions.values().cloned().collect()
        };
        for state in sessions {
            state.send_modify(|p| {
                p.release_epoch += 1;
                p.release_reason = Some(if p.recovery_failed {
                    ReleaseReason::RecoveryFailed
                } else {
                    ReleaseReason::Lagged
                });
                p.recovery_failed = false;
            });
        }
    }

    async fn recover(
        &self,
        session_id: &str,
        after: i64,
        up_to: Option<i64>,
    ) -> Result<Vec<crate::events::DurableEvent>, ()> {
        let Some(journal) = &self.journal else {
            log::warn!(
                "session {session_id}: events after session_seq {after} were missed and no journal is available to recover them"
            );
            return Ok(Vec::new());
        };
        journal
            .load_session_range(session_id, after, up_to)
            .await
            .map_err(|err| {
                log::warn!(
                    "session {session_id}: failed to recover events after session_seq {after}: {err}"
                );
            })
    }

    fn admitted(state: &SessionState, event: EventEnvelope) -> Admitted {
        let seq = event.session_seq();
        Admitted {
            guard: (seq > 0).then(|| DeliveredGuard {
                state: state.clone(),
                seq,
                release: None,
            }),
            event,
        }
    }

    /// Wait until this connection has delivered `session_seq >= tip` for the
    /// session, or until delivery is lost or `limit` elapses.
    pub(crate) async fn wait_for_tip(
        &self,
        session_id: &str,
        tip: i64,
        limit: Duration,
    ) -> WaitOutcome {
        let state = self.state(session_id);
        let mut rx = state.subscribe();
        let start_epoch = rx.borrow().release_epoch;

        let wait = async {
            rx.wait_for(|p| p.delivered >= tip || p.release_epoch > start_epoch)
                .await
                .map(|position| *position)
        };
        tokio::select! {
            result = wait => match result {
                Ok(position) if position.delivered >= tip => WaitOutcome::Delivered,
                Ok(position) => WaitOutcome::Released(
                    position.release_reason.unwrap_or(ReleaseReason::Lagged),
                ),
                Err(_) => WaitOutcome::Released(ReleaseReason::ConnectionClosed),
            },
            _ = self.closed.cancelled() => WaitOutcome::Released(ReleaseReason::ConnectionClosed),
            _ = tokio::time::sleep(limit) => WaitOutcome::Released(ReleaseReason::TimedOut),
        }
    }

    /// Hold a `session/prompt` response until this connection has delivered
    /// the updates the prompt produced (`tip` from the prompt reply), bounded
    /// by [`PROMPT_WAIT_LIMIT`]. A lost delivery is recorded as a warning and
    /// the response proceeds.
    pub(crate) async fn hold_response_for_updates(&self, session_id: &str, tip: i64) {
        if let WaitOutcome::Released(reason) =
            self.wait_for_tip(session_id, tip, PROMPT_WAIT_LIMIT).await
        {
            log::warn!(
                "session {session_id}: prompt response sent without confirming delivery of its updates (event_tip={tip}, reason={reason:?}); ordering is not guaranteed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{AgentEventKind, EphemeralEvent, EventOrigin};
    use crate::session::backend::StorageBackend;
    use crate::session::projection::NewDurableEvent;
    use crate::session::sqlite_storage::SqliteStorage;

    async fn journal() -> Arc<dyn EventJournal> {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        storage.event_journal()
    }

    async fn persist(journal: &Arc<dyn EventJournal>, session: &str, seq: i64) -> EventEnvelope {
        let stored = journal
            .append_durable(&NewDurableEvent {
                session_id: session.to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                session_seq: Some(seq),
                kind: AgentEventKind::Cancelled,
            })
            .await
            .unwrap();
        EventEnvelope::Durable(stored)
    }

    fn delta(session: &str, seq: i64) -> EventEnvelope {
        EventEnvelope::Ephemeral(EphemeralEvent {
            session_id: session.to_string(),
            session_seq: seq,
            timestamp: 0,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantContentDelta {
                content: "t".into(),
                message_id: "m".into(),
            },
        })
    }

    fn seqs(batch: &[Admitted]) -> Vec<i64> {
        batch.iter().map(|a| a.event.session_seq()).collect()
    }

    #[tokio::test]
    async fn in_order_events_pass_and_older_unrecovered_events_are_kept() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        for seq in 1..=3 {
            let batch = delivery.admit(delta("s", seq)).await;
            assert_eq!(seqs(&batch), vec![seq]);
        }
        // An older event that was never recovered (for example a backfilled
        // one) is handled, not silently dropped.
        assert_eq!(seqs(&delivery.admit(delta("s", 2)).await), vec![2]);
    }

    #[tokio::test]
    async fn unsequenced_events_are_not_tracked() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        let batch = delivery.admit(delta("s", 0)).await;
        assert_eq!(batch.len(), 1);
        assert!(batch[0].guard.is_none());
    }

    #[tokio::test]
    async fn gap_is_filled_from_the_journal_in_order() {
        let journal = journal().await;
        let events: Vec<_> = {
            let mut v = Vec::new();
            for seq in 1..=4 {
                v.push(persist(&journal, "s", seq).await);
            }
            v
        };
        let delivery = ConnectionDelivery::new(Some(journal), CancellationToken::new());
        assert_eq!(seqs(&delivery.admit(events[0].clone()).await), vec![1]);
        // 2 and 3 were missed; 4 arrives.
        let batch = delivery.admit(events[3].clone()).await;
        assert_eq!(seqs(&batch), vec![2, 3, 4]);
    }

    #[tokio::test]
    async fn gap_before_an_ephemeral_event_recovers_durable_then_delivers_it() {
        let journal = journal().await;
        persist(&journal, "s", 1).await;
        persist(&journal, "s", 3).await; // 2 was an ephemeral delta, 4 follows.
        let delivery = ConnectionDelivery::new(Some(journal), CancellationToken::new());
        delivery.admit(delta("s", 0)).await;
        // First tracked event establishes the baseline.
        assert_eq!(seqs(&delivery.admit(delta("s", 1)).await), vec![1]);
        let batch = delivery.admit(delta("s", 4)).await;
        assert_eq!(seqs(&batch), vec![3, 4]);
    }

    #[tokio::test]
    async fn lag_recovery_returns_missed_durable_events_and_later_live_ones_continue() {
        let journal = journal().await;
        let first = persist(&journal, "s", 1).await;
        let delivery = ConnectionDelivery::new(Some(journal.clone()), CancellationToken::new());
        delivery.admit(first).await;
        // Events 2 and 3 are published while the receiver lags.
        let second = persist(&journal, "s", 2).await;
        let third = persist(&journal, "s", 3).await;

        let recovered = delivery.recover_after_lag().await;
        assert_eq!(seqs(&recovered), vec![2, 3]);
        drop(recovered);
        delivery.finish_lag_recovery();

        // The lagged copies still sitting in the channel are duplicates now.
        assert!(delivery.admit(second).await.is_empty());
        assert!(delivery.admit(third).await.is_empty());
        let fourth = persist(&journal, "s", 4).await;
        assert_eq!(seqs(&delivery.admit(fourth).await), vec![4]);
    }

    #[tokio::test]
    async fn guard_advances_delivered_position_on_drop() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        let mut batch = delivery.admit(delta("s", 1)).await;
        assert_eq!(delivery.delivered("s"), 0);
        batch.pop();
        assert_eq!(delivery.delivered("s"), 1);
    }

    #[tokio::test]
    async fn wait_returns_when_the_tip_is_delivered() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        let waiter = {
            let delivery = delivery.clone();
            tokio::spawn(async move { delivery.wait_for_tip("s", 2, Duration::from_secs(5)).await })
        };
        tokio::task::yield_now().await;
        drop(delivery.admit(delta("s", 1)).await);
        assert!(!waiter.is_finished());
        drop(delivery.admit(delta("s", 2)).await);
        assert_eq!(waiter.await.unwrap(), WaitOutcome::Delivered);
    }

    #[tokio::test]
    async fn wait_is_released_on_close_timeout_and_lag() {
        let closed = CancellationToken::new();
        let delivery = ConnectionDelivery::new(None, closed.clone());

        let timed_out = delivery
            .wait_for_tip("s", 5, Duration::from_millis(20))
            .await;
        assert_eq!(timed_out, WaitOutcome::Released(ReleaseReason::TimedOut));

        let lag_waiter = {
            let delivery = delivery.clone();
            tokio::spawn(async move { delivery.wait_for_tip("s", 5, Duration::from_secs(5)).await })
        };
        tokio::task::yield_now().await;
        delivery.finish_lag_recovery();
        assert_eq!(
            lag_waiter.await.unwrap(),
            WaitOutcome::Released(ReleaseReason::Lagged)
        );

        let close_waiter = {
            let delivery = delivery.clone();
            tokio::spawn(async move { delivery.wait_for_tip("s", 5, Duration::from_secs(5)).await })
        };
        tokio::task::yield_now().await;
        closed.cancel();
        assert_eq!(
            close_waiter.await.unwrap(),
            WaitOutcome::Released(ReleaseReason::ConnectionClosed)
        );
    }

    #[tokio::test]
    async fn relay_disconnect_releases_waiters_after_it_was_delivered() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        let waiter = {
            let delivery = delivery.clone();
            tokio::spawn(async move { delivery.wait_for_tip("s", 9, Duration::from_secs(5)).await })
        };
        tokio::task::yield_now().await;
        let disconnect = EventEnvelope::Ephemeral(EphemeralEvent {
            session_id: "s".into(),
            session_seq: 0,
            timestamp: 0,
            origin: EventOrigin::Remote,
            source_node: None,
            kind: AgentEventKind::RemoteSessionDisconnected {
                message: "link died".into(),
                node_id: None,
            },
        });
        let batch = delivery.admit(disconnect).await;
        // Not released while the notification is still being handled.
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(batch);
        assert_eq!(
            waiter.await.unwrap(),
            WaitOutcome::Released(ReleaseReason::RelayDisconnected)
        );
    }

    #[tokio::test]
    async fn a_slow_connection_does_not_delay_another_connections_wait() {
        let fast = ConnectionDelivery::new(None, CancellationToken::new());
        let slow = ConnectionDelivery::new(None, CancellationToken::new());
        // Both connections admit event 1; only the fast one finishes handling.
        let slow_batch = slow.admit(delta("s", 1)).await;
        drop(fast.admit(delta("s", 1)).await);

        let outcome = fast.wait_for_tip("s", 1, Duration::from_secs(1)).await;
        assert_eq!(outcome, WaitOutcome::Delivered);
        let slow_outcome = slow.wait_for_tip("s", 1, Duration::from_millis(20)).await;
        assert_eq!(slow_outcome, WaitOutcome::Released(ReleaseReason::TimedOut));
        drop(slow_batch);
    }

    #[test]
    fn event_tip_round_trips_through_prompt_response_meta() {
        use crate::acp::protocol::StopReason;
        let response = with_event_tip(PromptResponse::new(StopReason::EndTurn), 42);
        let wire = serde_json::to_value(&response).unwrap();
        assert_eq!(wire["_meta"]["querymt"]["eventTip"], 42);

        let mut received: PromptResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(take_event_tip(&mut received), Some(42));
        assert!(
            received.meta.is_none(),
            "the tip is consumed, wire is clean"
        );
        assert_eq!(take_event_tip(&mut received), None);

        // No events: nothing attached; an old node's reply has no tip.
        let none = with_event_tip(PromptResponse::new(StopReason::EndTurn), 0);
        assert!(none.meta.is_none());
    }

    #[tokio::test]
    async fn failed_recovery_releases_waiters_with_recovery_failed() {
        struct Broken;
        #[async_trait::async_trait]
        impl EventJournal for Broken {
            async fn append_durable(
                &self,
                _: &NewDurableEvent,
            ) -> crate::session::error::SessionResult<crate::events::DurableEvent> {
                unimplemented!()
            }
            async fn load_session_stream(
                &self,
                _: &str,
                _: Option<i64>,
                _: Option<usize>,
            ) -> crate::session::error::SessionResult<Vec<crate::events::DurableEvent>>
            {
                Err(crate::session::error::SessionError::Other("boom".into()))
            }
            async fn load_global_stream(
                &self,
                _: Option<i64>,
                _: Option<usize>,
            ) -> crate::session::error::SessionResult<Vec<crate::events::DurableEvent>>
            {
                unimplemented!()
            }
            async fn delete_session_events_from(
                &self,
                _: &str,
                _: i64,
            ) -> crate::session::error::SessionResult<usize> {
                unimplemented!()
            }
            async fn append_durable_from_source(
                &self,
                _: &NewDurableEvent,
            ) -> crate::session::error::SessionResult<Option<crate::events::DurableEvent>>
            {
                unimplemented!()
            }
            async fn latest_source_seq(
                &self,
                _: &str,
                _: &str,
            ) -> crate::session::error::SessionResult<Option<i64>> {
                unimplemented!()
            }
            async fn remote_sync_cursor(
                &self,
                _: &str,
                _: &str,
            ) -> crate::session::error::SessionResult<Option<i64>> {
                unimplemented!()
            }
            async fn advance_remote_sync_cursor(
                &self,
                _: &str,
                _: &str,
                _: i64,
                _: bool,
            ) -> crate::session::error::SessionResult<()> {
                unimplemented!()
            }
            async fn load_remote_session_stream(
                &self,
                _: &str,
                _: &str,
            ) -> crate::session::error::SessionResult<Vec<crate::events::DurableEvent>>
            {
                unimplemented!()
            }
            async fn max_stream_seq(&self, _: &str) -> crate::session::error::SessionResult<i64> {
                unimplemented!()
            }
        }

        let delivery = ConnectionDelivery::new(Some(Arc::new(Broken)), CancellationToken::new());
        drop(delivery.admit(delta("s", 1)).await);
        let waiter = {
            let delivery = delivery.clone();
            tokio::spawn(async move { delivery.wait_for_tip("s", 9, Duration::from_secs(5)).await })
        };
        tokio::task::yield_now().await;
        assert!(delivery.recover_after_lag().await.is_empty());
        delivery.finish_lag_recovery();
        assert_eq!(
            waiter.await.unwrap(),
            WaitOutcome::Released(ReleaseReason::RecoveryFailed)
        );
    }
}

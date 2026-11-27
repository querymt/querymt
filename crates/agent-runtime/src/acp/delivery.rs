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
use crate::acp::snapshot_view::LoadSnapshotView;
use crate::session::projection::EventJournal;
use querymt_agent_core::events::{AgentEventKind, EventEnvelope};
use querymt_agent_services::event_fanout::{InflightText, LIVE_EVENT_CAPACITY};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, watch};
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

/// `_meta.querymt.messageId` key: the id of the user message a prompt inserted.
/// Unlike the event tip it stays on the response the client receives.
pub const MESSAGE_ID_META_KEY: &str = "messageId";

/// Attach the id of the user message the prompt inserted to a prompt reply.
pub(crate) fn with_message_id(mut response: PromptResponse, message_id: &str) -> PromptResponse {
    let meta = response.meta.get_or_insert_with(Meta::new);
    let namespace = meta
        .entry(crate::acp::shared::QUERYMT_META_NAMESPACE.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(namespace) = namespace.as_object_mut() {
        namespace.insert(
            MESSAGE_ID_META_KEY.to_string(),
            serde_json::Value::String(message_id.to_owned()),
        );
    }
    response
}

/// Remove and return the event tip of a prompt reply. A missing (or malformed)
/// tip means "no watermark": the response is not held back.
#[doc(hidden)]
pub fn take_event_tip(response: &mut PromptResponse) -> Option<i64> {
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
    // Once the baseline advances past a missing update, this connection
    // cannot prove delivery through that gap, even for a later waiter.
    first_missing: Option<i64>,
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
#[doc(hidden)]
pub struct Admitted {
    pub event: std::sync::Arc<EventEnvelope>,
    pub guard: Option<DeliveredGuard>,
}

/// Advances the delivered position of its session when dropped, and releases
/// waiters when the handled event announces lost delivery.
#[doc(hidden)]
pub struct DeliveredGuard {
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

/// Where a session load's answer stands in the session's event order: the
/// last event the answer contains.
///
/// Events are compared by `session_seq`, which durable and ephemeral events
/// of a session share, so one number cuts both kinds. For events without one,
/// durable events fall back to the connection-local stream sequence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadCut {
    /// Highest `session_seq` the load's snapshot or replay contains (`0` when unknown).
    pub session_seq: i64,
    /// Highest local `stream_seq` it contains (`0` when unknown).
    pub local_stream_seq: i64,
}

impl LoadCut {
    /// The cut of a load answered with a snapshot: its stream cursor and the
    /// newest event it contains. `None` when the response carries no snapshot.
    ///
    /// A connection that also seeds its translator from the snapshot reads
    /// both from one [`LoadSnapshotView`] instead.
    pub fn from_snapshot_meta(meta: Option<&Meta>) -> Option<Self> {
        LoadSnapshotView::from_meta(meta).map(|snapshot| snapshot.cut)
    }

    /// The cut of a load answered by replaying `events`.
    pub fn from_events(events: &[querymt_agent_core::events::AgentEvent]) -> Self {
        let mut cursor = querymt_agent_core::stream_cursor::StreamCursor::default();
        let mut session_seq = 0;
        for event in events {
            querymt_agent_core::stream_cursor::observe_cursor(&mut cursor, event);
            session_seq = session_seq.max(event.session_seq);
        }
        Self {
            session_seq,
            local_stream_seq: cursor.local_seq,
        }
    }

    /// True when the load's answer already contains `event`.
    fn covers(&self, event: &EventEnvelope) -> bool {
        let seq = event.session_seq();
        if seq > 0 && self.session_seq > 0 {
            return seq <= self.session_seq;
        }
        matches!(event, EventEnvelope::Durable(_))
            && !matches!(
                event.origin(),
                querymt_agent_core::events::EventOrigin::Remote
            )
            && event.seq() > 0
            && event.seq() <= self.local_stream_seq
    }
}

/// What a finished load makes redundant for the rest of the connection: the
/// events its answer contains and the deltas folded into its in-flight text.
#[derive(Debug, Clone, Copy)]
struct LoadFloor {
    cut: LoadCut,
    /// Streamed deltas at or before this `session_seq` are part of the
    /// in-flight chunk the connection received.
    inflight_through: i64,
}

impl LoadFloor {
    fn drops(&self, event: &EventEnvelope) -> bool {
        if self.cut.covers(event) {
            return true;
        }
        let seq = event.session_seq();
        // Only streamed text deltas are folded into the in-flight chunk;
        // every other ephemeral event is still owed to the connection.
        matches!(event, EventEnvelope::Ephemeral(_))
            && matches!(
                event.kind(),
                querymt_agent_core::events::AgentEventKind::AssistantContentDelta { .. }
                    | querymt_agent_core::events::AgentEventKind::AssistantThinkingDelta { .. }
            )
            && seq > 0
            && seq <= self.inflight_through
    }
}

/// Live events of one session held back while a load is in progress.
struct Hold {
    id: u64,
    events: VecDeque<Admitted>,
    /// More events arrived than the buffer bounds.
    overflowed: bool,
    /// Set when the load answered: the events are ready to be delivered.
    release: Option<Release>,
}

struct Release {
    inflight: Option<InflightText>,
}

/// What a forwarder delivers after a load answered: the in-flight text, then
/// the held events the answer does not contain, in order.
#[doc(hidden)]
pub struct ReleasedLoad {
    pub session_id: String,
    pub inflight: Option<InflightText>,
    pub events: Vec<Admitted>,
}

#[derive(Default)]
struct LoadState {
    holds: HashMap<String, Hold>,
    floors: HashMap<String, LoadFloor>,
    next_hold: u64,
}

/// The load held more live events than a connection can buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldOverflow;

/// A session load in progress on one connection. Dropping it without
/// [`LoadHold::release`] abandons the held events.
#[doc(hidden)]
pub struct LoadHold {
    delivery: Arc<ConnectionDelivery>,
    session_id: String,
    id: u64,
    released: bool,
}

impl LoadHold {
    /// Fails when live events overflowed the hold buffer: the load must then
    /// fail instead of answering with a gap.
    pub fn check(&self) -> Result<(), HoldOverflow> {
        let state = self.delivery.load.lock().unwrap_or_else(|p| p.into_inner());
        match state.holds.get(&self.session_id) {
            Some(hold) if hold.id == self.id && hold.overflowed => Err(HoldOverflow),
            _ => Ok(()),
        }
    }

    /// The load has answered: read the text still streaming at this point
    /// and deliver it, then the held events the answer does not contain.
    pub fn release_with(
        self,
        cut: Option<LoadCut>,
        fanout: &querymt_agent_services::event_fanout::EventFanout,
    ) {
        let inflight = fanout.inflight_text(&self.session_id);
        self.release(cut.unwrap_or_default(), inflight);
    }

    /// [`Self::release_with`] with the in-flight text already read.
    pub fn release(mut self, cut: LoadCut, inflight: Option<InflightText>) {
        self.released = true;
        let through = inflight.as_ref().map_or(0, |text| text.through_seq);
        {
            let mut state = self.delivery.load.lock().unwrap_or_else(|p| p.into_inner());
            let floor = LoadFloor {
                cut,
                inflight_through: through,
            };
            state.floors.insert(self.session_id.clone(), floor);
            match state.holds.get_mut(&self.session_id) {
                Some(hold) if hold.id == self.id => {
                    hold.events.retain(|admitted| !floor.drops(&admitted.event));
                    hold.release = Some(Release { inflight });
                }
                _ => return,
            }
        }
        self.delivery.released.notify_one();
    }
}

impl Drop for LoadHold {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let mut state = self.delivery.load.lock().unwrap_or_else(|p| p.into_inner());
        if state
            .holds
            .get(&self.session_id)
            .is_some_and(|h| h.id == self.id)
        {
            state.holds.remove(&self.session_id);
        }
    }
}

/// Delivery positions of one client connection.
#[doc(hidden)]
pub struct ConnectionDelivery {
    journal: Option<Arc<dyn EventJournal>>,
    sessions: Mutex<HashMap<String, SessionState>>,
    closed: CancellationToken,
    /// Session loads in progress and what finished loads made redundant.
    load: Mutex<LoadState>,
    /// Wakes the forwarder when a load released its held events.
    released: Notify,
    /// Most live events one load may hold.
    hold_limit: usize,
}

impl ConnectionDelivery {
    /// `journal` is used to recover missed durable events; without one, lag
    /// and gaps are logged and skipped. `closed` is the connection's
    /// cancellation token and releases every waiter.
    #[doc(hidden)]
    pub fn new(journal: Option<Arc<dyn EventJournal>>, closed: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            journal,
            sessions: Mutex::new(HashMap::new()),
            closed,
            load: Mutex::new(LoadState::default()),
            released: Notify::new(),
            hold_limit: LIVE_EVENT_CAPACITY,
        })
    }

    /// Start holding the live events of `session_id`, before the load reads
    /// its snapshot or replay. Events produced from now on wait in a bounded
    /// buffer until [`LoadHold::release`], so none is delivered before the
    /// load answered and none is missed between the read and the answer.
    #[doc(hidden)]
    pub fn begin_load(self: &Arc<Self>, session_id: &str) -> LoadHold {
        let mut state = self.load.lock().unwrap_or_else(|p| p.into_inner());
        state.next_hold += 1;
        let id = state.next_hold;
        // A newer load of the same session takes over what an older one held.
        let events = state
            .holds
            .remove(session_id)
            .map(|old| old.events)
            .unwrap_or_default();
        state.holds.insert(
            session_id.to_owned(),
            Hold {
                id,
                events,
                overflowed: false,
                release: None,
            },
        );
        // The previous load's floor no longer applies: this one reads afresh.
        state.floors.remove(session_id);
        LoadHold {
            delivery: self.clone(),
            session_id: session_id.to_owned(),
            id,
            released: false,
        }
    }

    /// Route one admitted event: `None` when it was held for a load in
    /// progress or the finished load's answer already contains it, the event
    /// otherwise. Dropping an event advances its delivered position.
    #[doc(hidden)]
    pub fn intercept(&self, admitted: Admitted) -> Option<Admitted> {
        let mut state = self.load.lock().unwrap_or_else(|p| p.into_inner());
        if state.holds.is_empty() && state.floors.is_empty() {
            return Some(admitted);
        }
        let session_id = admitted.event.session_id().to_owned();
        // Events that arrive after the answer are subject to what it contained.
        if state
            .floors
            .get(&session_id)
            .is_some_and(|floor| floor.drops(&admitted.event))
        {
            return None;
        }
        if let Some(hold) = state.holds.get_mut(&session_id) {
            // Once the answer is out the buffer must keep every event, or the
            // delivery order would break; the bound only protects the wait.
            if hold.release.is_none() && hold.events.len() >= self.hold_limit {
                hold.overflowed = true;
                return None;
            }
            hold.events.push_back(admitted);
            return None;
        }
        Some(admitted)
    }

    /// Wait until a load released held events.
    #[doc(hidden)]
    pub async fn released_load(&self) {
        self.released.notified().await;
    }

    /// Take the loads whose answer is out, with the events to deliver. The
    /// hold ends here, so later events pass straight through: the forwarder
    /// delivers the returned ones first.
    #[doc(hidden)]
    pub fn take_released(&self) -> Vec<ReleasedLoad> {
        let mut state = self.load.lock().unwrap_or_else(|p| p.into_inner());
        let ready: Vec<String> = state
            .holds
            .iter()
            .filter(|(_, hold)| hold.release.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        ready
            .into_iter()
            .filter_map(|session_id| {
                let hold = state.holds.remove(&session_id)?;
                let release = hold.release?;
                Some(ReleasedLoad {
                    session_id,
                    inflight: release.inflight,
                    events: hold.events.into(),
                })
            })
            .collect()
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
    #[doc(hidden)]
    pub async fn admit(&self, event: std::sync::Arc<EventEnvelope>) -> Vec<Admitted> {
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
                state.send_modify(|p| {
                    p.first_missing.get_or_insert(position.admitted + 1);
                });
                release(&state, ReleaseReason::RecoveryFailed);
                Vec::new()
            }
        };
        let mut out: Vec<Admitted> = Vec::with_capacity(recovered.len() + 1);
        let mut supplied = false;
        let mut highest = position.admitted;
        for durable in recovered {
            if durable.session_seq > highest + 1 {
                state.send_modify(|p| {
                    p.first_missing.get_or_insert(highest + 1);
                });
            }
            highest = highest.max(durable.session_seq);
            supplied |= durable.session_seq == seq;
            out.push(Self::admitted(
                &state,
                std::sync::Arc::new(EventEnvelope::Durable(durable)),
            ));
        }
        if !supplied {
            out.push(Self::admitted(&state, event));
        }
        state.send_modify(|p| {
            if highest < seq - 1 {
                p.first_missing.get_or_insert(highest + 1);
            }
            p.admitted = seq;
        });
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
    #[doc(hidden)]
    pub async fn recover_after_lag(&self) -> Vec<Admitted> {
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
                        if durable.session_seq > highest + 1 {
                            state.send_modify(|p| {
                                p.first_missing.get_or_insert(highest + 1);
                            });
                        }
                        highest = highest.max(durable.session_seq);
                        out.push(Self::admitted(
                            &state,
                            std::sync::Arc::new(EventEnvelope::Durable(durable)),
                        ));
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
                // A failed read does not prove that this session missed a sequence.
                // Release current waiters, but let admission detect actual gaps.
                Err(()) => state.send_modify(|p| p.recovery_failed = true),
            }
        }
        out
    }

    /// Record that lag handling completed: waiters whose watermark is still
    /// ahead of the delivered position are released, because the remainder
    /// may be ephemeral events that cannot be recovered.
    #[doc(hidden)]
    pub fn finish_lag_recovery(&self) {
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
    ) -> Result<Vec<querymt_agent_core::events::DurableEvent>, ()> {
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

    fn admitted(state: &SessionState, event: std::sync::Arc<EventEnvelope>) -> Admitted {
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
            rx.wait_for(|p| {
                p.delivered >= tip
                    || p.release_epoch > start_epoch
                    || p.first_missing.is_some_and(|seq| seq <= tip)
            })
            .await
            .map(|position| *position)
        };
        tokio::select! {
            result = wait => match result {
                Ok(position) if position.first_missing.is_some_and(|seq| seq <= tip) =>
                    WaitOutcome::Released(ReleaseReason::RecoveryFailed),
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
    use crate::session::backend::StorageBackend;
    use crate::session::projection::NewDurableEvent;
    use crate::session::sqlite_storage::SqliteStorage;
    use querymt_agent_core::events::{AgentEventKind, EphemeralEvent, EventOrigin};

    async fn journal() -> Arc<dyn EventJournal> {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        storage.event_journal()
    }

    async fn persist(
        journal: &Arc<dyn EventJournal>,
        session: &str,
        seq: i64,
    ) -> Arc<EventEnvelope> {
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
        Arc::new(EventEnvelope::Durable(stored))
    }

    #[test]
    fn inflight_floor_drops_only_streamed_deltas() {
        let floor = LoadFloor {
            cut: LoadCut::default(),
            inflight_through: 10,
        };
        let ephemeral = |seq, kind| {
            EventEnvelope::Ephemeral(EphemeralEvent {
                session_id: "s".into(),
                session_seq: seq,
                timestamp: 0,
                origin: EventOrigin::Local,
                source_node: None,
                kind,
            })
        };
        assert!(floor.drops(&delta("s", 9)));
        assert!(!floor.drops(&delta("s", 11)));
        let progress = ephemeral(
            9,
            AgentEventKind::WorktreeCommandOutput {
                code: None,
                slash_command: None,
                text: "out".into(),
            },
        );
        assert!(
            !floor.drops(&progress),
            "not folded into the in-flight text"
        );
    }

    fn delta(session: &str, seq: i64) -> Arc<EventEnvelope> {
        Arc::new(EventEnvelope::Ephemeral(EphemeralEvent {
            session_id: session.to_string(),
            session_seq: seq,
            timestamp: 0,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantContentDelta {
                content: "t".into(),
                message_id: "m".into(),
            },
        }))
    }

    fn seqs(batch: &[Admitted]) -> Vec<i64> {
        batch.iter().map(|a| a.event.session_seq()).collect()
    }

    fn durable(session: &str, seq: i64) -> Arc<EventEnvelope> {
        Arc::new(EventEnvelope::Durable(
            querymt_agent_core::events::DurableEvent {
                session_seq: seq,
                event_id: format!("e{seq}"),
                stream_seq: seq,
                session_id: session.to_string(),
                timestamp: 0,
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::Cancelled,
            },
        ))
    }

    /// Admit and route events the way a forwarder does; what passes is returned.
    async fn route(delivery: &ConnectionDelivery, events: Vec<Arc<EventEnvelope>>) -> Vec<i64> {
        let mut passed = Vec::new();
        for event in events {
            for admitted in delivery.admit(event).await {
                if let Some(admitted) = delivery.intercept(admitted) {
                    passed.push(admitted.event.session_seq());
                }
            }
        }
        passed
    }

    fn cut(seq: i64) -> LoadCut {
        LoadCut {
            session_seq: seq,
            local_stream_seq: seq,
        }
    }

    #[tokio::test]
    async fn event_during_the_load_is_delivered_once() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        let hold = delivery.begin_load("s");
        // 1..=3 are in the snapshot; 4 and 5 are produced while the load runs.
        let passed = route(&delivery, (1..=5).map(|seq| durable("s", seq)).collect()).await;
        assert!(passed.is_empty(), "nothing is delivered during the load");
        hold.check().unwrap();
        hold.release(cut(3), None);

        let released = delivery.take_released();
        assert_eq!(released.len(), 1);
        assert_eq!(seqs(&released[0].events), vec![4, 5]);
        assert!(delivery.take_released().is_empty(), "released once");
        // Later events flow straight through.
        assert_eq!(route(&delivery, vec![durable("s", 6)]).await, vec![6]);
    }

    #[tokio::test]
    async fn event_before_the_cut_point_is_not_delivered_again() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        let hold = delivery.begin_load("s");
        route(&delivery, vec![durable("s", 1), durable("s", 2)]).await;
        hold.release(cut(2), None);
        assert!(delivery.take_released()[0].events.is_empty());
        // A copy of an event the snapshot contains is still in the channel.
        let passed = route(&delivery, vec![durable("s", 2), durable("s", 3)]).await;
        assert_eq!(passed, vec![3]);
    }

    #[tokio::test]
    async fn events_after_the_answer_keep_their_order_behind_the_held_ones() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        let hold = delivery.begin_load("s");
        route(&delivery, vec![durable("s", 1), durable("s", 2)]).await;
        hold.release(cut(1), None);
        // Released but not yet taken by the forwarder: still behind the buffer.
        let passed = route(&delivery, vec![durable("s", 3)]).await;
        assert!(passed.is_empty());
        let released = delivery.take_released();
        assert_eq!(seqs(&released[0].events), vec![2, 3]);
    }

    #[tokio::test]
    async fn other_sessions_are_not_held() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        let _hold = delivery.begin_load("s");
        assert_eq!(route(&delivery, vec![durable("other", 1)]).await, vec![1]);
    }

    #[tokio::test]
    async fn streamed_deltas_in_the_inflight_text_are_not_sent_again() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        let hold = delivery.begin_load("s");
        // Deltas 3 and 4 streamed before the accumulator was read, delta 5 after.
        route(&delivery, vec![delta("s", 3), delta("s", 4)]).await;
        let inflight = InflightText {
            through_seq: 4,
            messages: Vec::new(),
        };
        hold.release(cut(2), Some(inflight));
        // A delta the accumulator already contains arrives late; 5 is new.
        let passed = route(&delivery, vec![delta("s", 4), delta("s", 5)]).await;
        assert_eq!(passed, Vec::<i64>::new(), "still behind the held buffer");
        let released = delivery.take_released();
        assert_eq!(seqs(&released[0].events), vec![5]);
        assert_eq!(released[0].inflight.as_ref().unwrap().through_seq, 4);
    }

    #[tokio::test]
    async fn too_many_held_events_fail_the_load() {
        let mut delivery = ConnectionDelivery::new(None, CancellationToken::new());
        Arc::get_mut(&mut delivery).unwrap().hold_limit = 2;
        let hold = delivery.begin_load("s");
        route(&delivery, (1..=3).map(|seq| durable("s", seq)).collect()).await;
        assert_eq!(hold.check(), Err(HoldOverflow));
    }

    #[tokio::test]
    async fn an_abandoned_load_stops_holding() {
        let delivery = ConnectionDelivery::new(None, CancellationToken::new());
        drop(delivery.begin_load("s"));
        assert_eq!(route(&delivery, vec![durable("s", 1)]).await, vec![1]);
    }

    #[test]
    fn cut_comes_from_a_snapshot_or_replayed_events() {
        let meta: Meta = serde_json::from_value(serde_json::json!({
            "querymt/sessionLoadSnapshot.v1": {
                "cursor": {"local_seq": 40, "remote_seq_by_source": {}},
                "audit": {"events": [{"session_seq": 7}, {"session_seq": 9}]},
            }
        }))
        .unwrap();
        assert_eq!(
            LoadCut::from_snapshot_meta(Some(&meta)),
            Some(LoadCut {
                session_seq: 9,
                local_stream_seq: 40
            })
        );
        assert_eq!(LoadCut::from_snapshot_meta(Some(&Meta::new())), None);
        assert_eq!(LoadCut::from_snapshot_meta(None), None);
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
        let batch = delivery.admit(Arc::new(disconnect)).await;
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
            ) -> crate::session::error::SessionResult<querymt_agent_core::events::DurableEvent>
            {
                unimplemented!()
            }
            async fn load_session_stream(
                &self,
                _: &str,
                _: Option<i64>,
                _: Option<usize>,
            ) -> crate::session::error::SessionResult<Vec<querymt_agent_core::events::DurableEvent>>
            {
                Err(crate::session::error::SessionError::Other("boom".into()))
            }
            async fn load_global_stream(
                &self,
                _: Option<i64>,
                _: Option<usize>,
            ) -> crate::session::error::SessionResult<Vec<querymt_agent_core::events::DurableEvent>>
            {
                unimplemented!()
            }
            async fn delete_session_events_range(
                &self,
                _: &str,
                _: i64,
                _: Option<i64>,
            ) -> crate::session::error::SessionResult<usize> {
                unimplemented!()
            }
            async fn append_durable_from_source(
                &self,
                _: &NewDurableEvent,
            ) -> crate::session::error::SessionResult<
                Option<querymt_agent_core::events::DurableEvent>,
            > {
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
            ) -> crate::session::error::SessionResult<Vec<querymt_agent_core::events::DurableEvent>>
            {
                unimplemented!()
            }
            async fn max_stream_seq(&self, _: &str) -> crate::session::error::SessionResult<i64> {
                unimplemented!()
            }
        }

        let delivery = ConnectionDelivery::new(Some(Arc::new(Broken)), CancellationToken::new());
        drop(delivery.admit(delta("gap", 1)).await);
        assert!(delivery.recover_after_lag().await.is_empty());
        delivery.finish_lag_recovery();
        drop(delivery.admit(delta("gap", 3)).await);
        assert_eq!(delivery.delivered("gap"), 3);
        assert_eq!(
            delivery.wait_for_tip("gap", 3, Duration::ZERO).await,
            WaitOutcome::Released(ReleaseReason::RecoveryFailed)
        );
        assert_eq!(
            delivery.wait_for_tip("gap", 1, Duration::ZERO).await,
            WaitOutcome::Delivered
        );

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
        drop(delivery.admit(delta("s", 2)).await);
        assert_eq!(
            delivery.wait_for_tip("s", 2, Duration::ZERO).await,
            WaitOutcome::Delivered,
            "a recovery failure without a sequence gap must not poison later waits"
        );
    }
}

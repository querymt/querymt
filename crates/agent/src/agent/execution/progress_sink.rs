//! Throttled, close-guarded progress sink for one MCP tool call.
//!
//! At most one update is emitted per [`THROTTLE`]. An update arriving inside
//! the window replaces the pending one and arms a single trailing timer, so
//! the latest report is still delivered. Emission and closing happen under
//! one lock, and the [`SinkCloseGuard`] closes the sink when the call ends or
//! is dropped (cancellation). Hence nothing is emitted after the guard drops,
//! and `ToolCallEnd` (sent only after the call returns) is always last.

use parking_lot::Mutex;
use querymt::mcp::progress::{ProgressSink, ProgressUpdate};
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

/// Minimum spacing between two progress events of one tool call.
pub(super) const THROTTLE: Duration = Duration::from_millis(250);

type Emit = dyn Fn(ProgressUpdate) + Send + Sync;

#[derive(Default)]
struct State {
    closed: bool,
    last_emit: Option<Instant>,
    pending: Option<ProgressUpdate>,
    timer_armed: bool,
}

struct Inner {
    state: Mutex<State>,
    emit: Box<Emit>,
    throttle: Duration,
}

/// Cheap-to-clone sink; see the module docs.
///
/// `emit` runs while the state lock is held (that is what orders it against
/// closing), so it must not block or take a lock that a caller of `report`
/// may hold. The agent's emitter only publishes to the event stream.
#[derive(Clone)]
pub(super) struct AgentProgressSink(Arc<Inner>);

/// Closes the sink and discards any pending update on drop.
#[must_use = "dropping the guard closes the sink"]
pub(super) struct SinkCloseGuard(Arc<Inner>);

impl AgentProgressSink {
    pub(super) fn new(
        throttle: Duration,
        emit: impl Fn(ProgressUpdate) + Send + Sync + 'static,
    ) -> (Self, SinkCloseGuard) {
        let inner = Arc::new(Inner {
            state: Mutex::default(),
            emit: Box::new(emit),
            throttle,
        });
        (Self(Arc::clone(&inner)), SinkCloseGuard(inner))
    }
}

impl Drop for SinkCloseGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        state.closed = true;
        state.pending = None;
    }
}

impl ProgressSink for AgentProgressSink {
    fn report(&self, update: ProgressUpdate) {
        let inner = &self.0;
        let mut state = inner.state.lock();
        if state.closed {
            return;
        }
        let now = Instant::now();
        let window_open = state
            .last_emit
            .is_none_or(|last| now.duration_since(last) >= inner.throttle);
        if window_open && !state.timer_armed {
            state.last_emit = Some(now);
            (inner.emit)(update);
            return;
        }
        state.pending = Some(update);
        if state.timer_armed {
            return;
        }
        // Not in a runtime (never the case for tool calls): keep the update
        // pending; the next report in an open window flushes it.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        state.timer_armed = true;
        let wake_at = state.last_emit.map_or(now, |last| last + inner.throttle);
        drop(state);
        let inner = Arc::clone(inner);
        runtime.spawn(async move {
            tokio::time::sleep_until(wake_at).await;
            let mut state = inner.state.lock();
            state.timer_armed = false;
            if state.closed {
                return;
            }
            if let Some(update) = state.pending.take() {
                state.last_emit = Some(Instant::now());
                (inner.emit)(update);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upd(n: u32) -> ProgressUpdate {
        ProgressUpdate {
            progress: Some(f64::from(n)),
            ..ProgressUpdate::default()
        }
    }

    fn collector() -> (Arc<Mutex<Vec<f64>>>, impl Fn(ProgressUpdate) + Send + Sync) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (seen, move |u: ProgressUpdate| {
            sink.lock().push(u.progress.unwrap())
        })
    }

    #[tokio::test(start_paused = true)]
    async fn burst_emits_first_then_latest_after_window() {
        let (seen, emit) = collector();
        let (sink, _guard) = AgentProgressSink::new(THROTTLE, emit);
        for n in 1..=20 {
            sink.report(upd(n));
            tokio::time::advance(Duration::from_millis(5)).await;
        }
        assert_eq!(
            *seen.lock(),
            [1.0],
            "only the first emits inside the window"
        );
        tokio::time::advance(THROTTLE).await;
        tokio::task::yield_now().await;
        assert_eq!(*seen.lock(), [1.0, 20.0], "trailing timer emits the latest");
    }

    #[tokio::test(start_paused = true)]
    async fn spaced_updates_all_emit() {
        let (seen, emit) = collector();
        let (sink, _guard) = AgentProgressSink::new(THROTTLE, emit);
        for n in 1..=3 {
            sink.report(upd(n));
            tokio::time::advance(THROTTLE).await;
        }
        assert_eq!(*seen.lock(), [1.0, 2.0, 3.0]);
    }

    #[tokio::test(start_paused = true)]
    async fn closing_discards_pending_and_blocks_later_reports() {
        let (seen, emit) = collector();
        let (sink, guard) = AgentProgressSink::new(THROTTLE, emit);
        sink.report(upd(1));
        sink.report(upd(2)); // pending, timer armed
        drop(guard);
        tokio::time::advance(THROTTLE * 2).await;
        tokio::task::yield_now().await;
        sink.report(upd(3));
        tokio::time::advance(THROTTLE * 2).await;
        tokio::task::yield_now().await;
        assert_eq!(*seen.lock(), [1.0]);
    }
}

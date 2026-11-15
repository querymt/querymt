//! Load/attach telemetry: how many history requests crossed the mesh and how
//! large the resulting snapshot was. Process-wide counters; a load logs the
//! delta between its start and end, so concurrent loads over-count (baseline
//! scenarios open one session at a time).

use std::sync::atomic::{AtomicU64, Ordering};

static MESH_HISTORY_REQUESTS: AtomicU64 = AtomicU64::new(0);
static MESH_HISTORY_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Record one remote history request (`GetEventStreamSince` or
/// `GetEventStreamChunk`) that returned `events` events.
pub(crate) fn record_mesh_history_request(events: usize) {
    MESH_HISTORY_REQUESTS.fetch_add(1, Ordering::Relaxed);
    MESH_HISTORY_EVENTS.fetch_add(events as u64, Ordering::Relaxed);
}

/// Counter values at the start of a load.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LoadProbe {
    requests: u64,
    events: u64,
    started: std::time::Instant,
}

impl LoadProbe {
    pub(crate) fn start() -> Self {
        Self {
            requests: MESH_HISTORY_REQUESTS.load(Ordering::Relaxed),
            events: MESH_HISTORY_EVENTS.load(Ordering::Relaxed),
            started: std::time::Instant::now(),
        }
    }

    /// `(mesh requests, mesh events)` since the probe started.
    pub(crate) fn mesh_delta(&self) -> (u64, u64) {
        (
            MESH_HISTORY_REQUESTS
                .load(Ordering::Relaxed)
                .saturating_sub(self.requests),
            MESH_HISTORY_EVENTS
                .load(Ordering::Relaxed)
                .saturating_sub(self.events),
        )
    }

    /// Log one finished load or attach.
    pub(crate) fn log_finished(&self, kind: &str, session_id: &str, snapshot: &serde_json::Value) {
        let (mesh_requests, mesh_events) = self.mesh_delta();
        let snapshot_bytes = serde_json::to_vec(snapshot).map(|b| b.len()).unwrap_or(0);
        let snapshot_events = snapshot
            .pointer("/audit/events")
            .and_then(|events| events.as_array())
            .map_or(0, Vec::len);
        log::info!(
            "session load telemetry: kind={kind}, session_id={session_id}, \
             snapshot_bytes={snapshot_bytes}, snapshot_events={snapshot_events}, \
             mesh_requests={mesh_requests}, mesh_events={mesh_events}, elapsed_ms={}",
            self.started.elapsed().as_millis()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reports_the_delta_since_start() {
        let probe = LoadProbe::start();
        record_mesh_history_request(200);
        record_mesh_history_request(50);
        let (requests, events) = probe.mesh_delta();
        // Other tests may record concurrently, so only lower bounds hold.
        assert!(requests >= 2);
        assert!(events >= 250);
    }
}

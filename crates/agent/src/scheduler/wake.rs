//! Deadline queue and event accumulator logic for `SchedulerActor`.
//!
//! The deadline queue is a min-heap of `(OffsetDateTime, schedule_public_id)`
//! pairs. A single `tokio::time::sleep_until` targets the nearest deadline.
//!
//! The event accumulator tracks per-schedule event counts with debounce.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use time::OffsetDateTime;
use tokio::task::JoinHandle;

/// Entry in the deadline queue: `(next_run_at, schedule_public_id)`.
///
/// Uses `Reverse` so the earliest deadline is at the top of the max-heap,
/// making it a min-heap.
pub(crate) type DeadlineEntry = Reverse<(i64, String)>;

/// Min-heap of `(unix_timestamp_nanos, schedule_public_id)`.
///
/// We store `i64` unix-timestamp-nanos instead of `OffsetDateTime` because
/// `OffsetDateTime` does not implement `Ord` (only `PartialOrd`).
#[derive(Debug, Default)]
pub(crate) struct DeadlineQueue {
    heap: BinaryHeap<DeadlineEntry>,
}

impl DeadlineQueue {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
        }
    }

    /// Insert a deadline for a schedule.
    pub fn insert(&mut self, next_run_at: OffsetDateTime, schedule_public_id: String) {
        let nanos = next_run_at.unix_timestamp_nanos() as i64;
        self.heap.push(Reverse((nanos, schedule_public_id)));
    }

    /// Peek at the earliest deadline without removing it.
    pub fn peek(&self) -> Option<(OffsetDateTime, &str)> {
        self.heap.peek().map(|Reverse((nanos, id))| {
            let dt = OffsetDateTime::from_unix_timestamp_nanos(*nanos as i128)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH);
            (dt, id.as_str())
        })
    }

    /// Remove all entries for a given schedule public ID.
    ///
    /// This is O(n) but acceptable since the number of active schedules is
    /// expected to be small (dozens, not thousands).
    pub fn remove(&mut self, schedule_public_id: &str) {
        let old_heap = std::mem::take(&mut self.heap);
        self.heap = old_heap
            .into_iter()
            .filter(|Reverse((_, id))| id != schedule_public_id)
            .collect();
    }

    /// Number of entries in the queue.
    pub fn len(&self) -> usize {
        self.heap.len()
    }
}

/// Per-schedule event accumulator for event-driven triggers.
///
/// Tracks how many matching events have been received and whether the
/// debounce window is active.
#[derive(Debug)]
pub(crate) struct EventAccumulator {
    /// Number of matching events received since last reset.
    pub count: u32,
    /// Threshold count before firing.
    pub threshold: u32,
    /// If set, the debounce window is active until this time.
    pub debounce_until: Option<OffsetDateTime>,
    /// Handle to the debounce timer task (aborted on reset).
    pub debounce_handle: Option<JoinHandle<()>>,
}

impl EventAccumulator {
    pub fn new(threshold: u32) -> Self {
        Self {
            count: 0,
            threshold,
            debounce_until: None,
            debounce_handle: None,
        }
    }

    /// Increment the event counter. Returns `true` if the threshold is now met.
    pub fn increment(&mut self) -> bool {
        self.count += 1;
        self.count >= self.threshold
    }

    /// Reset the counter (called after a cycle fires or completes).
    pub fn reset(&mut self) {
        self.count = 0;
        self.debounce_until = None;
        if let Some(handle) = self.debounce_handle.take() {
            handle.abort();
        }
    }

    /// Check if the threshold has been met.
    pub fn threshold_met(&self) -> bool {
        self.count >= self.threshold
    }

    /// Check if the debounce window is currently active.
    pub fn is_debouncing(&self) -> bool {
        if let Some(until) = self.debounce_until {
            OffsetDateTime::now_utc() < until
        } else {
            false
        }
    }
}

/// Metadata for an active (in-flight) scheduled execution cycle.
///
/// Keyed by `schedule_public_id` in the actor's `active_cycles` HashMap,
/// so the schedule/session IDs are not duplicated here.
#[derive(Debug)]
pub(crate) struct ActiveCycle {
    pub started_at: OffsetDateTime,
    /// Handle to the timeout task that sends `CycleFailed` if
    /// `max_runtime_seconds` is exceeded.
    pub timeout_handle: JoinHandle<()>,
}

/// Compute the next run time for an interval schedule with jitter.
///
/// `jitter_percent` is 0-100. The actual interval is randomized within
/// `[interval * (1 - jitter/100), interval * (1 + jitter/100)]`.
pub(crate) fn compute_next_run_at(
    interval_seconds: u64,
    jitter_percent: u8,
    from: OffsetDateTime,
) -> OffsetDateTime {
    let jitter_fraction = (jitter_percent as f64) / 100.0;
    let jitter_range = interval_seconds as f64 * jitter_fraction;

    // Use a simple deterministic jitter based on current nanos
    // (not cryptographically random, but sufficient for scheduling)
    let nanos = from.unix_timestamp_nanos() as u64;
    let pseudo_random = (nanos % 1000) as f64 / 1000.0; // 0.0..1.0
    let jitter_offset = (pseudo_random * 2.0 - 1.0) * jitter_range;

    let actual_interval = (interval_seconds as f64 + jitter_offset).max(1.0) as i64;
    from + time::Duration::seconds(actual_interval)
}

/// Compute backoff delay for failure recovery.
///
/// Uses exponential backoff: `min(base * 2^failures, 3600)` with jitter.
/// Returns a `Duration` representing the delay from `now`.
pub(crate) fn compute_backoff(
    backoff_base_seconds: u64,
    consecutive_failures: u32,
    jitter_percent: u8,
) -> time::Duration {
    let exp = 2u64.saturating_pow(consecutive_failures);
    let raw_delay = backoff_base_seconds.saturating_mul(exp).min(3600);
    let now = OffsetDateTime::now_utc();
    let jittered = compute_next_run_at(raw_delay, jitter_percent, now);
    let delay = (jittered - now).max(time::Duration::seconds(1));
    delay
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    // ── DeadlineQueue ────────────────────────────────────────────────────

    #[test]
    fn deadline_queue_insert_and_peek() {
        let q = &mut DeadlineQueue::new();
        assert_eq!(q.len(), 0);

        let t1 = OffsetDateTime::now_utc() + time::Duration::hours(2);
        let t2 = OffsetDateTime::now_utc() + time::Duration::hours(1);

        q.insert(t1, "sched-later".to_string());
        q.insert(t2, "sched-sooner".to_string());

        assert_eq!(q.len(), 2);

        // Peek should return the earlier deadline
        let (peeked_time, peeked_id) = q.peek().unwrap();
        assert_eq!(peeked_id, "sched-sooner");
        assert!(peeked_time <= t2 + time::Duration::seconds(1));
    }

    #[test]
    fn deadline_queue_remove_by_id() {
        let q = &mut DeadlineQueue::new();
        let now = OffsetDateTime::now_utc();

        q.insert(now + time::Duration::hours(1), "keep".to_string());
        q.insert(now + time::Duration::hours(2), "remove".to_string());
        q.insert(now + time::Duration::hours(3), "keep2".to_string());

        q.remove("remove");

        assert_eq!(q.len(), 2);

        // Peek returns earliest remaining
        let (_, id) = q.peek().unwrap();
        assert_eq!(id, "keep");
    }

    #[test]
    fn deadline_queue_remove_nonexistent_is_noop() {
        let mut q = DeadlineQueue::new();
        let now = OffsetDateTime::now_utc();
        q.insert(now + time::Duration::hours(1), "exists".to_string());

        q.remove("nonexistent");
        assert_eq!(q.len(), 1);
    }

    // ── EventAccumulator ─────────────────────────────────────────────────

    #[test]
    fn event_accumulator_increment_and_threshold() {
        let mut acc = EventAccumulator::new(3);
        assert!(!acc.threshold_met());
        assert!(!acc.increment()); // count=1
        assert!(!acc.increment()); // count=2
        assert!(acc.increment()); // count=3, threshold met
        assert!(acc.threshold_met());
    }

    #[test]
    fn event_accumulator_reset() {
        let mut acc = EventAccumulator::new(2);
        acc.increment();
        acc.increment();
        assert!(acc.threshold_met());

        acc.reset();
        assert!(!acc.threshold_met());
        assert_eq!(acc.count, 0);
        assert!(acc.debounce_until.is_none());
    }

    #[test]
    fn event_accumulator_debounce() {
        let mut acc = EventAccumulator::new(1);
        assert!(!acc.is_debouncing());

        // Set debounce to future
        acc.debounce_until = Some(OffsetDateTime::now_utc() + time::Duration::hours(1));
        assert!(acc.is_debouncing());

        // Set debounce to past
        acc.debounce_until = Some(OffsetDateTime::now_utc() - time::Duration::hours(1));
        assert!(!acc.is_debouncing());
    }

    // ── compute_next_run_at ──────────────────────────────────────────────

    #[test]
    fn compute_next_run_at_no_jitter() {
        let now = OffsetDateTime::now_utc();
        let next = compute_next_run_at(3600, 0, now);
        let diff = (next - now).whole_seconds();
        assert_eq!(diff, 3600);
    }

    #[test]
    fn compute_next_run_at_with_jitter_stays_reasonable() {
        let now = OffsetDateTime::now_utc();
        let next = compute_next_run_at(3600, 10, now);
        let diff = (next - now).whole_seconds();
        // With 10% jitter, should be within [3240, 3960]
        assert!(diff >= 3240, "diff={} too small", diff);
        assert!(diff <= 3960, "diff={} too large", diff);
    }

    #[test]
    fn compute_next_run_at_minimum_one_second() {
        let now = OffsetDateTime::now_utc();
        // Even with massive jitter on a tiny interval, floor is 1 second
        let next = compute_next_run_at(1, 100, now);
        let diff = (next - now).whole_seconds();
        assert!(diff >= 1, "diff={} should be at least 1", diff);
    }

    // ── compute_backoff ──────────────────────────────────────────────────

    #[test]
    fn compute_backoff_increases_exponentially() {
        let d1 = compute_backoff(60, 0, 0);
        let d2 = compute_backoff(60, 1, 0);
        let d3 = compute_backoff(60, 2, 0);

        assert_eq!(d1.whole_seconds(), 60);
        assert_eq!(d2.whole_seconds(), 120);
        assert_eq!(d3.whole_seconds(), 240);
    }

    #[test]
    fn compute_backoff_caps_at_3600() {
        let d = compute_backoff(60, 10, 0);
        // 60 * 2^10 = 61440, capped to 3600
        assert_eq!(d.whole_seconds(), 3600);
    }
}

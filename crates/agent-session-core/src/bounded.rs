//! Bounded state: the debug log ring and the replay-dedup hash set.

use querymt_agent_protocol_types::hash::RapidHash;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use typeshare::typeshare;

/// Debug log capacity (newest entries kept).
pub const DEBUG_LOG_CAPACITY: usize = 1_000;
/// Number of newest live notifications remembered for replay dedup.
pub const DEDUP_CAPACITY: usize = 2_000;

/// One applied input, summarized.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugEntry {
    pub kind: String,
    pub summary: String,
    pub message_id: Option<String>,
}

/// The debug log: the newest [`DEBUG_LOG_CAPACITY`] entries and a drop count.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugLog {
    pub entries: Vec<DebugEntry>,
    #[typeshare(serialized_as = "number")]
    pub dropped: u64,
}

#[derive(Debug, Default)]
pub(crate) struct DebugRing {
    entries: VecDeque<DebugEntry>,
    dropped: u64,
}

impl DebugRing {
    pub(crate) fn push(&mut self, entry: DebugEntry) {
        if self.entries.len() == DEBUG_LOG_CAPACITY {
            self.entries.pop_front();
            self.dropped += 1;
        }
        self.entries.push_back(entry);
    }

    pub(crate) fn snapshot(&self) -> DebugLog {
        DebugLog {
            entries: self.entries.iter().cloned().collect(),
            dropped: self.dropped,
        }
    }
}

/// Hashes of the newest applied live notifications.
#[derive(Debug, Default)]
pub(crate) struct SeenLive {
    order: VecDeque<u64>,
    set: BTreeSet<u64>,
}

impl SeenLive {
    pub(crate) fn hash(text: &str) -> u64 {
        RapidHash::new(text.as_bytes()).as_u64()
    }

    pub(crate) fn contains(&self, hash: u64) -> bool {
        self.set.contains(&hash)
    }

    /// Remember a live notification. The same hash is kept once, so repeated
    /// identical live chunks do not evict other entries.
    pub(crate) fn remember(&mut self, hash: u64) {
        if !self.set.insert(hash) {
            return;
        }
        self.order.push_back(hash);
        if self.order.len() > DEDUP_CAPACITY
            && let Some(oldest) = self.order.pop_front()
        {
            self.set.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: usize) -> DebugEntry {
        DebugEntry {
            kind: "acp_notification".into(),
            summary: format!("#{n}"),
            message_id: None,
        }
    }

    #[test]
    fn debug_log_keeps_the_newest_entries_in_the_ring() {
        let mut ring = DebugRing::default();
        for n in 0..1_500 {
            ring.push(entry(n));
        }
        let log = ring.snapshot();
        assert_eq!(log.entries.len(), 1_000);
        assert_eq!(log.dropped, 500);
        assert_eq!(
            log.entries.first().map(|e| e.summary.as_str()),
            Some("#500")
        );
        assert_eq!(
            log.entries.last().map(|e| e.summary.as_str()),
            Some("#1499")
        );
    }

    #[test]
    fn seen_live_is_bounded() {
        let mut seen = SeenLive::default();
        for n in 0..(DEDUP_CAPACITY as u64 + 10) {
            seen.remember(n);
        }
        assert!(!seen.contains(0));
        assert!(seen.contains(DEDUP_CAPACITY as u64 + 9));
        assert_eq!(seen.set.len(), DEDUP_CAPACITY);
    }
}

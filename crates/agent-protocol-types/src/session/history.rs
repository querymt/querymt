//! History-window payloads carried by load snapshots and older-history pages.

use crate::accounting::ScopeTotals;
use crate::protocol::ContentBlock;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

pub const DEFAULT_WINDOW_TURNS: usize = 20;
pub const DEFAULT_WINDOW_BYTES: usize = 1024 * 1024;
pub const DEFAULT_TRUNCATE_BYTES: usize = 32 * 1024;
pub const MAX_WINDOW_TURNS: usize = 100;
pub const MAX_WINDOW_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TRUNCATE_BYTES: usize = 256 * 1024;

/// A client's request for a recent-history window.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LoadWindow {
    #[typeshare(serialized_as = "number")]
    pub max_turns: usize,
    #[typeshare(serialized_as = "number")]
    pub max_bytes: usize,
    #[typeshare(serialized_as = "number")]
    pub truncate_bytes: usize,
}

impl Default for LoadWindow {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_WINDOW_TURNS,
            max_bytes: DEFAULT_WINDOW_BYTES,
            truncate_bytes: DEFAULT_TRUNCATE_BYTES,
        }
    }
}

impl LoadWindow {
    /// Clamp every value into `1..=` its own agent-side maximum.
    pub fn clamped(self) -> Self {
        Self {
            max_turns: self.max_turns.clamp(1, MAX_WINDOW_TURNS),
            max_bytes: self.max_bytes.clamp(1, MAX_WINDOW_BYTES),
            truncate_bytes: self.truncate_bytes.clamp(1, MAX_TRUNCATE_BYTES),
        }
    }
}

/// Where the returned events sit in the session's history.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryInfo {
    /// No older history exists before the returned events.
    pub complete: bool,
    /// Opaque token for the `session/history` request that returns the turns
    /// preceding the returned events. Absent when `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// The owner was unreachable; the history was served from the local copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
}

/// Which part of a tool call a truncation marker or content request refers to.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolContentPart {
    Result,
    Arguments,
}

/// A tool result or argument string that was shortened in a response.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruncatedEntry {
    pub tool_call_id: String,
    pub part: ToolContentPart,
    /// JSON pointer to the shortened string inside the arguments; absent for
    /// results and for arguments that are not JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[typeshare(serialized_as = "number")]
    pub full_bytes: u64,
}

/// Exclusive token categories, as in the live usage meta.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBreakdown {
    #[typeshare(serialized_as = "number")]
    pub input_tokens: u64,
    #[typeshare(serialized_as = "number")]
    pub output_tokens: u64,
    #[typeshare(serialized_as = "number")]
    pub reasoning_tokens: u64,
    #[typeshare(serialized_as = "number")]
    pub cache_read_tokens: u64,
    #[typeshare(serialized_as = "number")]
    pub cache_write_tokens: u64,
}

impl UsageBreakdown {
    pub fn from_usage(usage: &querymt::Usage) -> Self {
        Self {
            input_tokens: u64::from(usage.input_tokens),
            output_tokens: u64::from(usage.output_tokens),
            reasoning_tokens: u64::from(usage.reasoning_tokens),
            cache_read_tokens: u64::from(usage.cache_read),
            cache_write_tokens: u64::from(usage.cache_write),
        }
    }

    pub fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
    }
}

/// Whole-session token usage (the live `_meta.querymt.usage` v1 fields) plus
/// the state a client needs to keep live usage correct after a window.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryUsage {
    pub latest_request: Option<UsageBreakdown>,
    pub session_totals: Option<UsageBreakdown>,
    pub partial: bool,
    /// Number of main-request completions in the session.
    #[typeshare(serialized_as = "number")]
    pub request_count: u64,
    /// Context occupancy reported by the last request, and its position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<number>")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<number>")]
    pub context_position: Option<i64>,
    /// Session-lifetime accounting totals from the shared reducer.
    #[typeshare(serialized_as = "any")]
    pub accounting: ScopeTotals,
    /// Usage of the events *before* the returned window. A consumer that
    /// replays the window's own events seeds from this to reach whole-session
    /// totals without counting any event twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_window: Option<PriorUsage>,
}

/// Usage accumulated over the part of history preceding a window.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorUsage {
    /// Journal position of the window's first event; the prior totals cover
    /// exactly the events with a smaller position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<number>")]
    pub window_start: Option<i64>,
    pub session_totals: Option<UsageBreakdown>,
    pub partial: bool,
    #[typeshare(serialized_as = "number")]
    pub request_count: u64,
    #[typeshare(serialized_as = "any")]
    pub accounting: ScopeTotals,
}

/// The session's current model, from its last `ProviderChanged`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryProvider {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<number>")]
    pub context_limit: Option<u64>,
    #[typeshare(serialized_as = "number")]
    pub position: i64,
}

/// How a pending input will reach the run.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputDelivery {
    Steer,
    Queue,
}

/// Where a pending input is in its lifecycle.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingInputState {
    Accepted,
    Queued,
}

/// A queued or steering input that has not started or been applied yet.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInputSummary {
    pub input_id: String,
    pub delivery: InputDelivery,
    pub state: PendingInputState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<number>")]
    pub position: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[typeshare(serialized_as = "any")]
    pub blocks: Vec<ContentBlock>,
    /// Acceptance time in milliseconds since the epoch.
    #[typeshare(serialized_as = "number")]
    pub created_at: i64,
}

/// A child session spawned by this session.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildSessionLink {
    pub child_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// State computed over the entire session, independent of the window.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistorySummary {
    pub usage: SummaryUsage,
    pub provider: Option<SummaryProvider>,
    pub pending_inputs: Vec<PendingInputSummary>,
    pub child_sessions: Vec<ChildSessionLink>,
}

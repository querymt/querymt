//! Session header (everything except the turns) and change reports.

use crate::input::UndoFrame;
use querymt_agent_protocol_types::delegation_update::DelegationUpdateNotification;
use querymt_agent_protocol_types::protocol::PlanEntry;
use querymt_agent_protocol_types::runtime_status::SessionRuntimeStatus;
use querymt_agent_protocol_types::session::history::{
    PendingInputSummary, SummaryProvider, UsageBreakdown,
};
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

/// What the session is doing right now.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunState {
    #[default]
    Idle,
    Submitting,
    Thinking,
    Streaming,
    ToolRunning,
    Cancelled,
    Failed,
}

/// Context occupancy from the latest `usage_update`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    #[typeshare(serialized_as = "number")]
    pub used: u64,
    #[typeshare(serialized_as = "number")]
    pub size: u64,
}

/// Cumulative cost, taken only from accounting data.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostView {
    pub amount: f64,
    pub currency: String,
    /// The amount is a complete total, not a lower bound.
    pub complete: bool,
}

/// Token breakdown (`_meta.querymt.usage`) or the windowed summary's totals.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageView {
    pub latest_request: Option<UsageBreakdown>,
    pub session_totals: Option<UsageBreakdown>,
    /// Totals are approximate.
    pub partial: bool,
}

/// Where older history stands.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPaging {
    /// Older history exists before the first turn.
    pub has_older: bool,
    /// Token for the next `_querymt/session/history` request.
    pub before: Option<String>,
    /// The owner was unreachable; history came from a local copy.
    pub stale: bool,
}

/// The plan most recently set by `plan` or `plan_update`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanView {
    pub plan_id: Option<String>,
    /// Entries as received (ACP plan entries).
    #[typeshare(serialized_as = "any")]
    pub entries: Vec<PlanEntry>,
    /// Inline Markdown content for markdown plans.
    pub markdown: Option<String>,
}

/// The session header: all whole-session state.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub session_id: String,
    pub schema_version: u32,
    pub run_state: RunState,
    pub active_tool_call_id: Option<String>,
    #[typeshare(serialized_as = "Option<string>")]
    pub last_stop_reason: Option<String>,
    pub last_error: Option<String>,
    pub context: Option<ContextUsage>,
    pub tokens: Option<TokenUsageView>,
    pub cost: Option<CostView>,
    pub provider: Option<SummaryProvider>,
    pub title: Option<String>,
    pub current_mode: Option<String>,
    /// ACP config options, merged by id, kept as received.
    #[typeshare(serialized_as = "any")]
    pub config_options: Vec<serde_json::Value>,
    pub plan: Option<PlanView>,
    pub undo_stack: Vec<UndoFrame>,
    pub pending_inputs: Vec<PendingInputSummary>,
    pub delegations: Vec<DelegationUpdateNotification>,
    pub runtime: Option<SessionRuntimeStatus>,
    pub history: HistoryPaging,
}

/// Why an input did not apply.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputError {
    /// Index of the input within its batch.
    #[typeshare(serialized_as = "number")]
    pub input_index: u32,
    /// History position of the failing event inside a snapshot or page.
    #[typeshare(serialized_as = "Option<number>")]
    pub seq: Option<i64>,
    pub message: String,
}

/// What one `apply` call changed.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeReport {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    pub header_changed: bool,
    /// Inputs for another session.
    #[typeshare(serialized_as = "number")]
    pub ignored: u32,
    /// Unknown update or event kinds that were skipped.
    #[typeshare(serialized_as = "number")]
    pub unknown: u32,
    /// Replayed inputs already applied live.
    #[typeshare(serialized_as = "number")]
    pub duplicates: u32,
    pub errors: Vec<InputError>,
}

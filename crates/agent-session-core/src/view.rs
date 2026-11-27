//! Session header (everything except the turns) and change reports.

use crate::input::UndoFrame;
use querymt_agent_protocol_types::accounting::AccountingSnapshot;
use querymt_agent_protocol_types::events::{SlashCommandEffect, SlashCommandSource};
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
    /// The run waits for the user (a permission or a question).
    WaitingForUser,
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

/// A child session of this session (a subagent), as `subagent_update`
/// patches describe it. The tool call that spawned it carries the link
/// (`child_session_id` on its entry).
#[typeshare]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentView {
    /// The child's session id; the key of this list.
    pub session_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// The child's foreground state as the agent reported it: `running`,
    /// `idle`, `requires_action`, `unknown` or an implementation-specific value.
    pub state: Option<String>,
    /// `_meta.querymt.stage` of the update that last set the state.
    pub stage: Option<String>,
    /// What the client may do with the child, as received.
    #[typeshare(serialized_as = "any")]
    pub capabilities: Option<serde_json::Value>,
    pub cwd: Option<String>,
    pub additional_directories: Option<Vec<String>>,
    /// `_meta` of the update, as received.
    #[typeshare(serialized_as = "any")]
    pub meta: Option<serde_json::Value>,
}

/// Where a cursor sits in the view: next to an entry.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorPlacement {
    /// The entry the cursor is anchored to.
    pub entry_id: String,
    /// `before` or `after` that entry.
    pub position: String,
}

/// A session cursor the agent announced. The id is the agent's and opaque;
/// the first anchor is kept for the life of the cursor.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorView {
    pub cursor_id: String,
    /// The anchor as received (ACP `CursorAnchor`).
    #[typeshare(serialized_as = "any")]
    pub anchor: serde_json::Value,
    /// The methods that accept the cursor (ACP `CursorCapabilities`), as received.
    #[typeshare(serialized_as = "any")]
    pub capabilities: Option<serde_json::Value>,
    #[typeshare(serialized_as = "any")]
    pub meta: Option<serde_json::Value>,
    /// Where the anchored entity is in the view; `None` while it is not (unplaced).
    pub placement: Option<CursorPlacement>,
}

/// The session's active time: run time, on the host's clock.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveTime {
    /// Total time of the ended runs, in milliseconds.
    #[typeshare(serialized_as = "number")]
    pub total_ms: u64,
    /// Host time (ms since the epoch) the active run started, if one is
    /// active. The client adds `now - active_since_ms` while it runs.
    #[typeshare(serialized_as = "Option<number>")]
    pub active_since_ms: Option<i64>,
}

/// A slash command the agent advertises for the session
/// (`available_commands_update`).
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommandView {
    /// The command name without a leading `/`.
    pub name: String,
    pub description: String,
    /// The hint of the command's input, when it takes input. Empty when the
    /// command takes input but gives no hint.
    pub input_hint: Option<String>,
    /// Where the command comes from, as the agent's descriptor says
    /// (`_meta.querymt.source`). `None` when the agent sent none or a value
    /// this core does not know. The same type as an invocation record's
    /// `source`, so the two compare directly.
    pub source: Option<SlashCommandSource>,
    /// What the command does (`_meta.querymt.effect`); `None` as for `source`.
    /// The same type as an invocation record's `effect`.
    pub effect: Option<SlashCommandEffect>,
    /// What the command asks of the user's input (`_meta.querymt.input`);
    /// `None` as for `source`.
    pub input: Option<CommandInput>,
    /// The submissions the command runs from (`_meta.querymt.accepts`).
    /// `None` when the agent sent no descriptor, which is not the same as
    /// `Some` of an empty list (a command that runs from no submission): a
    /// client picks its fallback from `None`. Elements this core does not
    /// know are dropped.
    pub accepts: Option<Vec<CommandSubmission>>,
}

/// What a slash command asks of the user's input.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandInput {
    /// The command takes no input.
    None,
    /// The command works with or without input.
    Optional,
    /// The command is meant to be sent with input.
    Required,
}

/// A way of submitting a prompt that a command runs from.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandSubmission {
    /// `session/prompt`.
    Prompt,
    /// Queued input.
    Queue,
}

/// The session header: all whole-session state.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub session_id: String,
    pub schema_version: u32,
    pub run_state: RunState,
    /// The id of the run that is running, when there is one.
    pub active_run_id: Option<String>,
    pub active_tool_call_id: Option<String>,
    #[typeshare(serialized_as = "Option<string>")]
    pub last_stop_reason: Option<String>,
    pub last_error: Option<String>,
    pub context: Option<ContextUsage>,
    pub tokens: Option<TokenUsageView>,
    pub cost: Option<CostView>,
    /// The latest accounting snapshot as the agent sent it (load snapshot,
    /// `_meta.querymt.accounting`). `cost` is derived from it. Empty for a
    /// session without model calls.
    pub accounting: Option<AccountingSnapshot>,
    pub provider: Option<SummaryProvider>,
    pub title: Option<String>,
    pub current_mode: Option<String>,
    /// ACP config options, merged by id, kept as received.
    #[typeshare(serialized_as = "any")]
    pub config_options: Vec<serde_json::Value>,
    /// The slash commands of the agent's latest `available_commands_update`,
    /// replaced as a whole. Loads do not change them.
    pub available_commands: Vec<SlashCommandView>,
    pub plan: Option<PlanView>,
    pub undo_stack: Vec<UndoFrame>,
    pub pending_inputs: Vec<PendingInputSummary>,
    /// Child sessions in the order they first appeared, merged by session id.
    pub subagents: Vec<SubagentView>,
    /// Session cursors in the order they were announced, keyed by cursor id.
    pub cursors: Vec<CursorView>,
    pub runtime: Option<SessionRuntimeStatus>,
    /// Run time of the session; `None` until a snapshot or a state update
    /// reported it. History pages do not change it.
    pub active_time: Option<ActiveTime>,
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

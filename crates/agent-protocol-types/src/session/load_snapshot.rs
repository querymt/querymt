//! Session load snapshot payload (`querymt/sessionLoadSnapshot.v1`) and the
//! structured user prompt records served with loads and history pages.

use crate::accounting::AccountingSnapshot;
use crate::delegation_update::DelegationUpdateNotification;
use crate::protocol::{ContentBlock, CursorUpdate, SubagentUpdate};
use crate::runtime_status::SessionRuntimeStatus;
use crate::session::history::{HistoryInfo, SessionHistorySummary, TruncatedEntry};
use crate::session::views::AuditView;
use crate::stream_cursor::StreamCursor;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserPromptRecord {
    pub message_id: String,
    /// Zero-based position in persisted message history; this is not an event sequence.
    #[typeshare(serialized_as = "number")]
    pub message_order: u64,
    #[typeshare(serialized_as = "number")]
    pub timestamp: i64,
    #[typeshare(serialized_as = "any")]
    pub blocks: Vec<ContentBlock>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLoadSnapshot {
    #[serde(rename = "runtimeStatus")]
    pub runtime_status: SessionRuntimeStatus,
    pub audit: AuditView,
    pub cursor: StreamCursor,
    /// Authoritative replacement accounting at `cursor`, reduced from the same
    /// audit events by the shared accounting reducer. Present even when
    /// historical replay is skipped or the context limit is unknown.
    #[typeshare(serialized_as = "AccountingSnapshot")]
    pub accounting: AccountingSnapshot,
    #[serde(rename = "delegationUpdates")]
    pub delegation_updates: Vec<DelegationUpdateNotification>,
    /// The latest ACP `subagent_update` of every child session the session
    /// created, including delegations still running. Same state as
    /// `delegationUpdates`, in the form `session/update` carries.
    #[serde(default, rename = "subagentUpdates")]
    #[typeshare(serialized_as = "any")]
    pub subagent_updates: Vec<SubagentUpdate>,
    /// The `cursor_update` of every position the returned events anchor, each
    /// announced once, with the ids history replay announces. The agent sends
    /// it only to a client that advertised `session.cursors`.
    #[serde(
        default,
        rename = "cursorUpdates",
        skip_serializing_if = "Vec::is_empty"
    )]
    #[typeshare(serialized_as = "any")]
    pub cursor_updates: Vec<CursorUpdate>,
    #[serde(
        default,
        rename = "userPrompts",
        skip_serializing_if = "Option::is_none"
    )]
    pub user_prompts: Option<Vec<UserPromptRecord>>,
    /// Present only for windowed loads: where the returned events sit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryInfo>,
    /// Present only for windowed loads: whole-session state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SessionHistorySummary>,
    /// Present only for windowed loads that shortened tool content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Vec<TruncatedEntry>>,
}

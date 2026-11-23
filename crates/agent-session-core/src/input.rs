//! Inputs a client feeds to the reducer.
//!
//! These are an in-process API between a client's ACP transport and the
//! reducer, not a wire protocol between agent and client.

use querymt_agent_protocol_types::protocol::{ContentBlock, StopReason};
use querymt_agent_protocol_types::runtime_status::SessionRuntimeStatus;
use querymt_agent_protocol_types::session::history::ToolContentPart;
use querymt_agent_protocol_types::session::input_state::SessionInputStateNotification;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

/// One frame of a session's undo stack, as the agent reports it.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoFrame {
    pub message_id: String,
    /// The undo that produced this frame failed part-way.
    pub restore_failed: bool,
}

/// One input to [`crate::SessionCore::apply`].
///
/// Adjacently tagged (`type` and `data`) because typeshare cannot express
/// internally tagged enums with struct variants.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionInput {
    /// The raw `session/load` response.
    SnapshotLoaded {
        #[typeshare(serialized_as = "any")]
        response: serde_json::Value,
    },
    /// One ACP `session/update` notification, as received.
    AcpNotification {
        #[typeshare(serialized_as = "any")]
        notification: serde_json::Value,
        /// The client marks buffered or replayed notifications, for dedup.
        #[serde(default)]
        replayed: bool,
        #[typeshare(serialized_as = "number")]
        at_ms: i64,
    },
    /// The raw `_querymt/session/history` response.
    HistoryPage {
        #[typeshare(serialized_as = "any")]
        response: serde_json::Value,
    },
    /// A prompt the client just submitted. The same id goes out as
    /// `_meta.querymt.client_prompt_id` on `session/prompt`.
    PromptOptimistic {
        client_prompt_id: String,
        #[typeshare(serialized_as = "any")]
        blocks: Vec<ContentBlock>,
        #[typeshare(serialized_as = "number")]
        at_ms: i64,
    },
    /// The raw `querymt/session/stateUpdate` notification params
    /// (`sessionId`, `state`, `runId`, `stopReason`).
    StateUpdate {
        #[typeshare(serialized_as = "any")]
        notification: serde_json::Value,
        #[typeshare(serialized_as = "number")]
        at_ms: i64,
    },
    /// The `session/prompt` request returned.
    PromptCompleted {
        /// The prompt this response answers, when the client sent one.
        #[serde(default)]
        client_prompt_id: Option<String>,
        /// The user message the prompt inserted (`_meta.querymt.messageId` of
        /// the response); it links a pending prompt whose echo carried no
        /// client prompt id.
        #[serde(default)]
        message_id: Option<String>,
        #[typeshare(serialized_as = "string")]
        stop_reason: StopReason,
        #[typeshare(serialized_as = "number")]
        at_ms: i64,
    },
    /// The `session/prompt` request failed.
    PromptFailed {
        client_prompt_id: String,
        message: String,
    },
    RuntimeObserved {
        status: SessionRuntimeStatus,
    },
    InputState {
        notification: SessionInputStateNotification,
    },
    /// Replaces the undo stack.
    UndoStack {
        frames: Vec<UndoFrame>,
    },
    /// Merges config options by id.
    ConfigOptions {
        #[typeshare(serialized_as = "any")]
        options: Vec<serde_json::Value>,
    },
    /// Full tool content fetched after a truncated load.
    ToolContent {
        tool_call_id: String,
        part: ToolContentPart,
        content: String,
    },
}

//! Turn entries: the unit a client renders and caches.

use querymt_agent_protocol_types::protocol::ContentBlock;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

/// Delivery state of a user prompt.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PromptStatus {
    /// Shown optimistically; the agent has not echoed it yet.
    Pending,
    /// Confirmed by the agent (or loaded from history).
    Sent,
    /// The client reported that submitting failed.
    Failed,
}

/// A user message.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub message_id: Option<String>,
    pub client_prompt_id: Option<String>,
    #[typeshare(serialized_as = "any")]
    pub blocks: Vec<ContentBlock>,
    pub status: PromptStatus,
    pub error: Option<String>,
    /// Client time the prompt was submitted; only optimistic prompts have one.
    #[typeshare(serialized_as = "Option<number>")]
    pub at_ms: Option<i64>,
}

/// One assistant message: all text chunks that share a message id.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub message_id: Option<String>,
    pub text: String,
}

/// One reasoning part of an assistant message.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningPart {
    pub message_id: Option<String>,
    /// Empty when the agent sent no `_meta.querymt.reasoning_part_id`.
    pub part_id: String,
    pub text: String,
}

/// A tool call with its latest known state.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEntry {
    pub tool_call_id: String,
    pub title: String,
    /// ACP tool kind, snake case (`read`, `edit`, `execute`, ...).
    pub kind: String,
    /// ACP tool call status: `pending`, `in_progress`, `completed` or `failed`.
    pub status: String,
    #[typeshare(serialized_as = "any")]
    pub raw_input: Option<serde_json::Value>,
    #[typeshare(serialized_as = "any")]
    pub raw_output: Option<serde_json::Value>,
    /// ACP tool call content, kept as received.
    #[typeshare(serialized_as = "any")]
    pub content: Vec<serde_json::Value>,
    /// Full size in bytes when the result in this view is only a prefix.
    #[typeshare(serialized_as = "Option<number>")]
    pub result_full_bytes: Option<u64>,
    /// Full size in bytes when the arguments in this view are only a prefix.
    #[typeshare(serialized_as = "Option<number>")]
    pub arguments_full_bytes: Option<u64>,
    /// Child session spawned by this tool call (delegation).
    pub child_session_id: Option<String>,
}

/// Lifecycle state of a compaction.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompactionState {
    InProgress,
    Completed,
    Failed,
    Cancelled,
    /// A status this reducer does not know; never treated as completed.
    Other,
}

impl CompactionState {
    /// Completed, failed and cancelled compactions never return to in progress.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// A context compaction boundary.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    pub compaction_id: String,
    pub state: CompactionState,
    /// The raw status string when `state` is `other`.
    pub other_status: Option<String>,
    #[typeshare(serialized_as = "any")]
    pub summary: Vec<ContentBlock>,
    pub error: Option<String>,
    #[typeshare(serialized_as = "any")]
    pub meta: Option<serde_json::Map<String, serde_json::Value>>,
}

/// What an entry holds.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EntryKind {
    UserMessage(UserMessage),
    AssistantMessage(AssistantMessage),
    Reasoning(ReasoningPart),
    ToolCall(ToolCallEntry),
    Compaction(CompactionEntry),
}

/// One renderable item of a turn. `id` never changes; `revision` grows
/// whenever the entry changes.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    #[typeshare(serialized_as = "number")]
    pub revision: u64,
    pub kind: EntryKind,
}

/// A user prompt (or compaction boundary) and everything that followed it.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    /// Grows whenever any entry of the turn changes.
    #[typeshare(serialized_as = "number")]
    pub revision: u64,
    pub entries: Vec<Entry>,
}

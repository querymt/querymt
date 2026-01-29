use crate::index::merkle::DiffPaths;
use querymt::{
    FunctionCall, ToolCall,
    chat::{ChatMessage, ChatRole, MessageType},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum MessagePart {
    Text {
        content: String,
    },
    Reasoning {
        content: String,
        time_ms: Option<u64>,
    },
    StepStart {
        step_id: String,
        description: String,
    },
    StepFinish {
        step_id: String,
        success: bool,
        cost: Option<f64>,
    },
    ToolUse(ToolCall),
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
        tool_name: Option<String>,
        tool_arguments: Option<String>,
        /// Timestamp when this tool result was marked as compacted (pruned)
        /// When set, the content should be replaced with a placeholder in LLM context
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted_at: Option<i64>,
    },
    Patch {
        id: String,
        files: Vec<String>,
        diff: String,
    },
    Snapshot {
        root_hash: crate::hash::RapidHash,
        changed_paths: DiffPaths,
    },
    Compaction {
        summary: String,
        original_token_count: usize,
    },
}

impl MessagePart {
    pub fn type_name(&self) -> &'static str {
        match self {
            MessagePart::Text { .. } => "text",
            MessagePart::Reasoning { .. } => "reasoning",
            MessagePart::StepStart { .. } => "step_start",
            MessagePart::StepFinish { .. } => "step_finish",
            MessagePart::ToolUse(_) => "tool_use",
            MessagePart::ToolResult { .. } => "tool_result",
            MessagePart::Patch { .. } => "patch",
            MessagePart::Snapshot { .. } => "snapshot",
            MessagePart::Compaction { .. } => "compaction",
        }
    }

    /// Get the diff summary for a Snapshot part, or None for other part types
    pub fn diff_summary(&self) -> Option<String> {
        match self {
            MessagePart::Snapshot { changed_paths, .. } => Some(changed_paths.summary()),
            _ => None,
        }
    }

    /// Get the changed paths for a Snapshot part, or None for other part types
    pub fn changed_paths(&self) -> Option<&DiffPaths> {
        match self {
            MessagePart::Snapshot { changed_paths, .. } => Some(changed_paths),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: String,
    pub session_id: String,
    pub role: ChatRole,
    pub parts: Vec<MessagePart>,
    pub created_at: i64,
    pub parent_message_id: Option<String>,
}

impl AgentMessage {
    pub fn new(session_id: String, role: ChatRole) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            session_id,
            role,
            parts: Vec::new(),
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        }
    }

    pub fn to_chat_message(&self) -> ChatMessage {
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();

        for part in &self.parts {
            match part {
                MessagePart::Text { content: t } => content.push_str(t),
                MessagePart::Reasoning { .. } => {
                    // Option: Exclude reasoning from context sent to LLM to save tokens
                }
                MessagePart::ToolUse(tc) => tool_calls.push(tc.clone()),
                MessagePart::ToolResult {
                    call_id,
                    content: res,
                    tool_name,
                    tool_arguments: _tool_arguments,
                    compacted_at,
                    ..
                } => {
                    // If compacted, return placeholder text instead of original content
                    let effective_content = if compacted_at.is_some() {
                        "[Old tool result content cleared]".to_string()
                    } else {
                        res.clone()
                    };
                    tool_results.push(ToolCall {
                        id: call_id.clone(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: tool_name.clone().unwrap_or_else(|| "unknown".to_string()),
                            arguments: effective_content.clone(),
                        },
                    });
                    content.push_str(&effective_content);
                }
                MessagePart::Snapshot { changed_paths, .. } => {
                    let summary = changed_paths.summary();
                    if !changed_paths.is_empty() {
                        content.push_str(&format!("\n[System: File changes: {}]\n", summary));
                    }
                }
                MessagePart::Compaction { summary, .. } => {
                    content.push_str(&format!("\n[Conversation summary]\n{}\n", summary));
                }
                _ => {}
            }
        }

        let message_type = if !tool_calls.is_empty() {
            MessageType::ToolUse(tool_calls)
        } else if !tool_results.is_empty() {
            MessageType::ToolResult(tool_results)
        } else {
            MessageType::Text
        };

        ChatMessage {
            role: self.role.clone(),
            message_type,
            content,
            cache: None,
        }
    }
}

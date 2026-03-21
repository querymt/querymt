use crate::agent::utils::render_prompt_for_llm;
use crate::index::merkle::DiffPaths;
use agent_client_protocol::ContentBlock;
use querymt::{
    ToolCall,
    chat::{ChatMessage, ChatRole, Content},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum MessagePart {
    Text {
        content: String,
    },
    Prompt {
        blocks: Vec<ContentBlock>,
    },
    Reasoning {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
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
        content: Vec<Content>,
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
    /// User-side compaction request: paired with the following Compaction (assistant) message
    /// to form a natural user→assistant exchange after context compaction.
    CompactionRequest {
        original_token_count: usize,
    },
    /// Turn snapshot start: worktree state before turn (user prompt)
    TurnSnapshotStart {
        turn_id: String,
        snapshot_id: String,
    },
    /// Turn snapshot patch: worktree state after turn completes, with changed files
    TurnSnapshotPatch {
        turn_id: String,
        snapshot_id: String,
        changed_paths: Vec<String>,
    },
}

impl MessagePart {
    pub fn type_name(&self) -> &'static str {
        match self {
            MessagePart::Text { .. } => "text",
            MessagePart::Prompt { .. } => "prompt",
            MessagePart::Reasoning { .. } => "reasoning",
            MessagePart::StepStart { .. } => "step_start",
            MessagePart::StepFinish { .. } => "step_finish",
            MessagePart::ToolUse(_) => "tool_use",
            MessagePart::ToolResult { .. } => "tool_result",
            MessagePart::Patch { .. } => "patch",
            MessagePart::Snapshot { .. } => "snapshot",
            MessagePart::Compaction { .. } => "compaction",
            MessagePart::CompactionRequest { .. } => "compaction_request",
            MessagePart::TurnSnapshotStart { .. } => "turn_snapshot_start",
            MessagePart::TurnSnapshotPatch { .. } => "turn_snapshot_patch",
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
    /// Provider that generated this assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_provider: Option<String>,
    /// Model that generated this assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model: Option<String>,
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
            source_provider: None,
            source_model: None,
        }
    }

    pub fn to_chat_message(&self) -> ChatMessage {
        self.to_chat_message_with_target(None, None, None)
    }

    pub fn to_chat_message_with_max_prompt_bytes(
        &self,
        max_prompt_bytes: Option<usize>,
    ) -> ChatMessage {
        self.to_chat_message_with_target(None, None, max_prompt_bytes)
    }

    pub fn to_chat_message_with_target(
        &self,
        target_provider: Option<&str>,
        target_model: Option<&str>,
        max_prompt_bytes: Option<usize>,
    ) -> ChatMessage {
        let mut blocks = Vec::new();

        let preserve_provider_metadata = match (
            target_provider,
            target_model,
            self.source_provider.as_deref(),
            self.source_model.as_deref(),
        ) {
            (Some(tp), Some(tm), Some(sp), Some(sm)) => tp == sp && tm == sm,
            _ => true,
        };

        for part in &self.parts {
            match part {
                MessagePart::Text { content } => {
                    blocks.push(Content::text(content));
                }
                MessagePart::Prompt {
                    blocks: prompt_blocks,
                } => {
                    blocks.push(Content::text(render_prompt_for_llm(
                        prompt_blocks,
                        max_prompt_bytes,
                    )));
                }
                MessagePart::Reasoning {
                    content, signature, ..
                } => {
                    blocks.push(Content::Thinking {
                        text: content.clone(),
                        signature: if preserve_provider_metadata {
                            signature.clone()
                        } else {
                            None
                        },
                    });
                }
                MessagePart::ToolUse(tc) => {
                    blocks.push(Content::tool_use(
                        &tc.id,
                        &tc.function.name,
                        serde_json::from_str(&tc.function.arguments)
                            .unwrap_or_else(|_| serde_json::Value::Object(Default::default())),
                    ));
                }
                MessagePart::ToolResult {
                    call_id,
                    content,
                    is_error,
                    tool_name,
                    compacted_at,
                    ..
                } => {
                    let inner = if compacted_at.is_some() {
                        vec![Content::text("[Old tool result content cleared]")]
                    } else {
                        content.clone()
                    };
                    blocks.push(Content::ToolResult {
                        id: call_id.clone(),
                        name: tool_name.clone(),
                        is_error: *is_error,
                        content: inner,
                    });
                }
                MessagePart::Snapshot { changed_paths, .. } => {
                    if !changed_paths.is_empty() {
                        blocks.push(Content::text(format!(
                            "\n[System: File changes: {}]",
                            changed_paths.summary()
                        )));
                    }
                }
                MessagePart::Compaction { summary, .. } => {
                    blocks.push(Content::text(summary));
                }
                MessagePart::CompactionRequest { .. } => {
                    blocks.push(Content::text("Summarize our conversation so far."));
                }
                _ => {}
            }
        }

        ChatMessage {
            role: self.role.clone(),
            content: blocks,
            cache: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentMessage, MessagePart};
    use agent_client_protocol::{ContentBlock, TextContent};
    use querymt::chat::{ChatRole, Content};

    #[test]
    fn to_chat_message_renders_prompt_blocks() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::Prompt {
                blocks: vec![ContentBlock::Text(TextContent::new("display".to_string()))],
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message();
        assert_eq!(chat.text(), "display");
    }

    #[test]
    fn to_chat_message_compaction_renders_summary_directly() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: "Summary of previous conversation".to_string(),
                original_token_count: 5000,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message();
        assert_eq!(chat.text(), "Summary of previous conversation");
        assert_eq!(chat.role, ChatRole::Assistant);
    }

    #[test]
    fn to_chat_message_compaction_request_renders_user_prompt() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::CompactionRequest {
                original_token_count: 5000,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message();
        assert_eq!(chat.text(), "Summarize our conversation so far.");
        assert_eq!(chat.role, ChatRole::User);
    }

    #[test]
    fn to_chat_message_compaction_pair_forms_valid_exchange() {
        let req = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::CompactionRequest {
                original_token_count: 5000,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        let sum = AgentMessage {
            id: "m2".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: "Here is the summary.".to_string(),
                original_token_count: 5000,
            }],
            created_at: 0,
            parent_message_id: Some("m1".to_string()),
            source_provider: None,
            source_model: None,
        };

        let req_chat = req.to_chat_message();
        let sum_chat = sum.to_chat_message();

        // User message followed by assistant message — valid API exchange
        assert_eq!(req_chat.role, ChatRole::User);
        assert_eq!(sum_chat.role, ChatRole::Assistant);

        // Neither has trailing whitespace in text content
        assert!(!req_chat.text().ends_with(char::is_whitespace));
        assert!(!sum_chat.text().ends_with(char::is_whitespace));
    }

    #[test]
    fn to_chat_message_tool_result_uses_content_blocks() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::ToolResult {
                call_id: "call-1".to_string(),
                content: vec![Content::text("tool output")],
                is_error: false,
                tool_name: Some("shell".to_string()),
                tool_arguments: Some("{}".to_string()),
                compacted_at: None,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message();
        assert!(chat.has_tool_result());
        // The tool result block should contain the text
        let tr = chat.content.iter().find(|b| b.is_tool_result()).unwrap();
        match tr {
            Content::ToolResult {
                id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(id, "call-1");
                assert!(!is_error);
                assert_eq!(content.len(), 1);
                assert_eq!(content[0].as_text(), Some("tool output"));
            }
            _ => panic!("Expected ToolResult"),
        }
    }

    #[test]
    fn to_chat_message_compacted_tool_result_uses_placeholder() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::ToolResult {
                call_id: "call-1".to_string(),
                content: vec![Content::text("original content")],
                is_error: false,
                tool_name: Some("shell".to_string()),
                tool_arguments: Some("{}".to_string()),
                compacted_at: Some(1234567890),
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message();
        let tr = chat.content.iter().find(|b| b.is_tool_result()).unwrap();
        match tr {
            Content::ToolResult { content, .. } => {
                assert_eq!(
                    content[0].as_text(),
                    Some("[Old tool result content cleared]")
                );
            }
            _ => panic!("Expected ToolResult"),
        }
    }

    #[test]
    fn to_chat_message_with_target_keeps_signature_for_same_model() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Reasoning {
                content: "reasoning".to_string(),
                signature: Some("sig-123".to_string()),
                time_ms: None,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: Some("anthropic".to_string()),
            source_model: Some("claude-sonnet-4-5".to_string()),
        };

        let chat =
            msg.to_chat_message_with_target(Some("anthropic"), Some("claude-sonnet-4-5"), None);

        match &chat.content[0] {
            Content::Thinking {
                signature: Some(sig),
                ..
            } => assert_eq!(sig, "sig-123"),
            _ => panic!("expected signed thinking block"),
        }
    }

    #[test]
    fn to_chat_message_with_target_drops_signature_on_model_switch() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Reasoning {
                content: "reasoning".to_string(),
                signature: Some("sig-123".to_string()),
                time_ms: None,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: Some("anthropic".to_string()),
            source_model: Some("claude-sonnet-4-5".to_string()),
        };

        let chat =
            msg.to_chat_message_with_target(Some("anthropic"), Some("claude-opus-4-1"), None);

        match &chat.content[0] {
            Content::Thinking {
                signature: None, ..
            } => {}
            _ => panic!("expected thinking block without signature"),
        }
    }
}

use async_trait::async_trait;
use querymt::Usage;
use querymt::error::LLMError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub seq: u64,
    pub timestamp: i64,
    pub session_id: String,
    pub kind: AgentEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEventKind {
    SessionCreated,
    PromptReceived {
        content: String,
    },
    UserMessageStored {
        content: String,
    },
    AssistantMessageStored {
        content: String,
    },
    LlmRequestStart {
        message_count: usize,
    },
    LlmRequestEnd {
        usage: Option<Usage>,
        tool_calls: usize,
    },
    ToolCallStart {
        tool_call_id: String,
        tool_name: String,
        arguments: String,
    },
    ToolCallEnd {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
        result: String,
    },
    SnapshotStart {
        policy: String,
    },
    SnapshotEnd {
        summary: Option<String>,
    },
    CompactionStart {
        token_estimate: usize,
    },
    CompactionEnd {
        summary: String,
        summary_len: usize,
    },
    MiddlewareInjected {
        message: String,
    },
    MiddlewareStopped {
        reason: String,
    },
    Cancelled,
    Error {
        message: String,
    },
}

#[async_trait]
pub trait EventObserver: Send + Sync {
    async fn on_event(&self, event: &AgentEvent) -> Result<(), LLMError>;
}

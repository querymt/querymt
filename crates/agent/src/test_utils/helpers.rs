//! Helper functions for creating test fixtures

use crate::middleware::{AgentStats, ConversationContext, ToolCall, ToolFunction};
use crate::session::store::{LLMConfig, Session};
use querymt::chat::{ChatMessage, ChatRole};
use std::sync::Arc;
use time::OffsetDateTime;

/// Creates a test conversation context with the given session ID and step count
pub fn test_context(session_id: &str, steps: usize) -> Arc<ConversationContext> {
    Arc::new(ConversationContext::new(
        session_id.into(),
        Arc::from([]),
        Arc::new(AgentStats {
            steps,
            ..Default::default()
        }),
        "mock".into(),
        "mock-model".into(),
    ))
}

/// Creates a test conversation context with the given number of user messages
/// for testing turn-based middleware
pub fn test_context_with_user_messages(
    session_id: &str,
    user_message_count: usize,
) -> Arc<ConversationContext> {
    let messages: Vec<ChatMessage> = (0..user_message_count)
        .map(|i| ChatMessage {
            role: ChatRole::User,
            message_type: querymt::chat::MessageType::Text,
            content: format!("User message {}", i),
            cache: None,
        })
        .collect();

    Arc::new(ConversationContext::new(
        session_id.into(),
        Arc::from(messages.into_boxed_slice()),
        Arc::new(AgentStats::default()),
        "mock".into(),
        "mock-model".into(),
    ))
}

/// Creates a mock tool call for middleware/state tests
pub fn mock_tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        function: ToolFunction {
            name: name.to_string(),
            arguments: args.to_string(),
        },
    }
}

/// Creates a mock querymt tool call for execution tests
pub fn mock_querymt_tool_call(id: &str, name: &str, args: &str) -> querymt::ToolCall {
    querymt::ToolCall {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: querymt::FunctionCall {
            name: name.to_string(),
            arguments: args.to_string(),
        },
    }
}

/// Creates a mock session for testing
pub fn mock_session(session_id: &str) -> Session {
    Session {
        id: 1,
        public_id: session_id.to_string(),
        name: None,
        cwd: None,
        created_at: Some(OffsetDateTime::now_utc()),
        updated_at: Some(OffsetDateTime::now_utc()),
        current_intent_snapshot_id: None,
        active_task_id: None,
        llm_config_id: Some(1),
        parent_session_id: None,
        fork_origin: None,
        fork_point_type: None,
        fork_point_ref: None,
        fork_instructions: None,
    }
}

/// Creates a mock LLM configuration for testing
pub fn mock_llm_config() -> LLMConfig {
    LLMConfig {
        id: 1,
        name: Some("test-config".to_string()),
        provider: "mock".to_string(),
        model: "mock-model".to_string(),
        params: None,
        created_at: Some(OffsetDateTime::now_utc()),
        updated_at: Some(OffsetDateTime::now_utc()),
    }
}

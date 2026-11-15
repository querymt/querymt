//! The `ChatSession` trait: a conversation handle with event callbacks.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// A chat session for maintaining conversation context
#[async_trait]
pub trait ChatSession: Send + Sync {
    /// Get the session ID
    fn id(&self) -> &str;

    /// Send a chat message within this session
    async fn chat(&self, prompt: &str) -> Result<String>;

    /// Register a callback for tool call events in this session
    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>);

    /// Register a callback for tool completion events in this session
    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>);

    /// Register a callback for message events in this session
    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>);

    /// Register a callback for error events in this session
    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>);
}

/// Extension trait for convenient callback registration
pub trait ChatSessionExt: ChatSession {
    fn on_tool_call(&self, callback: impl Fn(String, Value) + Send + Sync + 'static) {
        self.on_tool_call_boxed(Box::new(callback));
    }

    fn on_tool_complete(&self, callback: impl Fn(String, String) + Send + Sync + 'static) {
        self.on_tool_complete_boxed(Box::new(callback));
    }

    fn on_message(&self, callback: impl Fn(String, String) + Send + Sync + 'static) {
        self.on_message_boxed(Box::new(callback));
    }

    fn on_error(&self, callback: impl Fn(String) + Send + Sync + 'static) {
        self.on_error_boxed(Box::new(callback));
    }
}

// Blanket implementation
impl<T: ChatSession + ?Sized> ChatSessionExt for T {}

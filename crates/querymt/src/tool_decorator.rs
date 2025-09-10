use crate::{
    chat::{BasicChatProvider, ChatMessage, ChatResponse, FunctionTool, ToolChatProvider},
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    LLMProvider, Tool,
};
use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Adapter interface for your host‐side implementations
#[async_trait]
pub trait CallFunctionTool: Send + Sync {
    fn descriptor(&self) -> Tool;
    async fn call(&self, args: Value) -> anyhow::Result<String>;

    /// Returns the server name for server-aware tools (e.g., MCP tools).
    /// Returns None for tools that don't have server information.
    fn server_name(&self) -> Option<&str> {
        None
    }
}

/*
#[derive(Clone, Serialize, JsonSchema)]
pub struct CallableFunctionTool {
    /// Flattened into the JSON so the LLM sees `name`, `description`, `parameters`
    #[serde(flatten)]
    pub meta: FunctionTool,

    /// Not serialized; the actual Rust callback logic
    #[serde(skip)]
    callback:
        Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> + Send + Sync>,
}

impl CallableFunctionTool {
    /// Create a new one by giving it its metadata **and** an async function
    pub fn new<F, Fut>(meta: FunctionTool, f: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        Self {
            meta,
            callback: Arc::new(move |args| Box::pin(f(args))),
        }
    }
}

#[async_trait]
impl CallFunctionTool for CallableFunctionTool {
    fn descriptor(&self) -> FunctionTool {
        self.meta.clone()
    }

    async fn call(&self, args: Value) -> Result<String> {
        (self.callback)(args).await
    }
}
*/

pub struct ToolEnabledProvider {
    inner: Box<dyn LLMProvider + Send + Sync>,
    registry: HashMap<String, Box<dyn CallFunctionTool>>,
    tool_list: Vec<Tool>,
}

impl ToolEnabledProvider {
    pub fn new(
        inner: Box<dyn LLMProvider + Send + Sync>,
        registry: HashMap<String, Box<dyn CallFunctionTool>>,
    ) -> Self {
        let tool_list = registry.values().map(|t| t.descriptor()).collect();
        ToolEnabledProvider {
            inner,
            registry,
            tool_list,
        }
    }
}

#[async_trait]
impl CompletionProvider for ToolEnabledProvider {
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        self.inner.complete(req).await
    }
}

#[async_trait]
impl EmbeddingProvider for ToolEnabledProvider {
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        self.inner.embed(input).await
    }
}

#[async_trait]
impl BasicChatProvider for ToolEnabledProvider {
    async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError> {
        self.inner
            .chat_with_tools(messages, Some(&self.tool_list))
            .await
    }
}
#[async_trait]
impl ToolChatProvider for ToolEnabledProvider {
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        let to_send = tools.unwrap_or(&self.tool_list);
        self.inner.chat_with_tools(messages, Some(to_send)).await
    }
}

#[async_trait]
impl LLMProvider for ToolEnabledProvider {
    fn tools(&self) -> Option<&[Tool]> {
        Some(&self.tool_list)
    }

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, LLMError> {
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| LLMError::InvalidRequest(format!("unknown tool `{}`", name)))?;
        tool.call(args)
            .await
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))
    }

    fn tool_server_name(&self, name: &str) -> Option<&str> {
        self.registry.get(name).and_then(|tool| tool.server_name())
    }
}

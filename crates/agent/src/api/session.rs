//! Agent session implementation

use super::callbacks::EventCallbacksState;
use super::utils::latest_assistant_message;
use crate::agent::LocalAgentHandle as AgentHandle;
use crate::runner::ChatSession;
use crate::send_agent::SendAgent;
use agent_client_protocol::{ContentBlock, PromptRequest, TextContent};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use querymt::LLMParams;
use serde_json::Value;
use std::sync::Arc;

pub struct AgentSession {
    agent: Arc<AgentHandle>,
    session_id: String,
    callbacks: Arc<EventCallbacksState>,
}

impl AgentSession {
    pub(super) fn new(agent: Arc<AgentHandle>, session_id: String) -> Self {
        let callbacks = Arc::new(EventCallbacksState::new(Some(session_id.clone())));
        Self {
            agent,
            session_id,
            callbacks,
        }
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }

    pub fn on_tool_call<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.agent.subscribe_events());
        self
    }

    pub fn on_tool_complete<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.agent.subscribe_events());
        self
    }

    pub fn on_message<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.agent.subscribe_events());
        self
    }

    pub fn on_delegation<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_delegation(callback);
        self.callbacks
            .ensure_listener(self.agent.subscribe_events());
        self
    }

    pub fn on_error<F>(&self, callback: F) -> &Self
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.agent.subscribe_events());
        self
    }

    pub async fn chat(&self, prompt: &str) -> Result<String> {
        let request = PromptRequest::new(
            self.session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        self.agent
            .prompt(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let history = self
            .agent
            .config
            .provider
            .history_store()
            .get_history(&self.session_id)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        latest_assistant_message(&history).ok_or_else(|| anyhow!("No assistant response found"))
    }

    pub async fn set_provider(&self, provider: &str, model: &str) -> Result<()> {
        self.agent
            .set_provider(&self.session_id, provider, model)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(())
    }

    pub async fn set_llm_config(&self, config: LLMParams) -> Result<()> {
        self.agent
            .set_llm_config(&self.session_id, config)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl ChatSession for AgentSession {
    fn id(&self) -> &str {
        &self.session_id
    }

    async fn chat(&self, prompt: &str) -> Result<String> {
        AgentSession::chat(self, prompt).await
    }

    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>) {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.agent.subscribe_events());
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.agent.subscribe_events());
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.agent.subscribe_events());
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.agent.subscribe_events());
    }
}

//! Event callback system for the simple API

use crate::events::{AgentEventKind, EventEnvelope};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

pub type ToolCallCallback = Box<dyn Fn(String, Value) + Send + Sync>;
pub type ToolCompleteCallback = Box<dyn Fn(String, String) + Send + Sync>;
pub type MessageCallback = Box<dyn Fn(String, String) + Send + Sync>;
pub type DelegationCallback = Box<dyn Fn(String, String) + Send + Sync>;
pub type ErrorCallback = Box<dyn Fn(String) + Send + Sync>;

#[derive(Default)]
pub(super) struct EventCallbacks {
    pub on_tool_call: Option<ToolCallCallback>,
    pub on_tool_complete: Option<ToolCompleteCallback>,
    pub on_message: Option<MessageCallback>,
    pub on_delegation: Option<DelegationCallback>,
    pub on_error: Option<ErrorCallback>,
}

pub(super) struct EventCallbacksState {
    pub callbacks: Arc<Mutex<EventCallbacks>>,
    task: Mutex<Option<JoinHandle<()>>>,
    session_filter: Option<String>,
}

impl EventCallbacksState {
    pub fn new(session_filter: Option<String>) -> Self {
        Self {
            callbacks: Arc::new(Mutex::new(EventCallbacks::default())),
            task: Mutex::new(None),
            session_filter,
        }
    }

    pub fn on_tool_call<F>(&self, callback: F)
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        if let Ok(mut callbacks) = self.callbacks.lock() {
            callbacks.on_tool_call = Some(Box::new(callback));
        }
    }

    pub fn on_tool_complete<F>(&self, callback: F)
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        if let Ok(mut callbacks) = self.callbacks.lock() {
            callbacks.on_tool_complete = Some(Box::new(callback));
        }
    }

    pub fn on_message<F>(&self, callback: F)
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        if let Ok(mut callbacks) = self.callbacks.lock() {
            callbacks.on_message = Some(Box::new(callback));
        }
    }

    pub fn on_delegation<F>(&self, callback: F)
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        if let Ok(mut callbacks) = self.callbacks.lock() {
            callbacks.on_delegation = Some(Box::new(callback));
        }
    }

    pub fn on_error<F>(&self, callback: F)
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        if let Ok(mut callbacks) = self.callbacks.lock() {
            callbacks.on_error = Some(Box::new(callback));
        }
    }

    pub fn ensure_listener(&self, receiver: tokio::sync::broadcast::Receiver<EventEnvelope>) {
        let mut task = self.task.lock().unwrap();
        if task.is_some() {
            return;
        }
        let callbacks = self.callbacks.clone();
        let session_filter = self.session_filter.clone();
        let handle = tokio::spawn(async move {
            let mut receiver = receiver;
            loop {
                match receiver.recv().await {
                    Ok(envelope) => {
                        if let Some(filter) = session_filter.as_ref()
                            && envelope.session_id() != *filter
                        {
                            continue;
                        }
                        if let Ok(callbacks) = callbacks.lock() {
                            dispatch_event(&callbacks, &envelope);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *task = Some(handle);
    }
}

impl Drop for EventCallbacksState {
    fn drop(&mut self) {
        if let Ok(mut task) = self.task.lock()
            && let Some(handle) = task.take()
        {
            handle.abort();
        }
    }
}

fn dispatch_event(callbacks: &EventCallbacks, event: &EventEnvelope) {
    match event.kind() {
        AgentEventKind::ToolCallStart {
            tool_name,
            arguments,
            ..
        } => {
            if let Some(callback) = callbacks.on_tool_call.as_ref() {
                let args = serde_json::from_str(arguments)
                    .unwrap_or_else(|_| Value::String(arguments.clone()));
                callback(tool_name.clone(), args);
            }
        }
        AgentEventKind::ToolCallEnd {
            tool_name, result, ..
        } => {
            if let Some(callback) = callbacks.on_tool_complete.as_ref() {
                callback(tool_name.clone(), result.clone());
            }
        }
        AgentEventKind::UserMessageStored { content } => {
            if let Some(callback) = callbacks.on_message.as_ref() {
                callback("user".to_string(), content.clone());
            }
        }
        AgentEventKind::AssistantMessageStored { content, .. } => {
            if let Some(callback) = callbacks.on_message.as_ref() {
                callback("assistant".to_string(), content.clone());
            }
        }
        AgentEventKind::DelegationRequested { delegation } => {
            if let Some(callback) = callbacks.on_delegation.as_ref() {
                callback(
                    delegation.target_agent_id.clone(),
                    delegation.objective.clone(),
                );
            }
        }
        AgentEventKind::Error { message } => {
            if let Some(callback) = callbacks.on_error.as_ref() {
                callback(message.clone());
            }
        }
        _ => {}
    }
}

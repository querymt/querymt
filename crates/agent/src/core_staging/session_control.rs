//! Transport-neutral, persisted control state for a single session.

use std::collections::HashMap;

use querymt::chat::ReasoningEffort;
use serde::{Deserialize, Serialize};

use crate::agent::core::AgentMode;

/// Model and provider route selected for one session mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionModelBinding {
    pub model_id: String,
    pub provider: String,
    pub model: String,
    pub llm_config_id: i64,
    pub provider_node_id: Option<String>,
}

impl SessionModelBinding {
    pub fn mode_key(mode: AgentMode) -> String {
        mode.as_str().to_string()
    }
}

/// Authoritative control state owned by a session actor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionControlState {
    pub revision: u64,
    pub active_mode: AgentMode,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub effective_model: SessionModelBinding,
    pub mode_models: HashMap<String, SessionModelBinding>,
}

impl SessionControlState {
    pub fn binding_for(&self, mode: AgentMode) -> Option<&SessionModelBinding> {
        self.mode_models.get(mode.as_str())
    }

    pub fn set_binding(&mut self, mode: AgentMode, binding: SessionModelBinding) {
        self.mode_models.insert(mode.as_str().to_string(), binding);
    }
}

/// Rust-native model selection used by ACP, dashboard, and direct callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModelSelection {
    pub model_id: String,
    pub provider_node_id: Option<String>,
}

/// Result returned by every typed session-control operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionControlTransition {
    pub previous: SessionControlState,
    pub current: SessionControlState,
}

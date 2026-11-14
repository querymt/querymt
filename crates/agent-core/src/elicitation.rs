//! Elicitation action and response types.

use serde::{Deserialize, Serialize};

/// Action taken in response to an elicitation request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

/// Response to an elicitation request from the UI/ACP client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    pub content: Option<serde_json::Value>,
}

/// Content key carrying the selected option of a permission answer.
pub const PERMISSION_OPTION_KEY: &str = "option_id";

impl ElicitationResponse {
    /// Permission answer selecting `option_id`.
    pub fn permission_selected(option_id: impl Into<String>) -> Self {
        Self {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({ PERMISSION_OPTION_KEY: option_id.into() })),
        }
    }

    /// Option selected by a permission answer, if any.
    pub fn selected_permission_option(&self) -> Option<&str> {
        if self.action != ElicitationAction::Accept {
            return None;
        }
        self.content
            .as_ref()
            .and_then(|content| content.get(PERMISSION_OPTION_KEY))
            .and_then(|value| value.as_str())
    }
}

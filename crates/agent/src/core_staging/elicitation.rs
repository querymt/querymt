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

impl From<ElicitationAction> for rmcp::model::ElicitationAction {
    fn from(action: ElicitationAction) -> Self {
        match action {
            ElicitationAction::Accept => rmcp::model::ElicitationAction::Accept,
            ElicitationAction::Decline => rmcp::model::ElicitationAction::Decline,
            ElicitationAction::Cancel => rmcp::model::ElicitationAction::Cancel,
        }
    }
}

impl From<rmcp::model::ElicitationAction> for ElicitationAction {
    fn from(action: rmcp::model::ElicitationAction) -> Self {
        match action {
            rmcp::model::ElicitationAction::Accept => ElicitationAction::Accept,
            rmcp::model::ElicitationAction::Decline => ElicitationAction::Decline,
            rmcp::model::ElicitationAction::Cancel => ElicitationAction::Cancel,
            // Non-exhaustive upstream: treat unknown actions as a cancel.
            _ => ElicitationAction::Cancel,
        }
    }
}

/// Response to an elicitation request from the UI/ACP client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    pub content: Option<serde_json::Value>,
}

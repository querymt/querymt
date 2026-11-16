//! Structured user prompt records served with session loads, older-history
//! pages and peer prompt requests.

use crate::model::{AgentMessage, MessagePart};
use crate::protocol::ContentBlock;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserPromptRecord {
    pub message_id: String,
    /// Zero-based position in persisted message history; this is not an event sequence.
    #[typeshare(serialized_as = "number")]
    pub message_order: u64,
    #[typeshare(serialized_as = "number")]
    pub timestamp: i64,
    #[typeshare(serialized_as = "any")]
    pub blocks: Vec<ContentBlock>,
}

/// Restrict prompt records to `ids` (`None` keeps all).
pub fn filter_prompt_records(
    records: Vec<UserPromptRecord>,
    ids: Option<&[String]>,
) -> Vec<UserPromptRecord> {
    match ids {
        None => records,
        Some(ids) => records
            .into_iter()
            .filter(|record| ids.contains(&record.message_id))
            .collect(),
    }
}

pub fn user_prompt_records(messages: &[AgentMessage]) -> Vec<UserPromptRecord> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(message_order, message)| {
            let blocks = message.parts.iter().find_map(|part| match part {
                MessagePart::Prompt { blocks } | MessagePart::Steering { blocks, .. } => {
                    Some(blocks.clone())
                }
                _ => None,
            })?;
            Some(UserPromptRecord {
                message_id: message.id.clone(),
                message_order: message_order as u64,
                timestamp: message.created_at,
                blocks,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ImageContent, TextContent};
    use querymt::chat::ChatRole;

    #[test]
    fn user_prompt_projection_preserves_message_identity_order_and_blocks() {
        let messages = vec![
            AgentMessage {
                id: "assistant".to_string(),
                session_id: "s1".to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::Text {
                    content: "response".to_string(),
                }],
                created_at: 10,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
            AgentMessage {
                id: "user-1".to_string(),
                session_id: "s1".to_string(),
                role: ChatRole::User,
                parts: vec![MessagePart::Prompt {
                    blocks: vec![
                        ContentBlock::Text(TextContent::new("look")),
                        ContentBlock::Image(ImageContent::new("AQID", "image/png")),
                    ],
                }],
                created_at: 11,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
        ];

        let records = user_prompt_records(&messages);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message_id, "user-1");
        // messageOrder is the zero-based persisted history position, not the
        // PromptReceived event's stream sequence.
        assert_eq!(records[0].message_order, 1);
        assert_eq!(records[0].timestamp, 11);
        assert!(matches!(records[0].blocks[1], ContentBlock::Image(_)));
        let value = serde_json::to_value(&records[0]).unwrap();
        assert_eq!(value["messageId"], "user-1");
        assert_eq!(value["messageOrder"], 1);
        assert_eq!(value["blocks"][1]["type"], "image");
    }

    #[test]
    fn user_prompt_projection_includes_steering_parts() {
        let messages = vec![
            AgentMessage {
                id: "assistant".to_string(),
                session_id: "s1".to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::Text {
                    content: "response".to_string(),
                }],
                created_at: 10,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
            AgentMessage {
                id: "steer-1".to_string(),
                session_id: "s1".to_string(),
                role: ChatRole::User,
                parts: vec![MessagePart::Steering {
                    run_id: "run-1".to_string(),
                    client_input_id: Some("cid-1".to_string()),
                    blocks: vec![
                        ContentBlock::Text(TextContent::new("steer")),
                        ContentBlock::Image(ImageContent::new("AQID", "image/png")),
                    ],
                }],
                created_at: 12,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
        ];

        let records = user_prompt_records(&messages);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message_id, "steer-1");
        // messageOrder counts all persisted messages: the text-only assistant
        // message is skipped as a projection source but still occupies index 0.
        assert_eq!(records[0].message_order, 1);
        assert_eq!(records[0].timestamp, 12);
        assert!(matches!(records[0].blocks[0], ContentBlock::Text(_)));
        assert!(matches!(records[0].blocks[1], ContentBlock::Image(_)));
        let value = serde_json::to_value(&records[0]).unwrap();
        assert_eq!(value["messageId"], "steer-1");
        assert_eq!(value["messageOrder"], 1);
        assert_eq!(value["timestamp"], 12);
        assert_eq!(value["blocks"][1]["type"], "image");
    }
}

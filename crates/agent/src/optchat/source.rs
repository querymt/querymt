use anyhow::Result;
use querymt::chat::{ChatInputPart, ChatOutputItem, ChatRole, ToolResultPart};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{AgentMessage, MessagePart};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Entry {
    pub kind: String,
    pub text: String,
    pub date: i64,
    pub attachments: Vec<ToolResultPart>,
}

impl Entry {
    pub fn line(&self) -> String {
        format!("{}: {}", self.kind, self.text)
    }
}

pub(super) struct Source {
    pub id: String,
    pub fingerprint: String,
    pub entries: Vec<Entry>,
}

pub(super) fn sources(messages: &[AgentMessage]) -> Result<Vec<Source>> {
    messages
        .iter()
        .map(|message| {
            let mut entries = Vec::new();
            let kind = if message.role == ChatRole::User {
                "user"
            } else {
                "agent"
            };
            let mut add = |kind: &str, text: String, mut attachments: Vec<ToolResultPart>| {
                if text.is_empty() && attachments.is_empty() {
                    return;
                }
                let text = if kind == "echo" {
                    clip_tool_output(&text, 30_000)
                } else {
                    text
                };
                // Large ordinary messages are split, never discarded. Retrieval keeps attachments.
                let chunks = split_utf8(&text, 30_000);
                for (index, text) in chunks.into_iter().enumerate() {
                    entries.push(Entry {
                        kind: kind.into(),
                        text,
                        date: message.created_at,
                        attachments: if index == 0 {
                            std::mem::take(&mut attachments)
                        } else {
                            Vec::new()
                        },
                    });
                }
            };
            let canonical = message
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::Output { .. }));
            for part in &message.parts {
                match part {
                    MessagePart::Text { content } if !canonical => {
                        add(kind, content.clone(), vec![])
                    }
                    MessagePart::Prompt { blocks } | MessagePart::Steering { blocks, .. } => {
                        let parts = crate::model::convert_prompt_blocks(blocks, None)?;
                        let mut text = Vec::with_capacity(parts.len());
                        let mut attachments = Vec::new();
                        for part in parts {
                            match part {
                                ChatInputPart::Text { text: t } => text.push(t),
                                ChatInputPart::Attachment(media) => {
                                    text.push("[attachment: open original with zoom]".into());
                                    attachments.push(ToolResultPart::Attachment(media));
                                }
                                _ => {}
                            }
                        }
                        add("user", text.join("\n"), attachments);
                    }
                    MessagePart::ToolUse(call) if !canonical => add(
                        "tool",
                        format!("{} {}", call.function.name, call.function.arguments),
                        vec![],
                    ),
                    MessagePart::ToolResult {
                        content,
                        tool_name,
                        is_error,
                        ..
                    } => {
                        let text = content
                            .iter()
                            .filter_map(ToolResultPart::as_text)
                            .collect::<Vec<_>>()
                            .join("\n");
                        let attachments = content
                            .iter()
                            .filter(|p| p.as_attachment().is_some())
                            .cloned()
                            .collect();
                        add(
                            "echo",
                            format!(
                                "{}{}: {}",
                                tool_name.as_deref().unwrap_or("tool"),
                                if *is_error { " (error)" } else { "" },
                                text
                            ),
                            attachments,
                        );
                    }
                    MessagePart::Output { output } => {
                        for item in &output.items {
                            match item {
                                ChatOutputItem::Message(m) => {
                                    let mut text = Vec::new();
                                    let mut attachments = Vec::new();
                                    for part in &m.parts {
                                        match part {
                                            querymt::chat::ChatMessagePart::Text { text: value, .. } => text.push(value.as_str()),
                                            querymt::chat::ChatMessagePart::Refusal { refusal, .. } => text.push(refusal.as_str()),
                                            querymt::chat::ChatMessagePart::Media(media) => {
                                                text.push("[generated attachment: open original with zoom]");
                                                attachments.push(ToolResultPart::Attachment(media.clone()));
                                            }
                                            querymt::chat::ChatMessagePart::Opaque(_) => {}
                                        }
                                    }
                                    add("agent", text.join("\n"), attachments);
                                }
                                ChatOutputItem::FunctionCall(call) => {
                                    add("tool", format!("{} {}", call.name, call.arguments), vec![])
                                }
                                // Reasoning and provider-opaque state never enter the memory log.
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&entries)?));
            Ok(Source {
                id: message.id.clone(),
                fingerprint,
                entries,
            })
        })
        .collect()
}

pub(super) fn prefix_utf8(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn split_utf8(text: &str, max: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut remaining = text;
    let mut chunks = Vec::new();
    while !remaining.is_empty() {
        let chunk = prefix_utf8(remaining, max);
        chunks.push(chunk.to_string());
        remaining = &remaining[chunk.len()..];
    }
    chunks
}

fn clip_tool_output(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let half = max_chars / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(half)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n[OptChat tool output clipped: middle omitted]\n{tail}")
}

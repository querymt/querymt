use crate::acp::protocol::{ContentBlock, EmbeddedResourceResource};
use crate::agent::utils::truncate_to_bytes;
use crate::index::merkle::DiffPaths;
use base64::Engine as _;
use querymt::{
    ToolCall,
    chat::{ChatMessage, ChatRole, Content},
};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read};

pub const MAX_IMAGES_PER_PROMPT: usize = 8;
pub const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_TOTAL_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

const SUPPORTED_IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PromptContentError {
    #[error("prompt must contain at least one content block")]
    EmptyPrompt,
    #[error("unsupported image MIME type at block {index}: {mime_type}")]
    UnsupportedImageMime { index: usize, mime_type: String },
    #[error("invalid base64 attachment data at block {index}: {reason}")]
    InvalidBase64 { index: usize, reason: String },
    #[error("attachment at block {index} is {bytes} bytes; maximum is {max_bytes} bytes")]
    AttachmentTooLarge {
        index: usize,
        bytes: usize,
        max_bytes: usize,
    },
    #[error("attachment data at block {index} exceeds the maximum of {max_bytes} decoded bytes")]
    EncodedAttachmentTooLarge { index: usize, max_bytes: usize },
    #[error("prompt contains {count} images; maximum is {max_count}")]
    TooManyImages { count: usize, max_count: usize },
    #[error("prompt attachment data totals {bytes} bytes; maximum is {max_bytes} bytes")]
    AttachmentsTooLarge { bytes: usize, max_bytes: usize },
    #[error("text attachment at block {index} is not valid UTF-8")]
    InvalidTextResource { index: usize },
}

pub fn prompt_contains_images(blocks: &[ContentBlock]) -> bool {
    blocks.iter().any(|block| match block {
        ContentBlock::Image(_) => true,
        ContentBlock::Resource(resource) => match &resource.resource {
            EmbeddedResourceResource::BlobResourceContents(blob) => blob
                .mime_type
                .as_deref()
                .is_some_and(|mime| mime.starts_with("image/")),
            _ => false,
        },
        _ => false,
    })
}

pub fn validate_prompt_blocks(blocks: &[ContentBlock]) -> Result<(), PromptContentError> {
    if blocks.is_empty() {
        return Err(PromptContentError::EmptyPrompt);
    }

    let mut image_count = 0usize;
    let mut total_attachment_bytes = 0usize;
    for (index, block) in blocks.iter().enumerate() {
        match block {
            ContentBlock::Text(_) | ContentBlock::ResourceLink(_) | ContentBlock::Audio(_) => {}
            ContentBlock::Image(image) => {
                let bytes = validate_encoded_attachment(index, &image.data, false)?;
                validate_attachment_size(index, bytes, &mut total_attachment_bytes)?;
                validate_image(index, &image.mime_type, &mut image_count)?;
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    validate_attachment_size(index, text.text.len(), &mut total_attachment_bytes)?;
                }
                EmbeddedResourceResource::BlobResourceContents(blob) => {
                    let mime_type = blob
                        .mime_type
                        .as_deref()
                        .unwrap_or("application/octet-stream");
                    let bytes = validate_encoded_attachment(
                        index,
                        &blob.blob,
                        mime_type.starts_with("text/"),
                    )?;
                    validate_attachment_size(index, bytes, &mut total_attachment_bytes)?;
                    if mime_type.starts_with("image/") {
                        validate_image(index, mime_type, &mut image_count)?;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

pub fn convert_prompt_blocks(
    blocks: &[ContentBlock],
    max_text_bytes: Option<usize>,
) -> Result<Vec<Content>, PromptContentError> {
    if blocks.is_empty() {
        return Err(PromptContentError::EmptyPrompt);
    }

    let mut converted = Vec::with_capacity(blocks.len());
    let mut remaining_text_bytes = max_text_bytes;
    let mut image_count = 0usize;
    let mut total_attachment_bytes = 0usize;

    for (index, block) in blocks.iter().enumerate() {
        match block {
            ContentBlock::Text(text) => converted.push(Content::text(limit_text(
                &text.text,
                &mut remaining_text_bytes,
            ))),
            ContentBlock::Image(image) => {
                let data = decode_attachment(index, &image.data)?;
                validate_attachment_size(index, data.len(), &mut total_attachment_bytes)?;
                validate_image(index, &image.mime_type, &mut image_count)?;
                converted.push(Content::image(image.mime_type.clone(), data));
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    validate_attachment_size(index, text.text.len(), &mut total_attachment_bytes)?;
                    let contextual = format!("[Embedded Resource: {}]\n{}", text.uri, text.text);
                    converted.push(Content::text(limit_text(
                        &contextual,
                        &mut remaining_text_bytes,
                    )));
                }
                EmbeddedResourceResource::BlobResourceContents(blob) => {
                    let data = decode_attachment(index, &blob.blob)?;
                    validate_attachment_size(index, data.len(), &mut total_attachment_bytes)?;
                    let mime_type = blob
                        .mime_type
                        .as_deref()
                        .unwrap_or("application/octet-stream");
                    if mime_type.starts_with("image/") {
                        validate_image(index, mime_type, &mut image_count)?;
                        converted.push(Content::image(mime_type.to_string(), data));
                    } else if mime_type == "application/pdf" {
                        converted.push(Content::pdf(data));
                    } else if mime_type.starts_with("text/") {
                        let text = String::from_utf8(data)
                            .map_err(|_| PromptContentError::InvalidTextResource { index })?;
                        let contextual = format!("[Embedded Resource: {}]\n{}", blob.uri, text);
                        converted.push(Content::text(limit_text(
                            &contextual,
                            &mut remaining_text_bytes,
                        )));
                    } else {
                        let marker = format!(
                            "[Attached resource: {} ({mime_type}, {} bytes)]",
                            blob.uri,
                            data.len()
                        );
                        converted.push(Content::text(limit_text(
                            &marker,
                            &mut remaining_text_bytes,
                        )));
                    }
                }
                _ => converted.push(Content::text("[Unsupported embedded resource]")),
            },
            ContentBlock::ResourceLink(link) => {
                converted.push(Content::resource_link(link.uri.clone()));
            }
            ContentBlock::Audio(audio) => {
                let marker = format!("[Audio attachment: {}]", audio.mime_type);
                converted.push(Content::text(limit_text(
                    &marker,
                    &mut remaining_text_bytes,
                )));
            }
            _ => converted.push(Content::text("[Unsupported content block]")),
        }
    }

    Ok(converted)
}

fn validate_encoded_attachment(
    index: usize,
    encoded: &str,
    require_utf8: bool,
) -> Result<usize, PromptContentError> {
    validate_encoded_attachment_bound(index, encoded)?;
    let mut decoder = base64::read::DecoderReader::new(
        Cursor::new(encoded.as_bytes()),
        &base64::engine::general_purpose::STANDARD,
    );
    let mut buffer = [0u8; 8192];
    let mut utf8_tail = Vec::new();
    let mut decoded_bytes = 0usize;
    loop {
        let read =
            decoder
                .read(&mut buffer)
                .map_err(|error| PromptContentError::InvalidBase64 {
                    index,
                    reason: error.to_string(),
                })?;
        if read == 0 {
            break;
        }
        decoded_bytes = decoded_bytes.saturating_add(read);
        if require_utf8 {
            utf8_tail.extend_from_slice(&buffer[..read]);
            match std::str::from_utf8(&utf8_tail) {
                Ok(_) => utf8_tail.clear(),
                Err(error) if error.error_len().is_some() => {
                    return Err(PromptContentError::InvalidTextResource { index });
                }
                Err(error) => {
                    utf8_tail.drain(..error.valid_up_to());
                }
            }
        }
    }
    if require_utf8 && !utf8_tail.is_empty() {
        return Err(PromptContentError::InvalidTextResource { index });
    }
    Ok(decoded_bytes)
}

fn validate_encoded_attachment_bound(
    index: usize,
    encoded: &str,
) -> Result<(), PromptContentError> {
    // Standard base64 has no ignored whitespace, so this bounds allocation before decoding.
    let max_encoded_bytes = MAX_ATTACHMENT_BYTES.div_ceil(3).saturating_mul(4);
    if encoded.len() > max_encoded_bytes {
        return Err(PromptContentError::EncodedAttachmentTooLarge {
            index,
            max_bytes: MAX_ATTACHMENT_BYTES,
        });
    }
    Ok(())
}

fn decode_attachment(index: usize, encoded: &str) -> Result<Vec<u8>, PromptContentError> {
    validate_encoded_attachment_bound(index, encoded)?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| PromptContentError::InvalidBase64 {
            index,
            reason: error.to_string(),
        })
}

fn validate_attachment_size(
    index: usize,
    bytes: usize,
    total_attachment_bytes: &mut usize,
) -> Result<(), PromptContentError> {
    if bytes > MAX_ATTACHMENT_BYTES {
        return Err(PromptContentError::AttachmentTooLarge {
            index,
            bytes,
            max_bytes: MAX_ATTACHMENT_BYTES,
        });
    }

    *total_attachment_bytes = total_attachment_bytes.saturating_add(bytes);
    if *total_attachment_bytes > MAX_TOTAL_ATTACHMENT_BYTES {
        return Err(PromptContentError::AttachmentsTooLarge {
            bytes: *total_attachment_bytes,
            max_bytes: MAX_TOTAL_ATTACHMENT_BYTES,
        });
    }
    Ok(())
}

fn validate_image(
    index: usize,
    mime_type: &str,
    image_count: &mut usize,
) -> Result<(), PromptContentError> {
    if !SUPPORTED_IMAGE_MIME_TYPES.contains(&mime_type) {
        return Err(PromptContentError::UnsupportedImageMime {
            index,
            mime_type: mime_type.to_string(),
        });
    }
    *image_count += 1;
    if *image_count > MAX_IMAGES_PER_PROMPT {
        return Err(PromptContentError::TooManyImages {
            count: *image_count,
            max_count: MAX_IMAGES_PER_PROMPT,
        });
    }
    Ok(())
}

fn limit_text(text: &str, remaining: &mut Option<usize>) -> String {
    let Some(remaining_bytes) = remaining.as_mut() else {
        return text.to_string();
    };
    let limited = truncate_to_bytes(text, *remaining_bytes);
    *remaining_bytes = remaining_bytes.saturating_sub(limited.len());
    limited
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum MessagePart {
    Text {
        content: String,
    },
    Prompt {
        blocks: Vec<ContentBlock>,
    },
    Steering {
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_input_id: Option<String>,
        blocks: Vec<ContentBlock>,
    },
    Reasoning {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        time_ms: Option<u64>,
    },
    StepStart {
        step_id: String,
        description: String,
    },
    StepFinish {
        step_id: String,
        success: bool,
        cost: Option<f64>,
    },
    ToolUse(ToolCall),
    HookContext {
        event_name: String,
        handler_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        content: String,
    },
    ToolResult {
        call_id: String,
        content: Vec<Content>,
        is_error: bool,
        tool_name: Option<String>,
        tool_arguments: Option<String>,
        /// Timestamp when this tool result was marked as compacted (pruned)
        /// When set, the content should be replaced with a placeholder in LLM context
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted_at: Option<i64>,
    },
    Patch {
        id: String,
        files: Vec<String>,
        diff: String,
    },
    Snapshot {
        root_hash: crate::hash::RapidHash,
        changed_paths: DiffPaths,
    },
    Compaction {
        summary: String,
        original_token_count: usize,
    },
    /// User-side compaction request: paired with the following Compaction (assistant) message
    /// to form a natural user→assistant exchange after context compaction.
    CompactionRequest {
        original_token_count: usize,
    },
    /// Turn snapshot start: worktree state before turn (user prompt)
    TurnSnapshotStart {
        turn_id: String,
        snapshot_id: String,
    },
    /// Turn snapshot patch: worktree state after turn completes, with changed files
    TurnSnapshotPatch {
        turn_id: String,
        snapshot_id: String,
        changed_paths: Vec<String>,
    },
}

impl MessagePart {
    pub fn type_name(&self) -> &'static str {
        match self {
            MessagePart::Text { .. } => "text",
            MessagePart::Prompt { .. } => "prompt",
            MessagePart::Steering { .. } => "steering",
            MessagePart::Reasoning { .. } => "reasoning",
            MessagePart::StepStart { .. } => "step_start",
            MessagePart::StepFinish { .. } => "step_finish",
            MessagePart::ToolUse(_) => "tool_use",
            MessagePart::HookContext { .. } => "hook_context",
            MessagePart::ToolResult { .. } => "tool_result",
            MessagePart::Patch { .. } => "patch",
            MessagePart::Snapshot { .. } => "snapshot",
            MessagePart::Compaction { .. } => "compaction",
            MessagePart::CompactionRequest { .. } => "compaction_request",
            MessagePart::TurnSnapshotStart { .. } => "turn_snapshot_start",
            MessagePart::TurnSnapshotPatch { .. } => "turn_snapshot_patch",
        }
    }

    /// Get the diff summary for a Snapshot part, or None for other part types
    pub fn diff_summary(&self) -> Option<String> {
        match self {
            MessagePart::Snapshot { changed_paths, .. } => Some(changed_paths.summary()),
            _ => None,
        }
    }

    /// Get the changed paths for a Snapshot part, or None for other part types
    pub fn changed_paths(&self) -> Option<&DiffPaths> {
        match self {
            MessagePart::Snapshot { changed_paths, .. } => Some(changed_paths),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: String,
    pub session_id: String,
    pub role: ChatRole,
    pub parts: Vec<MessagePart>,
    pub created_at: i64,
    pub parent_message_id: Option<String>,
    /// Provider that generated this assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_provider: Option<String>,
    /// Model that generated this assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model: Option<String>,
}

impl AgentMessage {
    pub fn new(session_id: String, role: ChatRole) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            session_id,
            role,
            parts: Vec::new(),
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    pub fn to_chat_message(&self) -> Result<ChatMessage, PromptContentError> {
        self.to_chat_message_with_target(None, None, None)
    }

    pub fn to_chat_message_with_max_prompt_bytes(
        &self,
        max_prompt_bytes: Option<usize>,
    ) -> Result<ChatMessage, PromptContentError> {
        self.to_chat_message_with_target(None, None, max_prompt_bytes)
    }

    pub fn to_chat_message_with_target(
        &self,
        target_provider: Option<&str>,
        target_model: Option<&str>,
        max_prompt_bytes: Option<usize>,
    ) -> Result<ChatMessage, PromptContentError> {
        let mut blocks = Vec::new();

        let preserve_provider_metadata = match (
            target_provider,
            target_model,
            self.source_provider.as_deref(),
            self.source_model.as_deref(),
        ) {
            (Some(tp), Some(tm), Some(sp), Some(sm)) => tp == sp && tm == sm,
            _ => true,
        };

        for part in &self.parts {
            match part {
                MessagePart::Text { content } => {
                    blocks.push(Content::text(content));
                }
                MessagePart::Prompt {
                    blocks: prompt_blocks,
                }
                | MessagePart::Steering {
                    blocks: prompt_blocks,
                    ..
                } => {
                    blocks.extend(convert_prompt_blocks(prompt_blocks, max_prompt_bytes)?);
                }
                MessagePart::Reasoning {
                    content, signature, ..
                } => {
                    blocks.push(Content::Thinking {
                        text: content.clone(),
                        signature: if preserve_provider_metadata {
                            signature.clone()
                        } else {
                            None
                        },
                    });
                }
                MessagePart::ToolUse(tc) => {
                    blocks.push(Content::tool_use(
                        &tc.id,
                        &tc.function.name,
                        serde_json::from_str(&tc.function.arguments)
                            .unwrap_or_else(|_| serde_json::Value::Object(Default::default())),
                    ));
                }
                MessagePart::HookContext {
                    event_name,
                    handler_id,
                    tool_use_id,
                    content,
                } => {
                    let tool_label = tool_use_id
                        .as_deref()
                        .map(|id| format!(" tool_use_id={}", id))
                        .unwrap_or_default();
                    blocks.push(Content::text(format!(
                        "<hook-context event={} handler={}{}>\n{}\n</hook-context>",
                        event_name, handler_id, tool_label, content
                    )));
                }
                MessagePart::ToolResult {
                    call_id,
                    content,
                    is_error,
                    tool_name,
                    compacted_at,
                    ..
                } => {
                    let inner = if compacted_at.is_some() {
                        vec![Content::text("[Old tool result content cleared]")]
                    } else {
                        content.clone()
                    };
                    blocks.push(Content::ToolResult {
                        id: call_id.clone(),
                        name: tool_name.clone(),
                        is_error: *is_error,
                        content: inner,
                    });
                }
                MessagePart::Snapshot { changed_paths, .. } if !changed_paths.is_empty() => {
                    blocks.push(Content::text(format!(
                        "\n[System: File changes: {}]",
                        changed_paths.summary()
                    )));
                }
                MessagePart::Compaction { summary, .. } => {
                    blocks.push(Content::text(summary));
                }
                MessagePart::CompactionRequest { .. } => {
                    blocks.push(Content::text("Summarize our conversation so far."));
                }
                _ => {}
            }
        }

        if self.role == ChatRole::User && blocks.iter().any(|block| block.is_tool_result()) {
            blocks.sort_by_key(|block| if block.is_tool_result() { 0 } else { 1 });
        }

        Ok(ChatMessage {
            role: self.role.clone(),
            content: blocks,
            cache: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentMessage, MessagePart, PromptContentError};
    use crate::acp::protocol::{
        BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
        ImageContent, TextContent, TextResourceContents,
    };
    use base64::Engine as _;
    use querymt::chat::{ChatRole, Content};

    fn prompt_message(blocks: Vec<ContentBlock>) -> AgentMessage {
        AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::Prompt { blocks }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    fn image(data: &[u8], mime_type: &str) -> ContentBlock {
        ContentBlock::Image(ImageContent::new(
            base64::engine::general_purpose::STANDARD.encode(data),
            mime_type,
        ))
    }

    fn blob(data: &[u8], mime_type: &str, uri: &str) -> ContentBlock {
        ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::BlobResourceContents(
                BlobResourceContents::new(
                    base64::engine::general_purpose::STANDARD.encode(data),
                    uri,
                )
                .mime_type(mime_type.to_string()),
            ),
        ))
    }

    fn text_resource(text: impl Into<String>, uri: &str) -> ContentBlock {
        ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(text, uri)),
        ))
    }

    #[test]
    fn to_chat_message_renders_prompt_blocks() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::Prompt {
                blocks: vec![ContentBlock::Text(TextContent::new("display".to_string()))],
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message().unwrap();
        assert_eq!(chat.text(), "display");
    }

    #[test]
    fn to_chat_message_preserves_mixed_native_images_and_text() {
        let chat = prompt_message(vec![
            ContentBlock::Text(TextContent::new("before")),
            image(&[1, 2, 3], "image/png"),
            ContentBlock::Text(TextContent::new("after")),
            image(&[4, 5], "image/jpeg"),
        ])
        .to_chat_message()
        .unwrap();

        assert_eq!(chat.content.len(), 4);
        assert_eq!(chat.content[0].as_text(), Some("before"));
        assert!(matches!(
            &chat.content[1],
            Content::Image { mime_type, data }
                if mime_type == "image/png" && data == &[1, 2, 3]
        ));
        assert_eq!(chat.content[2].as_text(), Some("after"));
        assert!(matches!(
            &chat.content[3],
            Content::Image { mime_type, data }
                if mime_type == "image/jpeg" && data == &[4, 5]
        ));
    }

    #[test]
    fn to_chat_message_promotes_legacy_image_resource_and_preserves_other_resources() {
        let text_resource = ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(
                TextResourceContents::new("notes", "attachment:///notes.txt")
                    .mime_type("text/plain".to_string()),
            ),
        ));
        let chat = prompt_message(vec![
            blob(&[9, 8, 7], "image/webp", "attachment:///image.webp"),
            text_resource,
            blob(&[0x25, 0x50], "application/pdf", "attachment:///doc.pdf"),
            blob(
                &[6, 6],
                "application/octet-stream",
                "attachment:///data.bin",
            ),
        ])
        .to_chat_message()
        .unwrap();

        assert!(matches!(
            &chat.content[0],
            Content::Image { mime_type, data }
                if mime_type == "image/webp" && data == &[9, 8, 7]
        ));
        assert_eq!(
            chat.content[1].as_text(),
            Some("[Embedded Resource: attachment:///notes.txt]\nnotes")
        );
        assert!(matches!(&chat.content[2], Content::Pdf { data } if data == &[0x25, 0x50]));
        assert_eq!(
            chat.content[3].as_text(),
            Some("[Attached resource: attachment:///data.bin (application/octet-stream, 2 bytes)]")
        );
    }

    #[test]
    fn steering_uses_the_same_image_conversion() {
        let mut message = prompt_message(Vec::new());
        message.parts = vec![MessagePart::Steering {
            run_id: "run-1".to_string(),
            client_input_id: None,
            blocks: vec![image(&[3, 1, 4], "image/gif")],
        }];
        let chat = message.to_chat_message().unwrap();
        assert!(matches!(
            &chat.content[0],
            Content::Image { mime_type, data }
                if mime_type == "image/gif" && data == &[3, 1, 4]
        ));
    }

    #[test]
    fn invalid_base64_and_mime_are_explicit_errors() {
        let invalid_base64 = prompt_message(vec![ContentBlock::Image(ImageContent::new(
            "%%%",
            "image/png",
        ))]);
        assert!(matches!(
            invalid_base64.to_chat_message(),
            Err(PromptContentError::InvalidBase64 { index: 0, .. })
        ));
        assert!(matches!(
            super::validate_prompt_blocks(&[ContentBlock::Image(ImageContent::new(
                "%%%",
                "image/png",
            ))]),
            Err(PromptContentError::InvalidBase64 { index: 0, .. })
        ));

        let invalid_mime = prompt_message(vec![image(&[1], "image/svg+xml")]);
        assert_eq!(
            invalid_mime.to_chat_message().unwrap_err(),
            PromptContentError::UnsupportedImageMime {
                index: 0,
                mime_type: "image/svg+xml".to_string(),
            }
        );
    }

    #[test]
    fn image_count_limit_is_enforced() {
        let message = prompt_message(
            (0..=super::MAX_IMAGES_PER_PROMPT)
                .map(|_| image(&[1], "image/png"))
                .collect(),
        );
        assert!(matches!(
            message.to_chat_message(),
            Err(PromptContentError::TooManyImages { .. })
        ));
    }

    #[test]
    fn oversized_pdf_and_other_resources_are_rejected() {
        let oversized = vec![0; super::MAX_ATTACHMENT_BYTES + 1];
        for mime_type in ["application/pdf", "application/octet-stream"] {
            let message = prompt_message(vec![blob(
                &oversized,
                mime_type,
                "attachment:///oversized.bin",
            )]);
            assert_eq!(
                message.to_chat_message().unwrap_err(),
                PromptContentError::AttachmentTooLarge {
                    index: 0,
                    bytes: super::MAX_ATTACHMENT_BYTES + 1,
                    max_bytes: super::MAX_ATTACHMENT_BYTES,
                }
            );
        }
    }

    #[test]
    fn oversized_text_resource_is_rejected() {
        let message = prompt_message(vec![text_resource(
            "x".repeat(super::MAX_ATTACHMENT_BYTES + 1),
            "attachment:///oversized.txt",
        )]);

        assert_eq!(
            message.to_chat_message().unwrap_err(),
            PromptContentError::AttachmentTooLarge {
                index: 0,
                bytes: super::MAX_ATTACHMENT_BYTES + 1,
                max_bytes: super::MAX_ATTACHMENT_BYTES,
            }
        );
    }

    #[test]
    fn mixed_attachment_aggregate_limit_counts_text_resources_and_blobs() {
        let message = prompt_message(vec![
            blob(
                &vec![1; super::MAX_ATTACHMENT_BYTES],
                "application/pdf",
                "attachment:///document.pdf",
            ),
            text_resource(
                "x".repeat(super::MAX_ATTACHMENT_BYTES),
                "attachment:///notes.txt",
            ),
            blob(
                &[2],
                "application/octet-stream",
                "attachment:///payload.bin",
            ),
        ]);

        assert_eq!(
            message.to_chat_message().unwrap_err(),
            PromptContentError::AttachmentsTooLarge {
                bytes: super::MAX_TOTAL_ATTACHMENT_BYTES + 1,
                max_bytes: super::MAX_TOTAL_ATTACHMENT_BYTES,
            }
        );
    }

    #[test]
    fn mixed_attachment_aggregate_limit_counts_images_and_all_blobs() {
        let message = prompt_message(vec![
            image(&vec![1; super::MAX_ATTACHMENT_BYTES], "image/png"),
            blob(
                &vec![2; super::MAX_ATTACHMENT_BYTES],
                "application/pdf",
                "attachment:///document.pdf",
            ),
            blob(
                &[3],
                "application/octet-stream",
                "attachment:///payload.bin",
            ),
        ]);

        assert_eq!(
            message.to_chat_message().unwrap_err(),
            PromptContentError::AttachmentsTooLarge {
                bytes: super::MAX_TOTAL_ATTACHMENT_BYTES + 1,
                max_bytes: super::MAX_TOTAL_ATTACHMENT_BYTES,
            }
        );
    }

    #[test]
    fn empty_prompt_is_rejected() {
        assert_eq!(
            prompt_message(Vec::new()).to_chat_message().unwrap_err(),
            PromptContentError::EmptyPrompt
        );
    }

    #[test]
    fn text_limit_does_not_truncate_image_bytes() {
        let chat = prompt_message(vec![
            ContentBlock::Text(TextContent::new("long text")),
            image(&[1, 2, 3, 4], "image/png"),
        ])
        .to_chat_message_with_max_prompt_bytes(Some(4))
        .unwrap();
        assert!(matches!(&chat.content[1], Content::Image { data, .. } if data == &[1, 2, 3, 4]));
    }

    #[test]
    fn to_chat_message_compaction_renders_summary_directly() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: "Summary of previous conversation".to_string(),
                original_token_count: 5000,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message().unwrap();
        assert_eq!(chat.text(), "Summary of previous conversation");
        assert_eq!(chat.role, ChatRole::Assistant);
    }

    #[test]
    fn to_chat_message_compaction_request_renders_user_prompt() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::CompactionRequest {
                original_token_count: 5000,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message().unwrap();
        assert_eq!(chat.text(), "Summarize our conversation so far.");
        assert_eq!(chat.role, ChatRole::User);
    }

    #[test]
    fn to_chat_message_compaction_pair_forms_valid_exchange() {
        let req = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::CompactionRequest {
                original_token_count: 5000,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        let sum = AgentMessage {
            id: "m2".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: "Here is the summary.".to_string(),
                original_token_count: 5000,
            }],
            created_at: 0,
            parent_message_id: Some("m1".to_string()),
            source_provider: None,
            source_model: None,
        };

        let req_chat = req.to_chat_message().unwrap();
        let sum_chat = sum.to_chat_message().unwrap();

        // User message followed by assistant message — valid API exchange
        assert_eq!(req_chat.role, ChatRole::User);
        assert_eq!(sum_chat.role, ChatRole::Assistant);

        // Neither has trailing whitespace in text content
        assert!(!req_chat.text().ends_with(char::is_whitespace));
        assert!(!sum_chat.text().ends_with(char::is_whitespace));
    }

    #[test]
    fn to_chat_message_tool_result_uses_content_blocks() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::ToolResult {
                call_id: "call-1".to_string(),
                content: vec![Content::text("tool output")],
                is_error: false,
                tool_name: Some("shell".to_string()),
                tool_arguments: Some("{}".to_string()),
                compacted_at: None,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message().unwrap();
        assert!(chat.has_tool_result());
        // The tool result block should contain the text
        let tr = chat.content.iter().find(|b| b.is_tool_result()).unwrap();
        match tr {
            Content::ToolResult {
                id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(id, "call-1");
                assert!(!is_error);
                assert_eq!(content.len(), 1);
                assert_eq!(content[0].as_text(), Some("tool output"));
            }
            _ => panic!("Expected ToolResult"),
        }
    }

    #[test]
    fn to_chat_message_compacted_tool_result_uses_placeholder() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::ToolResult {
                call_id: "call-1".to_string(),
                content: vec![Content::text("original content")],
                is_error: false,
                tool_name: Some("shell".to_string()),
                tool_arguments: Some("{}".to_string()),
                compacted_at: Some(1234567890),
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message().unwrap();
        let tr = chat.content.iter().find(|b| b.is_tool_result()).unwrap();
        match tr {
            Content::ToolResult { content, .. } => {
                assert_eq!(
                    content[0].as_text(),
                    Some("[Old tool result content cleared]")
                );
            }
            _ => panic!("Expected ToolResult"),
        }
    }

    #[test]
    fn to_chat_message_with_target_keeps_signature_for_same_model() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Reasoning {
                content: "reasoning".to_string(),
                signature: Some("sig-123".to_string()),
                time_ms: None,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: Some("anthropic".to_string()),
            source_model: Some("claude-sonnet-4-5".to_string()),
        };

        let chat = msg
            .to_chat_message_with_target(Some("anthropic"), Some("claude-sonnet-4-5"), None)
            .unwrap();

        match &chat.content[0] {
            Content::Thinking {
                signature: Some(sig),
                ..
            } => assert_eq!(sig, "sig-123"),
            _ => panic!("expected signed thinking block"),
        }
    }

    #[test]
    fn to_chat_message_with_target_drops_signature_on_model_switch() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Reasoning {
                content: "reasoning".to_string(),
                signature: Some("sig-123".to_string()),
                time_ms: None,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: Some("anthropic".to_string()),
            source_model: Some("claude-sonnet-4-5".to_string()),
        };

        let chat = msg
            .to_chat_message_with_target(Some("anthropic"), Some("claude-opus-4-1"), None)
            .unwrap();

        match &chat.content[0] {
            Content::Thinking {
                signature: None, ..
            } => {}
            _ => panic!("expected thinking block without signature"),
        }
    }

    /// When a single User message contains multiple ToolResult parts
    /// interleaved with Snapshot parts (as produced by batched parallel
    /// tool calls that modify files), all ToolResult content blocks must
    /// appear before any Text blocks in the resulting ChatMessage.
    ///
    /// The Anthropic API fails to match tool_result blocks to their
    /// tool_use counterparts when non-tool_result content blocks are
    /// interleaved between them.
    #[test]
    fn to_chat_message_tool_results_before_snapshot_text() {
        use crate::hash::RapidHash;
        use crate::index::merkle::DiffPaths;

        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![
                MessagePart::ToolResult {
                    call_id: "call-a".to_string(),
                    content: vec![Content::text("result a")],
                    is_error: false,
                    tool_name: Some("edit".to_string()),
                    tool_arguments: None,
                    compacted_at: None,
                },
                MessagePart::Snapshot {
                    root_hash: RapidHash::new(b"h1"),
                    changed_paths: DiffPaths {
                        added: vec![],
                        modified: vec!["src/a.rs".into()],
                        removed: vec![],
                    },
                },
                MessagePart::ToolResult {
                    call_id: "call-b".to_string(),
                    content: vec![Content::text("result b")],
                    is_error: false,
                    tool_name: Some("edit".to_string()),
                    tool_arguments: None,
                    compacted_at: None,
                },
                MessagePart::Snapshot {
                    root_hash: RapidHash::new(b"h2"),
                    changed_paths: DiffPaths {
                        added: vec![],
                        modified: vec!["src/b.rs".into()],
                        removed: vec![],
                    },
                },
            ],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };

        let chat = msg.to_chat_message().unwrap();

        // Must have 2 ToolResult + at least 1 Text (from snapshots).
        let tool_result_count = chat.content.iter().filter(|b| b.is_tool_result()).count();
        assert_eq!(tool_result_count, 2);

        // All ToolResult blocks must appear before any Text block.
        let first_text_idx = chat
            .content
            .iter()
            .position(|b| matches!(b, Content::Text { .. }));
        let last_tool_result_idx = chat.content.iter().rposition(|b| b.is_tool_result());

        if let (Some(first_text), Some(last_tr)) = (first_text_idx, last_tool_result_idx) {
            assert!(
                last_tr < first_text,
                "all ToolResult blocks must come before any Text block, \
                 but last ToolResult is at index {} and first Text at index {}",
                last_tr,
                first_text
            );
        }
    }
}

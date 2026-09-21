use crate::acp::protocol::{ContentBlock, EmbeddedResourceResource};
use crate::agent::utils::truncate_to_bytes;
use crate::index::merkle::DiffPaths;
use base64::Engine as _;
use querymt::chat::{
    ChatInputPart, ChatOutput, ChatOutputItem, MediaKind, MediaPart, MediaSource, MediaType,
};
use querymt::{
    ToolCall,
    chat::{ChatMessage, ChatRole},
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
    #[error("invalid media type at block {index}: {mime_type}")]
    InvalidMediaType { index: usize, mime_type: String },
    #[error("invalid media attachment at block {index}: {reason}")]
    InvalidMedia { index: usize, reason: String },
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
) -> Result<Vec<ChatInputPart>, PromptContentError> {
    if blocks.is_empty() {
        return Err(PromptContentError::EmptyPrompt);
    }

    let mut converted = Vec::with_capacity(blocks.len());
    let mut remaining_text_bytes = max_text_bytes;
    let mut image_count = 0usize;
    let mut total_attachment_bytes = 0usize;

    for (index, block) in blocks.iter().enumerate() {
        match block {
            ContentBlock::Text(text) => converted.push(ChatInputPart::text(limit_text(
                &text.text,
                &mut remaining_text_bytes,
            ))),
            ContentBlock::Image(image) => {
                let data = decode_attachment(index, &image.data)?;
                validate_attachment_size(index, data.len(), &mut total_attachment_bytes)?;
                validate_image(index, &image.mime_type, &mut image_count)?;
                converted.push(inline_attachment(
                    MediaKind::Image,
                    &image.mime_type,
                    data,
                    index,
                )?);
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    validate_attachment_size(index, text.text.len(), &mut total_attachment_bytes)?;
                    let contextual = format!("[Embedded Resource: {}]\n{}", text.uri, text.text);
                    converted.push(ChatInputPart::text(limit_text(
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
                        converted.push(inline_attachment(
                            MediaKind::Image,
                            mime_type,
                            data,
                            index,
                        )?);
                    } else if mime_type == "application/pdf" {
                        converted.push(inline_attachment(
                            MediaKind::Document,
                            mime_type,
                            data,
                            index,
                        )?);
                    } else if mime_type.starts_with("text/") {
                        let text = String::from_utf8(data)
                            .map_err(|_| PromptContentError::InvalidTextResource { index })?;
                        let contextual = format!("[Embedded Resource: {}]\n{}", blob.uri, text);
                        converted.push(ChatInputPart::text(limit_text(
                            &contextual,
                            &mut remaining_text_bytes,
                        )));
                    } else {
                        let marker = format!(
                            "[Attached resource: {} ({mime_type}, {} bytes)]",
                            blob.uri,
                            data.len()
                        );
                        converted.push(ChatInputPart::text(limit_text(
                            &marker,
                            &mut remaining_text_bytes,
                        )));
                    }
                }
                _ => converted.push(ChatInputPart::text("[Unsupported embedded resource]")),
            },
            ContentBlock::ResourceLink(link) => {
                let mut media = MediaPart::new(
                    MediaKind::Other,
                    None,
                    MediaSource::Url {
                        url: link.uri.clone(),
                    },
                )
                .map_err(|error| PromptContentError::InvalidMedia {
                    index,
                    reason: error.to_string(),
                })?;
                media.filename = Some(link.name.clone());
                converted.push(ChatInputPart::attachment(media));
            }
            ContentBlock::Audio(audio) => {
                let marker = format!("[Audio attachment: {}]", audio.mime_type);
                converted.push(ChatInputPart::text(limit_text(
                    &marker,
                    &mut remaining_text_bytes,
                )));
            }
            _ => converted.push(ChatInputPart::text("[Unsupported content block]")),
        }
    }

    Ok(converted)
}

/// Build a validated inline attachment input part.
fn inline_attachment(
    kind: MediaKind,
    mime_type: &str,
    data: Vec<u8>,
    index: usize,
) -> Result<ChatInputPart, PromptContentError> {
    let media_type: MediaType =
        mime_type
            .parse()
            .map_err(|_| PromptContentError::InvalidMediaType {
                index,
                mime_type: mime_type.to_string(),
            })?;
    let media =
        MediaPart::new(kind, Some(media_type), MediaSource::Inline { data }).map_err(|error| {
            PromptContentError::InvalidMedia {
                index,
                reason: error.to_string(),
            }
        })?;
    Ok(ChatInputPart::attachment(media))
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

/// Whether the containing message's recorded legacy origin matches the target.
///
/// This is the only available signal for output that predates provenance
/// tracking; it compares the provider and model the owning assistant turn was
/// generated by against the projection target. A missing origin never matches.
fn legacy_origin_matches(
    target: Option<&OutputTarget>,
    source_provider: Option<&str>,
    source_model: Option<&str>,
) -> bool {
    match (target, source_provider, source_model) {
        (Some(target), Some(provider), Some(model)) => {
            target.provider == provider && target.model == model
        }
        _ => false,
    }
}

/// Decide whether a structured output may replay provider-only state (such as
/// reasoning signatures and opaque items) to the projection target.
///
/// Native replay is authorized only for an exactly compatible target: the
/// recorded provenance must match the target's full provider/protocol/model/
/// endpoint identity. A `None` target identity is treated as *unknown* and
/// therefore not exactly compatible, so native state is dropped and only the
/// portable projection is used.
///
/// Output without provenance predates provenance tracking. It may replay native
/// state only when the caller accepts the legacy provider/model comparison *and*
/// the containing message's recorded origin actually matches the target; a
/// missing or mismatched origin degrades to the portable projection.
fn native_replay_allowed(
    output: &querymt::chat::ChatOutput,
    target: Option<&OutputTarget>,
    source_provider: Option<&str>,
    source_model: Option<&str>,
) -> bool {
    let Some(target) = target else {
        return false;
    };
    let Some(provenance) = output.provenance.as_ref() else {
        return target.fallback_provider_model
            && legacy_origin_matches(Some(target), source_provider, source_model);
    };
    !provenance.endpoint.is_empty()
        && provenance.provider == target.provider
        && provenance.protocol == target.protocol
        && provenance.model == target.model
        && provenance.endpoint == target.endpoint
}

/// Project a structured output for a projection target.
///
/// An authorized native target receives the canonical output unchanged (full
/// provenance, opaque items, and provider-only continuation). Every other
/// target receives the canonical portable downgrade, which strips origin-scoped
/// state while retaining portable messages, visible reasoning, and call/result
/// correlation by `call_id`.
fn project_output_for_target(
    output: &querymt::chat::ChatOutput,
    target: Option<&OutputTarget>,
    source_provider: Option<&str>,
    source_model: Option<&str>,
) -> querymt::chat::ChatOutput {
    if native_replay_allowed(output, target, source_provider, source_model) {
        output.clone()
    } else {
        output.clone().into_portable()
    }
}

/// The projection target identity used to gate native provider state.
///
/// Carries the full provider/protocol/model/endpoint tuple so replay cannot
/// forward encrypted reasoning to a different endpoint or protocol that merely
/// shares a model name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputTarget {
    pub provider: String,
    pub protocol: String,
    pub model: String,
    pub endpoint: String,
    /// Whether output that carries no provenance may still replay native state.
    /// True only when the caller could not resolve protocol/endpoint identity
    /// and explicitly accepts the legacy provider/model-only comparison.
    pub fallback_provider_model: bool,
}

impl OutputTarget {
    /// Best-effort target identity when only provider/model are known.
    ///
    /// Protocol and endpoint are left empty, which never authorizes native
    /// replay for provenance-bearing output. Output without provenance is
    /// still accepted by legacy comparison so pre-existing history keeps
    /// working.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            protocol: String::new(),
            model: model.into(),
            endpoint: String::new(),
            fallback_provider_model: true,
        }
    }

    pub fn with_protocol(mut self, protocol: impl Into<String>) -> Self {
        self.protocol = protocol.into();
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

/// Deserialize a persisted reasoning part, accepting both shapes.
///
/// Rows written before reasoning was widened carry a flat
/// `{"content": "...", "signature": "..."}` pair; rows written after carry the
/// full canonical item. The legacy shape is lifted into a canonical item with
/// its text in `content` so old sessions keep loading unchanged.
fn deserialize_reasoning_item<'de, D>(
    deserializer: D,
) -> Result<querymt::chat::ChatReasoningItem, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use querymt::chat::{ChatReasoningItem, ChatReasoningPart};

    #[derive(Deserialize)]
    struct LegacyReasoning {
        #[serde(default)]
        content: String,
        #[serde(default)]
        signature: Option<String>,
    }

    let value = serde_json::Value::deserialize(deserializer)?;

    // Canonical items always carry at least one of the item-only fields; the
    // legacy shape never has them.
    let is_legacy = value.get("summary").is_none()
        && value.get("encrypted_content").is_none()
        && value.get("id").is_none();

    if is_legacy {
        let legacy: LegacyReasoning =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        return Ok(ChatReasoningItem {
            id: None,
            summary: Vec::new(),
            content: if legacy.content.is_empty() {
                Vec::new()
            } else {
                vec![ChatReasoningPart::text(legacy.content)]
            },
            encrypted_content: None,
            signature: legacy.signature,
            status: None,
            extensions: Default::default(),
        });
    }

    let item: ChatReasoningItem =
        serde_json::from_value(value).map_err(serde::de::Error::custom)?;
    Ok(item)
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
        /// Authoritative canonical reasoning for this turn.
        ///
        /// This carries the full item shape (summary, content, encrypted
        /// continuation, item id, status, extensions) so persisted history keeps
        /// fidelity the flat `(content, signature)` pair could not express.
        #[serde(flatten, deserialize_with = "deserialize_reasoning_item")]
        item: querymt::chat::ChatReasoningItem,
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
    /// Canonical structured output for an item-aware assistant turn.
    ///
    /// This part is the authoritative record of generated items; text, reasoning,
    /// and tool-use parts are not stored alongside it for the same turn.
    /// Replay projects it exactly once through portable content.
    Output {
        output: querymt::chat::ChatOutput,
    },
    HookContext {
        event_name: String,
        handler_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        content: String,
    },
    ToolResult {
        call_id: String,
        content: Vec<querymt::chat::ToolResultPart>,
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
            MessagePart::Output { .. } => "output",
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

    /// Function calls represented by this message.
    ///
    /// Covers both legacy `ToolUse` parts and the canonical structured output
    /// part, so call-scanning consumers (delegation detection, loop guards,
    /// summaries) keep working for item-aware assistant turns. Call identities
    /// are deduplicated preserving first occurrence and item order.
    pub fn function_calls(&self) -> Vec<querymt::ToolCall> {
        let mut calls = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for part in &self.parts {
            match part {
                MessagePart::ToolUse(call) => {
                    if seen.insert(call.id.clone()) {
                        calls.push(call.clone());
                    }
                }
                MessagePart::Output { output } => {
                    for call in output.tool_calls().unwrap_or_default() {
                        if seen.insert(call.id.clone()) {
                            calls.push(call);
                        }
                    }
                }
                _ => {}
            }
        }
        calls
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
        // A provider/model-only target cannot prove protocol/endpoint identity,
        // so it authorizes native replay only through the legacy fallback.
        let target = target_provider
            .zip(target_model)
            .map(|(provider, model)| OutputTarget::new(provider.to_string(), model.to_string()));
        self.to_chat_message_with_output_target(target.as_ref(), max_prompt_bytes)
    }

    /// Convert to a `ChatMessage`, authorizing native replay only for the full
    /// provider/protocol/model/endpoint target identity.
    pub fn to_chat_message_with_output_target(
        &self,
        target: Option<&OutputTarget>,
        max_prompt_bytes: Option<usize>,
    ) -> Result<ChatMessage, PromptContentError> {
        let mut input_parts = Vec::new();
        let mut output_items = Vec::new();
        // A canonical `Output` part, when present, is the assistant turn's sole
        // generated-content authority. Any legacy generated parts are ignored so
        // the authoritative output is never merged with a stale projection.
        let mut canonical_output: Option<ChatOutput> = None;

        for part in &self.parts {
            match part {
                MessagePart::Text { content } if self.role == ChatRole::Assistant => {
                    output_items.push(ChatOutputItem::Message(querymt::chat::ChatMessageItem {
                        id: None,
                        role: ChatRole::Assistant,
                        phase: None,
                        status: None,
                        parts: vec![querymt::chat::ChatMessagePart::Text {
                            text: content.clone(),
                            annotations: Vec::new(),
                            extensions: Default::default(),
                        }],
                        extensions: Default::default(),
                    }));
                }
                MessagePart::Text { content } => {
                    input_parts.push(ChatInputPart::text(content.clone()))
                }
                MessagePart::Prompt { blocks } | MessagePart::Steering { blocks, .. } => {
                    input_parts.extend(convert_prompt_blocks(blocks, max_prompt_bytes)?);
                }
                MessagePart::Reasoning { item, .. } => {
                    let mut item = item.clone();
                    let same_legacy_origin = legacy_origin_matches(
                        target,
                        self.source_provider.as_deref(),
                        self.source_model.as_deref(),
                    );
                    if !same_legacy_origin {
                        item.signature = None;
                        item.encrypted_content = None;
                        item.id = None;
                        item.extensions.clear();
                    }
                    output_items.push(ChatOutputItem::Reasoning(item));
                }
                MessagePart::ToolUse(call) => {
                    output_items.push(ChatOutputItem::FunctionCall(
                        querymt::chat::ChatFunctionCallItem {
                            item_id: None,
                            call_id: call.id.clone(),
                            name: call.function.name.clone(),
                            arguments: call.function.arguments.clone(),
                            status: None,
                            extensions: Default::default(),
                        },
                    ));
                }
                MessagePart::Output { output } => {
                    // The canonical output is the sole generated-content
                    // authority. When several output parts are present the first
                    // wins; legacy generated parts accumulated into
                    // `output_items` are discarded at the assistant return below.
                    if canonical_output.is_none() {
                        canonical_output = Some(project_output_for_target(
                            output,
                            target,
                            self.source_provider.as_deref(),
                            self.source_model.as_deref(),
                        ));
                    }
                }
                MessagePart::ToolResult {
                    call_id,
                    content,
                    is_error,
                    tool_name,
                    compacted_at,
                    ..
                } => {
                    let parts = if compacted_at.is_some() {
                        vec![querymt::chat::ToolResultPart::text(
                            "[Old tool result content cleared]",
                        )]
                    } else {
                        content.clone()
                    };
                    let mut result = querymt::chat::ToolResult::new(call_id.clone());
                    result.name = tool_name.clone();
                    result.is_error = *is_error;
                    result.parts = parts;
                    input_parts.push(ChatInputPart::tool_result(result));
                }
                MessagePart::HookContext {
                    event_name,
                    handler_id,
                    tool_use_id,
                    content,
                } => {
                    let tool_label = tool_use_id
                        .as_deref()
                        .map(|id| format!(" tool_use_id={id}"))
                        .unwrap_or_default();
                    input_parts.push(ChatInputPart::text(format!(
                        "<hook-context event={event_name} handler={handler_id}{tool_label}>\n{content}\n</hook-context>"
                    )));
                }
                MessagePart::Snapshot { changed_paths, .. } if !changed_paths.is_empty() => {
                    input_parts.push(ChatInputPart::text(format!(
                        "\n[System: File changes: {}]",
                        changed_paths.summary()
                    )));
                }
                MessagePart::Compaction { summary, .. } if self.role == ChatRole::Assistant => {
                    output_items.push(ChatOutputItem::Message(querymt::chat::ChatMessageItem {
                        id: None,
                        role: ChatRole::Assistant,
                        phase: None,
                        status: None,
                        parts: vec![querymt::chat::ChatMessagePart::Text {
                            text: summary.clone(),
                            annotations: Vec::new(),
                            extensions: Default::default(),
                        }],
                        extensions: Default::default(),
                    }));
                }
                MessagePart::Compaction { summary, .. } => {
                    input_parts.push(ChatInputPart::text(summary.clone()))
                }
                MessagePart::CompactionRequest { .. } => {
                    input_parts.push(ChatInputPart::text("Summarize our conversation so far."))
                }
                _ => {}
            }
        }

        if self.role == ChatRole::Assistant {
            let output = canonical_output.unwrap_or_else(|| ChatOutput {
                items: output_items,
                status: Some(querymt::chat::ChatOutputStatus::Completed),
                ..ChatOutput::default()
            });
            return Ok(ChatMessage::from_assistant_output(output));
        }

        if input_parts.iter().any(ChatInputPart::is_tool_result) {
            input_parts.sort_by_key(|part| !part.is_tool_result());
        }
        Ok(ChatMessage::from_user_parts(input_parts))
    }
}

/// Repair persisted transcripts that contain assistant tool calls without an
/// immediately following result. This keeps legacy or interrupted sessions
/// acceptable to providers that strictly validate call/result pairing.
pub(crate) fn repair_unmatched_tool_calls(messages: &mut Vec<AgentMessage>) -> usize {
    let mut repaired = 0;
    let mut pending = Vec::<ToolCall>::new();
    let mut index = 0;

    while index < messages.len() {
        if !pending.is_empty() {
            if messages[index].role == ChatRole::User {
                let returned = messages[index]
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        MessagePart::ToolResult { call_id, .. } => Some(call_id.as_str()),
                        _ => None,
                    })
                    .collect::<std::collections::HashSet<_>>();
                pending.retain(|call| !returned.contains(call.id.as_str()));
                if !pending.is_empty() {
                    let mut results = cancelled_tool_result_parts(&pending);
                    repaired += results.len();
                    results.append(&mut messages[index].parts);
                    messages[index].parts = results;
                }
                pending.clear();
            } else {
                let session_id = messages[index].session_id.clone();
                let created_at = messages[index].created_at;
                let parts = cancelled_tool_result_parts(&pending);
                repaired += parts.len();
                messages.insert(
                    index,
                    AgentMessage {
                        id: uuid::Uuid::new_v4().to_string(),
                        session_id,
                        role: ChatRole::User,
                        parts,
                        created_at,
                        parent_message_id: None,
                        source_provider: None,
                        source_model: None,
                    },
                );
                pending.clear();
                index += 1;
            }
        }

        if messages[index].role == ChatRole::Assistant {
            pending = messages[index].function_calls();
        }
        index += 1;
    }

    if !pending.is_empty() {
        let session_id = messages
            .last()
            .map(|message| message.session_id.clone())
            .unwrap_or_default();
        let parts = cancelled_tool_result_parts(&pending);
        repaired += parts.len();
        messages.push(AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id,
            role: ChatRole::User,
            parts,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        });
    }

    repaired
}

fn cancelled_tool_result_parts(calls: &[ToolCall]) -> Vec<MessagePart> {
    calls
        .iter()
        .map(|call| MessagePart::ToolResult {
            call_id: call.id.clone(),
            content: vec![querymt::chat::ToolResultPart::text(
                "Error: Cancelled before the tool returned a result",
            )],
            is_error: true,
            tool_name: Some(call.function.name.clone()),
            tool_arguments: Some(call.function.arguments.clone()),
            compacted_at: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        AgentMessage, MessagePart, OutputTarget, PromptContentError, repair_unmatched_tool_calls,
    };
    use crate::acp::protocol::{
        BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
        ImageContent, TextContent, TextResourceContents,
    };
    use base64::Engine as _;
    use querymt::chat::{ChatRole, ToolResultPart};

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

        let parts = chat.portable_input_parts();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0].as_text(), Some("before"));
        assert!(matches!(
            &parts[1],
            querymt::chat::ChatInputPart::Attachment(media)
                if media.kind == querymt::chat::MediaKind::Image
                    && media.media_type().map(ToString::to_string).as_deref()
                        == Some("image/png")
        ));
        assert_eq!(parts[2].as_text(), Some("after"));
        assert!(matches!(
            &parts[3],
            querymt::chat::ChatInputPart::Attachment(media)
                if media.kind == querymt::chat::MediaKind::Image
                    && media.media_type().map(ToString::to_string).as_deref()
                        == Some("image/jpeg")
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

        let parts = chat.portable_input_parts();
        assert!(matches!(
            &parts[0],
            querymt::chat::ChatInputPart::Attachment(media)
                if media.kind == querymt::chat::MediaKind::Image
                    && media.media_type().map(ToString::to_string).as_deref()
                        == Some("image/webp")
        ));
        assert_eq!(
            parts[1].as_text(),
            Some("[Embedded Resource: attachment:///notes.txt]\nnotes")
        );
        assert!(matches!(
            &parts[2],
            querymt::chat::ChatInputPart::Attachment(media)
                if media.kind == querymt::chat::MediaKind::Document
        ));
        assert_eq!(
            parts[3].as_text(),
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
            &chat.portable_input_parts()[0],
            querymt::chat::ChatInputPart::Attachment(media)
                if media.kind == querymt::chat::MediaKind::Image
                    && media.media_type().map(ToString::to_string).as_deref()
                        == Some("image/gif")
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
        assert!(matches!(
            &chat.portable_input_parts()[1],
            querymt::chat::ChatInputPart::Attachment(media)
                if media.kind == querymt::chat::MediaKind::Image
        ));
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
                content: vec![ToolResultPart::text("tool output")],
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
        // The tool result part should contain the text
        let tr = chat
            .portable_input_parts()
            .into_iter()
            .find_map(|part| match part {
                querymt::chat::ChatInputPart::ToolResult(result) => Some(result),
                _ => None,
            })
            .unwrap();
        assert_eq!(tr.call_id, "call-1");
        assert!(!tr.is_error);
        assert_eq!(tr.parts.len(), 1);
        assert_eq!(tr.parts[0].as_text(), Some("tool output"));
    }

    #[test]
    fn to_chat_message_compacted_tool_result_uses_placeholder() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::ToolResult {
                call_id: "call-1".to_string(),
                content: vec![ToolResultPart::text("original content")],
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
        let tr = chat
            .portable_input_parts()
            .into_iter()
            .find_map(|part| match part {
                querymt::chat::ChatInputPart::ToolResult(result) => Some(result),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            tr.parts[0].as_text(),
            Some("[Old tool result content cleared]")
        );
    }

    #[test]
    fn to_chat_message_with_target_keeps_signature_for_same_model() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Reasoning {
                item: ChatReasoningItem {
                    id: None,
                    summary: Vec::new(),
                    content: vec![ChatReasoningPart::text("reasoning")],
                    encrypted_content: None,
                    signature: Some("sig-123".to_string()),
                    status: None,
                    extensions: Default::default(),
                },
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

        // Signed legacy reasoning is retained as canonical output for same-origin
        // replay.
        assert_eq!(
            chat.output().and_then(|output| output.signature()),
            Some("sig-123".to_string())
        );
    }

    #[test]
    fn to_chat_message_with_target_drops_signature_on_model_switch() {
        let msg = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Reasoning {
                item: ChatReasoningItem {
                    id: None,
                    summary: Vec::new(),
                    content: vec![ChatReasoningPart::text("reasoning")],
                    encrypted_content: None,
                    signature: Some("sig-123".to_string()),
                    status: None,
                    extensions: Default::default(),
                },
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

        assert!(
            chat.output()
                .and_then(|output| output.signature())
                .is_none(),
            "signature must not survive a model switch"
        );
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
                    content: vec![ToolResultPart::text("result a")],
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
                    content: vec![ToolResultPart::text("result b")],
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

        // Must have 2 tool result parts + at least 1 text part (from snapshots).
        let parts = chat.portable_input_parts();
        let tool_result_count = parts.iter().filter(|p| p.is_tool_result()).count();
        assert_eq!(tool_result_count, 2);

        // All tool result parts must appear before any text part.
        let first_text_idx = parts.iter().position(|p| p.as_text().is_some());
        let last_tool_result_idx = parts.iter().rposition(|p| p.is_tool_result());

        if let (Some(first_text), Some(last_tr)) = (first_text_idx, last_tool_result_idx) {
            assert!(
                last_tr < first_text,
                "all tool result parts must come before any text part, \
                 but last tool result is at index {} and first text at index {}",
                last_tr,
                first_text
            );
        }
    }

    // ── Canonical structured output part (tasks 4.2 / 4.4) ───────────────────

    use querymt::chat::{
        ChatFunctionCallItem, ChatMessageItem, ChatMessagePart, ChatOpaqueItem, ChatOutput,
        ChatOutputItem, ChatOutputProvenance, ChatReasoningItem, ChatReasoningPart,
    };

    fn structured_output() -> ChatOutput {
        ChatOutput {
            response_id: Some("resp_1".into()),
            status: Some(querymt::chat::ChatOutputStatus::Completed),
            finish_reason: Some(querymt::chat::FinishReason::ToolCalls),
            provenance: Some(ChatOutputProvenance {
                provider: "openai".into(),
                protocol: "responses".into(),
                model: "gpt-5".into(),
                endpoint: "https://api.openai.com/v1/responses".into(),
            }),
            items: vec![
                // Encrypted-only reasoning must survive storage/reload intact.
                ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: Some("reasoning_1".into()),
                    summary: Vec::new(),
                    content: Vec::new(),
                    encrypted_content: Some("opaque-continuation".into()),
                    signature: Some("sig-1".into()),
                    status: None,
                    extensions: Default::default(),
                }),
                ChatOutputItem::Message(ChatMessageItem {
                    id: Some("message_1".into()),
                    role: ChatRole::Assistant,
                    phase: None,
                    status: None,
                    parts: vec![ChatMessagePart::Text {
                        text: "final answer".into(),
                        annotations: Vec::new(),
                        extensions: Default::default(),
                    }],
                    extensions: Default::default(),
                }),
                // Exact raw arguments, distinct item/call IDs.
                ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                    item_id: Some("item_1".into()),
                    call_id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{\"query\":\"rust\",\"raw\": 1 }".into(),
                    status: None,
                    extensions: Default::default(),
                }),
                // Unknown items remain opaque.
                ChatOutputItem::Opaque(ChatOpaqueItem {
                    original_type: "future_action".into(),
                    payload: serde_json::json!({"vendor": true}),
                }),
            ],
            ..ChatOutput::default()
        }
    }

    fn output_message(output: ChatOutput) -> AgentMessage {
        AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Output { output }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    #[test]
    fn function_calls_reads_legacy_parts_and_canonical_output() {
        let legacy = AgentMessage {
            id: "m1".to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![
                MessagePart::ToolUse(querymt::ToolCall {
                    id: "call_a".into(),
                    call_type: "function".into(),
                    function: querymt::FunctionCall {
                        name: "delegate".into(),
                        arguments: "{\"target_agent_id\":\"x\",\"objective\":\"y\"}".into(),
                    },
                }),
                MessagePart::Text {
                    content: "working".into(),
                },
            ],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        assert_eq!(legacy.function_calls().len(), 1);
        assert_eq!(legacy.function_calls()[0].function.name, "delegate");

        // Structured turn: calls live in the canonical output part.
        let mut output = structured_output();
        output
            .items
            .push(ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                item_id: Some("item_2".into()),
                call_id: "call_1".into(), // duplicate identity of item_1
                name: "lookup".into(),
                arguments: "{}".into(),
                status: None,
                extensions: Default::default(),
            }));
        let structured = output_message(output);
        let calls = structured.function_calls();
        assert_eq!(calls.len(), 1, "duplicate call identity collapses");
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(
            calls[0].function.arguments,
            "{\"query\":\"rust\",\"raw\": 1 }"
        );

        // Mixed legacy + structured in one message never duplicates identities.
        let mut mixed = structured;
        mixed.parts.push(MessagePart::ToolUse(querymt::ToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: querymt::FunctionCall {
                name: "lookup".into(),
                arguments: "{}".into(),
            },
        }));
        assert_eq!(mixed.function_calls().len(), 1);
    }

    #[test]
    fn repair_unmatched_tool_calls_prepends_result_to_next_user_message() {
        let assistant = AgentMessage {
            id: "assistant".into(),
            session_id: "s1".into(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::ToolUse(querymt::ToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: querymt::FunctionCall {
                    name: "question".into(),
                    arguments: "{}".into(),
                },
            })],
            created_at: 1,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        let prompt = AgentMessage {
            id: "user".into(),
            session_id: "s1".into(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
                content: "continue".into(),
            }],
            created_at: 2,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        let mut messages = vec![assistant, prompt];

        assert_eq!(repair_unmatched_tool_calls(&mut messages), 1);
        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[1].parts[0],
            MessagePart::ToolResult { call_id, is_error: true, .. } if call_id == "call_1"
        ));
        assert!(
            matches!(&messages[1].parts[1], MessagePart::Text { content } if content == "continue")
        );
    }

    #[test]
    fn repair_unmatched_tool_calls_preserves_matched_results() {
        let mut messages = vec![
            AgentMessage {
                id: "assistant".into(),
                session_id: "s1".into(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::ToolUse(querymt::ToolCall {
                    id: "call_1".into(),
                    call_type: "function".into(),
                    function: querymt::FunctionCall {
                        name: "shell".into(),
                        arguments: "{}".into(),
                    },
                })],
                created_at: 1,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
            AgentMessage {
                id: "result".into(),
                session_id: "s1".into(),
                role: ChatRole::User,
                parts: vec![MessagePart::ToolResult {
                    call_id: "call_1".into(),
                    content: vec![ToolResultPart::text("ok")],
                    is_error: false,
                    tool_name: Some("shell".into()),
                    tool_arguments: Some("{}".into()),
                    compacted_at: None,
                }],
                created_at: 2,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
        ];

        assert_eq!(repair_unmatched_tool_calls(&mut messages), 0);
        assert_eq!(messages[1].parts.len(), 1);
    }

    #[test]
    fn output_part_serde_round_trip_preserves_opaque_and_raw_data() {
        let part = MessagePart::Output {
            output: structured_output(),
        };
        let encoded = serde_json::to_string(&part).unwrap();
        let decoded: MessagePart = serde_json::from_str(&encoded).unwrap();

        let MessagePart::Output { output } = decoded else {
            panic!("expected Output part");
        };
        assert_eq!(output.items.len(), 4);

        let ChatOutputItem::Reasoning(reasoning) = &output.items[0] else {
            panic!("expected reasoning item");
        };
        assert_eq!(
            reasoning.encrypted_content.as_deref(),
            Some("opaque-continuation"),
            "encrypted-only reasoning survives the round trip"
        );
        assert_eq!(reasoning.signature.as_deref(), Some("sig-1"));

        let ChatOutputItem::FunctionCall(call) = &output.items[2] else {
            panic!("expected function call item");
        };
        assert_eq!(call.item_id.as_deref(), Some("item_1"));
        assert_eq!(call.call_id, "call_1");
        assert_eq!(
            call.arguments, "{\"query\":\"rust\",\"raw\": 1 }",
            "raw arguments stay byte-exact"
        );

        assert!(matches!(output.items[3], ChatOutputItem::Opaque(_)));
    }

    #[test]
    fn output_part_projects_once_without_duplicate_parts() {
        // Same-origin replay keeps the authoritative structured output and does
        // not duplicate it into a second portable projection.
        let msg = output_message(structured_output());
        let chat = msg
            .to_chat_message_with_output_target(
                Some(&full_target("openai", "responses", "gpt-5")),
                None,
            )
            .unwrap();

        let output = chat
            .output()
            .expect("same-origin target retains structured output");
        // The message text and the function call each appear exactly once as
        // canonical items.
        let text_parts = chat
            .portable_input_parts()
            .iter()
            .filter(|part| part.as_text() == Some("final answer"))
            .count();
        assert_eq!(text_parts, 1, "message text projected once");
        let calls = output.tool_calls().unwrap();
        assert_eq!(calls.len(), 1, "call projected once");
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "lookup");
        // Raw arguments are preserved byte-exact in canonical output.
        assert_eq!(
            calls[0].function.arguments,
            "{\"query\":\"rust\",\"raw\": 1 }"
        );
    }

    #[test]
    fn output_part_never_projects_invalid_arguments_or_opaque_execution() {
        let mut output = structured_output();
        output
            .items
            .push(ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                item_id: Some("item_2".into()),
                call_id: "call_2".into(),
                name: "lookup".into(),
                arguments: "{invalid".into(),
                status: None,
                extensions: Default::default(),
            }));
        let msg = output_message(output);

        // Portable projection (no native target) preserves typed calls for
        // correlation and leaves argument validation to the executor.
        let chat = msg.to_chat_message().unwrap();
        assert!(
            chat.output()
                .unwrap()
                .function_calls()
                .any(|call| call.arguments == "{invalid"),
            "invalid arguments remain byte-exact for explicit validation"
        );
        assert_eq!(
            chat.output().unwrap().items.len(),
            3,
            "opaque items and reasoning without visible text are dropped"
        );
    }

    #[test]
    fn output_part_projection_does_not_leak_opaque_state_across_targets() {
        let msg = output_message(structured_output());
        let before = serde_json::to_string(&msg).unwrap();

        // Same provider/model origin: reasoning signature may be forwarded.
        let same = msg
            .to_chat_message_with_target(Some("openai"), Some("gpt-5"), None)
            .unwrap();
        // Visible reasoning is empty, so no thinking block is projected at all;
        // opaque state must never appear as portable content either way.
        assert!(
            !serde_json::to_string(&same)
                .unwrap()
                .contains("opaque-continuation"),
            "encrypted continuation is never projected into portable content"
        );

        // Different model: portable projection only.
        let switched = msg
            .to_chat_message_with_target(Some("openai"), Some("gpt-4o"), None)
            .unwrap();
        assert!(switched.output().is_some());
        assert!(
            !serde_json::to_string(&switched)
                .unwrap()
                .contains("opaque-continuation")
        );

        // Stored original is never mutated by projection.
        assert_eq!(serde_json::to_string(&msg).unwrap(), before);
    }

    #[test]
    fn output_part_signature_follows_provenance_and_model_switch() {
        let mut output = structured_output();
        // Give the reasoning visible summary so thinking blocks are projected.
        if let ChatOutputItem::Reasoning(reasoning) = &mut output.items[0] {
            reasoning.summary = vec![ChatReasoningPart::text("visible reasoning")];
        }
        let msg = output_message(output);

        // Only the exact provider/protocol/model/endpoint target retains native
        // provider state (the reasoning signature).
        let same_origin = msg
            .to_chat_message_with_output_target(
                Some(&full_target("openai", "responses", "gpt-5")),
                None,
            )
            .unwrap();
        assert_eq!(
            same_origin.output().and_then(|output| output.signature()),
            Some("sig-1".to_string())
        );
        assert!(
            same_origin.output().is_some(),
            "native continuation preserved"
        );

        // A provider/model-only target cannot prove protocol/endpoint identity,
        // so it degrades to the portable projection and drops the signature.
        let same_model_only = msg
            .to_chat_message_with_target(Some("openai"), Some("gpt-5"), None)
            .unwrap();
        assert!(same_model_only.output().is_some());

        let switched = msg
            .to_chat_message_with_output_target(
                Some(&full_target("openai", "responses", "gpt-4o")),
                None,
            )
            .unwrap();
        assert!(
            switched
                .portable_input_parts()
                .iter()
                .any(|part| part.as_text() == Some("visible reasoning"))
        );
        assert!(switched.output().is_some());
    }

    #[test]
    fn output_part_without_provenance_uses_message_origin_for_native_replay() {
        let mut output = structured_output();
        output.provenance = None;
        let mut msg = output_message(output);
        msg.source_provider = Some("openai".into());
        msg.source_model = Some("gpt-5".into());

        // A matching legacy origin authorizes native replay through the fallback.
        let same = msg
            .to_chat_message_with_target(Some("openai"), Some("gpt-5"), None)
            .unwrap();
        assert_eq!(
            same.output().and_then(|output| output.signature()),
            Some("sig-1".to_string()),
            "matching message origin replays native state without provenance"
        );

        // A different provider/model degrades to the portable projection.
        let other = msg
            .to_chat_message_with_target(Some("openai"), Some("gpt-4o"), None)
            .unwrap();
        assert!(
            !other
                .output()
                .expect("portable projection keeps portable output")
                .requires_item_aware_fidelity(),
            "mismatched message origin must not replay native state"
        );
    }

    /// Build the exact target identity used by provenance-bearing fixtures.
    fn full_target(provider: &str, protocol: &str, model: &str) -> OutputTarget {
        OutputTarget::new(provider, model)
            .with_protocol(protocol)
            .with_endpoint("https://api.openai.com/v1/responses")
    }
}

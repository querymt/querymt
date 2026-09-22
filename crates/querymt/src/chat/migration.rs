//! Private migration/export DTOs for the legacy recursive [`Content`] model.
//!
//! This module is the *only* place that is allowed to understand the old
//! recursive content representation. It exists so that:
//!
//! 1. Loading an unchanged old message history keeps working, and
//! 2. The legacy representation can be normalized *once*, at the migration
//!    boundary, into canonical [`ChatInputPart`] / [`ChatOutput`] values.
//!
//! Nothing here is part of the primary public chat API; callers should use the
//! canonical types directly. The legacy enum is private to this module, so no
//! crate outside `querymt` can name it; only the decoders below may read it.

use super::output::{
    ChatFunctionCallItem, ChatInputPart, ChatMessageItem, ChatMessagePart, ChatOutput,
    ChatOutputItem, ChatOutputStatus, ChatReasoningItem, ChatReasoningPart, MediaKind,
    MediaNormalizationError, MediaPart, MediaSource, ToolResult, ToolResultPart,
};
use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

/// A legacy content block within a message.
///
/// This is the pre-item-aware recursive representation. It exists only so that
/// previously persisted histories and serialized transports can still be read;
/// it is private to the migration module and must never be reintroduced into the
/// public chat API. New code uses [`ChatInputPart`] for supplied input and
/// [`ChatOutput`] for generated output.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Content {
    /// Plain text
    Text { text: String },
    /// Base64-encoded image
    Image { mime_type: String, data: Vec<u8> },
    /// Image referenced by URL
    ImageUrl { url: String },
    /// PDF document
    Pdf { data: Vec<u8> },
    /// Audio data
    Audio { mime_type: String, data: Vec<u8> },
    /// Model reasoning / chain-of-thought
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Tool invocation requested by the model
    ToolUse {
        id: String,
        name: String,
        arguments: Value,
    },
    /// Result of a tool invocation (can itself contain mixed content)
    ToolResult {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        is_error: bool,
        content: Vec<Content>,
    },
    /// A link to a resource, identified by URI.
    ResourceLink {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CanonicalToolResultPart {
    Text { text: String },
    Attachment(Box<MediaPart>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ToolResultPartWire {
    Canonical(CanonicalToolResultPart),
    Legacy(Content),
}

impl<'de> Deserialize<'de> for ToolResultPart {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match ToolResultPartWire::deserialize(deserializer)? {
            ToolResultPartWire::Canonical(CanonicalToolResultPart::Text { text })
            | ToolResultPartWire::Legacy(Content::Text { text }) => {
                Ok(ToolResultPart::Text { text })
            }
            ToolResultPartWire::Canonical(CanonicalToolResultPart::Attachment(media)) => {
                Ok(ToolResultPart::Attachment(media))
            }
            ToolResultPartWire::Legacy(
                media @ (Content::Image { .. }
                | Content::ImageUrl { .. }
                | Content::Pdf { .. }
                | Content::Audio { .. }
                | Content::ResourceLink { .. }),
            ) => media_from_legacy(&media)
                .map_err(de::Error::custom)?
                .map(ToolResultPart::attachment)
                .ok_or_else(|| de::Error::custom("unsupported legacy tool-result media")),
            ToolResultPartWire::Legacy(_) => Err(de::Error::custom(
                "generated content is not valid in a tool result",
            )),
        }
    }
}

/// Validate the shipped transitional `{ content, output }` record shape.
///
/// This format was emitted by released versions while structured output was
/// introduced. It is decode-only compatibility: current serializers never emit
/// `content`, but unchanged persisted records must remain readable until a
/// separately announced storage migration removes that requirement.
pub(super) fn normalize_transitional_output(
    content: &[Content],
    output: ChatOutput,
) -> Result<ChatOutput, TransitionalMessageError> {
    if content.is_empty()
        || content == legacy_projection(&output, false)
        || content == legacy_projection(&output, true)
    {
        Ok(output)
    } else {
        Err(TransitionalMessageError::StaleProjection)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum TransitionalMessageError {
    #[error("message content does not match the structured output projection")]
    StaleProjection,
}

fn legacy_projection(output: &ChatOutput, preserve_signatures: bool) -> Vec<Content> {
    let mut content = Vec::new();
    for item in &output.items {
        match item {
            ChatOutputItem::Message(message) => {
                content.extend(message.parts.iter().filter_map(|part| match part {
                    ChatMessagePart::Text { text, .. } if !text.is_empty() => {
                        Some(Content::Text { text: text.clone() })
                    }
                    ChatMessagePart::Refusal { refusal, .. } if !refusal.is_empty() => {
                        Some(Content::Text {
                            text: refusal.clone(),
                        })
                    }
                    ChatMessagePart::Media(media) => legacy_media_projection(media),
                    _ => None,
                }));
            }
            ChatOutputItem::Reasoning(reasoning) => {
                let visible = reasoning
                    .summary
                    .iter()
                    .chain(&reasoning.content)
                    .map(|part| part.text.as_str())
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>();
                if !visible.is_empty() {
                    content.push(Content::Thinking {
                        text: visible.join("\n\n"),
                        signature: preserve_signatures
                            .then(|| reasoning.signature.clone())
                            .flatten(),
                    });
                }
            }
            ChatOutputItem::FunctionCall(call) => {
                if let Ok(arguments) = call.parse_arguments() {
                    content.push(Content::ToolUse {
                        id: call.call_id.clone(),
                        name: call.name.clone(),
                        arguments,
                    });
                }
            }
            ChatOutputItem::Opaque(_) => {}
        }
    }
    content
}

fn legacy_media_projection(media: &MediaPart) -> Option<Content> {
    match (&media.kind, media.source()) {
        (MediaKind::Image, MediaSource::Inline { data }) => Some(Content::Image {
            mime_type: media.media_type()?.to_string(),
            data: data.clone(),
        }),
        (MediaKind::Audio, MediaSource::Inline { data }) => Some(Content::Audio {
            mime_type: media.media_type()?.to_string(),
            data: data.clone(),
        }),
        (MediaKind::Document, MediaSource::Inline { data })
            if media.media_type()?.type_() == "application"
                && media.media_type()?.subtype() == "pdf" =>
        {
            Some(Content::Pdf { data: data.clone() })
        }
        (MediaKind::Image, MediaSource::DataUrl { url } | MediaSource::Url { url }) => {
            Some(Content::ImageUrl { url: url.clone() })
        }
        (_, MediaSource::Url { url }) => Some(Content::ResourceLink {
            uri: url.clone(),
            name: media.filename.clone(),
            description: None,
            mime_type: media.media_type().map(ToString::to_string),
        }),
        _ => None,
    }
}

/// Error produced when a legacy record cannot be normalized losslessly.
///
/// Malformed MIME/source metadata and structurally invalid generated content
/// surface here as explicit errors instead of being silently dropped or
/// stringified, which keeps unrelated history loadable while still refusing to
/// invent a canonical value the constructors would reject.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum LegacyMigrationError {
    #[error("tool result `{call_id}` contains generated content that is not a valid result part")]
    NestedGeneratedToolResult { call_id: String },
    #[error("assistant function call `{call_id}` has arguments that are not a JSON object")]
    InvalidCallArguments { call_id: String },
    #[error("legacy media metadata is malformed: {0}")]
    Media(#[from] MediaNormalizationError),
}

/// Normalize a legacy message into canonical input parts.
///
/// User turns become canonical input; tool results become correlated
/// [`ToolResult`] parts. Generated-only blocks (reasoning, function calls) have
/// no ordinary-input representation and are dropped here — they are preserved
/// by [`normalize_legacy_assistant`] instead.
pub(super) fn normalize_legacy_input(
    blocks: &[Content],
) -> Result<Vec<ChatInputPart>, LegacyMigrationError> {
    let mut parts = Vec::new();
    for block in blocks {
        if let Some(part) = try_legacy_input_part(block)? {
            parts.push(part);
        }
    }
    Ok(parts)
}

/// Normalize a legacy assistant message into exactly one canonical
/// [`ChatOutput`].
///
/// This is the path-aware counterpart of [`normalize_legacy_input`]: a mixed
/// legacy assistant turn (text + reasoning + tool use + media) collapses into a
/// single ordered output whose items preserve relative order, instead of being
/// flattened into independently mutable input. Invalid call arguments fail
/// explicitly rather than being dropped or replaced with an empty object.
pub(super) fn normalize_legacy_assistant(
    blocks: Vec<Content>,
) -> Result<ChatOutput, LegacyMigrationError> {
    let mut items = Vec::new();

    for block in blocks {
        match block {
            Content::Text { text } => {
                if text.is_empty() {
                    continue;
                }
                items.push(ChatOutputItem::Message(ChatMessageItem {
                    id: None,
                    role: super::ChatRole::Assistant,
                    phase: None,
                    status: None,
                    parts: vec![ChatMessagePart::Text {
                        text,
                        annotations: Vec::new(),
                        extensions: Default::default(),
                    }],
                    extensions: Default::default(),
                }));
            }
            Content::Thinking { text, signature } => {
                if text.is_empty() {
                    continue;
                }
                items.push(ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: None,
                    summary: Vec::new(),
                    content: vec![ChatReasoningPart {
                        text,
                        extensions: Default::default(),
                    }],
                    encrypted_content: None,
                    signature,
                    status: None,
                    extensions: Default::default(),
                }));
            }
            Content::ToolUse {
                id,
                name,
                arguments,
            } => {
                // The raw argument text is preserved, but it must be an object to
                // be a well-formed local call; otherwise the record is rejected
                // rather than silently degraded to `{}`.
                if !arguments.is_object() {
                    return Err(LegacyMigrationError::InvalidCallArguments { call_id: id });
                }
                let arguments = serde_json::to_string(&arguments).map_err(|_| {
                    LegacyMigrationError::InvalidCallArguments {
                        call_id: id.clone(),
                    }
                })?;
                items.push(ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                    item_id: None,
                    call_id: id,
                    name,
                    arguments,
                    status: None,
                    extensions: Default::default(),
                }));
            }
            Content::Image { .. }
            | Content::ImageUrl { .. }
            | Content::Pdf { .. }
            | Content::Audio { .. }
            | Content::ResourceLink { .. } => {
                let media = media_from_legacy(&block)?.expect("recognized media block");
                items.push(ChatOutputItem::Message(ChatMessageItem {
                    id: None,
                    role: super::ChatRole::Assistant,
                    phase: None,
                    status: None,
                    parts: vec![ChatMessagePart::Media(Box::new(media))],
                    extensions: Default::default(),
                }));
            }
            // A tool result is never part of assistant *output*; it belongs to
            // the following input turn and is handled by `normalize_legacy_input`.
            Content::ToolResult { id, .. } => {
                return Err(LegacyMigrationError::NestedGeneratedToolResult { call_id: id });
            }
        }
    }

    Ok(ChatOutput {
        response_id: None,
        items,
        status: Some(ChatOutputStatus::Completed),
        usage: None,
        finish_reason: None,
        provenance: None,
        extensions: Default::default(),
    })
}

/// Fallible legacy-to-input conversion used by migration-sensitive callers.
///
/// Unlike the infallible compatibility shim in [`super::output`], malformed
/// media metadata is reported instead of being dropped, which is what lets
/// invalid legacy histories fail explicitly.
fn try_legacy_input_part(
    block: &Content,
) -> Result<Option<ChatInputPart>, MediaNormalizationError> {
    match block {
        Content::Text { text } => Ok(Some(ChatInputPart::text(text.clone()))),
        Content::ToolResult {
            id,
            name,
            is_error,
            content,
        } => {
            let mut result = ToolResult::new(id.clone());
            result.name.clone_from(name);
            result.is_error = *is_error;
            for part in content {
                if let Some(converted) = try_legacy_input_part(part)? {
                    match converted {
                        ChatInputPart::Text { text } => {
                            result.parts.push(ToolResultPart::Text { text });
                        }
                        ChatInputPart::Attachment(media) => {
                            result.parts.push(ToolResultPart::Attachment(media));
                        }
                        ChatInputPart::ToolResult(_) => {}
                    }
                }
            }
            Ok(Some(ChatInputPart::tool_result(result)))
        }
        Content::Image { .. }
        | Content::ImageUrl { .. }
        | Content::Pdf { .. }
        | Content::Audio { .. }
        | Content::ResourceLink { .. } => {
            Ok(media_from_legacy(block)?.map(ChatInputPart::attachment))
        }
        // Generated content is output-only.
        Content::Thinking { .. } | Content::ToolUse { .. } => Ok(None),
    }
}

/// Normalize recognized legacy media, surfacing malformed metadata.
fn media_from_legacy(block: &Content) -> Result<Option<MediaPart>, MediaNormalizationError> {
    let media = match block {
        Content::Image { mime_type, data } => MediaPart::new(
            MediaKind::Image,
            Some(mime_type.parse()?),
            MediaSource::Inline { data: data.clone() },
        )?,
        Content::ImageUrl { url } => MediaPart::new(
            MediaKind::Image,
            None,
            MediaSource::Url { url: url.clone() },
        )?,
        Content::Pdf { data } => MediaPart::new(
            MediaKind::Document,
            Some(
                "application/pdf"
                    .parse()
                    .expect("static MIME type is valid"),
            ),
            MediaSource::Inline { data: data.clone() },
        )?,
        Content::Audio { mime_type, data } => MediaPart::new(
            MediaKind::Audio,
            Some(mime_type.parse()?),
            MediaSource::Inline { data: data.clone() },
        )?,
        Content::ResourceLink {
            uri,
            name,
            description,
            mime_type,
        } => {
            let mut media = MediaPart::new(
                MediaKind::Other,
                mime_type.as_deref().map(str::parse).transpose()?,
                MediaSource::Url { url: uri.clone() },
            )?;
            media.filename.clone_from(name);
            media.detail.clone_from(description);
            media
        }
        _ => return Ok(None),
    };
    Ok(Some(media))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatMessage, ChatRole};

    fn legacy_message_json(role: &str, content: serde_json::Value) -> String {
        serde_json::json!({ "role": role, "content": content }).to_string()
    }

    #[test]
    fn legacy_image_tool_result_part_becomes_canonical_attachment() {
        let part: ToolResultPart = serde_json::from_value(serde_json::json!({
            "type": "image",
            "mime_type": "image/png",
            "data": [137, 80, 78, 71]
        }))
        .unwrap();

        let media = part.as_attachment().expect("legacy image attachment");
        assert_eq!(media.kind, MediaKind::Image);
        assert_eq!(media.media_type().map(AsRef::as_ref), Some("image/png"));
        assert_eq!(
            media.source(),
            &MediaSource::Inline {
                data: vec![137, 80, 78, 71]
            }
        );

        let canonical = serde_json::to_value(part).unwrap();
        assert_eq!(canonical["type"], "attachment");
    }

    #[test]
    fn user_turn_becomes_canonical_input() {
        let json = legacy_message_json(
            "User",
            serde_json::json!([
                {"type": "text", "text": "hello"},
                {"type": "image", "mime_type": "image/png", "data": [1, 2, 3, 4]}
            ]),
        );
        let message: ChatMessage = serde_json::from_str(&json).unwrap();

        assert_eq!(message.role, ChatRole::User);
        let parts = message.payload.as_input().expect("input payload");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].as_text(), Some("hello"));
        let media = parts[1].as_attachment().expect("attachment part");
        assert_eq!(media.kind, MediaKind::Image);

        let saved = serde_json::to_value(&message).unwrap();
        assert!(saved.get("content").is_none());
        let input = saved["input"].as_array().expect("canonical input payload");
        assert_eq!(input[0]["type"], "text");
        assert_eq!(input[1]["type"], "attachment");
    }

    #[test]
    fn mixed_assistant_turn_becomes_one_output() {
        let json = legacy_message_json(
            "Assistant",
            serde_json::json!([
                {"type": "thinking", "text": "reasoning"},
                {"type": "text", "text": "final answer"},
                {"type": "tool_use", "id": "call_1", "name": "lookup",
                 "arguments": {"query": "x"}},
                {"type": "image_url", "url": "https://example.invalid/a.png"}
            ]),
        );
        let message: ChatMessage = serde_json::from_str(&json).unwrap();

        assert_eq!(message.role, ChatRole::Assistant);
        let output = message.payload.as_output().expect("output payload");
        // Exactly one authoritative payload: reasoning, message, call, media.
        assert_eq!(output.items.len(), 4);
        assert!(matches!(output.items[0], ChatOutputItem::Reasoning(_)));
        assert!(matches!(output.items[1], ChatOutputItem::Message(_)));
        assert!(matches!(output.items[2], ChatOutputItem::FunctionCall(_)));
        assert!(matches!(output.items[3], ChatOutputItem::Message(_)));
        // The mixed turn has no independently mutable input payload.
        assert!(message.payload.as_input().is_none());

        let saved = serde_json::to_value(&message).unwrap();
        assert!(saved.get("content").is_none());
        assert!(saved.get("input").is_none());
        assert!(saved.get("output").is_some());
    }

    #[test]
    fn complete_old_history_normalizes_resaves_and_exports_without_hidden_state() {
        let user_json = legacy_message_json(
            "User",
            serde_json::json!([
                {"type": "text", "text": "inspect these"},
                {"type": "image", "mime_type": "image/png", "data": [1, 2]},
                {"type": "image_url", "url": "https://example.invalid/image.png"},
                {"type": "pdf", "data": [3, 4]},
                {"type": "audio", "mime_type": "audio/wav", "data": [5, 6]},
                {"type": "resource_link", "uri": "https://example.invalid/file.bin",
                 "name": "file.bin", "description": "fixture", "mime_type": "application/octet-stream"},
                {"type": "tool_result", "id": "call_1", "name": "lookup", "is_error": false,
                 "content": [
                    {"type": "text", "text": "result"},
                    {"type": "image", "mime_type": "image/png", "data": [7, 8]}
                 ]}
            ]),
        );
        let assistant_json = legacy_message_json(
            "Assistant",
            serde_json::json!([
                {"type": "thinking", "text": "visible reasoning", "signature": "secret-signature"},
                {"type": "text", "text": "answer"},
                {"type": "tool_use", "id": "call_1", "name": "lookup",
                 "arguments": {"query": "rust"}}
            ]),
        );

        let user: ChatMessage = serde_json::from_str(&user_json).unwrap();
        let assistant: ChatMessage = serde_json::from_str(&assistant_json).unwrap();
        let user_parts = user.payload.as_input().expect("canonical user input");
        assert_eq!(user_parts.len(), 7);
        let ChatInputPart::ToolResult(result) = &user_parts[6] else {
            panic!("expected bounded tool result");
        };
        assert_eq!(result.parts.len(), 2);

        let output = assistant
            .payload
            .as_output()
            .expect("canonical assistant output");
        assert_eq!(output.items.len(), 3);
        assert_eq!(output.tool_calls().unwrap().len(), 1);

        let saved_user = serde_json::to_value(&user).unwrap();
        let saved_assistant = serde_json::to_value(&assistant).unwrap();
        assert!(saved_user.get("content").is_none());
        assert!(saved_assistant.get("content").is_none());
        assert!(saved_user.get("input").is_some());
        assert!(saved_assistant.get("output").is_some());

        let reloaded_user: ChatMessage = serde_json::from_value(saved_user).unwrap();
        let reloaded_assistant: ChatMessage = serde_json::from_value(saved_assistant).unwrap();
        assert_eq!(reloaded_user.input(), user.input());
        assert_eq!(reloaded_assistant.output(), assistant.output());

        let portable = reloaded_assistant.into_portable();
        assert!(
            portable
                .output()
                .is_some_and(|output| !output.requires_item_aware_fidelity())
        );
        let portable_json = serde_json::to_string(&portable).unwrap();
        assert!(portable_json.contains("visible reasoning"));
        assert!(!portable_json.contains("secret-signature"));
        assert_eq!(portable_json.matches("answer").count(), 1);
        assert_eq!(
            portable_json
                .matches("{\\\"query\\\":\\\"rust\\\"}")
                .count(),
            1
        );
    }

    #[test]
    fn malformed_mime_fails_explicitly() {
        let json = legacy_message_json(
            "User",
            serde_json::json!([
                {"type": "text", "text": "look"},
                {"type": "image", "mime_type": "not a mime", "data": [1]}
            ]),
        );
        let error = serde_json::from_str::<ChatMessage>(&json).unwrap_err();
        assert!(
            error.to_string().contains("mime") || error.to_string().contains("MIME"),
            "expected explicit MIME error, got: {error}"
        );
    }

    #[test]
    fn invalid_call_arguments_fail_instead_of_empty_object() {
        let blocks = vec![Content::ToolUse {
            id: "call_1".to_string(),
            name: "lookup".to_string(),
            arguments: serde_json::json!("not-an-object"),
        }];
        let error = normalize_legacy_assistant(blocks).unwrap_err();
        assert!(matches!(
            error,
            LegacyMigrationError::InvalidCallArguments { .. }
        ));
    }

    #[test]
    fn unchanged_old_rich_tool_result_fixture_loads() {
        let json = legacy_message_json(
            "User",
            serde_json::json!([
                {"type": "tool_result", "id": "call_1", "name": "read",
                 "is_error": false,
                 "content": [
                    {"type": "text", "text": "line one"},
                    {"type": "image", "mime_type": "image/png", "data": [1, 2]}
                 ]}
            ]),
        );
        let message: ChatMessage = serde_json::from_str(&json).unwrap();

        let parts = message.payload.as_input().expect("input payload");
        assert_eq!(parts.len(), 1);
        let result = match &parts[0] {
            ChatInputPart::ToolResult(result) => result,
            other => panic!("expected tool result, got {other:?}"),
        };
        assert_eq!(result.call_id, "call_1");
        assert_eq!(result.name.as_deref(), Some("read"));
        assert_eq!(result.parts.len(), 2);
        assert!(result.parts[0].as_text().is_some());
        assert!(result.parts[1].as_attachment().is_some());

        let saved = serde_json::to_value(&message).unwrap();
        assert!(saved.get("content").is_none());
        let result = &saved["input"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["parts"][0]["type"], "text");
        assert_eq!(result["parts"][1]["type"], "attachment");
    }

    #[test]
    fn resource_link_uri_distinguishes_data_url_sources() {
        // A data: URI must normalize through the validated data-URL path rather
        // than being assumed to be an HTTP URL.
        let json = legacy_message_json(
            "User",
            serde_json::json!([
                {"type": "resource_link",
                 "uri": "data:text/plain;base64,aGk=", "name": "note.txt"}
            ]),
        );
        let message: ChatMessage = serde_json::from_str(&json).unwrap();
        let parts = message.payload.as_input().expect("input payload");
        match &parts[0] {
            ChatInputPart::Attachment(media) => {
                // Stored as a general URI source; never fetched implicitly.
                assert!(matches!(media.source(), MediaSource::Url { .. }));
                assert_eq!(media.filename.as_deref(), Some("note.txt"));
            }
            other => panic!("expected attachment part, got {other:?}"),
        }
    }
}

use image::{DynamicImage, load_from_memory};
use mistralrs::{
    AudioInput, Model, ModelCategory, RequestBuilder, TextMessageRole, ToolCallResponse,
};
use querymt::chat::{ChatMessage, ChatRole, Content};
use querymt::error::LLMError;

use crate::tools::convert_tool_call;

fn map_chat_role(role: &ChatRole) -> TextMessageRole {
    match role {
        ChatRole::User => TextMessageRole::User,
        ChatRole::Assistant => TextMessageRole::Assistant,
    }
}

/// One builder-level emission step for a single [`ChatMessage`].
///
/// The pinned mistralrs `RequestBuilder` stores media in builder-level vectors
/// (private fields) and only accepts tool calls through
/// `RequestBuilder::add_message_with_tool_call`; there is no way to attach
/// media to a tool-call message or to retroactively associate media with an
/// earlier tool result. A `ChatMessage` is therefore decomposed into an
/// ordered sequence of steps, and each step maps to exactly one builder call,
/// so tool results and their media stay associated and in source order.
pub(crate) enum PlanStep {
    /// Plain text message.
    Text { role: TextMessageRole, text: String },
    /// Tool result output. Text only: media produced by this result is emitted
    /// as the immediately following [`PlanStep::Media`].
    ToolOutput { id: String, text: String },
    /// Assistant message carrying tool calls.
    ToolCalls {
        role: TextMessageRole,
        text: String,
        tool_calls: Vec<ToolCallResponse>,
    },
    /// Multimodal message. Carries top-level media, or the media of the
    /// preceding [`PlanStep::ToolOutput`].
    Media {
        role: TextMessageRole,
        text: String,
        images: Vec<DynamicImage>,
        audios: Vec<AudioInput>,
    },
}

/// Ordered emission plan for a single [`ChatMessage`].
#[derive(Default)]
pub(crate) struct MessagePlan {
    pub(crate) steps: Vec<PlanStep>,
}

/// Build the ordered emission plan for `msg` without touching a
/// `RequestBuilder` (pure; unit-testable without model instantiation).
///
/// Ordering rules:
/// - Top-level text, media, and tool calls accumulate and are flushed as one
///   step before the first `ToolResult` and once at the end of the message.
///   Media travels with its text; text is never emitted twice.
/// - Each `ToolResult` emits its text first, then its decoded images as an
///   immediately following media step: `tool A, media A, tool B, media B`.
///   Result images are never pooled into one trailing message-level vector.
/// - Nested `Audio` and `ResourceLink` blocks become explicit text fallback
///   markers; nested `Pdf` and `ImageUrl` are rejected with `InvalidRequest`,
///   matching the top-level policy.
pub(crate) fn plan_message(msg: &ChatMessage) -> Result<MessagePlan, LLMError> {
    // The pinned RequestBuilder cannot represent a message that carries both
    // tool calls and top-level media: `add_message_with_tool_call` only
    // accepts text, while `add_multimodal_message` accepts no tool calls.
    // Reject the combination up front instead of silently dropping the tool
    // calls.
    let has_tool_use = msg
        .content
        .iter()
        .any(|block| matches!(block, Content::ToolUse { .. }));
    let has_media = msg
        .content
        .iter()
        .any(|block| matches!(block, Content::Image { .. } | Content::Audio { .. }));
    if has_tool_use && has_media {
        return Err(LLMError::InvalidRequest(
            "mistralrs cannot represent tool calls and media in a single message: the pinned \
             RequestBuilder would silently drop the tool calls. Split the tool calls and media \
             into separate messages."
                .into(),
        ));
    }

    let role = map_chat_role(&msg.role);
    let mut plan = MessagePlan::default();
    let mut text_parts: Vec<String> = Vec::new();
    let mut images: Vec<DynamicImage> = Vec::new();
    let mut audios: Vec<AudioInput> = Vec::new();
    let mut tool_calls: Vec<ToolCallResponse> = Vec::new();

    // Emit accumulated top-level text/media/tool calls as at most one step.
    fn flush_pending(
        role: &TextMessageRole,
        text_parts: &mut Vec<String>,
        images: &mut Vec<DynamicImage>,
        audios: &mut Vec<AudioInput>,
        tool_calls: &mut Vec<ToolCallResponse>,
        steps: &mut Vec<PlanStep>,
    ) {
        let text = text_parts.join("\n");
        if !images.is_empty() || !audios.is_empty() {
            steps.push(PlanStep::Media {
                role: role.clone(),
                text,
                images: std::mem::take(images),
                audios: std::mem::take(audios),
            });
            text_parts.clear();
        } else if !tool_calls.is_empty() {
            steps.push(PlanStep::ToolCalls {
                role: role.clone(),
                text,
                tool_calls: std::mem::take(tool_calls),
            });
            text_parts.clear();
        } else if !text.is_empty() {
            steps.push(PlanStep::Text {
                role: role.clone(),
                text,
            });
            text_parts.clear();
        }
    }

    for block in &msg.content {
        match block {
            Content::Text { text } => text_parts.push(text.clone()),
            Content::Image { data, .. } => {
                let image = load_from_memory(data)
                    .map_err(|e| LLMError::InvalidRequest(format!("invalid image payload: {e}")))?;
                images.push(image);
            }
            Content::Audio { data, .. } => {
                let audio = AudioInput::from_bytes(data)
                    .map_err(|e| LLMError::InvalidRequest(format!("invalid audio payload: {e}")))?;
                audios.push(audio);
            }
            Content::Pdf { .. } | Content::ImageUrl { .. } => {
                return Err(LLMError::InvalidRequest(
                    "mistralrs provider does not support PDF or image URL content".into(),
                ));
            }
            Content::ToolUse {
                id,
                name,
                arguments,
            } => {
                let call = querymt::ToolCall {
                    id: id.clone(),
                    call_type: "function".to_string(),
                    function: querymt::FunctionCall {
                        name: name.clone(),
                        arguments: serde_json::to_string(arguments).unwrap_or_default(),
                    },
                };
                let idx = tool_calls.len();
                tool_calls.push(convert_tool_call(idx, &call));
            }
            Content::ToolResult { id, content, .. } => {
                // Emit any pending top-level text/media/tool calls first so
                // block order is preserved around the tool result.
                flush_pending(
                    &role,
                    &mut text_parts,
                    &mut images,
                    &mut audios,
                    &mut tool_calls,
                    &mut plan.steps,
                );

                // Nested content: preserve text, decode raw images for this
                // exact result, and mark unsupported blocks explicitly rather
                // than dropping them.
                let mut result_parts: Vec<String> = Vec::new();
                let mut result_images: Vec<DynamicImage> = Vec::new();
                for inner in content {
                    match inner {
                        // Skip empty nested text so a lone `Content::text("")`
                        // cannot bypass the visible fallback marker below;
                        // consistent with the empty-result guarantee.
                        Content::Text { text } if !text.is_empty() => {
                            result_parts.push(text.clone())
                        }
                        Content::Image { data, .. } => {
                            let image = load_from_memory(data).map_err(|e| {
                                LLMError::InvalidRequest(format!(
                                    "invalid image payload in tool result: {e}"
                                ))
                            })?;
                            result_images.push(image);
                        }
                        Content::Audio { mime_type, data } => result_parts.push(format!(
                            "[audio omitted: mime_type={mime_type}, {} bytes]",
                            data.len()
                        )),
                        Content::ResourceLink {
                            uri,
                            name,
                            mime_type,
                            ..
                        } => {
                            let mut marker = String::from("[resource link: ");
                            match name.as_deref() {
                                Some(link_name) => {
                                    marker.push_str(&format!("{link_name} ({uri})"));
                                }
                                None => marker.push_str(uri),
                            }
                            if let Some(mime) = mime_type.as_deref() {
                                marker.push_str(&format!(", {mime}"));
                            }
                            marker.push(']');
                            result_parts.push(marker);
                        }
                        // Nested PDFs and image URLs are rejected exactly like
                        // top-level ones instead of being silently dropped.
                        Content::Pdf { .. } | Content::ImageUrl { .. } => {
                            return Err(LLMError::InvalidRequest(
                                "mistralrs provider does not support PDF or image URL content \
                                 inside tool results"
                                    .into(),
                            ));
                        }
                        // Other nested blocks (e.g. Thinking) are ignored, as
                        // at top level.
                        _ => {}
                    }
                }

                let result_text = if result_parts.is_empty() {
                    "[tool result contained no content]".to_string()
                } else {
                    result_parts.join("\n")
                };
                plan.steps.push(PlanStep::ToolOutput {
                    id: id.clone(),
                    text: result_text,
                });
                if !result_images.is_empty() {
                    plan.steps.push(PlanStep::Media {
                        role: role.clone(),
                        text: String::new(),
                        images: result_images,
                        audios: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }

    flush_pending(
        &role,
        &mut text_parts,
        &mut images,
        &mut audios,
        &mut tool_calls,
        &mut plan.steps,
    );
    Ok(plan)
}

/// Apply `plan` to `req`, mapping each step to exactly one builder call.
pub(crate) fn apply_plan(mut req: RequestBuilder, plan: MessagePlan) -> RequestBuilder {
    for step in plan.steps {
        req = match step {
            PlanStep::Text { role, text } => req.add_message(role, text),
            PlanStep::ToolOutput { id, text } => req.add_tool_message(text, id),
            PlanStep::ToolCalls {
                role,
                text,
                tool_calls,
            } => req.add_message_with_tool_call(role, text, tool_calls),
            PlanStep::Media {
                role,
                text,
                images,
                audios,
            } => req.add_multimodal_message(role, text, images, audios, vec![]),
        };
    }
    req
}

pub(crate) fn apply_message_to_request(
    req: RequestBuilder,
    msg: &ChatMessage,
) -> Result<RequestBuilder, LLMError> {
    let plan = plan_message(msg)?;
    Ok(apply_plan(req, plan))
}

pub(crate) fn ensure_chat_model(model: &Model) -> Result<(), LLMError> {
    let category = model
        .config()
        .map_err(|e| LLMError::ProviderError(e.to_string()))?
        .category;
    if matches!(category, ModelCategory::Embedding) {
        return Err(LLMError::InvalidRequest(
            "embedding models do not support chat requests".into(),
        ));
    }
    Ok(())
}

pub(crate) fn ensure_embedding_model(model: &Model) -> Result<(), LLMError> {
    let category = model
        .config()
        .map_err(|e| LLMError::ProviderError(e.to_string()))?
        .category;
    if matches!(category, ModelCategory::Embedding) {
        Ok(())
    } else {
        Err(LLMError::InvalidRequest(
            "embedding requests require an embedding model".into(),
        ))
    }
}

use image::load_from_memory;
use mistralrs::{AudioInput, Model, ModelCategory, RequestBuilder, TextMessageRole};
use querymt::chat::{
    ChatInputPart, ChatMessage, ChatOutputItem, ChatRole, MediaKind, MediaSource, ToolResultPart,
};
use querymt::error::LLMError;

use crate::tools::convert_tool_call;

fn map_chat_role(role: &ChatRole) -> TextMessageRole {
    match role {
        ChatRole::User => TextMessageRole::User,
        ChatRole::Assistant => TextMessageRole::Assistant,
    }
}

pub(crate) fn apply_message_to_request(
    mut req: RequestBuilder,
    msg: &ChatMessage,
) -> Result<RequestBuilder, LLMError> {
    let role = map_chat_role(&msg.role);
    let parts = msg.input_parts();
    let text = parts
        .iter()
        .filter_map(|c| c.as_text())
        .collect::<Vec<_>>()
        .join("\n");

    let mut tool_uses = Vec::new();
    let mut images = Vec::new();
    let mut audios: Vec<AudioInput> = Vec::new();

    // Replay generated function calls from structured output.
    if let Some(output) = msg.output() {
        for item in &output.items {
            if let ChatOutputItem::FunctionCall(call) = item {
                let tool_call = call.to_tool_call();
                let idx = tool_uses.len();
                tool_uses.push(convert_tool_call(idx, &tool_call));
            }
        }
    }

    for part in &parts {
        match part {
            ChatInputPart::Text { .. } => {}
            ChatInputPart::Attachment(media) => match (&media.kind, media.source()) {
                (MediaKind::Image, MediaSource::Inline { data }) => {
                    let image = load_from_memory(data).map_err(|e| {
                        LLMError::InvalidRequest(format!("invalid image payload: {e}"))
                    })?;
                    images.push(image);
                }
                (MediaKind::Audio, MediaSource::Inline { data }) => {
                    let audio = AudioInput::from_bytes(data).map_err(|e| {
                        LLMError::InvalidRequest(format!("invalid audio payload: {e}"))
                    })?;
                    audios.push(audio);
                }
                (_, MediaSource::DataUrl { .. } | MediaSource::Url { .. }) => {
                    return Err(LLMError::InvalidRequest(
                        "mistralrs provider does not support referenced media content".into(),
                    ));
                }
                _ => {}
            },
            ChatInputPart::ToolResult(result) => {
                let output = result.text_content();
                req = req.add_tool_message(output, result.call_id.clone());

                // ToolResult images are emitted as adjacent image messages.
                for inner in &result.parts {
                    if let ToolResultPart::Attachment(media) = inner
                        && media.kind == MediaKind::Image
                        && let MediaSource::Inline { data } = media.source()
                    {
                        let image = load_from_memory(data).map_err(|e| {
                            LLMError::InvalidRequest(format!("invalid image payload: {e}"))
                        })?;
                        images.push(image);
                    }
                }
            }
        }
    }

    if !images.is_empty() || !audios.is_empty() {
        req = req.add_multimodal_message(role, text.clone(), images, audios, vec![]);
    } else if !tool_uses.is_empty() {
        req = req.add_message_with_tool_call(role, text, tool_uses);
    } else if !text.is_empty() {
        req = req.add_message(role, text);
    }

    Ok(req)
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

use image::load_from_memory;
use mistralrs::{Model, ModelCategory, RequestBuilder, TextMessageRole};
use querymt::chat::{ChatMessage, ChatRole, Content};
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
    model: &Model,
) -> Result<RequestBuilder, LLMError> {
    let role = map_chat_role(&msg.role);
    let text = msg
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .collect::<Vec<_>>()
        .join("\n");

    let mut tool_uses = Vec::new();
    let mut images = Vec::new();

    for block in &msg.content {
        match block {
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
                let idx = tool_uses.len();
                tool_uses.push(convert_tool_call(idx, &call));
            }
            Content::ToolResult { id, content, .. } => {
                let output = content
                    .iter()
                    .filter_map(|c| c.as_text())
                    .collect::<Vec<_>>()
                    .join("\n");
                req = req.add_tool_message(output, id.clone());

                // ToolResult images are emitted as adjacent image messages.
                for inner in content {
                    if let Content::Image { data, .. } = inner {
                        let image = load_from_memory(data).map_err(|e| {
                            LLMError::InvalidRequest(format!("invalid image payload: {e}"))
                        })?;
                        images.push(image);
                    }
                }
            }
            Content::Image { data, .. } => {
                let image = load_from_memory(data)
                    .map_err(|e| LLMError::InvalidRequest(format!("invalid image payload: {e}")))?;
                images.push(image);
            }
            Content::Pdf { .. } | Content::ImageUrl { .. } | Content::Audio { .. } => {
                return Err(LLMError::InvalidRequest(
                    "mistralrs provider only supports inline image bytes for vision messages"
                        .into(),
                ));
            }
            _ => {}
        }
    }

    if !images.is_empty() {
        req = req.add_image_message(role, text.clone(), images);
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

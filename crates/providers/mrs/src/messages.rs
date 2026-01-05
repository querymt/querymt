use image::load_from_memory;
use mistralrs::{Model, ModelCategory, RequestBuilder, TextMessageRole};
use querymt::chat::{ChatMessage, ChatRole, MessageType};
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
    match &msg.message_type {
        MessageType::Text => {
            let role = map_chat_role(&msg.role);
            Ok(req.add_message(role, msg.content.clone()))
        }
        MessageType::ToolResult(calls) => {
            for call in calls {
                req = req.add_tool_message(call.function.arguments.clone(), call.id.clone());
            }
            Ok(req)
        }
        MessageType::ToolUse(calls) => {
            let role = map_chat_role(&msg.role);
            let tool_calls = calls
                .iter()
                .enumerate()
                .map(|(index, call)| convert_tool_call(index, call))
                .collect();
            Ok(req.add_message_with_tool_call(role, msg.content.clone(), tool_calls))
        }
        MessageType::Image((_, raw_bytes)) => {
            let role = map_chat_role(&msg.role);
            let image = load_from_memory(raw_bytes)
                .map_err(|e| LLMError::InvalidRequest(format!("invalid image payload: {e}")))?;
            req.add_image_message(role, msg.content.clone(), vec![image], model)
                .map_err(|e| LLMError::InvalidRequest(format!("{:#}", e)))
        }
        MessageType::Pdf(_) | MessageType::ImageURL(_) => Err(LLMError::InvalidRequest(
            "mistralrs provider only supports inline image bytes for vision messages".into(),
        )),
    }
}

pub(crate) fn ensure_chat_model(model: &Model) -> Result<(), LLMError> {
    let category = model
        .config()
        .map_err(|e| LLMError::ProviderError(e))?
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
        .map_err(|e| LLMError::ProviderError(e))?
        .category;
    if matches!(category, ModelCategory::Embedding) {
        Ok(())
    } else {
        Err(LLMError::InvalidRequest(
            "embedding requests require an embedding model".into(),
        ))
    }
}

//! Building the Anthropic chat request from canonical messages.

use crate::auth::{oauth_billing_header, oauth_text_block};
use crate::config::Anthropic;
use crate::model::ThinkingClass;
use crate::wire::request::{
    AnthropicCompleteRequest, AnthropicMessage, ImageSource, ImageUrlContent, MessageContent,
    ToolResultContent,
};
use crate::wire::system::{AnthropicSystemPrompt, cache_control_from_hint};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use http::header::CONTENT_TYPE;
use http::{Method, Request};
use querymt::chat::{
    ChatInputPart, ChatMessage, ChatOutputItem, ChatRole, MediaKind, MediaPart, MediaSource, Tool,
    ToolChoice, ToolResultPart,
};
use querymt::error::LLMError;
use serde_json::Value;
use std::collections::HashMap;

/// `input` for replaying a stored assistant tool call.
///
/// Anthropic requires `input` to be a JSON object. Stored arguments that do not
/// parse to one are replayed as `{}`, a protocol placeholder only: the raw
/// stored arguments are untouched (this only reads them), the agent answered the
/// call with an error result instead of running it, and the placeholder is never
/// executable input. The warning carries the call ID, never the argument text.
pub(crate) fn replay_tool_input(call: &querymt::chat::ChatFunctionCallItem) -> Value {
    match call.parse_arguments() {
        Ok(input @ Value::Object(_)) => input,
        _ => {
            log::warn!(
                "Replaying tool call {} with `input: {{}}`: its stored arguments are not a JSON object",
                call.call_id
            );
            Value::Object(Default::default())
        }
    }
}

/// Convert a canonical attachment into an Anthropic message content block.
///
/// Unsupported sources (provider file references) return `None` rather than
/// fabricating content.
pub(crate) fn attachment_to_message_content(media: &MediaPart) -> Option<MessageContent> {
    match (&media.kind, media.source()) {
        (MediaKind::Image, MediaSource::Inline { data }) => {
            let media_type = media.media_type()?.to_string();
            Some(MessageContent::Image {
                content_type: "image",
                source: ImageSource {
                    source_type: "base64",
                    media_type,
                    data: BASE64.encode(data),
                },
                cache_control: None,
            })
        }
        (MediaKind::Image, MediaSource::DataUrl { url } | MediaSource::Url { url }) => {
            Some(MessageContent::ImageUrl {
                content_type: "image_url",
                image_url: ImageUrlContent { url: url.clone() },
                cache_control: None,
            })
        }
        (MediaKind::Document, MediaSource::Inline { data }) => {
            let media_type = media
                .media_type()
                .map(ToString::to_string)
                .unwrap_or_else(|| "application/pdf".to_string());
            Some(MessageContent::Document {
                content_type: "document",
                source: ImageSource {
                    source_type: "base64",
                    media_type,
                    data: BASE64.encode(data),
                },
                cache_control: None,
            })
        }
        _ => None,
    }
}

/// Convert one canonical tool-result part into an Anthropic result content block.
pub(crate) fn tool_result_content(part: &ToolResultPart) -> Option<ToolResultContent> {
    match part {
        ToolResultPart::Text { text } => Some(ToolResultContent::Text {
            content_type: "text",
            text: text.clone(),
        }),
        ToolResultPart::Attachment(media) => match media.source() {
            MediaSource::Inline { data } if media.kind == MediaKind::Image => {
                Some(ToolResultContent::Image {
                    content_type: "image",
                    source: ImageSource {
                        source_type: "base64",
                        media_type: media.media_type()?.to_string(),
                        data: BASE64.encode(data),
                    },
                })
            }
            _ => None,
        },
    }
}

impl Anthropic {
    /// Builds the wire request for the settings in `self` (streaming is
    /// controlled by `self.stream`).
    pub(crate) fn build_chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        if self.resolved_key().is_empty() {
            return Err(LLMError::AuthError("Missing Anthropic API key".to_string()));
        }
        self.declared_tool_names(tools)?;

        let anthropic_messages: Vec<AnthropicMessage> = messages
            .iter()
            .map(|m| {
                let mut content: Vec<MessageContent> = Vec::new();

                // Replay generated reasoning and function calls from structured
                // output; these are output-only semantics.
                if let Some(output) = m.output() {
                    for item in &output.items {
                        match item {
                            ChatOutputItem::Reasoning(reasoning) => {
                                let is_redacted = reasoning
                                    .extensions
                                    .get(querymt::chat::ANTHROPIC_BLOCK_EXTENSION)
                                    .and_then(Value::as_str)
                                    == Some(querymt::chat::ANTHROPIC_REDACTED_THINKING);
                                if is_redacted {
                                    // Opaque payload, replayed unchanged.
                                    if let Some(data) = &reasoning.encrypted_content {
                                        content.push(MessageContent::RedactedThinking {
                                            content_type: "redacted_thinking",
                                            data: data.clone(),
                                        });
                                    }
                                } else if let Some(signature) = &reasoning.signature {
                                    // Anthropic requires signed thinking blocks when
                                    // replaying assistant history, even when the text
                                    // is empty (display "omitted"). Unsigned ones are
                                    // skipped.
                                    let text = reasoning
                                        .summary
                                        .iter()
                                        .chain(&reasoning.content)
                                        .map(|part| part.text.as_str())
                                        .collect::<Vec<_>>()
                                        .join("\n\n");
                                    content.push(MessageContent::Thinking {
                                        content_type: "thinking",
                                        thinking: text,
                                        signature: signature.clone(),
                                        cache_control: None,
                                    });
                                }
                            }
                            ChatOutputItem::FunctionCall(call) => {
                                let input = replay_tool_input(call);
                                content.push(MessageContent::ToolUse {
                                    content_type: "tool_use",
                                    id: call.call_id.clone(),
                                    name: self.prefix_tool_name(&call.name),
                                    input,
                                    cache_control: None,
                                });
                            }
                            _ => {}
                        }
                    }
                }

                for part in m.portable_input_parts() {
                    match part {
                        ChatInputPart::Text { text } => {
                            // Avoid Anthropic API error: "text content blocks must be non-empty"
                            if !text.is_empty() {
                                content.push(MessageContent::Text {
                                    content_type: "text",
                                    text,
                                    cache_control: None,
                                });
                            }
                        }
                        ChatInputPart::Attachment(media) => {
                            if let Some(block) = attachment_to_message_content(&media) {
                                content.push(block);
                            }
                        }
                        ChatInputPart::ToolResult(result) => {
                            // Anthropic supports multi-content tool results natively:
                            // { type: "tool_result", tool_use_id, content: [text, image, ...] }
                            let tool_content: Vec<ToolResultContent> = result
                                .parts
                                .iter()
                                .filter_map(tool_result_content)
                                .collect();
                            content.push(MessageContent::ToolResult {
                                content_type: "tool_result",
                                tool_use_id: result.call_id.clone(),
                                is_error: result.is_error.then_some(true),
                                content: tool_content,
                                cache_control: None,
                            });
                        }
                    }
                }

                // Apply cache_control to the last content block if present
                if let Some(cache_hint) = &m.cache
                    && let Some(last) = content.last_mut()
                {
                    last.set_cache_control(cache_control_from_hint(cache_hint));
                }

                AnthropicMessage {
                    role: match m.role {
                        ChatRole::User => "user",
                        ChatRole::Assistant => "assistant",
                    },
                    content,
                }
            })
            .collect();

        let maybe_tool_slice: Option<&[Tool]> = tools.or(self.tools.as_deref());
        let anthropic_tools = maybe_tool_slice.map(|slice| self.build_tools(slice));

        let tool_choice = match self.tool_choice {
            Some(ToolChoice::Auto) => {
                Some(HashMap::from([("type".to_string(), "auto".to_string())]))
            }
            Some(ToolChoice::Any) => Some(HashMap::from([("type".to_string(), "any".to_string())])),
            Some(ToolChoice::Tool(ref tool_name)) => Some(HashMap::from([
                ("type".to_string(), "tool".to_string()),
                ("name".to_string(), self.prefix_tool_name(tool_name)),
            ])),
            Some(ToolChoice::None) => {
                Some(HashMap::from([("type".to_string(), "none".to_string())]))
            }
            None => None,
        };

        let final_tool_choice = if anthropic_tools.is_some() {
            tool_choice.clone()
        } else {
            None
        };

        let (mut thinking, output_config) = self.thinking_for_request();

        // Forced tool use (`any` or a named tool) interacts with thinking.
        let forced_tool_choice = final_tool_choice
            .as_ref()
            .and_then(|tc| tc.get("type"))
            .is_some_and(|t| t == "any" || t == "tool");
        if forced_tool_choice {
            if self
                .parsed_model()
                .is_some_and(|m| m.rejects_forced_tool_choice())
            {
                return Err(LLMError::InvalidRequest(format!(
                    "Model {} does not support a forced tool_choice (`any` or a named tool)",
                    self.model
                )));
            }
            if self.thinking_class() == ThinkingClass::Budget && thinking.is_some() {
                log::debug!(target: "qmt_anthropic", "dropping thinking for {}: forced tool_choice", self.model);
                thinking = None;
            }
        }
        let output_config = if thinking.is_some() {
            output_config
        } else {
            None
        };
        let (temperature, top_p, top_k) = self.sampling_for(thinking.is_some());

        // Use sanitized system prompt for OAuth requests
        let mut sanitized_system = self.sanitize_system_prompt();
        if self.is_oauth() {
            self.validate_oauth_version()?;
            if let Some(first_user) = anthropic_messages
                .iter()
                .find(|message| message.role == "user")
            {
                let text = first_user
                    .content
                    .iter()
                    .find_map(|block| match block {
                        MessageContent::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or_default();
                if let Some(AnthropicSystemPrompt::Blocks(blocks)) = &mut sanitized_system {
                    blocks.insert(
                        0,
                        oauth_text_block(oauth_billing_header(text, self.oauth_version())),
                    );
                }
            }
        }

        let req_body = AnthropicCompleteRequest {
            messages: anthropic_messages,
            model: &self.model,
            max_tokens: Some(self.max_tokens),
            temperature,
            system: sanitized_system,
            stream: self.stream,
            top_p,
            top_k,
            tools: anthropic_tools,
            tool_choice: final_tool_choice,
            thinking,
            output_config,
        };

        let json_req = serde_json::to_vec(&req_body)?;
        let mut url = Anthropic::default_base_url().join("messages")?;

        // Add beta query parameter for OAuth requests
        if self.is_oauth() {
            url.query_pairs_mut().append_pair("beta", "true");
        }

        let builder = Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json");

        let builder = self.add_auth_headers(builder);

        Ok(builder.body(json_req)?)
    }
}

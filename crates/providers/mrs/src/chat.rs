use std::collections::HashMap;

use futures::TryFutureExt;
use mistralrs::{ChatCompletionResponse, RequestBuilder, ResponseOk};
use querymt::chat::{ChatMessage, ChatProvider, ChatResponse, StreamChunk, Tool};
use querymt::error::LLMError;
use querymt::{FunctionCall, ToolCall, Usage};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::messages::{apply_message_to_request, ensure_chat_model};
use crate::model::MistralRS;
use crate::streaming::{
    MistralToolUseState, flush_tool_states, parse_mistral_done_response, parse_mistral_stream_chunk,
};
use crate::tools::{build_mistral_tools, map_tool_choice};

#[derive(Debug, Deserialize)]
struct MistralChatResponse {
    text: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

impl std::fmt::Display for MistralChatResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.text)
    }
}

impl From<ChatCompletionResponse> for MistralChatResponse {
    fn from(value: ChatCompletionResponse) -> Self {
        let choice = value.choices.first();
        let tool_calls = choice
            .and_then(|choice| choice.message.tool_calls.as_ref())
            .map(|calls| {
                calls
                    .iter()
                    .map(|call| ToolCall {
                        id: call.id.clone(),
                        call_type: call.tp.to_string(),
                        function: FunctionCall {
                            name: call.function.name.clone(),
                            arguments: call.function.arguments.clone(),
                        },
                    })
                    .collect()
            });
        let usage = Some(Usage {
            input_tokens: u32::try_from(value.usage.prompt_tokens).unwrap_or(u32::MAX),
            output_tokens: u32::try_from(value.usage.completion_tokens).unwrap_or(u32::MAX),
            ..Default::default()
        });
        let finish_reason = choice.map(|choice| choice.finish_reason.clone());

        MistralChatResponse {
            text: choice.and_then(|choice| choice.message.content.clone()),
            tool_calls,
            usage,
            finish_reason,
        }
    }
}

impl ChatResponse for MistralChatResponse {
    fn text(&self) -> Option<String> {
        self.text.clone()
    }
    fn usage(&self) -> Option<querymt::Usage> {
        self.usage.clone()
    }
    fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
        self.tool_calls.clone()
    }
    fn thinking(&self) -> Option<String> {
        None
    }
    fn finish_reason(&self) -> Option<querymt::chat::FinishReason> {
        self.finish_reason
            .clone()
            .map(|reason| match reason.as_str() {
                "stop" => querymt::chat::FinishReason::Stop,
                "tool_calls" => querymt::chat::FinishReason::ToolCalls,
                "length" => querymt::chat::FinishReason::Length,
                _other => querymt::chat::FinishReason::Other,
            })
    }
}

fn build_chat_request(
    provider: &MistralRS,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
) -> Result<RequestBuilder, LLMError> {
    ensure_chat_model(&provider.mrs_model)?;
    let mut req = RequestBuilder::new();
    for msg in messages {
        req = apply_message_to_request(req, msg, &provider.mrs_model)?;
    }

    let tools = tools.or(provider.config.tools.as_deref());
    if let Some(tool_list) = tools {
        let mistral_tools = build_mistral_tools(tool_list)?;
        if !mistral_tools.is_empty() {
            req = req.set_tools(mistral_tools.clone());
            if let Some(choice) = provider.config.tool_choice.as_ref() {
                req = req.set_tool_choice(map_tool_choice(choice, &mistral_tools)?);
            }
        }
    }

    Ok(req)
}

#[async_trait::async_trait]
impl ChatProvider for MistralRS {
    fn supports_streaming(&self) -> bool {
        true
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        let req = build_chat_request(self, messages, tools)?;
        let response = self
            .mrs_model
            .send_chat_request(req)
            .map_err(|e| LLMError::InvalidRequest(format!("{:#}", e)))
            .await?;

        let response = MistralChatResponse::from(response);
        Ok(Box::new(response))
    }

    async fn chat_stream_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
        LLMError,
    > {
        let req = build_chat_request(self, messages, tools)?;

        let model = std::sync::Arc::clone(&self.mrs_model);
        let (tx, rx) = mpsc::unbounded_channel();
        let task_tx = tx.clone();

        let task = async move {
            let mut stream = match model.stream_chat_request(req).await {
                Ok(stream) => stream,
                Err(e) => {
                    let _ = task_tx.send(Err(LLMError::InvalidRequest(format!("{:#}", e))));
                    return;
                }
            };

            let mut tool_states: HashMap<usize, MistralToolUseState> = HashMap::new();
            let mut done_emitted = false;
            let mut usage_emitted = false;

            while let Some(resp) = stream.next().await {
                let mut chunks = match resp.as_result() {
                    Ok(ResponseOk::Chunk(chunk)) => parse_mistral_stream_chunk(
                        chunk,
                        &mut tool_states,
                        &mut done_emitted,
                        &mut usage_emitted,
                    ),
                    Ok(ResponseOk::Done(done)) => parse_mistral_done_response(
                        done,
                        &mut tool_states,
                        &mut done_emitted,
                        &mut usage_emitted,
                    ),
                    Ok(other) => {
                        let _ = task_tx.send(Err(LLMError::ProviderError(format!(
                            "unexpected mistral.rs stream response: {:#?}",
                            other
                        ))));
                        return;
                    }
                    Err(e) => {
                        let _ = task_tx.send(Err(LLMError::ProviderError(format!("{:#}", e))));
                        return;
                    }
                };

                for chunk in chunks.drain(..) {
                    if task_tx.send(Ok(chunk)).is_err() {
                        return;
                    }
                }
            }

            if !done_emitted {
                let mut chunks = Vec::new();
                flush_tool_states(&mut tool_states, &mut chunks);
                chunks.push(StreamChunk::Done {
                    stop_reason: "end_turn".to_string(),
                });
                for chunk in chunks {
                    let _ = task_tx.send(Ok(chunk));
                }
            }
        };

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(task);
            }
            Err(_) => {
                std::thread::spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            let _ = tx.send(Err(LLMError::ProviderError(format!("{:#}", e))));
                            return;
                        }
                    };
                    runtime.block_on(task);
                });
            }
        }

        let output_stream = futures::stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|item| (item, rx))
        });

        Ok(Box::pin(output_stream))
    }
}

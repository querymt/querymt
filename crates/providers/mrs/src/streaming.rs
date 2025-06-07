use std::collections::HashMap;

use mistralrs::{ChatCompletionChunkResponse, ChatCompletionResponse, ToolCallResponse};
use querymt::{FunctionCall, ToolCall, Usage};
use querymt::chat::StreamChunk;

#[derive(Default, Debug)]
pub(crate) struct MistralToolUseState {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments_buffer: String,
    pub(crate) started: bool,
}

fn usage_from_mistral(usage: &mistralrs::Usage) -> Usage {
    Usage {
        input_tokens: u32::try_from(usage.prompt_tokens).unwrap_or(u32::MAX),
        output_tokens: u32::try_from(usage.completion_tokens).unwrap_or(u32::MAX),
    }
}

fn map_finish_reason(reason: &str) -> String {
    match reason {
        "tool_calls" => "tool_use".to_string(),
        "stop" => "end_turn".to_string(),
        "length" => "max_tokens".to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn flush_tool_states(
    tool_states: &mut HashMap<usize, MistralToolUseState>,
    chunks: &mut Vec<StreamChunk>,
) {
    for (index, state) in tool_states.drain() {
        if state.started {
            chunks.push(StreamChunk::ToolUseComplete {
                index,
                tool_call: ToolCall {
                    id: state.id,
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: state.name,
                        arguments: state.arguments_buffer,
                    },
                },
            });
        }
    }
}

fn emit_tool_call_chunks(
    call: &ToolCallResponse,
    tool_states: &mut HashMap<usize, MistralToolUseState>,
    chunks: &mut Vec<StreamChunk>,
) {
    let index = call.index;
    let state = tool_states.entry(index).or_default();

    if !call.id.is_empty() {
        state.id = call.id.clone();
    }

    if !call.function.name.is_empty() {
        state.name = call.function.name.clone();
        if !state.started {
            state.started = true;
            chunks.push(StreamChunk::ToolUseStart {
                index,
                id: state.id.clone(),
                name: state.name.clone(),
            });
        }
    }

    if !call.function.arguments.is_empty() {
        state.arguments_buffer.push_str(&call.function.arguments);
        chunks.push(StreamChunk::ToolUseInputDelta {
            index,
            partial_json: call.function.arguments.clone(),
        });
    }
}

pub(crate) fn parse_mistral_stream_chunk(
    chunk: ChatCompletionChunkResponse,
    tool_states: &mut HashMap<usize, MistralToolUseState>,
    done_emitted: &mut bool,
    usage_emitted: &mut bool,
) -> Vec<StreamChunk> {
    let mut chunks = Vec::new();

    for choice in chunk.choices {
        if let Some(content) = &choice.delta.content {
            if !content.is_empty() {
                chunks.push(StreamChunk::Text(content.clone()));
            }
        }

        if let Some(tool_calls) = &choice.delta.tool_calls {
            for call in tool_calls {
                emit_tool_call_chunks(call, tool_states, &mut chunks);
            }
        }

        if let Some(finish_reason) = &choice.finish_reason {
            flush_tool_states(tool_states, &mut chunks);
            chunks.push(StreamChunk::Done {
                stop_reason: map_finish_reason(finish_reason),
            });
            *done_emitted = true;
        }
    }

    if let Some(usage) = &chunk.usage {
        if !*usage_emitted {
            chunks.push(StreamChunk::Usage(usage_from_mistral(usage)));
            *usage_emitted = true;
        }
    }

    chunks
}

pub(crate) fn parse_mistral_done_response(
    response: ChatCompletionResponse,
    tool_states: &mut HashMap<usize, MistralToolUseState>,
    done_emitted: &mut bool,
    usage_emitted: &mut bool,
) -> Vec<StreamChunk> {
    let mut chunks = Vec::new();

    if let Some(choice) = response.choices.get(0) {
        if let Some(tool_calls) = &choice.message.tool_calls {
            for call in tool_calls {
                emit_tool_call_chunks(call, tool_states, &mut chunks);
            }
        }

        flush_tool_states(tool_states, &mut chunks);

        if !*done_emitted {
            chunks.push(StreamChunk::Done {
                stop_reason: map_finish_reason(&choice.finish_reason),
            });
            *done_emitted = true;
        }
    }

    if !*usage_emitted {
        chunks.push(StreamChunk::Usage(usage_from_mistral(&response.usage)));
        *usage_emitted = true;
    }

    chunks
}

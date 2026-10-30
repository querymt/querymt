//! Server-sent-event stream parser.

mod blocks;
mod state;

use crate::config::Anthropic;
use crate::error::map_anthropic_api_error;
use crate::stream::state::AnthropicBlockState;
use crate::wire::response::AnthropicStreamResponse;
use crate::wire::usage::AnthropicUsage;
use querymt::Usage;
use querymt::chat::http::ChatStreamParser;
use querymt::chat::{ChatOutputStatus, FinishReason, LENGTH_CUTOFF_DETAIL, StructuredStreamEvent};
use querymt::error::LLMError;
use std::collections::{BTreeMap, HashMap};

/// Wrap a structured event as a stream chunk.
pub(crate) fn structured(event: StructuredStreamEvent) -> querymt::chat::StreamChunk {
    querymt::chat::StreamChunk::Structured(event)
}

pub(crate) struct AnthropicStreamParser {
    /// Raw, presence-aware usage accumulated across this attempt's events.
    pub(crate) usage: Option<AnthropicUsage>,
    /// Whether a `message_delta` has reported usage for this response.
    pub(crate) final_usage: bool,
    /// Whether response metadata has been emitted (it must precede every item).
    pub(crate) metadata_sent: bool,
    /// Set once the response terminal is emitted; later block events emit nothing.
    pub(crate) terminated: bool,
    pub(crate) oauth: bool,
    /// Request-scoped wire-to-original tool-name mapping.
    pub(crate) tool_names: HashMap<String, String>,
    /// In-flight content blocks keyed by block index (also the output index).
    pub(crate) blocks: BTreeMap<usize, AnthropicBlockState>,
}

impl AnthropicStreamParser {
    /// Emit in-progress metadata if no `message_start` has done so yet.
    pub(crate) fn ensure_metadata(&mut self, chunks: &mut Vec<querymt::chat::StreamChunk>) {
        if !self.metadata_sent {
            self.metadata_sent = true;
            chunks.push(in_progress_metadata(None, None));
        }
    }

    /// Usage for the terminal: the overlaid final value when `message_delta`
    /// reported usage, otherwise the start usage (output count withheld).
    pub(crate) fn terminal_usage(&self) -> Option<Usage> {
        let usage = self.usage.clone()?;
        Some(if self.final_usage {
            Usage::from(usage)
        } else {
            usage.into_start_usage()
        })
    }
}

/// In-progress response metadata, optionally carrying start usage.
pub(crate) fn in_progress_metadata(
    response_id: Option<String>,
    usage: Option<Usage>,
) -> querymt::chat::StreamChunk {
    structured(StructuredStreamEvent::ResponseMetadata {
        response_id,
        status: Some(ChatOutputStatus::InProgress),
        usage,
        finish_reason: None,
        provenance: None,
    })
}

impl ChatStreamParser for AnthropicStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<querymt::chat::StreamChunk>, LLMError> {
        let text = std::str::from_utf8(chunk).map_err(|e| LLMError::GenericError(e.to_string()))?;
        let mut chunks = Vec::new();

        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }

                let stream_resp: AnthropicStreamResponse =
                    serde_json::from_str(data).map_err(|e| LLMError::ResponseFormatError {
                        message: format!("Failed to parse Anthropic stream data: {}", e),
                        raw_response: data.to_string(),
                    })?;

                match stream_resp.response_type.as_str() {
                    "error" => {
                        return Err(map_anthropic_api_error(
                            stream_resp.error.as_ref(),
                            None,
                            None,
                            false,
                        )
                        .into());
                    }
                    _ if self.terminated => {
                        // Events after the terminal (late block stops, message_stop,
                        // a repeated message_start) carry nothing the consumer can
                        // still accept.
                    }
                    "message_start" if self.metadata_sent => {
                        log::warn!(
                            target: "qmt_anthropic",
                            "ignoring repeated message_start within one response"
                        );
                    }
                    "message_start" => {
                        let message = stream_resp.message;
                        let response_id = message.as_ref().and_then(|m| m.id.clone());
                        let usage = message.and_then(|m| m.usage);
                        let start_usage = usage.as_ref().map(|u| u.clone().into_start_usage());
                        if usage.is_some() {
                            self.usage = usage;
                        }
                        self.metadata_sent = true;
                        chunks.push(in_progress_metadata(response_id, start_usage));
                    }
                    "content_block_start" => {
                        if let (Some(index), Some(block)) =
                            (stream_resp.index, stream_resp.content_block)
                        {
                            self.ensure_metadata(&mut chunks);
                            self.start_block(index, block, &mut chunks);
                        }
                    }
                    "content_block_delta" => {
                        if let (Some(index), Some(delta)) = (stream_resp.index, stream_resp.delta) {
                            self.apply_delta(index, delta, &mut chunks);
                        }
                    }
                    "content_block_stop" => {
                        if let Some(index) = stream_resp.index
                            && let Some(state) = self.blocks.remove(&index)
                        {
                            chunks.push(structured(StructuredStreamEvent::ItemCompleted {
                                output_index: index,
                                item: state.into_item(false),
                            }));
                        }
                    }
                    "message_delta" => {
                        if let Some(usage) = stream_resp.usage {
                            // Overlay onto the start event so fields this delta
                            // omits (input, cache) keep their start values.
                            self.usage = Some(match self.usage.take() {
                                Some(mut earlier) => {
                                    earlier.overlay(usage);
                                    earlier
                                }
                                None => usage,
                            });
                            self.final_usage = true;
                        }

                        if let Some(delta) = stream_resp.delta
                            && let Some(stop_reason) = delta.stop_reason
                        {
                            self.ensure_metadata(&mut chunks);
                            let finish_reason = Anthropic::map_stop_reason_with_details(
                                &stop_reason,
                                delta.stop_details.as_ref(),
                            );
                            let length = finish_reason == FinishReason::Length;
                            // Truncation leaves tool blocks open. Complete each one
                            // exactly once, in index order, marked invalid, before
                            // the terminal.
                            if length {
                                let open: Vec<usize> = self
                                    .blocks
                                    .iter()
                                    .filter(|(_, state)| {
                                        matches!(state, AnthropicBlockState::Tool(_))
                                    })
                                    .map(|(index, _)| *index)
                                    .collect();
                                for index in open {
                                    if let Some(state) = self.blocks.remove(&index) {
                                        chunks.push(structured(
                                            StructuredStreamEvent::ItemCompleted {
                                                output_index: index,
                                                item: state.into_item(true),
                                            },
                                        ));
                                    }
                                }
                            }
                            chunks.push(structured(StructuredStreamEvent::ResponseTerminal {
                                status: if length {
                                    ChatOutputStatus::Incomplete
                                } else {
                                    ChatOutputStatus::Completed
                                },
                                usage: self.terminal_usage(),
                                finish_reason: Some(finish_reason),
                                detail: length.then(|| LENGTH_CUTOFF_DETAIL.to_string()),
                            }));
                            self.terminated = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(chunks)
    }
}

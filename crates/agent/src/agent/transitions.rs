//! State transition methods for the agent execution state machine
//!
//! This module contains all the state transition logic used by the execution
//! state machine. Each transition method handles a specific state and returns
//! the next state to transition to.

use crate::agent::core::{QueryMTAgent, SessionRuntime};
use crate::delegation::{format_delegation_completion_message, format_delegation_failure_message};
use crate::events::{AgentEvent, AgentEventKind};
use crate::middleware::{
    ConversationContext, ExecutionState, LlmResponse, TokenUsage, ToolCall as MiddlewareToolCall,
    ToolFunction, ToolResult, WaitCondition, WaitReason,
};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::TaskStatus;
use crate::session::runtime::RuntimeContext;
use agent_client_protocol::{ContentBlock, ContentChunk, SessionUpdate, StopReason, TextContent};
use anyhow::Context;
use futures_util::StreamExt;
use log::{debug, info};
use querymt::chat::{ChatRole, FinishReason, StreamChunk};
use querymt::plugin::extism_impl::ExtismChatResponse;
use querymt::{ToolCall, Usage};
use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use tracing::instrument;
use uuid::Uuid;

/// Parameters for processing tool calls transition
pub(crate) struct ProcessingToolCallsParams<'a> {
    pub remaining_calls: &'a Arc<[MiddlewareToolCall]>,
    pub results: &'a Arc<[ToolResult]>,
    pub context: &'a Arc<ConversationContext>,
    pub runtime: Option<&'a SessionRuntime>,
    pub runtime_context: &'a mut RuntimeContext,
    pub cancel_rx: &'a watch::Receiver<bool>,
    pub session_id: &'a str,
}

impl QueryMTAgent {
    #[instrument(
        name = "agent.state.before_turn",
        skip(self, context, runtime, cancel_rx),
        fields(
            session_id = %session_id,
            steps = %context.stats.steps
        )
    )]
    pub(crate) async fn transition_before_turn(
        &self,
        context: &Arc<ConversationContext>,
        runtime: Option<&SessionRuntime>,
        cancel_rx: &watch::Receiver<bool>,
        session_id: &str,
    ) -> Result<ExecutionState, anyhow::Error> {
        debug!(
            "BeforeTurn: session={}, steps={}",
            session_id, context.stats.steps
        );

        if *cancel_rx.borrow() {
            return Ok(ExecutionState::Cancelled);
        }

        let provider_context = self
            .provider
            .with_session(Some(session_id.to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get provider context: {}", e))?;
        let provider = provider_context
            .provider()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to build provider: {}", e))?;

        let tools = self.collect_tools(provider, runtime);

        // Serialize tools to compute hash
        let tools_json =
            serde_json::to_vec(&tools).context("Failed to serialize tools for hash computation")?;
        let new_hash = crate::hash::RapidHash::new(&tools_json);

        // Check if changed (or first time)
        let should_emit = if let Some(runtime) = runtime {
            let mut current = runtime.current_tools_hash.lock().unwrap();
            let changed = current.is_none_or(|h| h != new_hash);
            if changed {
                *current = Some(new_hash);
            }
            changed
        } else {
            true // No runtime = first time, emit
        };

        if should_emit {
            self.emit_event(
                session_id,
                crate::events::AgentEventKind::ToolsAvailable {
                    tools: tools.clone(),
                    tools_hash: new_hash,
                },
            );
        }

        Ok(ExecutionState::CallLlm {
            context: context.clone(),
            tools: Arc::from(tools.into_boxed_slice()),
        })
    }

    #[instrument(
        name = "agent.llm_call",
        skip(self, context, tools, cancel_rx),
        fields(
            session_id = %session_id,
            message_count = %context.messages.len(),
            tool_count = %tools.len(),
            input_tokens = tracing::field::Empty,
            output_tokens = tracing::field::Empty,
            finish_reason = tracing::field::Empty,
            cost_usd = tracing::field::Empty
        )
    )]
    pub(crate) async fn transition_call_llm(
        &self,
        context: &Arc<ConversationContext>,
        tools: &Arc<[querymt::chat::Tool]>,
        cancel_rx: &watch::Receiver<bool>,
        session_id: &str,
    ) -> Result<ExecutionState, anyhow::Error> {
        debug!(
            "CallLlm: session={}, messages={}",
            session_id,
            context.messages.len()
        );

        if *cancel_rx.borrow() {
            return Ok(ExecutionState::Cancelled);
        }

        self.emit_event(
            session_id,
            AgentEventKind::LlmRequestStart {
                message_count: context.messages.len(),
            },
        );

        let provider_context = self
            .provider
            .with_session(Some(session_id.to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get provider context: {}", e))?;

        let response = if tools.is_empty() {
            provider_context
                .submit_request(&context.messages)
                .await
                .map_err(|e| anyhow::anyhow!("LLM request failed: {}", e))?
        } else {
            let provider = provider_context
                .provider()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to build provider: {}", e))?;

            // NOTE(codex): Codex backend is streaming-only.
            // Backend sends `{"detail":"Stream must be set to true"}`
            // Keep this special-case for now narrow to avoid changing behavior for other providers.
            if context.provider.as_ref() == "codex" {
                let mut stream = provider
                    .chat_stream_with_tools(&context.messages, Some(tools))
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("LLM streaming request with tools failed: {}", e)
                    })?;

                let mut text = String::new();
                let mut tool_calls: Vec<ToolCall> = Vec::new();
                let mut tool_call_ids = std::collections::HashSet::new();
                let mut usage: Option<Usage> = None;

                while let Some(item) = stream.next().await {
                    if *cancel_rx.borrow() {
                        return Ok(ExecutionState::Cancelled);
                    }

                    match item.map_err(|e| {
                        anyhow::anyhow!("LLM streaming request with tools failed: {}", e)
                    })? {
                        StreamChunk::Text(delta) => text.push_str(&delta),
                        StreamChunk::ToolUseComplete { tool_call, .. } => {
                            if tool_call_ids.insert(tool_call.id.clone()) {
                                tool_calls.push(tool_call);
                            }
                        }
                        StreamChunk::Usage(u) => usage = Some(u),
                        StreamChunk::Done { .. } => break,
                        _ => {}
                    }
                }

                let finish_reason = if tool_calls.is_empty() {
                    Some(FinishReason::Stop)
                } else {
                    Some(FinishReason::ToolCalls)
                };

                Box::new(ExtismChatResponse {
                    text: if text.is_empty() { None } else { Some(text) },
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    thinking: None,
                    usage,
                    finish_reason,
                })
            } else {
                provider
                    .chat_with_tools(&context.messages, Some(tools))
                    .await
                    .map_err(|e| anyhow::anyhow!("LLM request with tools failed: {}", e))?
            }
        };

        let usage = response
            .usage()
            .map(|u| TokenUsage::new(u.input_tokens as u64, u.output_tokens as u64));
        let response_content = response.text().unwrap_or_default();
        let tool_calls = response.tool_calls().unwrap_or_default();
        let finish_reason = response.finish_reason();

        // Calculate cost for this request using ModelPricing methods
        let (request_cost, cumulative_cost) = if let Some(usage_info) = response.usage() {
            let pricing = provider_context.get_pricing().await.ok().flatten();
            let request_cost = pricing.as_ref().and_then(|p| {
                p.calculate_cost(
                    usage_info.input_tokens as u64,
                    usage_info.output_tokens as u64,
                )
            });

            // Calculate cumulative cost
            let cumulative_cost = pricing.as_ref().and_then(|p| {
                p.calculate_cost(
                    context.stats.total_input_tokens + usage_info.input_tokens as u64,
                    context.stats.total_output_tokens + usage_info.output_tokens as u64,
                )
            });

            (request_cost, cumulative_cost)
        } else {
            (None, None)
        };

        info!(
            "Session {} received provider response ({} chars, {} tool call(s), finish: {:?}, cost: ${:.4?})",
            session_id,
            response_content.len(),
            tool_calls.len(),
            finish_reason,
            request_cost,
        );

        // Record metrics in the tracing span
        let span = tracing::Span::current();
        if let Some(usage_info) = response.usage() {
            span.record("input_tokens", usage_info.input_tokens as u64);
            span.record("output_tokens", usage_info.output_tokens as u64);
        }
        span.record("finish_reason", format!("{:?}", finish_reason).as_str());
        if let Some(cost) = request_cost {
            span.record("cost_usd", cost);
        }

        self.emit_event(
            session_id,
            AgentEventKind::LlmRequestEnd {
                usage: response.usage(),
                tool_calls: tool_calls.len(),
                finish_reason,
                cost_usd: request_cost,
                cumulative_cost_usd: cumulative_cost,
            },
        );

        let llm_tool_calls: Vec<MiddlewareToolCall> = tool_calls
            .into_iter()
            .map(|tc| MiddlewareToolCall {
                id: tc.id.clone(),
                function: ToolFunction {
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                },
            })
            .collect();

        Ok(ExecutionState::AfterLlm {
            response: Arc::new(LlmResponse::new(
                response_content,
                llm_tool_calls,
                usage,
                finish_reason,
            )),
            context: context.clone(),
        })
    }

    #[instrument(
        name = "agent.state.after_llm",
        skip(self, response, context, runtime_context, cancel_rx),
        fields(
            session_id = %session_id,
            has_tool_calls = %response.has_tool_calls()
        )
    )]
    pub(crate) async fn transition_after_llm(
        &self,
        response: &Arc<LlmResponse>,
        context: &Arc<ConversationContext>,
        runtime_context: &mut RuntimeContext,
        cancel_rx: &watch::Receiver<bool>,
        session_id: &str,
    ) -> Result<ExecutionState, anyhow::Error> {
        debug!(
            "AfterLlm: session={}, has_tool_calls={}",
            session_id,
            response.has_tool_calls()
        );

        if *cancel_rx.borrow() {
            return Ok(ExecutionState::Cancelled);
        }

        // Record progress for LLM response
        let progress_description = if response.has_tool_calls() {
            format!(
                "Received response with {} tool call(s)",
                response.tool_calls.len()
            )
        } else {
            "Received response from LLM".to_string()
        };

        let progress_entry = runtime_context
            .record_progress(
                crate::session::domain::ProgressKind::Note,
                progress_description,
                None,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to record progress: {}", e))?;

        self.emit_event(
            session_id,
            AgentEventKind::ProgressRecorded { progress_entry },
        );

        let mut parts = Vec::new();
        if !response.content.is_empty() {
            self.send_session_update(
                session_id,
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(response.content.clone()),
                ))),
            );
            parts.push(MessagePart::Text {
                content: response.content.clone(),
            });
        }

        for call in &response.tool_calls {
            parts.push(MessagePart::ToolUse(querymt::ToolCall {
                id: call.id.clone(),
                call_type: "function".to_string(),
                function: querymt::FunctionCall {
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                },
            }));
        }

        let assistant_msg = AgentMessage {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        };

        let provider_context = self
            .provider
            .with_session(Some(session_id.to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get provider context: {}", e))?;

        provider_context
            .add_message(assistant_msg.clone())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store assistant message: {}", e))?;

        self.emit_event(
            session_id,
            AgentEventKind::AssistantMessageStored {
                content: response.content.clone(),
            },
        );

        let mut messages = (*context.messages).to_vec();
        messages.push(assistant_msg.to_chat_message());

        // Update stats with usage and cost information
        let mut updated_stats = (*context.stats).clone();
        if let Some(token_usage) = &response.usage {
            updated_stats.total_input_tokens += token_usage.input_tokens;
            updated_stats.total_output_tokens += token_usage.output_tokens;
            updated_stats.steps += 1;

            // Update cost information if pricing is available
            if let Ok(Some(pricing)) = provider_context.get_pricing().await {
                updated_stats.update_costs(&pricing);
            }
        }

        let new_context = Arc::new(ConversationContext::new(
            context.session_id.clone(),
            Arc::from(messages.into_boxed_slice()),
            Arc::new(updated_stats),
            context.provider.clone(),
            context.model.clone(),
        ));

        // Use finish_reason as the source of truth for execution flow control
        match response.finish_reason {
            Some(FinishReason::ToolCalls) => {
                // Model wants to call tools - process them
                if response.tool_calls.len() == 1 {
                    Ok(ExecutionState::BeforeToolCall {
                        call: Arc::new(response.tool_calls[0].clone()),
                        context: new_context,
                    })
                } else {
                    Ok(ExecutionState::ProcessingToolCalls {
                        remaining_calls: Arc::from(response.tool_calls.clone().into_boxed_slice()),
                        results: Arc::from(Vec::new().into_boxed_slice()),
                        context: new_context,
                    })
                }
            }

            Some(FinishReason::Stop) => {
                // Model is done - mark active task as complete if any
                if runtime_context.active_task.is_some() {
                    if let Err(e) = runtime_context.update_task_status(TaskStatus::Done).await {
                        debug!("Failed to auto-complete task on stop: {}", e);
                    } else if let Some(task) = runtime_context.active_task.clone() {
                        self.emit_event(session_id, AgentEventKind::TaskStatusChanged { task });
                    }
                }
                Ok(ExecutionState::Complete)
            }

            Some(FinishReason::Length) => {
                // Model hit token limit
                Ok(ExecutionState::Stopped {
                    reason: StopReason::MaxTokens,
                    message: "Model hit token limit".into(),
                })
            }

            Some(FinishReason::ContentFilter) => {
                // Response blocked by content filter
                Ok(ExecutionState::Stopped {
                    reason: StopReason::EndTurn,
                    message: "Response blocked by content filter".into(),
                })
            }

            Some(FinishReason::Error)
            | Some(FinishReason::Other)
            | Some(FinishReason::Unknown)
            | None => {
                // Fallback for backwards compatibility - check tool_calls
                if response.tool_calls.is_empty() {
                    Ok(ExecutionState::Complete)
                } else if response.tool_calls.len() == 1 {
                    Ok(ExecutionState::BeforeToolCall {
                        call: Arc::new(response.tool_calls[0].clone()),
                        context: new_context,
                    })
                } else {
                    Ok(ExecutionState::ProcessingToolCalls {
                        remaining_calls: Arc::from(response.tool_calls.clone().into_boxed_slice()),
                        results: Arc::from(Vec::new().into_boxed_slice()),
                        context: new_context,
                    })
                }
            }
        }
    }

    #[instrument(
        name = "agent.state.before_tool",
        skip(self, call, context, runtime, runtime_context, cancel_rx),
        fields(
            session_id = %session_id,
            tool_name = %call.function.name
        )
    )]
    pub(crate) async fn transition_before_tool(
        &self,
        call: &Arc<MiddlewareToolCall>,
        context: &Arc<ConversationContext>,
        runtime: Option<&SessionRuntime>,
        runtime_context: &mut RuntimeContext,
        cancel_rx: &watch::Receiver<bool>,
        session_id: &str,
    ) -> Result<ExecutionState, anyhow::Error> {
        debug!(
            "BeforeToolCall: session={}, tool={}",
            session_id, call.function.name
        );

        if *cancel_rx.borrow() {
            return Ok(ExecutionState::Cancelled);
        }

        let tool_result = self
            .execute_tool_call(call, context, runtime, runtime_context, session_id)
            .await?;

        Ok(ExecutionState::AfterTool {
            result: Arc::new(tool_result),
            context: context.clone(),
        })
    }

    #[instrument(
        name = "agent.state.after_tool",
        skip(self, result, context, runtime_context),
        fields(
            session_id = %session_id,
            tool_name = %result.tool_name.as_deref().unwrap_or("unknown"),
            is_error = %result.is_error
        )
    )]
    pub(crate) async fn transition_after_tool(
        &self,
        result: &Arc<ToolResult>,
        context: &Arc<ConversationContext>,
        runtime_context: &mut RuntimeContext,
        session_id: &str,
    ) -> Result<ExecutionState, anyhow::Error> {
        debug!(
            "AfterTool: session={}, tool={}, is_error={}",
            session_id,
            result.tool_name.as_deref().unwrap_or("unknown"),
            result.is_error
        );

        let provider_context = self
            .provider
            .with_session(Some(session_id.to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get provider context: {}", e))?;

        let mut parts = vec![MessagePart::ToolResult {
            call_id: result.call_id.clone(),
            content: result.content.clone(),
            is_error: result.is_error,
            tool_name: result.tool_name.clone(),
            tool_arguments: result.tool_arguments.clone(),
        }];
        if let Some(ref snapshot) = result.snapshot_part {
            parts.push(snapshot.clone());
        }

        let result_msg = crate::model::AgentMessage {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::User,
            parts,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        };

        provider_context
            .add_message(result_msg.clone())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store tool result: {}", e))?;

        let mut messages = (*context.messages).to_vec();
        messages.push(result_msg.to_chat_message());

        let new_context = Arc::new(ConversationContext::new(
            context.session_id.clone(),
            Arc::from(messages.into_boxed_slice()),
            context.stats.clone(),
            context.provider.clone(),
            context.model.clone(),
        ));

        if let Some(wait_condition) = self
            .record_tool_side_effects(result, runtime_context, session_id)
            .await
        {
            return Ok(ExecutionState::WaitingForEvent {
                context: new_context,
                wait: wait_condition,
            });
        }

        Ok(ExecutionState::BeforeTurn {
            context: new_context,
        })
    }

    #[instrument(
        name = "agent.state.processing_tools",
        skip(self, params),
        fields(
            session_id = %params.session_id,
            remaining = %params.remaining_calls.len(),
            completed = %params.results.len()
        )
    )]
    pub(crate) async fn transition_processing_tool_calls(
        &self,
        params: ProcessingToolCallsParams<'_>,
    ) -> Result<ExecutionState, anyhow::Error> {
        let ProcessingToolCallsParams {
            remaining_calls,
            results,
            context,
            runtime,
            runtime_context,
            cancel_rx,
            session_id,
        } = params;

        debug!(
            "ProcessingToolCalls: session={}, remaining={}, completed={}",
            session_id,
            remaining_calls.len(),
            results.len()
        );

        if *cancel_rx.borrow() {
            return Ok(ExecutionState::Cancelled);
        }

        if remaining_calls.is_empty() {
            // All tool calls processed, store all results and continue
            return self
                .store_all_tool_results(results, context, runtime_context, session_id)
                .await;
        }

        // Process the next tool call
        let current_call = &remaining_calls[0];
        let tool_result = self
            .execute_tool_call(current_call, context, runtime, runtime_context, session_id)
            .await?;

        // Add result to accumulated results
        let mut new_results = (*results).to_vec();
        new_results.push(tool_result);

        // Remove the processed call from remaining
        let new_remaining = &remaining_calls[1..];

        Ok(ExecutionState::ProcessingToolCalls {
            remaining_calls: Arc::from(new_remaining),
            results: Arc::from(new_results.into_boxed_slice()),
            context: context.clone(),
        })
    }

    #[instrument(
        name = "agent.state.waiting",
        skip(self, wait, context, runtime_context, cancel_rx, event_rx),
        fields(
            session_id = %session_id,
            reason = ?wait.reason
        )
    )]
    pub(crate) async fn transition_waiting_for_event(
        &self,
        wait: &WaitCondition,
        context: &Arc<ConversationContext>,
        runtime_context: &mut RuntimeContext,
        cancel_rx: &mut watch::Receiver<bool>,
        event_rx: &mut broadcast::Receiver<AgentEvent>,
        session_id: &str,
    ) -> Result<ExecutionState, anyhow::Error> {
        debug!(
            "WaitingForEvent: session={}, reason={:?}",
            session_id, wait.reason
        );

        if *cancel_rx.borrow() {
            return Ok(ExecutionState::Cancelled);
        }

        loop {
            tokio::select! {
                _ = cancel_rx.changed() => {
                    if *cancel_rx.borrow() {
                        return Ok(ExecutionState::Cancelled);
                    }
                }
                event = event_rx.recv() => {
                    let event = match event {
                        Ok(event) => event,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Ok(ExecutionState::Stopped {
                                reason: StopReason::EndTurn,
                                message: "Event stream closed while waiting".into(),
                            });
                        }
                    };

                    if event.session_id != session_id {
                        continue;
                    }

                    if let Some(message) = match_wait_event(wait, &event) {
                        let new_context = self
                            .inject_wait_message(context, runtime_context, session_id, message)
                            .await?;
                        return Ok(ExecutionState::BeforeTurn {
                            context: new_context,
                        });
                    }
                }
            }
        }
    }

    async fn inject_wait_message(
        &self,
        context: &Arc<ConversationContext>,
        _runtime_context: &mut RuntimeContext,
        session_id: &str,
        content: String,
    ) -> Result<Arc<ConversationContext>, anyhow::Error> {
        let provider_context = self
            .provider
            .with_session(Some(session_id.to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get provider context: {}", e))?;

        let agent_msg = AgentMessage {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
                content: content.clone(),
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        };

        provider_context
            .add_message(agent_msg)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store wait message: {}", e))?;

        self.emit_event(
            session_id,
            AgentEventKind::UserMessageStored {
                content: content.clone(),
            },
        );

        Ok(Arc::new(context.inject_message(content)))
    }
}

fn match_wait_event(wait: &WaitCondition, event: &AgentEvent) -> Option<String> {
    match wait.reason {
        WaitReason::Delegation => match &event.kind {
            AgentEventKind::DelegationCompleted {
                delegation_id,
                result,
            } => {
                if !wait.correlation_ids.is_empty() && !wait.correlation_ids.contains(delegation_id)
                {
                    return None;
                }
                let summary = result.as_deref().unwrap_or("No summary provided.");
                Some(format_delegation_completion_message(delegation_id, summary))
            }
            AgentEventKind::DelegationFailed {
                delegation_id,
                error,
            } => {
                if !wait.correlation_ids.is_empty() && !wait.correlation_ids.contains(delegation_id)
                {
                    return None;
                }
                Some(format_delegation_failure_message(delegation_id, error))
            }
            _ => None,
        },
    }
}

//! State transition methods for the agent execution state machine
//!
//! This module contains all the state transition logic used by the execution
//! state machine. Each transition method handles a specific state and returns
//! the next state to transition to.

use crate::agent::core::QueryMTAgent;
use crate::agent::execution_context::ExecutionContext;
use crate::delegation::{format_delegation_completion_message, format_delegation_failure_message};
use crate::events::{AgentEvent, AgentEventKind, ExecutionMetrics, StopType};
use crate::middleware::{
    ConversationContext, ExecutionState, LlmResponse, ToolCall as MiddlewareToolCall, ToolFunction,
    ToolResult, WaitCondition, WaitReason, calculate_context_tokens,
};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::TaskStatus;
use agent_client_protocol::{ContentBlock, ContentChunk, SessionUpdate, TextContent};
use anyhow::{Context, anyhow};
use futures_util::StreamExt;
use futures_util::future::join_all;
use log::{debug, info};
use querymt::chat::{CacheHint, ChatMessage, ChatRole, FinishReason, StreamChunk};
use querymt::plugin::extism_impl::ExtismChatResponse;
use querymt::{ToolCall, Usage};
use std::sync::Arc;
use time;
use tokio::sync::{broadcast, watch};
use tracing::instrument;
use uuid::Uuid;

/// Apply cache breakpoints to the last 2 messages in the conversation.
/// This enables prompt caching for providers that support it (e.g., Anthropic).
fn apply_cache_breakpoints(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let len = messages.len();
    messages
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            let mut m = msg.clone();
            // Mark last 2 messages with ephemeral cache hint (default TTL)
            if len >= 2 && i >= len - 2 {
                m.cache = Some(CacheHint::Ephemeral { ttl_seconds: None });
            }
            m
        })
        .collect()
}

/// Parameters for processing tool calls transition
pub(crate) struct ProcessingToolCallsParams<'a> {
    pub remaining_calls: &'a Arc<[MiddlewareToolCall]>,
    pub results: &'a Arc<[ToolResult]>,
    pub context: &'a Arc<ConversationContext>,
    pub exec_ctx: &'a mut ExecutionContext,
    pub cancel_rx: &'a watch::Receiver<bool>,
}

impl QueryMTAgent {
    #[instrument(
        name = "agent.state.before_turn",
        skip(self, context, exec_ctx, cancel_rx),
        fields(
            session_id = %exec_ctx.session_id,
            steps = %context.stats.steps
        )
    )]
    pub(crate) async fn transition_before_llm_call(
        &self,
        context: &Arc<ConversationContext>,
        exec_ctx: &ExecutionContext,
        cancel_rx: &watch::Receiver<bool>,
    ) -> Result<ExecutionState, anyhow::Error> {
        debug!(
            "BeforeLlmCall: session={}, steps={}",
            exec_ctx.session_id, context.stats.steps
        );

        if *cancel_rx.borrow() {
            return Ok(ExecutionState::Cancelled);
        }

        let provider = exec_ctx
            .session_handle
            .provider()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to build provider: {}", e))?;

        let tools = self.collect_tools(provider, Some(exec_ctx.runtime.as_ref()));

        // Serialize tools to compute hash
        let tools_json =
            serde_json::to_vec(&tools).context("Failed to serialize tools for hash computation")?;
        let new_hash = crate::hash::RapidHash::new(&tools_json);

        // Check if changed (or first time)
        let mut current = exec_ctx.runtime.current_tools_hash.lock().unwrap();
        let changed = current.is_none_or(|h| h != new_hash);
        if changed {
            *current = Some(new_hash);
        }
        let should_emit = changed;

        if should_emit {
            self.emit_event(
                &exec_ctx.session_id,
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
        skip(self, context, tools, cancel_rx, exec_ctx),
        fields(
            session_id = %exec_ctx.session_id,
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
        exec_ctx: &ExecutionContext,
    ) -> Result<ExecutionState, anyhow::Error> {
        let session_id = &exec_ctx.session_id;
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

        let session_handle = &exec_ctx.session_handle;

        // Apply cache breakpoints to last 2 messages for providers that support caching
        let messages_with_cache = apply_cache_breakpoints(&context.messages);

        let response = if tools.is_empty() {
            // Path A: No tools - RETRY ENABLED
            // Race the LLM request with cancellation for faster cancellation
            let cancel_rx_clone = cancel_rx.clone();
            self.call_llm_with_retry(session_id, cancel_rx, || {
                let messages_with_cache = &messages_with_cache;
                let mut cancel_rx_clone = cancel_rx_clone.clone();
                async move {
                    tokio::select! {
                        result = session_handle.submit_request(messages_with_cache) => {
                            result.map_err(|e| anyhow::anyhow!("LLM request failed: {}", e))
                        }
                        _ = cancel_rx_clone.changed() => {
                            Err(anyhow!("Cancelled"))
                        }
                    }
                }
            })
            .await?
        } else {
            let provider = session_handle
                .provider()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to build provider: {}", e))?;

            // NOTE(codex): Codex backend is streaming-only.
            // Backend sends `{"detail":"Stream must be set to true"}`
            // Keep this special-case for now narrow to avoid changing behavior for other providers.
            // TODO: Remove this hack once codex streaming is replaced
            if context.provider.as_ref() == "codex" {
                // Path B: Codex streaming hack - NO RETRY (temporary)
                let mut stream = provider
                    .chat_stream_with_tools(&messages_with_cache, Some(tools))
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
                        StreamChunk::Thinking(_) => {
                            // Thinking/reasoning content is not included in the main text.
                            // The agent does not currently persist or display thinking content.
                        }
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
                // Path C: With tools, non-codex - RETRY ENABLED
                // Race the LLM request with cancellation for faster cancellation
                let cancel_rx_clone = cancel_rx.clone();
                self.call_llm_with_retry(session_id, cancel_rx, || {
                    let provider = &provider;
                    let messages_with_cache = &messages_with_cache;
                    let tools = tools.as_ref();
                    let mut cancel_rx_clone = cancel_rx_clone.clone();
                    async move {
                        tokio::select! {
                            result = provider.chat_with_tools(messages_with_cache, Some(tools)) => {
                                result.map_err(|e| anyhow::anyhow!("LLM request with tools failed: {}", e))
                            }
                            _ = cancel_rx_clone.changed() => {
                                Err(anyhow!("Cancelled"))
                            }
                        }
                    }
                }).await?
            }
        };

        let usage = response.usage();
        let response_content = response.text().unwrap_or_default();
        let tool_calls = response.tool_calls().unwrap_or_default();
        let finish_reason = response.finish_reason();

        // Calculate cost for this request using ModelPricing methods
        let (request_cost, cumulative_cost) = if let Some(usage_info) = response.usage() {
            let pricing = session_handle.get_pricing();
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

        let context_tokens = calculate_context_tokens(response.usage().as_ref());

        self.emit_event(
            session_id,
            AgentEventKind::LlmRequestEnd {
                usage: response.usage(),
                tool_calls: tool_calls.len(),
                finish_reason,
                cost_usd: request_cost,
                cumulative_cost_usd: cumulative_cost,
                context_tokens,
                metrics: ExecutionMetrics {
                    steps: context.stats.steps + 1,
                    turns: context.stats.turns,
                },
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
        skip(self, response, context, exec_ctx, cancel_rx),
        fields(
            session_id = %exec_ctx.session_id,
            has_tool_calls = %response.has_tool_calls()
        )
    )]
    pub(crate) async fn transition_after_llm(
        &self,
        response: &Arc<LlmResponse>,
        context: &Arc<ConversationContext>,
        exec_ctx: &mut ExecutionContext,
        cancel_rx: &watch::Receiver<bool>,
    ) -> Result<ExecutionState, anyhow::Error> {
        debug!(
            "AfterLlm: session={}, has_tool_calls={}",
            exec_ctx.session_id,
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

        let progress_entry = exec_ctx
            .state
            .record_progress(
                crate::session::domain::ProgressKind::Note,
                progress_description,
                None,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Failed to record progress: {}", e))?;

        self.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::ProgressRecorded { progress_entry },
        );

        let mut parts = Vec::new();
        if !response.content.is_empty() {
            self.send_session_update(
                &exec_ctx.session_id,
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
            session_id: exec_ctx.session_id.clone(),
            role: ChatRole::Assistant,
            parts,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        };

        exec_ctx
            .add_message(assistant_msg.clone())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store assistant message: {}", e))?;

        self.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::AssistantMessageStored {
                content: response.content.clone(),
                message_id: Some(assistant_msg.id.clone()),
            },
        );

        let mut messages = (*context.messages).to_vec();
        messages.push(assistant_msg.to_chat_message());

        // Update stats with usage and cost information
        let mut updated_stats = (*context.stats).clone();
        if let Some(token_usage) = &response.usage {
            updated_stats.total_input_tokens += token_usage.input_tokens as u64;
            updated_stats.total_output_tokens += token_usage.output_tokens as u64;
            updated_stats.reasoning_tokens += token_usage.reasoning_tokens as u64;
            updated_stats.cache_read_tokens += token_usage.cache_read as u64;
            updated_stats.cache_write_tokens += token_usage.cache_write as u64;
            updated_stats.context_tokens = calculate_context_tokens(Some(token_usage)) as usize;
            updated_stats.steps += 1;

            // Update cost information if pricing is available
            if let Some(pricing) = exec_ctx.session_handle.get_pricing() {
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
                // Model wants to call tools - always use ProcessingToolCalls, even for 1 tool
                if !response.tool_calls.is_empty() {
                    Ok(ExecutionState::ProcessingToolCalls {
                        remaining_calls: Arc::from(response.tool_calls.clone().into_boxed_slice()),
                        results: Arc::from(Vec::new().into_boxed_slice()),
                        context: new_context,
                    })
                } else {
                    Ok(ExecutionState::Complete)
                }
            }

            Some(FinishReason::Stop) => {
                // Model is done - mark active task as complete if any
                if exec_ctx.state.active_task.is_some() {
                    if let Err(e) = exec_ctx.state.update_task_status(TaskStatus::Done).await {
                        debug!("Failed to auto-complete task on stop: {}", e);
                    } else if let Some(task) = exec_ctx.state.active_task.clone() {
                        self.emit_event(
                            &exec_ctx.session_id,
                            AgentEventKind::TaskStatusChanged { task },
                        );
                    }
                }
                Ok(ExecutionState::Complete)
            }

            Some(FinishReason::Length) => {
                // Model hit token limit
                Ok(ExecutionState::Stopped {
                    message: "Model hit token limit".into(),
                    stop_type: StopType::ModelTokenLimit,
                    context: Some(new_context),
                })
            }

            Some(FinishReason::ContentFilter) => {
                // Response blocked by content filter
                Ok(ExecutionState::Stopped {
                    message: "Response blocked by content filter".into(),
                    stop_type: StopType::ContentFilter,
                    context: Some(new_context),
                })
            }

            Some(FinishReason::Error)
            | Some(FinishReason::Other)
            | Some(FinishReason::Unknown)
            | None => {
                // Fallback for backwards compatibility - check tool_calls
                if response.tool_calls.is_empty() {
                    Ok(ExecutionState::Complete)
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
        name = "agent.state.processing_tools",
        skip(self, params),
        fields(
            session_id = %params.exec_ctx.session_id,
            remaining = %params.remaining_calls.len(),
            completed = %params.results.len()
        )
    )]
    pub(crate) async fn transition_processing_tool_calls(
        &self,
        mut params: ProcessingToolCallsParams<'_>,
    ) -> Result<ExecutionState, anyhow::Error> {
        let ProcessingToolCallsParams {
            remaining_calls,
            results,
            context,
            exec_ctx,
            cancel_rx,
        } = &mut params;

        debug!(
            "ProcessingToolCalls: session={}, remaining={}, completed={}",
            exec_ctx.session_id,
            remaining_calls.len(),
            results.len()
        );

        if *cancel_rx.borrow() {
            return Ok(ExecutionState::Cancelled);
        }

        if remaining_calls.is_empty() {
            // All tool calls processed, store all results and continue
            return self
                .store_all_tool_results(results, context, exec_ctx)
                .await;
        }

        // Execute ALL remaining tool calls in parallel
        debug!(
            "Executing {} tool calls in parallel for session {}",
            remaining_calls.len(),
            exec_ctx.session_id
        );

        let futures: Vec<_> = remaining_calls
            .iter()
            .map(|call| self.execute_tool_call(call, exec_ctx))
            .collect();

        // Race tool execution with cancellation for faster cancellation
        let mut cancel_rx_clone = cancel_rx.clone();
        let tool_results = tokio::select! {
            results = join_all(futures) => results,
            _ = cancel_rx_clone.changed() => {
                return Ok(ExecutionState::Cancelled);
            }
        };

        // Collect results, propagating errors
        let mut all_results = (**results).to_vec();
        for result in tool_results {
            all_results.push(result?);
        }

        debug!(
            "Completed {} tool calls for session {}",
            all_results.len() - results.len(),
            exec_ctx.session_id
        );

        // All tools processed — return with empty remaining_calls
        // Next iteration will hit the is_empty() branch and call store_all_tool_results
        Ok(ExecutionState::ProcessingToolCalls {
            remaining_calls: Arc::from(Vec::<MiddlewareToolCall>::new().into_boxed_slice()),
            results: Arc::from(all_results.into_boxed_slice()),
            context: context.clone(),
        })
    }

    #[instrument(
        name = "agent.state.waiting",
        skip(self, wait, context, exec_ctx, cancel_rx, event_rx),
        fields(
            session_id = %exec_ctx.session_id,
            reason = ?wait.reason
        )
    )]
    pub(crate) async fn transition_waiting_for_event(
        &self,
        wait: &WaitCondition,
        context: &Arc<ConversationContext>,
        exec_ctx: &ExecutionContext,
        cancel_rx: &mut watch::Receiver<bool>,
        event_rx: &mut broadcast::Receiver<AgentEvent>,
    ) -> Result<ExecutionState, anyhow::Error> {
        debug!(
            "WaitingForEvent: session={}, reason={:?}",
            exec_ctx.session_id, wait.reason
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
                                message: "Event stream closed while waiting".into(),
                                stop_type: StopType::Other,
                                context: Some(context.clone()),
                            });
                        }
                    };

                    if event.session_id != exec_ctx.session_id {
                        continue;
                    }

                    if let Some(message) = match_wait_event(wait, &event) {
                        let new_context = self
                            .inject_wait_message(context, exec_ctx, message)
                            .await?;
                        return Ok(ExecutionState::BeforeLlmCall {
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
        exec_ctx: &ExecutionContext,
        content: String,
    ) -> Result<Arc<ConversationContext>, anyhow::Error> {
        let agent_msg = AgentMessage {
            id: Uuid::new_v4().to_string(),
            session_id: exec_ctx.session_id.clone(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
                content: content.clone(),
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        };

        exec_ctx
            .add_message(agent_msg)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store wait message: {}", e))?;

        self.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::UserMessageStored {
                content: content.clone(),
            },
        );

        Ok(Arc::new(context.inject_message(content)))
    }

    /// Helper to execute LLM call with retry logic for rate limiting.
    /// Excludes the codex streaming hack which will be removed later.
    async fn call_llm_with_retry<F, Fut>(
        &self,
        session_id: &str,
        cancel_rx: &watch::Receiver<bool>,
        mut call_fn: F,
    ) -> Result<Box<dyn querymt::chat::ChatResponse>, anyhow::Error>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<
                Output = Result<Box<dyn querymt::chat::ChatResponse>, anyhow::Error>,
            >,
    {
        let max_retries = self.rate_limit_config.max_retries;
        let mut attempt = 0;

        loop {
            attempt += 1;

            // Check for cancellation before each attempt
            if *cancel_rx.borrow() {
                return Err(anyhow!("Cancelled"));
            }

            // Attempt the LLM call
            match call_fn().await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    // Check if this is a rate limit error
                    if let Some((message, retry_after)) = extract_rate_limit_info(&e) {
                        // This is a rate limit error
                        if attempt >= max_retries {
                            // Exceeded max retries - fail permanently
                            return Err(anyhow!(
                                "Rate limit exceeded after {} attempts: {}",
                                max_retries,
                                message
                            ));
                        }

                        // Calculate wait time (exponential backoff or retry-after header)
                        let wait_secs = self.calculate_rate_limit_wait(retry_after, attempt);
                        let started_at = time::OffsetDateTime::now_utc().unix_timestamp();

                        info!(
                            "Session {} rate limited, attempt {}/{}, waiting {}s",
                            session_id, attempt, max_retries, wait_secs
                        );

                        // Emit RateLimited event
                        self.emit_event(
                            session_id,
                            AgentEventKind::RateLimited {
                                message: message.clone(),
                                wait_secs,
                                started_at,
                                attempt,
                                max_attempts: max_retries,
                            },
                        );

                        // Wait with cancellation support
                        let cancelled = self.wait_with_cancellation(wait_secs, cancel_rx).await;
                        if cancelled {
                            debug!(
                                "Rate limit wait cancelled for session {} during attempt {}",
                                session_id, attempt
                            );
                            return Err(anyhow!("Cancelled during rate limit wait"));
                        }

                        info!(
                            "Session {} resuming after rate limit wait, attempt {}",
                            session_id,
                            attempt + 1
                        );

                        // Emit RateLimitResume event
                        self.emit_event(
                            session_id,
                            AgentEventKind::RateLimitResume {
                                attempt: attempt + 1,
                            },
                        );

                        // Continue loop to retry
                        continue;
                    } else {
                        // Not a rate limit error - propagate immediately
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Calculate wait time based on retry-after header or exponential backoff.
    fn calculate_rate_limit_wait(&self, retry_after: Option<u64>, attempt: usize) -> u64 {
        match retry_after {
            Some(secs) => secs,
            None => {
                // Exponential backoff: default_wait_secs * backoff_multiplier^(attempt-1)
                let base = self.rate_limit_config.default_wait_secs as f64;
                let multiplier = self.rate_limit_config.backoff_multiplier;
                (base * multiplier.powi((attempt - 1) as i32)) as u64
            }
        }
    }

    /// Wait for specified seconds with cancellation support.
    /// Returns true if cancelled, false if wait completed.
    async fn wait_with_cancellation(
        &self,
        wait_secs: u64,
        cancel_rx: &watch::Receiver<bool>,
    ) -> bool {
        let mut cancel_rx = cancel_rx.clone();

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(wait_secs)) => {
                false // Wait completed
            }
            _ = cancel_rx.changed() => {
                *cancel_rx.borrow() // Return true if actually cancelled
            }
        }
    }
}

/// Extract rate limit information from an error if it's rate-related.
/// Returns (message, retry_after_secs) if the error indicates rate limiting.
fn extract_rate_limit_info(error: &anyhow::Error) -> Option<(String, Option<u64>)> {
    // Check error chain for LLMError::RateLimited
    let error_string = error.to_string();

    // Check for rate limit indicators in the error message
    if error_string.to_lowercase().contains("rate limit")
        || error_string.contains("429")
        || error_string.to_lowercase().contains("too many requests")
    {
        // Try to extract retry-after value from common patterns
        // Pattern 1: "retry after X seconds"
        // Pattern 2: "retry_after: X"
        // For now, return None and let the backoff logic handle it
        return Some((error_string, None));
    }

    None
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

//! Core execution logic for the agent
//!
//! This module contains the main entry points for agent execution:
//! - `run_prompt`: Processes a prompt request and executes the agent loop
//! - `execute_cycle_state_machine`: The core state machine that drives execution
//!
//! State transitions are implemented in the `transitions` module.
//! Tool execution logic is in the `tool_execution` module.

use crate::agent::core::QueryMTAgent;
use crate::agent::execution_context::ExecutionContext;
use crate::agent::transitions::ProcessingToolCallsParams;
use crate::agent::utils::{format_prompt_blocks, format_prompt_user_text_only};
use crate::events::{AgentEventKind, ExecutionMetrics, StopType};
use crate::middleware::{AgentStats, ConversationContext, ExecutionState};
use crate::model::{AgentMessage, MessagePart};
use crate::session::compaction::SessionCompaction;
use crate::session::provider::SessionHandle;
use crate::session::pruning::{
    PruneConfig, SimpleTokenEstimator, compute_prune_candidates, extract_call_ids,
};
use crate::session::runtime::RuntimeContext;
use agent_client_protocol::{
    ContentChunk, Error, PromptRequest, PromptResponse, SessionUpdate, StopReason,
};
use log::{debug, info, trace, warn};
use querymt::chat::{ChatRole, MessageType};
use std::sync::Arc;
use tokio::sync::watch;
use tracing::instrument;
use uuid::Uuid;

/// Outcome of a single execution cycle
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleOutcome {
    Completed,
    Cancelled,
    Stopped(StopReason),
}

impl QueryMTAgent {
    /// Processes a prompt request and executes the agent loop.
    #[instrument(
        name = "agent.run_prompt",
        skip(self, req),
        fields(
            session_id = %req.session_id,
            prompt_blocks = req.prompt.len(),
            otel.kind = "server"
        )
    )]
    pub async fn run_prompt(&self, req: PromptRequest) -> Result<PromptResponse, Error> {
        let session_id = req.session_id.to_string();
        info!(
            "Prompt request for session {} with {} block(s)",
            session_id,
            req.prompt.len()
        );

        let runtime = {
            let runtime_map = self.session_runtime.lock().await;
            runtime_map.get(&session_id).cloned()
        };

        let Some(runtime) = runtime else {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "unknown session",
                "sessionId": session_id,
            })));
        };

        // 1. Setup Cancellation
        let (tx, rx) = watch::channel(false);
        {
            let mut active = self.active_sessions.lock().await;
            active.insert(session_id.clone(), tx);
        }

        // 2. Get Session Context
        let context = self
            .provider
            .with_session(&session_id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        // 3. Create and load RuntimeContext
        let mut runtime_context =
            RuntimeContext::new(self.provider.history_store(), session_id.clone())
                .await
                .map_err(|e| Error::new(-32000, e.to_string()))?;
        runtime_context
            .load_working_context()
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        // 3a. Clean up revert state if a new prompt is sent while in reverted state
        if let Err(e) = self.cleanup_revert_on_prompt(&session_id).await {
            warn!("Failed to clean up revert state: {}", e);
        }

        // 4. Store User Messages
        // Full content for LLM and events (includes attachments)
        let full_content = format_prompt_blocks(&req.prompt, self.max_prompt_bytes);

        // User text only for intent snapshot (clean, no attachments)
        let user_text = format_prompt_user_text_only(&req.prompt);

        for block in &req.prompt {
            self.send_session_update(
                &session_id,
                SessionUpdate::UserMessageChunk(ContentChunk::new(block.clone())),
            );
        }

        // Generate message ID first so we can include it in the event
        let message_id = Uuid::new_v4().to_string();

        self.emit_event(
            &session_id,
            AgentEventKind::PromptReceived {
                content: full_content.clone(),
                message_id: Some(message_id.clone()),
            },
        );

        // Update intent snapshot with clean user text (no attachment content)
        runtime_context
            .update_intent_snapshot(user_text, None, None)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        let agent_msg = AgentMessage {
            id: message_id,
            session_id: session_id.clone(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
                content: full_content.clone(),
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        };

        if let Err(e) = context.add_message(agent_msg).await {
            let mut active = self.active_sessions.lock().await;
            active.remove(&session_id);
            self.emit_event(
                &session_id,
                AgentEventKind::Error {
                    message: e.to_string(),
                },
            );
            return Err(Error::new(-32000, e.to_string()));
        }
        self.emit_event(
            &session_id,
            AgentEventKind::UserMessageStored {
                content: full_content.clone(),
            },
        );

        // 5. Execute Agent Loop using State Machine
        info!("Initializing execution cycle for session {}", session_id);
        let mut exec_ctx = ExecutionContext::new(session_id.clone(), runtime, runtime_context);
        let result = self
            .execute_cycle_state_machine(&context, &mut exec_ctx, rx)
            .await;

        // 6. Cleanup
        {
            let mut active = self.active_sessions.lock().await;
            active.remove(&session_id);
        }

        match result {
            Ok(CycleOutcome::Completed) => Ok(PromptResponse::new(StopReason::EndTurn)),
            Ok(CycleOutcome::Cancelled) => Ok(PromptResponse::new(StopReason::Cancelled)),
            Ok(CycleOutcome::Stopped(stop_reason)) => Ok(PromptResponse::new(stop_reason)),
            Err(e) => {
                // Emit error event so UI can display the error and reset thinking state
                self.emit_event(
                    &session_id,
                    AgentEventKind::Error {
                        message: e.to_string(),
                    },
                );
                Err(Error::new(-32000, e.to_string()))
            }
        }
    }

    /// Executes a single agent cycle using state machine pattern
    #[instrument(
        name = "agent.execute_cycle",
        skip(self, context, exec_ctx, cancel_rx),
        fields(
            session_id = %context.session().public_id,
            provider = tracing::field::Empty,
            model = tracing::field::Empty
        )
    )]
    pub(crate) async fn execute_cycle_state_machine(
        &self,
        context: &SessionHandle,
        exec_ctx: &mut ExecutionContext,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Result<CycleOutcome, anyhow::Error> {
        debug!(
            "Starting state machine execution for session: {}",
            context.session().public_id
        );

        let driver = self.create_driver();

        let messages: Arc<[querymt::chat::ChatMessage]> =
            Arc::from(context.history().await.into_boxed_slice());
        let turns = messages
            .iter()
            .filter(|msg| matches!(msg.role, ChatRole::User))
            .filter(|msg| matches!(msg.message_type, MessageType::Text))
            .count();
        let stats = AgentStats {
            turns,
            ..Default::default()
        };
        let stats = Arc::new(stats);

        // Fetch current provider/model for this session
        let llm_config = self
            .provider
            .history_store()
            .get_session_llm_config(&context.session().public_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get LLM config: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("No LLM config for session"))?;

        let initial_context = Arc::new(ConversationContext::new(
            context.session().public_id.as_str().into(),
            messages,
            stats,
            llm_config.provider.as_str().into(),
            llm_config.model.as_str().into(),
        ));

        // Record provider and model in the span
        tracing::Span::current().record("provider", llm_config.provider.as_str());
        tracing::Span::current().record("model", llm_config.model.as_str());

        let mut state = ExecutionState::BeforeLlmCall {
            context: initial_context,
        };
        let mut event_rx = self.event_bus.subscribe();

        state = driver
            .run_turn_start(state)
            .await
            .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;

        loop {
            if *cancel_rx.borrow() {
                debug!(
                    "State machine cancelled for session {}",
                    context.session().public_id
                );
                return Ok(CycleOutcome::Cancelled);
            }

            let state_name = state.name();
            trace!(
                "State machine iteration: {} for session {}",
                state_name,
                context.session().public_id
            );

            state = match state {
                ExecutionState::BeforeLlmCall { .. } => {
                    let state = driver
                        .run_step_start(state)
                        .await
                        .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;

                    // Take pre-step snapshot for undo/redo support
                    // Store step_id and snapshot_id temporarily for persistence
                    let mut snapshot_data: Option<(String, String)> = None;

                    if let Some(ref backend) = self.snapshot_backend
                        && let Some(ref worktree) = self.snapshot_root
                    {
                        let step_id = Uuid::new_v4().to_string();
                        *exec_ctx.runtime.current_step_id.lock().unwrap() = Some(step_id.clone());
                        match backend.track(worktree).await {
                            Ok(snapshot_id) => {
                                *exec_ctx.runtime.pre_step_snapshot.lock().unwrap() =
                                    Some(snapshot_id.clone());
                                debug!(
                                    "Pre-step snapshot created: {} (step {})",
                                    snapshot_id, step_id
                                );
                                // Store for persistence below
                                snapshot_data = Some((step_id, snapshot_id));
                            }
                            Err(e) => {
                                warn!("Pre-step snapshot failed: {}", e);
                            }
                        }
                    }

                    match state {
                        ExecutionState::BeforeLlmCall {
                            context: ref conv_context,
                        } => {
                            // Persist StepSnapshotStart message part if we took a snapshot
                            // Note: `context` here refers to the outer SessionContext parameter
                            if let Some((step_id, snapshot_id)) = snapshot_data {
                                let start_part = MessagePart::StepSnapshotStart {
                                    step_id: step_id.clone(),
                                    snapshot_id: snapshot_id.clone(),
                                };

                                let snapshot_msg = AgentMessage {
                                    id: Uuid::new_v4().to_string(),
                                    session_id: context.session().public_id.clone(),
                                    role: ChatRole::Assistant,
                                    parts: vec![start_part],
                                    created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                                    parent_message_id: None,
                                };

                                if let Err(e) = context.add_message(snapshot_msg).await {
                                    warn!("Failed to store snapshot start: {}", e);
                                } else {
                                    debug!("Pre-step snapshot start stored (step {})", step_id);
                                }
                            }

                            self.transition_before_llm_call(conv_context, exec_ctx, &cancel_rx)
                                .await?
                        }
                        other => other,
                    }
                }

                ExecutionState::CallLlm {
                    ref context,
                    ref tools,
                } => {
                    self.transition_call_llm(context, tools, &cancel_rx, &context.session_id)
                        .await?
                }

                ExecutionState::AfterLlm { .. } => {
                    let state = driver
                        .run_after_llm(state)
                        .await
                        .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;
                    match state {
                        ExecutionState::AfterLlm {
                            ref response,
                            ref context,
                        } => {
                            self.transition_after_llm(response, context, exec_ctx, &cancel_rx)
                                .await?
                        }
                        other => other,
                    }
                }

                ExecutionState::ProcessingToolCalls { .. } => {
                    let state = driver
                        .run_processing_tool_calls(state)
                        .await
                        .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;
                    match state {
                        ExecutionState::ProcessingToolCalls {
                            ref remaining_calls,
                            ref results,
                            ref context,
                        } => {
                            self.transition_processing_tool_calls(ProcessingToolCallsParams {
                                remaining_calls,
                                results,
                                context,
                                exec_ctx,
                                cancel_rx: &cancel_rx,
                            })
                            .await?
                        }
                        other => other,
                    }
                }

                ExecutionState::WaitingForEvent {
                    ref context,
                    ref wait,
                } => {
                    self.transition_waiting_for_event(
                        wait,
                        context,
                        exec_ctx,
                        &mut cancel_rx,
                        &mut event_rx,
                    )
                    .await?
                }

                ExecutionState::Complete => {
                    // Run turn-end middleware BEFORE exiting (e.g., dedup review)
                    let turn_end_state = driver
                        .run_turn_end(ExecutionState::Complete)
                        .await
                        .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;
                    
                    match turn_end_state {
                        ExecutionState::BeforeLlmCall { .. } => {
                            // Middleware wants another step (e.g., dedup review)
                            // Continue the loop with the new state
                            turn_end_state
                        }
                        ExecutionState::Complete => {
                            // Truly done — proceed with existing snapshot/cleanup
                            debug!("State machine reached Complete state");

                            // Take post-step snapshot and record changes for undo/redo
                            if let Some(ref backend) = self.snapshot_backend
                                && let Some(ref worktree) = self.snapshot_root
                            {
                                let pre_id = exec_ctx.runtime.pre_step_snapshot.lock().unwrap().take();
                                let step_id = exec_ctx.runtime.current_step_id.lock().unwrap().take();

                                if let (Some(pre_id), Some(step_id)) = (pre_id, step_id) {
                                    match backend.track(worktree).await {
                                        Ok(post_id) => {
                                            if pre_id != post_id {
                                                let changed = backend
                                                    .diff(worktree, &pre_id, &post_id)
                                                    .await
                                                    .unwrap_or_default();
                                                if !changed.is_empty() {
                                                    let patch_part = MessagePart::StepSnapshotPatch {
                                                        step_id: step_id.clone(),
                                                        snapshot_id: post_id.clone(),
                                                        changed_paths: changed
                                                            .iter()
                                                            .map(|p| p.to_string_lossy().to_string())
                                                            .collect(),
                                                    };

                                                    // Store patch as a system message
                                                    let snapshot_msg = AgentMessage {
                                                        id: Uuid::new_v4().to_string(),
                                                        session_id: context.session().public_id.clone(),
                                                        role: ChatRole::Assistant,
                                                        parts: vec![patch_part],
                                                        created_at: time::OffsetDateTime::now_utc()
                                                            .unix_timestamp(),
                                                        parent_message_id: None,
                                                    };
                                                    if let Err(e) = context.add_message(snapshot_msg).await
                                                    {
                                                        warn!("Failed to store snapshot patch: {}", e);
                                                    } else {
                                                        debug!(
                                                            "Post-step snapshot patch stored: {} changed files (step {})",
                                                            changed.len(),
                                                            step_id
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            warn!("Post-step snapshot failed: {}", e);
                                        }
                                    }
                                }

                                // Run GC if snapshot backend is configured
                                let _ = backend.gc(worktree, &self.snapshot_gc_config).await;
                            }

                            // Run pruning if enabled
                            if self.pruning_config.enabled
                                && let Err(e) = self.run_pruning(context).await
                            {
                                warn!("Pruning failed: {}", e);
                                // Don't fail the whole operation, just log the warning
                            }

                            return Ok(CycleOutcome::Completed);
                        }
                        ExecutionState::Stopped { ref message, stop_type } => {
                            // Middleware stopped execution
                            info!("Turn-end middleware stopped: {} ({:?})", message, stop_type);
                            turn_end_state  // Will be handled in the next loop iteration
                        }
                        other => other, // Cancelled, etc.
                    }
                }

                ExecutionState::Stopped {
                    ref message,
                    stop_type,
                } => {
                    info!("State machine stopped: {} ({:?})", message, stop_type);

                    // Handle context threshold specially - trigger AI compaction if enabled
                    if stop_type == StopType::ContextThreshold && self.compaction_config.auto {
                        info!("Context threshold reached, triggering AI compaction");

                        match self.run_ai_compaction(context, &state).await {
                            Ok(new_state) => {
                                // Compaction succeeded, continue with the new state
                                state = new_state;
                                continue;
                            }
                            Err(e) => {
                                warn!("AI compaction failed: {}", e);
                                // Fall through to normal stop handling
                            }
                        }
                    }

                    // Get metrics from current context if available
                    let metrics = state
                        .context()
                        .map(|ctx| ExecutionMetrics {
                            steps: ctx.stats.steps,
                            turns: ctx.stats.turns,
                        })
                        .unwrap_or_default();

                    // Emit MiddlewareStopped event so UI can display the reason
                    self.emit_event(
                        &context.session().public_id,
                        AgentEventKind::MiddlewareStopped {
                            stop_type,
                            reason: message.to_string(),
                            metrics,
                        },
                    );

                    // Convert to protocol StopReason
                    return Ok(CycleOutcome::Stopped(StopReason::from(stop_type)));
                }

                ExecutionState::Cancelled => {
                    debug!("State machine cancelled");
                    return Ok(CycleOutcome::Cancelled);
                }
            };
        }
    }

    /// Run pruning on the session history
    ///
    /// This marks old tool results as compacted based on the pruning configuration.
    async fn run_pruning(&self, context: &SessionHandle) -> Result<(), anyhow::Error> {
        let session_id = &context.session().public_id;
        let messages = context
            .get_agent_history()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get agent history: {}", e))?;

        // Build prune config from agent config
        let prune_config = PruneConfig {
            protect_tokens: self.pruning_config.protect_tokens,
            minimum_tokens: self.pruning_config.minimum_tokens,
            protected_tools: self.pruning_config.protected_tools.clone(),
        };

        let estimator = SimpleTokenEstimator;
        let analysis = compute_prune_candidates(&messages, &prune_config, &estimator);

        if analysis.should_prune && !analysis.candidates.is_empty() {
            let call_ids = extract_call_ids(&analysis.candidates);
            info!(
                "Pruning {} tool results ({} tokens) for session {}",
                call_ids.len(),
                analysis.prunable_tokens,
                session_id
            );

            let updated = self
                .provider
                .history_store()
                .mark_tool_results_compacted(session_id, &call_ids)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to mark tool results compacted: {}", e))?;

            debug!("Marked {} tool results as compacted", updated);
        } else {
            debug!(
                "Pruning skipped: should_prune={}, candidates={}, prunable_tokens={}",
                analysis.should_prune,
                analysis.candidates.len(),
                analysis.prunable_tokens
            );
        }

        Ok(())
    }

    /// Run AI compaction when context threshold is reached
    ///
    /// This generates a summary of the conversation and creates a compaction message,
    /// then returns a new state to continue execution with filtered history.
    async fn run_ai_compaction(
        &self,
        context: &SessionHandle,
        current_state: &ExecutionState,
    ) -> Result<ExecutionState, anyhow::Error> {
        let session_id = &context.session().public_id;
        let messages = context
            .get_agent_history()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get agent history: {}", e))?;

        // Emit CompactionStart event
        let token_estimate = messages
            .iter()
            .map(|m| {
                m.parts
                    .iter()
                    .map(|p| match p {
                        MessagePart::Text { content } => content.len() / 4,
                        MessagePart::ToolResult { content, .. } => content.len() / 4,
                        _ => 0,
                    })
                    .sum::<usize>()
            })
            .sum();

        self.emit_event(
            session_id,
            AgentEventKind::CompactionStart { token_estimate },
        );

        // Get the provider for compaction
        // Use compaction-specific provider/model if configured, otherwise use session's provider
        let provider = self
            .provider
            .with_session(session_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get session provider: {}", e))?;

        let llm_provider = provider
            .provider()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get LLM provider: {}", e))?;

        let llm_config = self
            .provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get LLM config: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("No LLM config for session"))?;

        let model = self
            .compaction_config
            .model
            .as_ref()
            .unwrap_or(&llm_config.model);

        // Build retry config
        let retry_config = crate::session::compaction::RetryConfig {
            max_retries: self.compaction_config.retry.max_retries,
            initial_backoff_ms: self.compaction_config.retry.initial_backoff_ms,
            backoff_multiplier: self.compaction_config.retry.backoff_multiplier,
        };

        // Generate the compaction summary
        let result = self
            .compaction
            .process(&messages, llm_provider, model, &retry_config)
            .await
            .map_err(|e| anyhow::anyhow!("Compaction failed: {}", e))?;

        info!(
            "Compaction generated summary: {} tokens -> {} tokens",
            result.original_token_count, result.summary_token_count
        );

        // Create and store the compaction message
        let compaction_msg = SessionCompaction::create_compaction_message(
            session_id,
            &result.summary,
            result.original_token_count,
        );

        context
            .add_message(compaction_msg)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store compaction message: {}", e))?;

        // Emit CompactionEnd event
        self.emit_event(
            session_id,
            AgentEventKind::CompactionEnd {
                summary: result.summary.clone(),
                summary_len: result.summary.len(),
            },
        );

        // Get the new filtered history and create a new state to continue
        let new_messages = context
            .get_agent_history()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get new history: {}", e))?;
        let filtered_messages =
            crate::session::compaction::filter_to_effective_history(new_messages);

        // Convert AgentMessages to ChatMessages for the ConversationContext
        let chat_messages: Vec<querymt::chat::ChatMessage> = filtered_messages
            .iter()
            .map(|m| m.to_chat_message())
            .collect();

        // Create new conversation context with filtered history
        let new_context = if let Some(ctx) = current_state.context() {
            Arc::new(ConversationContext::new(
                ctx.session_id.clone(),
                Arc::from(chat_messages.into_boxed_slice()),
                ctx.stats.clone(),
                ctx.provider.clone(),
                ctx.model.clone(),
            ))
        } else {
            return Err(anyhow::anyhow!("No context available for compaction"));
        };

        // Return BeforeLlmCall state to continue execution
        Ok(ExecutionState::BeforeLlmCall {
            context: new_context,
        })
    }
}

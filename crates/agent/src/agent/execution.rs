//! Core execution logic for the agent
//!
//! This module contains the main entry points for agent execution:
//! - `run_prompt`: Processes a prompt request and executes the agent loop
//! - `execute_cycle_state_machine`: The core state machine that drives execution
//!
//! State transitions are implemented in the `transitions` module.
//! Tool execution logic is in the `tool_execution` module.

use crate::agent::core::{QueryMTAgent, SessionRuntime};
use crate::agent::transitions::ProcessingToolCallsParams;
use crate::agent::utils::format_prompt_blocks;
use crate::events::AgentEventKind;
use crate::middleware::{AgentStats, ConversationContext, ExecutionState, MiddlewareDriver};
use crate::model::{AgentMessage, MessagePart};
use crate::session::provider::SessionContext;
use crate::session::runtime::RuntimeContext;
use agent_client_protocol::{
    ContentChunk, Error, PromptRequest, PromptResponse, SessionUpdate, StopReason,
};
use log::{debug, info, trace};
use querymt::chat::ChatRole;
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

        // 4. Store User Messages
        let content = format_prompt_blocks(&req.prompt, self.max_prompt_bytes);
        for block in &req.prompt {
            self.send_session_update(
                &session_id,
                SessionUpdate::UserMessageChunk(ContentChunk::new(block.clone())),
            );
        }
        self.emit_event(
            &session_id,
            AgentEventKind::PromptReceived {
                content: content.clone(),
            },
        );

        // Update intent snapshot on prompt received
        runtime_context
            .update_intent_snapshot(content.clone(), None, None)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        let agent_msg = AgentMessage {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.clone(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
                content: content.clone(),
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
        self.emit_event(&session_id, AgentEventKind::UserMessageStored { content });

        // 5. Execute Agent Loop using State Machine
        info!("Initializing execution cycle for session {}", session_id);
        let result = self
            .execute_cycle_state_machine(&context, Some(runtime.as_ref()), &mut runtime_context, rx)
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
            Err(e) => Err(Error::new(-32000, e.to_string())),
        }
    }

    /// Executes a single agent cycle using state machine pattern
    #[instrument(
        name = "agent.execute_cycle",
        skip(self, context, runtime, runtime_context, cancel_rx),
        fields(
            session_id = %context.session().public_id,
            provider = tracing::field::Empty,
            model = tracing::field::Empty
        )
    )]
    pub(crate) async fn execute_cycle_state_machine(
        &self,
        context: &SessionContext,
        runtime: Option<&SessionRuntime>,
        runtime_context: &mut RuntimeContext,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Result<CycleOutcome, anyhow::Error> {
        debug!(
            "Starting state machine execution for session: {}",
            context.session().public_id
        );

        let driver = self.create_driver();

        let messages = Arc::from(context.history().await.into_boxed_slice());
        let stats = Arc::new(AgentStats::default());

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

        let mut state = ExecutionState::BeforeTurn {
            context: initial_context,
        };
        let mut event_rx = self.event_bus.subscribe();

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

            state = driver
                .next_state(state)
                .await
                .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;

            state = match state {
                ExecutionState::BeforeTurn { ref context } => {
                    self.transition_before_turn(context, runtime, &cancel_rx, &context.session_id)
                        .await?
                }

                ExecutionState::CallLlm {
                    ref context,
                    ref tools,
                } => {
                    self.transition_call_llm(context, tools, &cancel_rx, &context.session_id)
                        .await?
                }

                ExecutionState::AfterLlm {
                    ref response,
                    ref context,
                } => {
                    self.transition_after_llm(
                        response,
                        context,
                        runtime_context,
                        &cancel_rx,
                        &context.session_id,
                    )
                    .await?
                }

                ExecutionState::BeforeToolCall {
                    ref call,
                    ref context,
                } => {
                    self.transition_before_tool(
                        call,
                        context,
                        runtime,
                        runtime_context,
                        &cancel_rx,
                        &context.session_id,
                    )
                    .await?
                }

                ExecutionState::AfterTool {
                    ref result,
                    ref context,
                } => {
                    self.transition_after_tool(
                        result,
                        context,
                        runtime_context,
                        &context.session_id,
                    )
                    .await?
                }

                ExecutionState::ProcessingToolCalls {
                    ref remaining_calls,
                    ref results,
                    ref context,
                } => {
                    self.transition_processing_tool_calls(ProcessingToolCallsParams {
                        remaining_calls,
                        results,
                        context,
                        runtime,
                        runtime_context,
                        cancel_rx: &cancel_rx,
                        session_id: &context.session_id,
                    })
                    .await?
                }

                ExecutionState::WaitingForEvent {
                    ref context,
                    ref wait,
                } => {
                    self.transition_waiting_for_event(
                        wait,
                        context,
                        runtime_context,
                        &mut cancel_rx,
                        &mut event_rx,
                        &context.session_id,
                    )
                    .await?
                }

                ExecutionState::Complete => {
                    debug!("State machine reached Complete state");
                    return Ok(CycleOutcome::Completed);
                }

                ExecutionState::Stopped {
                    reason,
                    ref message,
                } => {
                    info!("State machine stopped: {}", message);
                    return Ok(CycleOutcome::Stopped(reason));
                }

                ExecutionState::Cancelled => {
                    debug!("State machine cancelled");
                    return Ok(CycleOutcome::Cancelled);
                }
            };
        }
    }
}

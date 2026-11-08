use super::runtime::{INSTRUCTIONS, Runtime, Turn};
use anyhow::{Context, Result};
use async_trait::async_trait;
use querymt::chat::ChatMessage;
use std::sync::Arc;

use crate::agent::core::SessionRuntime;
use crate::events::StopType;
use crate::middleware::{ConversationContext, ExecutionState, MiddlewareDriver};
use crate::model::{AgentMessage, MessagePart, OutputTarget};

pub(super) struct MemoryMiddleware(pub Arc<Runtime>);

impl MemoryMiddleware {
    async fn rewrite(
        &self,
        context: &ConversationContext,
        new_turn: bool,
    ) -> Result<Arc<ConversationContext>> {
        let id = context.session_id.as_ref();
        let session = self.0.session(id).await?;
        let handle = self.0.provider.with_session(id).await?;
        let history = handle.get_agent_history().await?;
        if new_turn {
            let index = history
                .iter()
                .rposition(|m| {
                    m.parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::Prompt { .. }))
                })
                .context("OptChat needs a durable user prompt")?;
            let prefix = self.0.prepare(id, history[..index].to_vec()).await?;
            *session
                .turn
                .lock()
                .map_err(|_| anyhow::anyhow!("OptChat turn mutex poisoned"))? = Some(Turn {
                prompt_id: history[index].id.clone(),
                prefix,
            });
        }
        let turn = session
            .turn
            .lock()
            .map_err(|_| anyhow::anyhow!("OptChat turn mutex poisoned"))?
            .clone()
            .context("OptChat turn has not been initialized")?;
        let index = history
            .iter()
            .position(|m| m.id == turn.prompt_id)
            .context("OptChat current prompt disappeared")?;
        let config = handle.llm_config();
        let target = config.map(|cfg| {
            OutputTarget::new(cfg.provider.clone(), cfg.model.clone())
                .with_protocol(cfg.protocol.clone())
                .with_endpoint(cfg.endpoint.clone())
        });
        let mut messages = turn.prefix;
        messages.extend(project(
            &history[index..],
            target.as_ref(),
            handle.execution_config().and_then(|c| c.max_prompt_bytes),
        )?);
        let mut next = context.clone();
        next.messages = Arc::from(messages);
        let mut stats = (*next.stats).clone();
        stats.context_tokens = estimate(&next.messages)? / 4;
        next.stats = Arc::new(stats);
        Ok(Arc::new(next))
    }
}

pub(super) fn project(
    history: &[AgentMessage],
    target: Option<&OutputTarget>,
    max_prompt_bytes: Option<usize>,
) -> Result<Vec<ChatMessage>> {
    history
        .iter()
        .filter(|m| {
            m.parts.iter().any(|p| {
                matches!(
                    p,
                    MessagePart::Prompt { .. }
                        | MessagePart::Steering { .. }
                        | MessagePart::Text { .. }
                        | MessagePart::ToolUse(_)
                        | MessagePart::ToolResult { .. }
                        | MessagePart::Output { .. }
                        | MessagePart::HookContext { .. }
                )
            })
        })
        .map(|m| Ok(m.to_chat_message_with_output_target(target, max_prompt_bytes)?))
        .collect()
}

fn estimate(messages: &[ChatMessage]) -> Result<usize> {
    messages.iter().try_fold(0usize, |bytes, message| {
        let size = serde_json::to_vec(message)
            .context("failed to measure OptChat input")?
            .len();
        bytes
            .checked_add(size)
            .context("OptChat input size overflow")
    })
}

#[async_trait]
impl MiddlewareDriver for MemoryMiddleware {
    async fn on_turn_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> crate::middleware::Result<ExecutionState> {
        match state {
            ExecutionState::BeforeLlmCall { context } => Ok(ExecutionState::BeforeLlmCall {
                context: self.rewrite(&context, true).await?,
            }),
            other => Ok(other),
        }
    }

    async fn on_step_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> crate::middleware::Result<ExecutionState> {
        let ExecutionState::BeforeLlmCall { context } = state else {
            return Ok(state);
        };
        let has_memory = context
            .messages
            .first()
            .and_then(|m| m.input())
            .is_some_and(|parts| parts.iter().any(|p| p.as_text() == Some(INSTRUCTIONS)));
        let context = if has_memory {
            context
        } else {
            self.rewrite(&context, false).await?
        };
        let estimated = estimate(&context.messages)?;
        let max_context = crate::model_info::get_model_info(&context.provider, &context.model)
            .and_then(|m| m.limits.context)
            .unwrap_or(0) as usize;
        let configured_budget = self
            .0
            .settings
            .max_turn_bytes
            .saturating_add(self.0.settings.view_max_bytes);
        let budget = if max_context > 0 {
            configured_budget.min(max_context.saturating_mul(2))
        } else {
            configured_budget
        };
        // Byte admission is conservative, not a tokenizer. The provider still
        // owns exact token limits. Never invoke conventional compaction here.
        if estimated > budget {
            return Ok(ExecutionState::Stopped { message:"OptChat current turn reached its safe context budget. History is retained; send a follow-up to continue with a fresh memory view.".into(),stop_type:StopType::Other,context:Some(context) });
        }
        Ok(ExecutionState::BeforeLlmCall { context })
    }

    async fn on_turn_end(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> crate::middleware::Result<ExecutionState> {
        let ExecutionState::Complete { context } = state else {
            return Ok(state);
        };
        // Core completion reloads normal history. Restore the frozen view before
        // stop hooks, task guards or other middleware can continue this turn.
        let next = self.rewrite(&context, false).await?;
        let history = self
            .0
            .provider
            .with_session(&context.session_id)
            .await
            .map_err(anyhow::Error::from)?
            .get_agent_history()
            .await
            .map_err(anyhow::Error::from)?;
        if self
            .0
            .background(&context.session_id, history)
            .await
            .is_err()
        {
            log::warn!("OptChat background ingestion paused; source history remains durable");
        }
        Ok(ExecutionState::Complete { context: next })
    }

    fn reset(&self) {}

    fn name(&self) -> &'static str {
        "OptChatMemory"
    }
}

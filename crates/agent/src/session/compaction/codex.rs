//! Agent-side Codex context policy. Wire serialization stays in the plugin;
//! retries, accounting, checkpoint storage and lifecycle stay in the runtime.

use super::{CompactionResult, RetryConfig, SessionCompaction};
use crate::accounting::DispatchLedger;
use crate::model::{AgentMessage, MessagePart, OutputTarget};
use anyhow::Result;
use querymt::chat::{
    ChatCall, ChatOpaqueItem, ChatOutput, ChatOutputItem, ChatOutputProvenance, ChatRole,
    FinishReason, Tool,
};
use querymt::error::LLMError;
use std::sync::Arc;

// Keep the retained prefix in the checkpoint so reloads and forks are self-contained.
const RETAINED_MESSAGES: &str = "querymt_codex_retained_messages";
const COMPACTION_NOTICE: &str = "Earlier context was compacted into encrypted Codex state.";
pub(crate) const COMPACTION_STRATEGY: &str = "codex_native";

pub(crate) struct CompactionContext<'a> {
    pub target: &'a OutputTarget,
    pub tools: &'a [Tool],
    pub threshold: Option<usize>,
}

pub(crate) fn needs_target_resolution(target: &OutputTarget) -> bool {
    target.provider == "codex" && (target.protocol.is_empty() || target.endpoint.is_empty())
}

pub(crate) fn supports_compaction(target: &OutputTarget) -> bool {
    target.provider == "codex" && target.protocol == "responses" && !target.endpoint.is_empty()
}

pub(crate) fn resolve_target(target: &mut OutputTarget, params: Option<&serde_json::Value>) {
    if target.provider != "codex" {
        return;
    }
    // SQLite configs currently leave protocol/endpoint empty. Codex has one
    // wire protocol; derive its endpoint rather than losing native reasoning.
    if target.protocol.is_empty() {
        target.protocol = "responses".into();
    }
    if target.endpoint.is_empty() {
        let base_url = params
            .and_then(|params| params.get("base_url"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("https://chatgpt.com/backend-api/codex/");
        target.endpoint = reqwest::Url::parse(base_url)
            .ok()
            .and_then(|url| url.join("responses").ok())
            .map(|url| url.to_string())
            .unwrap_or_default();
    }
}

pub(crate) fn has_compaction(output: &ChatOutput) -> bool {
    output.items.iter().any(|item| {
        matches!(item,
            ChatOutputItem::Opaque(item) if item.original_type == "compaction"
        )
    })
}

/// None leaves non-Codex output on the runtime's existing portable path.
pub(crate) fn portable_output(output: &ChatOutput) -> Option<ChatOutput> {
    if !output
        .provenance
        .as_ref()
        .is_some_and(|origin| origin.provider == "codex")
    {
        return None;
    }
    let mut portable = output.clone().into_portable();
    // Some providers ignore unsigned reasoning items. Carry visible summaries
    // as ordinary assistant text rather than losing them on a provider switch.
    portable.items = portable
        .items
        .into_iter()
        .flat_map(|item| match item {
            ChatOutputItem::Reasoning(reasoning) => {
                ChatOutput::from_projections(None, Some(reasoning.visible_text()), None, None, None)
                    .items
            }
            item => vec![item],
        })
        .collect();
    Some(portable)
}

impl SessionCompaction {
    /// Codex uses the normal streamed Responses transport with an in-band trigger.
    pub(crate) async fn process_codex(
        &self,
        messages: &[AgentMessage],
        provider: Arc<dyn querymt::chat::ChatProvider>,
        request_context: CompactionContext<'_>,
        retry_config: &RetryConfig,
        max_prompt_bytes: Option<usize>,
        ledger: &DispatchLedger,
    ) -> Result<CompactionResult> {
        let CompactionContext {
            target,
            tools,
            threshold,
        } = request_context;
        let mut chat_messages = messages
            .iter()
            .map(|message| {
                message.to_chat_message_with_output_target(Some(target), max_prompt_bytes)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        chat_messages.push(querymt::chat::ChatMessage::from_assistant_output(
            ChatOutput {
                provenance: Some(ChatOutputProvenance {
                    provider: target.provider.clone(),
                    protocol: target.protocol.clone(),
                    model: target.model.clone(),
                    endpoint: target.endpoint.clone(),
                }),
                items: vec![ChatOutputItem::Opaque(ChatOpaqueItem {
                    original_type: "compaction_trigger".into(),
                    payload: serde_json::json!({"type": "compaction_trigger"}),
                })],
                ..Default::default()
            },
        ));
        let mut output = self
            .call_with_retry(
                &ChatCall::new(&chat_messages).tools(tools),
                provider,
                &target.model,
                retry_config,
                messages.first().map(|message| message.session_id.as_str()),
                ledger,
            )
            .await?;
        // Keep reasoning (including visible summaries) and the checkpoint, not
        // an incidental assistant reply to the compaction request.
        output.items.retain(|item| match item {
            ChatOutputItem::Reasoning(_) => true,
            ChatOutputItem::Opaque(opaque) => opaque.original_type == "compaction",
            _ => false,
        });
        let checkpoint_tokens = self.estimator.estimate(&output.estimate_text());
        // Leave room for the checkpoint and request instructions/tools rather
        // than retaining a prefix that immediately crosses the same threshold.
        let retained_budget = threshold
            .map(|threshold| {
                threshold
                    .saturating_sub(checkpoint_tokens)
                    .saturating_sub((threshold / 10).max(1024))
            })
            .unwrap_or(64_000)
            .min(64_000);
        let retained = retained_user_messages(messages, retained_budget);
        output
            .extensions
            .insert(RETAINED_MESSAGES.into(), serde_json::to_value(&retained)?);
        Ok(CompactionResult {
            summary: COMPACTION_NOTICE.into(),
            original_token_count: self.estimate_messages_tokens(messages, max_prompt_bytes),
            // Output usage is not a measured replacement-window size. Use the visible
            // retained context estimate until the next normal response reports usage.
            summary_token_count: self.estimate_messages_tokens(&retained, max_prompt_bytes)
                + checkpoint_tokens,
            native_output: Some(output),
        })
    }
}

/// Recognize only native requests; the caller validates text summaries otherwise.
pub(super) fn validate_compaction(
    call: &ChatCall<'_>,
    output: &ChatOutput,
) -> Option<std::result::Result<(), LLMError>> {
    Some(validate_output(output, compaction_trigger(call)?))
}

pub(super) fn request_strategy(call: &ChatCall<'_>) -> Option<&'static str> {
    compaction_trigger(call).map(|_| COMPACTION_STRATEGY)
}

fn compaction_trigger<'a>(call: &'a ChatCall<'_>) -> Option<&'a ChatOutput> {
    call.messages
        .last()
        .and_then(|message| message.output())
        .filter(|output| {
            output.items.iter().any(|item| {
                matches!(item,
                    ChatOutputItem::Opaque(opaque) if opaque.original_type == "compaction_trigger"
                )
            })
        })
}

fn validate_output(output: &ChatOutput, trigger: &ChatOutput) -> std::result::Result<(), LLMError> {
    let mut compactions = output.items.iter().filter_map(|item| match item {
        ChatOutputItem::Opaque(opaque) if opaque.original_type == "compaction" => Some(opaque),
        _ => None,
    });
    let valid = compactions.next().is_some_and(|item| {
        item.payload.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
            && item
                .payload
                .get("encrypted_content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|content| !content.is_empty())
    }) && compactions.next().is_none();
    if !output.is_successful()
        || output.provenance != trigger.provenance
        || !valid
        || output.function_calls().next().is_some()
        || matches!(
            output.finish_reason,
            Some(
                FinishReason::Length
                    | FinishReason::ContentFilter
                    | FinishReason::Error
                    | FinishReason::ToolCalls
            )
        )
    {
        return Err(LLMError::InvalidRequest(
            "Codex compaction must complete with exactly one valid encrypted compaction item"
                .into(),
        ));
    }
    Ok(())
}

pub(super) fn retained_messages(message: &AgentMessage) -> Option<Vec<AgentMessage>> {
    if !message
        .parts
        .iter()
        .any(|part| matches!(part, MessagePart::Compaction { .. }))
    {
        return None;
    }
    message.parts.iter().find_map(|part| match part {
        MessagePart::Output { output } => {
            serde_json::from_value(output.extensions.get(RETAINED_MESSAGES)?.clone()).ok()
        }
        _ => None,
    })
}

fn user_message(message: &AgentMessage) -> Option<AgentMessage> {
    if message.role != ChatRole::User {
        return None;
    }
    let mut retained = message.clone();
    // Tool outputs belong to the encrypted state, not the retained user context.
    retained.parts.retain(|part| {
        matches!(
            part,
            MessagePart::Text { .. }
                | MessagePart::Prompt { .. }
                | MessagePart::Steering { .. }
                | MessagePart::HookContext { .. }
        )
    });
    (!retained.parts.is_empty()).then_some(retained)
}

fn retained_user_messages(messages: &[AgentMessage], mut budget: usize) -> Vec<AgentMessage> {
    let estimator = SessionCompaction::new();
    let mut retained = Vec::new();
    for message in messages.iter().rev().filter_map(user_message) {
        let tokens = estimator.estimate_messages_tokens(std::slice::from_ref(&message), None);
        if tokens > budget {
            break;
        }
        budget -= tokens;
        retained.push(message);
    }
    retained.reverse();
    retained
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_providers_keep_their_identity_and_portable_path() {
        let mut target = OutputTarget::new("xai", "grok");
        resolve_target(&mut target, None);
        assert_eq!(target.provider, "xai");
        assert!(target.protocol.is_empty());
        assert!(target.endpoint.is_empty());
        assert!(!supports_compaction(&target));

        let mut output =
            ChatOutput::from_projections(Some("visible reasoning".into()), None, None, None, None);
        output.provenance = Some(ChatOutputProvenance {
            provider: "xai".into(),
            protocol: "responses".into(),
            model: "grok".into(),
            endpoint: "https://example.com/responses".into(),
        });
        assert!(portable_output(&output).is_none());
        assert!(validate_compaction(&ChatCall::new(&[]), &output).is_none());
    }

    #[test]
    fn codex_identity_and_visible_reasoning_are_resolved_without_mutating_output() {
        let mut target = OutputTarget::new("codex", "model");
        let params = serde_json::json!({"base_url":"https://example.com/codex/"});
        resolve_target(&mut target, Some(&params));
        assert_eq!(target.protocol, "responses");
        assert_eq!(target.endpoint, "https://example.com/codex/responses");
        assert!(supports_compaction(&target));

        let mut output =
            ChatOutput::from_projections(Some("visible reasoning".into()), None, None, None, None);
        output.provenance = Some(ChatOutputProvenance {
            provider: target.provider,
            protocol: target.protocol,
            model: target.model,
            endpoint: target.endpoint,
        });
        if let ChatOutputItem::Reasoning(reasoning) = &mut output.items[0] {
            reasoning.encrypted_content = Some("encrypted-state".into());
        }
        let original = output.clone();
        let portable = portable_output(&output).unwrap();
        assert_eq!(portable.text().as_deref(), Some("visible reasoning"));
        assert!(
            !serde_json::to_string(&portable)
                .unwrap()
                .contains("encrypted-state")
        );
        assert_eq!(output, original);
    }
}

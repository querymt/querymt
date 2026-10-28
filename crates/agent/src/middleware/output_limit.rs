//! Recovery from replies cut off by the output-token limit.
//!
//! A reply that ends with a length finish reason used to end the run with
//! `Stopped { ModelTokenLimit }`. This middleware turns that into recovery:
//!
//! 1. **Escalate** (`on_after_llm`): when a request below the model's output
//!    ceiling is cut off, discard the reply and retry once at the ceiling.
//! 2. **Continue** (`on_stopped`): when the reply is still cut off, resume with a
//!    temporary instruction (no recap, split large writes), up to
//!    `max_continuations` times per turn.
//!
//! # TOML
//!
//! ```toml
//! [[middleware]]
//! type = "output_limit_recovery"
//! enabled = true                       # default
//! escalate = true                      # retry once at the model's output ceiling
//! max_continuations = 3                # continuation attempts per turn
//! lower_effort_on_thinking_only = true # cheaper reasoning after a thinking-only cut-off
//! instruction = "..."                  # `{cap}` expands to the cap that was hit
//! ```

use crate::agent::agent_config::AgentConfig;
use crate::agent::core::SessionRuntime;
use crate::event_sink::EventSink;
use crate::events::{AgentEventKind, OutputLimitRecoveryAction, StopType};
use crate::middleware::factory::MiddlewareFactory;
use crate::middleware::{
    ContextPriority, ConversationContext, ExecutionState, LlmResponse, MiddlewareDriver,
    ModelProfile, Result, apply_usage_to_stats,
};
use crate::model_info::get_model_info;
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use log::debug;
use parking_lot::Mutex;
use querymt::chat::{FinishReason, GenerationOptions, ReasoningEffort};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

/// Key under which the continuation instruction is stored as a context fragment.
pub(crate) const FRAGMENT_KEY: &str = "output_limit_recovery";

/// Default continuation instruction. `{cap}` expands to the cap that was hit.
pub const DEFAULT_INSTRUCTION: &str = "Your previous response was cut off by the output-token limit ({cap} tokens). \
Continue directly from where you stopped without repeating or summarising earlier output. \
Split large file writes or edits into several smaller tool calls, and keep reasoning brief.";

/// Configuration for [`OutputLimitRecoveryMiddleware`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct OutputLimitConfig {
    /// When `false` the factory declines to build the middleware.
    pub enabled: bool,
    /// Retry a cut-off request once at the model's output ceiling.
    pub escalate: bool,
    /// Maximum continuation attempts per turn.
    pub max_continuations: u8,
    /// Continuation instruction; `{cap}` expands to the cap that was hit.
    pub instruction: String,
    /// Lower reasoning effort for the continuation of a thinking-only cut-off.
    pub lower_effort_on_thinking_only: bool,
}

impl Default for OutputLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            escalate: true,
            max_continuations: 3,
            instruction: DEFAULT_INSTRUCTION.to_owned(),
            lower_effort_on_thinking_only: true,
        }
    }
}

/// Recovery progress within one turn of one session.
#[derive(Debug, Default)]
pub(crate) struct TurnState {
    /// The one-shot escalation to the ceiling has been spent.
    pub(crate) escalated: bool,
    /// Effective escalation cap, including any context-window clamp.
    escalated_cap: Option<u32>,
    /// Continuations issued so far.
    pub(crate) continuations: u8,
    /// The continuation instruction is currently part of the context.
    pub(crate) fragment_active: bool,
    /// The most recent reply was cut off and carried only reasoning. Such a
    /// reply is never stored, so this is the only record of it.
    pub(crate) last_cut_thinking_only: bool,
}

/// One step down from `current`, never below `Low`. Provider default (`None`)
/// is treated as `Medium` so the continuation is no more costly than that.
fn lowered_effort(current: Option<ReasoningEffort>) -> ReasoningEffort {
    match current {
        None | Some(ReasoningEffort::High | ReasoningEffort::Max) => ReasoningEffort::Medium,
        Some(ReasoningEffort::Medium | ReasoningEffort::Low) => ReasoningEffort::Low,
    }
}

/// Overrides for a continuation request.
///
/// Overrides last one request, so an escalation has to be repeated for the
/// continuation or it would silently fall back to the initial cap.
fn continuation_options(
    profile: &ModelProfile,
    escalated_cap: Option<u32>,
    lower_effort: bool,
) -> GenerationOptions {
    let supported = profile.capabilities.options;
    let mut options = GenerationOptions::NONE;
    if supported.max_tokens {
        options.max_tokens = escalated_cap;
    }
    if lower_effort && supported.reasoning_effort {
        options.reasoning_effort = Some(lowered_effort(profile.reasoning_effort));
    }
    options
}

/// The output cap a request was sent with: its applied override, else the
/// session's initial cap.
fn cap_used(profile: &ModelProfile, applied: GenerationOptions) -> Option<u32> {
    applied.max_tokens.or(profile.caps.map(|caps| caps.initial))
}

/// Cap to escalate to, or `None` when escalation cannot help.
///
/// Escalation needs a provider that honours per-request `max_tokens`, a known
/// ceiling, and room above the cap the request already used. `context_room` is
/// how many output tokens still fit in the model's context window.
fn escalation_target(
    profile: &ModelProfile,
    used: Option<u32>,
    context_room: Option<u32>,
) -> Option<u32> {
    if !profile.capabilities.options.max_tokens {
        return None;
    }
    let ceiling = profile.caps?.ceiling;
    let used = used?;
    let target = context_room.map_or(ceiling, |room| ceiling.min(room));
    (target > used).then_some(target)
}

/// Output tokens that still fit in the context window after this reply's input.
fn context_room(context: &ConversationContext, response: &LlmResponse) -> Option<u32> {
    let limit = get_model_info(&context.provider, &context.model)?.context_limit()?;
    let usage = response.usage.as_ref()?;
    let input =
        u64::from(usage.input_tokens) + u64::from(usage.cache_read) + u64::from(usage.cache_write);
    Some(u32::try_from(limit.saturating_sub(input)).unwrap_or(u32::MAX))
}

/// Middleware that recovers from replies cut off by the output-token limit.
pub struct OutputLimitRecoveryMiddleware {
    pub(crate) config: OutputLimitConfig,
    /// Drivers are shared by every session of an agent, so state is keyed by session.
    turns: Mutex<HashMap<Arc<str>, TurnState>>,
    /// Where recovery steps are published; set by `bind_agent`.
    events: ArcSwapOption<EventSink>,
}

/// One recovery step, as reported to clients.
struct Recovery<'a> {
    context: &'a ConversationContext,
    attempt: u8,
    action: OutputLimitRecoveryAction,
    from_max_tokens: Option<u32>,
    to_max_tokens: Option<u32>,
    thinking_only: bool,
    discarded_message_id: Option<String>,
}

impl OutputLimitRecoveryMiddleware {
    pub fn new(config: OutputLimitConfig) -> Self {
        Self {
            config,
            turns: Mutex::new(HashMap::new()),
            events: ArcSwapOption::empty(),
        }
    }

    /// Publish a recovery step on the session's ordered event stream.
    /// Never waits for the event journal.
    fn notify(&self, step: Recovery<'_>) {
        let Some(sink) = self.events.load_full() else {
            return;
        };
        sink.session_stream(&step.context.session_id)
            .emit(AgentEventKind::OutputLimitRecovery {
                turn: u32::try_from(step.context.stats.turns).unwrap_or(u32::MAX),
                attempt: u32::from(step.attempt),
                action: step.action,
                from_max_tokens: step.from_max_tokens,
                to_max_tokens: step.to_max_tokens,
                thinking_only: step.thinking_only,
                discarded_message_id: step.discarded_message_id,
            });
    }

    /// Run `f` on the session's per-turn state, creating it on first use.
    pub(crate) fn with_turn<R>(
        &self,
        session_id: &Arc<str>,
        f: impl FnOnce(&mut TurnState) -> R,
    ) -> R {
        f(self.turns.lock().entry(Arc::clone(session_id)).or_default())
    }

    fn clear_turn(&self, session_id: &str) {
        self.turns.lock().remove(session_id);
    }
}

impl Default for OutputLimitRecoveryMiddleware {
    fn default() -> Self {
        Self::new(OutputLimitConfig::default())
    }
}

impl OutputLimitRecoveryMiddleware {
    /// Discard a cut-off reply and retry the same request at `target`.
    ///
    /// The discarded attempt was billed, so its usage is folded into the stats
    /// the retried request starts from.
    fn escalate_to(
        context: &ConversationContext,
        response: &LlmResponse,
        target: u32,
    ) -> ExecutionState {
        let mut stats = (*context.stats).clone();
        apply_usage_to_stats(
            &mut stats,
            response.usage.as_ref(),
            context.profile.pricing.as_ref(),
        );
        let mut next = context
            .clone()
            .with_request_overrides(GenerationOptions::NONE.with_max_tokens(target));
        next.stats = Arc::new(stats);
        ExecutionState::BeforeLlmCall {
            context: Arc::new(next),
        }
    }
}

#[async_trait]
impl MiddlewareDriver for OutputLimitRecoveryMiddleware {
    async fn on_after_llm(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        let ExecutionState::AfterLlm { response, context } = state else {
            return Ok(state);
        };
        let cut = response.finish_reason == Some(FinishReason::Length);
        let thinking_only = cut && response.is_thinking_only_length_cutoff();
        self.with_turn(&context.session_id, |turn| {
            turn.last_cut_thinking_only = thinking_only;
        });

        // A reply that finished normally ends any continuation: drop the instruction.
        if !cut {
            let was_active = self.with_turn(&context.session_id, |turn| {
                std::mem::take(&mut turn.fragment_active)
            });
            let context = if was_active {
                Arc::new(context.remove_fragment(FRAGMENT_KEY))
            } else {
                context
            };
            return Ok(ExecutionState::AfterLlm { response, context });
        }

        if !self.config.escalate {
            return Ok(ExecutionState::AfterLlm { response, context });
        }
        let used = cap_used(&context.profile, response.applied);
        let target = escalation_target(&context.profile, used, context_room(&context, &response));
        let escalate = target.filter(|_| {
            self.with_turn(&context.session_id, |turn| {
                if turn.escalated {
                    false
                } else {
                    turn.escalated = true;
                    turn.escalated_cap = target;
                    true
                }
            })
        });
        match escalate {
            Some(target) => {
                debug!("output limit hit at {used:?} tokens; retrying at {target}");
                self.notify(Recovery {
                    context: &context,
                    attempt: 1,
                    action: OutputLimitRecoveryAction::Escalate,
                    from_max_tokens: used,
                    to_max_tokens: Some(target),
                    thinking_only,
                    discarded_message_id: response.message_id.clone(),
                });
                Ok(Self::escalate_to(&context, &response, target))
            }
            None => Ok(ExecutionState::AfterLlm { response, context }),
        }
    }

    async fn on_stopped(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        let ExecutionState::Stopped {
            stop_type: StopType::ModelTokenLimit,
            context: Some(context),
            ..
        } = &state
        else {
            return Ok(state);
        };
        let used = cap_used(&context.profile, context.last_applied);
        let max_continuations = self.config.max_continuations;
        let plan = self.with_turn(&context.session_id, |turn| {
            (turn.continuations < max_continuations).then(|| {
                turn.continuations += 1;
                turn.fragment_active = true;
                (
                    turn.continuations,
                    turn.escalated_cap,
                    turn.last_cut_thinking_only,
                )
            })
        });
        let Some((attempt, escalated_cap, thinking_only)) = plan else {
            return Ok(state);
        };

        let cap = used.map_or_else(|| "unknown".to_owned(), |cap| cap.to_string());
        let instruction = self.config.instruction.replace("{cap}", &cap);
        let options = continuation_options(
            &context.profile,
            escalated_cap,
            thinking_only && self.config.lower_effort_on_thinking_only,
        );
        debug!(
            "output limit stop recovered by continuation (thinking_only={thinking_only}, {options:?})"
        );
        self.notify(Recovery {
            context,
            attempt,
            action: OutputLimitRecoveryAction::Continue,
            from_max_tokens: used,
            to_max_tokens: options.max_tokens,
            thinking_only,
            discarded_message_id: None,
        });
        let next = context
            .upsert_fragment(FRAGMENT_KEY, instruction, ContextPriority::High)
            .with_request_overrides(options);
        Ok(ExecutionState::BeforeLlmCall {
            context: Arc::new(next),
        })
    }

    async fn on_turn_start(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
        if let Some(context) = state.context() {
            self.clear_turn(&context.session_id);
        }
        Ok(state)
    }

    fn bind_agent(&self, _registry: &Arc<crate::tools::ToolRegistry>, events: &Arc<EventSink>) {
        self.events.store(Some(Arc::clone(events)));
    }

    fn reset(&self) {
        self.turns.lock().clear();
    }

    fn name(&self) -> &'static str {
        "OutputLimitRecoveryMiddleware"
    }
}

/// Factory for `type = "output_limit_recovery"`.
pub struct OutputLimitRecoveryFactory;

impl MiddlewareFactory for OutputLimitRecoveryFactory {
    fn type_name(&self) -> &'static str {
        "output_limit_recovery"
    }

    fn create(
        &self,
        config: &serde_json::Value,
        _agent_config: &AgentConfig,
    ) -> anyhow::Result<Arc<dyn MiddlewareDriver>> {
        let cfg: OutputLimitConfig = serde_json::from_value(config.clone())?;
        if !cfg.enabled {
            return Err(anyhow::anyhow!("Middleware disabled"));
        }
        Ok(Arc::new(OutputLimitRecoveryMiddleware::new(cfg)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{AgentStats, ConversationContext};
    use serde_json::json;

    fn stopped(
        session: &str,
        stop_type: StopType,
        profile: Arc<ModelProfile>,
        applied: GenerationOptions,
    ) -> ExecutionState {
        ExecutionState::Stopped {
            message: "cut off".into(),
            stop_type,
            context: Some(Arc::new(
                (*context_with(session, profile))
                    .clone()
                    .with_last_applied(applied),
            )),
        }
    }

    fn context(session: &str) -> Arc<ConversationContext> {
        context_with(session, Arc::new(ModelProfile::default()))
    }

    fn context_with(session: &str, profile: Arc<ModelProfile>) -> Arc<ConversationContext> {
        Arc::new(
            ConversationContext::new(
                session.into(),
                Arc::from(Vec::new()),
                Arc::new(AgentStats::default()),
                "anthropic".into(),
                "claude".into(),
            )
            .with_profile(profile),
        )
    }

    #[test]
    fn config_defaults_match_the_documented_values() {
        let cfg: OutputLimitConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(cfg, OutputLimitConfig::default());
        assert!(cfg.enabled && cfg.escalate && cfg.lower_effort_on_thinking_only);
        assert_eq!(cfg.max_continuations, 3);
        assert!(cfg.instruction.contains("{cap}"));
    }

    #[test]
    fn config_accepts_partial_overrides() {
        let cfg: OutputLimitConfig = serde_json::from_value(json!({
            "escalate": false,
            "max_continuations": 1,
            "instruction": "go on",
        }))
        .unwrap();
        assert!(!cfg.escalate);
        assert_eq!(cfg.max_continuations, 1);
        assert_eq!(cfg.instruction, "go on");
        assert!(cfg.enabled && cfg.lower_effort_on_thinking_only);
    }

    #[test]
    fn config_rejects_wrongly_typed_fields() {
        assert!(
            serde_json::from_value::<OutputLimitConfig>(json!({"max_continuations": "x"})).is_err()
        );
    }

    #[test]
    fn registry_has_output_limit_recovery() {
        assert!(
            crate::middleware::MIDDLEWARE_REGISTRY
                .type_names()
                .contains(&"output_limit_recovery")
        );
    }

    #[test]
    fn factory_reports_its_type_name() {
        assert_eq!(
            OutputLimitRecoveryFactory.type_name(),
            "output_limit_recovery"
        );
    }

    #[tokio::test]
    async fn turn_start_clears_only_the_starting_session() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let (a, b): (Arc<str>, Arc<str>) = ("a".into(), "b".into());
        mw.with_turn(&a, |t| t.escalated = true);
        mw.with_turn(&b, |t| t.continuations = 2);

        let state = ExecutionState::BeforeLlmCall {
            context: context("a"),
        };
        let state = mw.on_turn_start(state, None).await.unwrap();

        assert!(matches!(state, ExecutionState::BeforeLlmCall { .. }));
        assert!(!mw.with_turn(&a, |t| t.escalated));
        assert_eq!(mw.with_turn(&b, |t| t.continuations), 2);
    }

    #[test]
    fn reset_clears_every_session() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let a: Arc<str> = "a".into();
        mw.with_turn(&a, |t| t.fragment_active = true);
        mw.reset();
        assert!(!mw.with_turn(&a, |t| t.fragment_active));
    }

    // ── escalation (5.2) ────────────────────────────────────────────────

    /// A profile whose provider honours every option, starting at `max_tokens`.
    fn profile_for(max_tokens: u32, ceiling: u32) -> Arc<ModelProfile> {
        Arc::new(ModelProfile {
            caps: Some(crate::model_heuristics::OutputCaps {
                initial: max_tokens.min(ceiling),
                ceiling,
            }),
            capabilities: querymt::chat::ChatCapabilities::NONE
                .with_options(querymt::chat::GenerationOptionSupport::ALL),
            reasoning_effort: None,
            pricing: None,
        })
    }

    fn reply(finish: FinishReason) -> Arc<LlmResponse> {
        Arc::new(LlmResponse::new(
            "partial".into(),
            Vec::new(),
            Some(querymt::Usage {
                input_tokens: 100,
                output_tokens: 32_000,
                reasoning_tokens: 0,
                cache_read: 0,
                cache_write: 0,
            }),
            Some(finish),
        ))
    }

    async fn after_llm(
        mw: &OutputLimitRecoveryMiddleware,
        finish: FinishReason,
        profile: &Arc<ModelProfile>,
    ) -> ExecutionState {
        let state = ExecutionState::AfterLlm {
            response: reply(finish),
            context: context_with("s", profile.clone()),
        };
        mw.on_after_llm(state, None).await.unwrap()
    }

    #[test]
    fn escalation_target_requires_room_above_the_used_cap() {
        let p = profile_for(32_000, 128_000);
        assert_eq!(escalation_target(&p, Some(32_000), None), Some(128_000));
        assert_eq!(
            escalation_target(&p, Some(32_000), Some(64_000)),
            Some(64_000)
        );
        assert_eq!(escalation_target(&p, Some(32_000), Some(32_000)), None);
        assert_eq!(escalation_target(&p, Some(128_000), None), None);
    }

    #[test]
    fn escalation_target_needs_provider_support_and_known_caps() {
        let mut p = (*profile_for(32_000, 128_000)).clone();
        p.capabilities.options = querymt::chat::GenerationOptionSupport::NONE;
        assert_eq!(escalation_target(&p, Some(32_000), None), None);

        let mut p = (*profile_for(32_000, 128_000)).clone();
        p.caps = None;
        assert_eq!(escalation_target(&p, Some(32_000), None), None);
        assert_eq!(cap_used(&p, GenerationOptions::NONE), None);
    }

    #[test]
    fn cap_used_prefers_the_applied_override_over_the_initial_cap() {
        let p = profile_for(32_000, 128_000);
        assert_eq!(cap_used(&p, GenerationOptions::NONE), Some(32_000));
        assert_eq!(
            cap_used(&p, GenerationOptions::NONE.with_max_tokens(128_000)),
            Some(128_000)
        );
    }

    #[tokio::test]
    async fn length_cutoff_below_ceiling_escalates_with_usage_counted() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let runtime = profile_for(32_000, 128_000);

        let state = after_llm(&mw, FinishReason::Length, &runtime).await;

        let ExecutionState::BeforeLlmCall { context } = state else {
            panic!("expected BeforeLlmCall, got {}", state.name());
        };
        assert_eq!(context.request_overrides.max_tokens, Some(128_000));
        assert_eq!(context.stats.total_output_tokens, 32_000);
        assert_eq!(context.stats.total_input_tokens, 100);
    }

    #[tokio::test]
    async fn escalation_happens_once_per_turn() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let runtime = profile_for(32_000, 128_000);
        assert!(matches!(
            after_llm(&mw, FinishReason::Length, &runtime).await,
            ExecutionState::BeforeLlmCall { .. }
        ));

        // Second cut-off in the same turn passes through to the normal path.
        assert!(matches!(
            after_llm(&mw, FinishReason::Length, &runtime).await,
            ExecutionState::AfterLlm { .. }
        ));
    }

    #[tokio::test]
    async fn cutoff_at_the_ceiling_passes_through_without_spending_the_escalation() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let runtime = profile_for(128_000, 128_000);
        assert!(matches!(
            after_llm(&mw, FinishReason::Length, &runtime).await,
            ExecutionState::AfterLlm { .. }
        ));
        assert!(!mw.with_turn(&"s".into(), |t| t.escalated));
    }

    // ── continuation (5.3) ──────────────────────────────────────────────

    async fn stop(
        mw: &OutputLimitRecoveryMiddleware,
        profile: &Arc<ModelProfile>,
        stop_type: StopType,
    ) -> ExecutionState {
        stop_with(mw, profile, GenerationOptions::NONE, stop_type).await
    }

    async fn stop_with(
        mw: &OutputLimitRecoveryMiddleware,
        profile: &Arc<ModelProfile>,
        applied: GenerationOptions,
        stop_type: StopType,
    ) -> ExecutionState {
        mw.on_stopped(stopped("s", stop_type, profile.clone(), applied), None)
            .await
            .unwrap()
    }

    fn recovered(state: ExecutionState) -> Arc<ConversationContext> {
        match state {
            ExecutionState::BeforeLlmCall { context } => context,
            other => panic!("expected BeforeLlmCall, got {}", other.name()),
        }
    }

    #[test]
    fn lowered_effort_steps_down_and_floors_at_low() {
        assert_eq!(lowered_effort(None), ReasoningEffort::Medium);
        assert_eq!(
            lowered_effort(Some(ReasoningEffort::Max)),
            ReasoningEffort::Medium
        );
        assert_eq!(
            lowered_effort(Some(ReasoningEffort::High)),
            ReasoningEffort::Medium
        );
        assert_eq!(
            lowered_effort(Some(ReasoningEffort::Medium)),
            ReasoningEffort::Low
        );
        assert_eq!(
            lowered_effort(Some(ReasoningEffort::Low)),
            ReasoningEffort::Low
        );
    }

    #[tokio::test]
    async fn stop_continues_with_the_instruction_naming_the_cap() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let runtime = profile_for(128_000, 128_000);

        let context = recovered(stop(&mw, &runtime, StopType::ModelTokenLimit).await);

        let fragment = context
            .fragments
            .iter()
            .find(|fragment| fragment.key == FRAGMENT_KEY)
            .expect("instruction fragment");
        assert!(
            fragment.content.contains("128000 tokens"),
            "{}",
            fragment.content
        );
        assert_eq!(fragment.priority, ContextPriority::High);
        assert!(
            context.request_overrides.is_empty(),
            "no escalation, plain reply"
        );
    }

    #[tokio::test]
    async fn other_stop_types_pass_through_untouched() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let runtime = profile_for(128_000, 128_000);
        for stop_type in [StopType::ContextThreshold, StopType::ContentFilter] {
            let state = stop(&mw, &runtime, stop_type).await;
            assert!(matches!(state, ExecutionState::Stopped { .. }));
        }
        assert_eq!(mw.with_turn(&"s".into(), |t| t.continuations), 0);
    }

    #[tokio::test]
    async fn text_continuation_follows_the_retained_assistant_reply() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let profile = profile_for(128_000, 128_000);
        let mut context = (*context_with("s", profile)).clone();
        context.messages = Arc::from([
            querymt::chat::ChatMessage::user()
                .text("write a report")
                .build(),
            querymt::chat::ChatMessage::assistant()
                .text("partial report")
                .build(),
        ]);
        let state = ExecutionState::Stopped {
            message: "cut off".into(),
            stop_type: StopType::ModelTokenLimit,
            context: Some(Arc::new(context)),
        };
        let context = recovered(mw.on_stopped(state, None).await.unwrap());
        let messages = context.request_messages();
        assert_eq!(messages[1].text(), "partial report");
        assert_eq!(messages[2].role, querymt::chat::ChatRole::User);
        assert!(messages[2].text().contains("Continue directly"));
        assert_eq!(context.messages.len(), 2, "guidance stays temporary");
    }

    #[tokio::test]
    async fn continuation_preserves_the_clamped_escalation_cap() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let profile = profile_for(32_000, 128_000);
        mw.with_turn(&"s".into(), |turn| {
            turn.escalated = true;
            turn.escalated_cap = Some(50_000);
        });
        let context = recovered(
            stop_with(
                &mw,
                &profile,
                GenerationOptions::NONE.with_max_tokens(50_000),
                StopType::ModelTokenLimit,
            )
            .await,
        );
        assert_eq!(context.request_overrides.max_tokens, Some(50_000));
    }

    #[tokio::test]
    async fn successful_intermediate_replies_do_not_replenish_continuations() {
        let mw = OutputLimitRecoveryMiddleware::new(OutputLimitConfig {
            max_continuations: 2,
            ..Default::default()
        });
        let profile = profile_for(128_000, 128_000);
        for expected in 1..=2 {
            let context = recovered(stop(&mw, &profile, StopType::ModelTokenLimit).await);
            mw.on_after_llm(
                ExecutionState::AfterLlm {
                    response: reply(FinishReason::ToolCalls),
                    context,
                },
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                mw.with_turn(&"s".into(), |turn| turn.continuations),
                expected
            );
        }
        assert!(matches!(
            stop(&mw, &profile, StopType::ModelTokenLimit).await,
            ExecutionState::Stopped { .. }
        ));
    }

    #[tokio::test]
    async fn continuation_repeats_the_escalated_cap() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let runtime = profile_for(32_000, 128_000);
        // Spend the escalation, then hit the ceiling and stop.
        after_llm(&mw, FinishReason::Length, &runtime).await;
        let at_ceiling = GenerationOptions::NONE.with_max_tokens(128_000);

        let context =
            recovered(stop_with(&mw, &runtime, at_ceiling, StopType::ModelTokenLimit).await);

        assert_eq!(context.request_overrides.max_tokens, Some(128_000));
    }

    #[tokio::test]
    async fn thinking_only_cutoff_lowers_effort_when_the_provider_honours_it() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let mut with_effort = (*profile_for(128_000, 128_000)).clone();
        with_effort.reasoning_effort = Some(ReasoningEffort::High);
        let with_effort = Arc::new(with_effort);
        let thinking_only = ExecutionState::AfterLlm {
            response: Arc::new(LlmResponse::new(
                String::new(),
                Vec::new(),
                None,
                Some(FinishReason::Length),
            )),
            context: context_with("s", with_effort.clone()),
        };
        mw.on_after_llm(thinking_only, None).await.unwrap();

        let context = recovered(stop(&mw, &with_effort, StopType::ModelTokenLimit).await);
        assert_eq!(
            context.request_overrides.reasoning_effort,
            Some(ReasoningEffort::Medium)
        );

        // A provider that ignores effort gets no override.
        let mut unsupported = (*with_effort).clone();
        unsupported.capabilities.options.reasoning_effort = false;
        let context = recovered(stop(&mw, &Arc::new(unsupported), StopType::ModelTokenLimit).await);
        assert_eq!(context.request_overrides.reasoning_effort, None);
    }

    #[tokio::test]
    async fn mixed_cutoff_keeps_the_configured_effort() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let runtime = profile_for(128_000, 128_000);
        // `reply` carries text, so it is not thinking-only.
        after_llm(&mw, FinishReason::Length, &runtime).await;

        let context = recovered(stop(&mw, &runtime, StopType::ModelTokenLimit).await);

        assert_eq!(context.request_overrides.reasoning_effort, None);
    }

    #[tokio::test]
    async fn instruction_is_removed_after_the_next_normal_reply() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let runtime = profile_for(128_000, 128_000);
        let with_fragment = recovered(stop(&mw, &runtime, StopType::ModelTokenLimit).await);
        // `stop` used session "s", the same one `after_llm` below replies for.
        let state = ExecutionState::AfterLlm {
            response: reply(FinishReason::Stop),
            context: with_fragment,
        };

        let state = mw.on_after_llm(state, None).await.unwrap();

        let ExecutionState::AfterLlm { context, .. } = state else {
            panic!("expected AfterLlm, got {}", state.name());
        };
        assert!(context.fragments.iter().all(|f| f.key != FRAGMENT_KEY));
        assert_eq!(mw.with_turn(&"s".into(), |t| t.continuations), 1);
    }

    #[tokio::test]
    async fn continuations_are_capped_per_turn() {
        let mw = OutputLimitRecoveryMiddleware::new(OutputLimitConfig {
            max_continuations: 2,
            ..Default::default()
        });
        let runtime = profile_for(128_000, 128_000);
        for _ in 0..2 {
            recovered(stop(&mw, &runtime, StopType::ModelTokenLimit).await);
        }

        let state = stop(&mw, &runtime, StopType::ModelTokenLimit).await;

        assert!(matches!(
            state,
            ExecutionState::Stopped {
                stop_type: StopType::ModelTokenLimit,
                ..
            }
        ));
    }

    // ── events (5.4) ────────────────────────────────────────────────────

    /// Next `OutputLimitRecovery` event on `rx`, with its durability.
    async fn next_recovery(
        rx: &mut tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
    ) -> (AgentEventKind, bool) {
        let deadline = std::time::Duration::from_secs(5);
        loop {
            let envelope = tokio::time::timeout(deadline, rx.recv())
                .await
                .expect("recovery event published")
                .expect("fanout open");
            if matches!(envelope.kind(), AgentEventKind::OutputLimitRecovery { .. }) {
                return (envelope.kind().clone(), envelope.is_durable());
            }
        }
    }

    async fn bound_middleware() -> (
        OutputLimitRecoveryMiddleware,
        tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
        crate::test_utils::TestAgent,
    ) {
        let agent = crate::test_utils::TestAgent::new().await;
        let rx = agent.config.event_sink.fanout().subscribe();
        let mw = OutputLimitRecoveryMiddleware::default();
        mw.bind_agent(&agent.config.tool_registry, &agent.config.event_sink);
        (mw, rx, agent)
    }

    #[tokio::test]
    async fn escalation_publishes_a_durable_recovery_event() {
        let (mw, mut rx, _agent) = bound_middleware().await;
        let runtime = profile_for(32_000, 128_000);
        let response = Arc::new(
            LlmResponse::new(String::new(), Vec::new(), None, Some(FinishReason::Length))
                .with_message_id("m-1".into()),
        );
        let state = ExecutionState::AfterLlm {
            response,
            context: context_with("s", runtime),
        };

        mw.on_after_llm(state, None).await.unwrap();

        let (kind, durable) = next_recovery(&mut rx).await;
        assert!(durable, "recovery events belong in the session journal");
        let AgentEventKind::OutputLimitRecovery {
            attempt,
            action,
            from_max_tokens,
            to_max_tokens,
            thinking_only,
            discarded_message_id,
            ..
        } = kind
        else {
            unreachable!()
        };
        assert_eq!(attempt, 1);
        assert_eq!(action, OutputLimitRecoveryAction::Escalate);
        assert_eq!(from_max_tokens, Some(32_000));
        assert_eq!(to_max_tokens, Some(128_000));
        assert!(thinking_only);
        assert_eq!(discarded_message_id.as_deref(), Some("m-1"));
    }

    #[tokio::test]
    async fn each_continuation_publishes_a_numbered_recovery_event() {
        let (mw, mut rx, _agent) = bound_middleware().await;
        let runtime = profile_for(128_000, 128_000);

        for expected in 1..=2 {
            recovered(stop(&mw, &runtime, StopType::ModelTokenLimit).await);
            let (kind, _) = next_recovery(&mut rx).await;
            let AgentEventKind::OutputLimitRecovery {
                attempt,
                action,
                from_max_tokens,
                discarded_message_id,
                ..
            } = kind
            else {
                unreachable!()
            };
            assert_eq!(attempt, expected);
            assert_eq!(action, OutputLimitRecoveryAction::Continue);
            assert_eq!(from_max_tokens, Some(128_000));
            assert_eq!(discarded_message_id, None);
        }
    }

    #[tokio::test]
    async fn passing_through_publishes_nothing() {
        let (_mw, mut rx, agent) = bound_middleware().await;
        let runtime = profile_for(128_000, 128_000);

        // Cut off at the ceiling with no continuations left to spend.
        let capped = OutputLimitRecoveryMiddleware::new(OutputLimitConfig {
            max_continuations: 0,
            ..Default::default()
        });
        capped.bind_agent(&agent.config.tool_registry, &agent.config.event_sink);
        stop(&capped, &runtime, StopType::ModelTokenLimit).await;
        after_llm(&capped, FinishReason::Stop, &runtime).await;

        let quiet = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            loop {
                let envelope = rx.recv().await.expect("fanout open");
                if matches!(envelope.kind(), AgentEventKind::OutputLimitRecovery { .. }) {
                    return;
                }
            }
        })
        .await;
        assert!(quiet.is_err(), "no recovery happened, so no event");
    }

    #[tokio::test]
    async fn recovery_event_is_persisted_in_the_session_journal() {
        let (mw, _rx, agent) = bound_middleware().await;
        let session_id = agent.create_session().await;
        let runtime = profile_for(128_000, 128_000);
        let stopped = ExecutionState::Stopped {
            message: "cut off".into(),
            stop_type: StopType::ModelTokenLimit,
            context: Some(context_with(&session_id, runtime)),
        };

        mw.on_stopped(stopped, None).await.unwrap();

        // Emission is detached from the caller, so wait for the journal write.
        let journal = agent.config.event_sink.journal().clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let stored = loop {
            let events = journal
                .load_session_stream(&session_id, None, None)
                .await
                .unwrap();
            if let Some(event) = events
                .into_iter()
                .find(|event| matches!(event.kind, AgentEventKind::OutputLimitRecovery { .. }))
            {
                break event;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "recovery event never reached the journal"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        let AgentEventKind::OutputLimitRecovery {
            action, attempt, ..
        } = stored.kind
        else {
            unreachable!()
        };
        assert_eq!(action, OutputLimitRecoveryAction::Continue);
        assert_eq!(attempt, 1);
    }

    #[tokio::test]
    async fn unbound_middleware_recovers_silently() {
        let mw = OutputLimitRecoveryMiddleware::default();
        let runtime = profile_for(128_000, 128_000);
        recovered(stop(&mw, &runtime, StopType::ModelTokenLimit).await);
    }

    #[tokio::test]
    async fn disabled_escalation_and_normal_replies_pass_through() {
        let runtime = profile_for(32_000, 128_000);
        let off = OutputLimitRecoveryMiddleware::new(OutputLimitConfig {
            escalate: false,
            ..Default::default()
        });
        assert!(matches!(
            after_llm(&off, FinishReason::Length, &runtime).await,
            ExecutionState::AfterLlm { .. }
        ));

        let on = OutputLimitRecoveryMiddleware::default();
        assert!(matches!(
            after_llm(&on, FinishReason::Stop, &runtime).await,
            ExecutionState::AfterLlm { .. }
        ));
    }
}

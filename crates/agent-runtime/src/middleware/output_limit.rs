//! Recovery from replies cut off by the output-token limit.
//!
//! A reply that ends with a length finish reason used to end the run with
//! `Stopped { ModelTokenLimit }`. This middleware turns that into recovery:
//!
//! 1. **Escalate** (`on_after_llm`): when a request below the model's output
//!    ceiling is cut off, discard the reply and retry once at the ceiling.
//!    The escalated cap then applies to every later request of the run
//!    (`on_step_start`), because request overrides last one request only.
//! 2. **Continue** (`on_stopped`): when the reply is still cut off, resume with a
//!    stored `output_limit` notice (no recap, split large writes), once per
//!    continuation, up to `max_continuations` times per run. The notice stays
//!    in history.
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
use crate::middleware::factory::MiddlewareFactory;
use crate::middleware::{
    HookCx, LlmResponse, Middleware, ModelProfile, ReplyVerdict, Result, StepVerdict, Stop,
    StopVerdict, TurnInfo, TurnMiddleware,
};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use log::debug;
use querymt::chat::{FinishReason, GenerationOptions, ReasoningEffort};
use querymt_agent_core::events::{AgentEventKind, OutputLimitRecoveryAction, StopType};
use querymt_agent_core::model::NoticeSource;
use querymt_agent_services::event_sink::EventSink;
use querymt_agent_services::model_info::get_model_info;
use serde::Deserialize;
use std::sync::Arc;

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
fn context_room(provider: &str, model: &str, response: &LlmResponse) -> Option<u32> {
    let limit = get_model_info(provider, model)?.context_limit()?;
    let usage = response.usage.as_ref()?;
    let input =
        u64::from(usage.input_tokens) + u64::from(usage.cache_read) + u64::from(usage.cache_write);
    Some(u32::try_from(limit.saturating_sub(input)).unwrap_or(u32::MAX))
}

/// Middleware that recovers from replies cut off by the output-token limit.
pub struct OutputLimitRecoveryMiddleware {
    pub(crate) config: OutputLimitConfig,
    /// Where recovery steps are published; set by `bind_agent`.
    events: ArcSwapOption<EventSink>,
}

impl OutputLimitRecoveryMiddleware {
    pub fn new(config: OutputLimitConfig) -> Self {
        Self {
            config,
            events: ArcSwapOption::empty(),
        }
    }
}

impl Default for OutputLimitRecoveryMiddleware {
    fn default() -> Self {
        Self::new(OutputLimitConfig::default())
    }
}

impl Middleware for OutputLimitRecoveryMiddleware {
    fn name(&self) -> &'static str {
        "OutputLimitRecoveryMiddleware"
    }

    fn begin_turn(&self, _turn: &TurnInfo<'_>) -> Box<dyn TurnMiddleware> {
        Box::new(OutputLimitTurn {
            config: self.config.clone(),
            events: self.events.load_full(),
            escalated: false,
            escalated_cap: None,
            continuations: 0,
            last_cut_thinking_only: false,
        })
    }

    fn bind_agent(&self, _registry: &Arc<crate::tools::ToolRegistry>, events: &Arc<EventSink>) {
        self.events.store(Some(Arc::clone(events)));
    }
}

/// Recovery progress within one run.
pub(crate) struct OutputLimitTurn {
    config: OutputLimitConfig,
    events: Option<Arc<EventSink>>,
    /// The one-shot escalation to the ceiling has been spent.
    pub(crate) escalated: bool,
    /// Effective escalation cap, including any context-window clamp.
    escalated_cap: Option<u32>,
    /// Continuations issued so far.
    pub(crate) continuations: u8,
    /// The most recent reply was cut off and carried only reasoning. Such a
    /// reply is never stored, so this is the only record of it.
    pub(crate) last_cut_thinking_only: bool,
}

/// One recovery step, as reported to clients.
struct Recovery {
    attempt: u8,
    action: OutputLimitRecoveryAction,
    from_max_tokens: Option<u32>,
    to_max_tokens: Option<u32>,
    thinking_only: bool,
    discarded_message_id: Option<String>,
}

impl OutputLimitTurn {
    /// Publish a recovery step on the session's ordered event stream.
    /// Never waits for the event journal.
    fn notify(&self, cx: &HookCx<'_>, step: Recovery) {
        let Some(sink) = &self.events else {
            return;
        };
        sink.session_stream(cx.session_id())
            .emit(AgentEventKind::OutputLimitRecovery {
                turn: u32::try_from(cx.stats().turns).unwrap_or(u32::MAX),
                attempt: u32::from(step.attempt),
                action: step.action,
                from_max_tokens: step.from_max_tokens,
                to_max_tokens: step.to_max_tokens,
                thinking_only: step.thinking_only,
                discarded_message_id: step.discarded_message_id,
            });
    }
}

#[async_trait]
impl TurnMiddleware for OutputLimitTurn {
    /// Discard a cut-off reply and retry the same request at the ceiling.
    ///
    /// The discarded attempt was billed, so the loop folds its token usage
    /// into the stats the retried request starts from. Its cost is already
    /// recorded by accounting and reaches the monetary stats fields when they
    /// are refreshed from the cycle totals before the next model step.
    async fn on_after_llm(
        &mut self,
        cx: &mut HookCx<'_>,
        reply: &LlmResponse,
    ) -> Result<ReplyVerdict> {
        let cut = reply.finish_reason == Some(FinishReason::Length);
        let thinking_only = cut && reply.is_thinking_only_length_cutoff();
        self.last_cut_thinking_only = thinking_only;

        // The continuation instruction is a stored notice and stays in history,
        // so a reply that finished normally changes nothing.
        if !cut || !self.config.escalate || self.escalated {
            return Ok(ReplyVerdict::Proceed);
        }

        let used = cap_used(cx.profile(), reply.applied);
        let Some(target) = escalation_target(
            cx.profile(),
            used,
            context_room(cx.provider(), cx.model(), reply),
        ) else {
            return Ok(ReplyVerdict::Proceed);
        };

        self.escalated = true;
        self.escalated_cap = Some(target);
        debug!("output limit hit at {used:?} tokens; retrying at {target}");
        self.notify(
            cx,
            Recovery {
                attempt: 1,
                action: OutputLimitRecoveryAction::Escalate,
                from_max_tokens: used,
                to_max_tokens: Some(target),
                thinking_only,
                discarded_message_id: reply.message_id.clone(),
            },
        );
        cx.set_request_overrides(GenerationOptions::NONE.with_max_tokens(target));
        Ok(ReplyVerdict::Retry)
    }

    /// Keep an escalated cap for the rest of the run. Overrides last one
    /// request, so without this the next tool-loop step would fall back to the
    /// initial cap, and a cut-off there could no longer escalate. An override
    /// another middleware already set wins. The request-time clamp to the
    /// context room still applies.
    async fn on_step_start(&mut self, cx: &mut HookCx<'_>) -> Result<StepVerdict> {
        if cx.request_overrides().max_tokens.is_some()
            || !cx.profile().capabilities.options.max_tokens
        {
            return Ok(StepVerdict::Proceed);
        }
        if let Some(cap) = self.escalated_cap {
            let mut overrides = cx.request_overrides();
            overrides.max_tokens = Some(cap);
            cx.set_request_overrides(overrides);
        }
        Ok(StepVerdict::Proceed)
    }

    async fn on_stopped(&mut self, cx: &mut HookCx<'_>, stop: &Stop) -> Result<StopVerdict> {
        if stop.stop_type != StopType::ModelTokenLimit
            || self.continuations >= self.config.max_continuations
        {
            return Ok(StopVerdict::Pass);
        }
        self.continuations += 1;
        let attempt = self.continuations;
        let thinking_only = self.last_cut_thinking_only;

        let used = cap_used(cx.profile(), cx.last_applied());
        let cap = used.map_or_else(|| "unknown".to_owned(), |cap| cap.to_string());
        let instruction = self.config.instruction.replace("{cap}", &cap);
        let options = continuation_options(
            cx.profile(),
            self.escalated_cap,
            thinking_only && self.config.lower_effort_on_thinking_only,
        );
        debug!(
            "output limit stop recovered by continuation (thinking_only={thinking_only}, {options:?})"
        );
        self.notify(
            cx,
            Recovery {
                attempt,
                action: OutputLimitRecoveryAction::Continue,
                from_max_tokens: used,
                to_max_tokens: options.max_tokens,
                thinking_only,
                discarded_message_id: None,
            },
        );
        // Stored once per continuation, after any retained partial reply.
        cx.queue_notice(NoticeSource::OutputLimit, instruction);
        cx.set_request_overrides(options);
        Ok(StopVerdict::Recover)
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
    ) -> anyhow::Result<Option<Arc<dyn Middleware>>> {
        let cfg: OutputLimitConfig = serde_json::from_value(config.clone())?;
        if !cfg.enabled {
            return Ok(None);
        }
        Ok(Some(Arc::new(OutputLimitRecoveryMiddleware::new(cfg))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{AgentStats, ConversationContext, ExecutionState, TurnStack};
    use crate::test_utils::HookRig;
    use serde_json::json;

    /// One run of the middleware: its per-run state and a rig to call its hooks.
    struct Run {
        stack: TurnStack,
        rig: HookRig,
    }

    impl Default for Run {
        fn default() -> Self {
            Self::new(OutputLimitRecoveryMiddleware::default())
        }
    }

    impl Run {
        fn new(middleware: OutputLimitRecoveryMiddleware) -> Self {
            Self {
                stack: HookRig::stack_of(middleware),
                rig: HookRig::new(),
            }
        }

        async fn on_after_llm(&mut self, state: ExecutionState) -> Result<ExecutionState> {
            self.rig.after_llm(&mut self.stack, state).await
        }

        async fn on_step_start(&mut self, state: ExecutionState) -> Result<ExecutionState> {
            self.rig.step_start(&mut self.stack, state).await
        }

        async fn on_stopped(&mut self, state: ExecutionState) -> Result<ExecutionState> {
            self.rig.stopped(&mut self.stack, state).await
        }
    }

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

    // ── escalation (5.2) ────────────────────────────────────────────────

    /// A profile whose provider honours every option, starting at `max_tokens`.
    fn profile_for(max_tokens: u32, ceiling: u32) -> Arc<ModelProfile> {
        Arc::new(ModelProfile {
            caps: Some(querymt_agent_services::model_heuristics::OutputCaps {
                initial: max_tokens.min(ceiling),
                ceiling,
            }),
            capabilities: querymt::chat::ChatCapabilities::NONE
                .with_options(querymt::chat::GenerationOptionSupport::ALL),
            reasoning_effort: None,
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
                cache_write_tiers: None,
            }),
            Some(finish),
        ))
    }

    async fn after_llm(
        mw: &mut Run,
        finish: FinishReason,
        profile: &Arc<ModelProfile>,
    ) -> ExecutionState {
        let state = ExecutionState::AfterLlm {
            response: reply(finish),
            context: context_with("s", profile.clone()),
        };
        mw.on_after_llm(state).await.unwrap()
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
        let mut mw = Run::default();
        let runtime = profile_for(32_000, 128_000);

        let state = after_llm(&mut mw, FinishReason::Length, &runtime).await;

        let ExecutionState::BeforeLlmCall { context } = state else {
            panic!("expected BeforeLlmCall, got {}", state.name());
        };
        assert_eq!(context.request_overrides.max_tokens, Some(128_000));
        assert_eq!(context.stats.total_output_tokens, 32_000);
        assert_eq!(context.stats.total_input_tokens, 100);
    }

    async fn step_start(
        mw: &mut Run,
        context: Arc<ConversationContext>,
    ) -> Arc<ConversationContext> {
        match mw
            .on_step_start(ExecutionState::BeforeLlmCall { context })
            .await
            .unwrap()
        {
            ExecutionState::BeforeLlmCall { context } => context,
            other => panic!("expected BeforeLlmCall, got {}", other.name()),
        }
    }

    #[tokio::test]
    async fn escalated_cap_applies_to_later_steps_of_the_turn() {
        let mut mw = Run::default();
        let profile = profile_for(32_000, 128_000);
        // Before any escalation the step is untouched.
        let fresh = step_start(&mut mw, context_with("s", profile.clone())).await;
        assert_eq!(fresh.request_overrides.max_tokens, None);

        after_llm(&mut mw, FinishReason::Length, &profile).await;
        // A later tool-loop step starts from a context without overrides.
        let next = step_start(&mut mw, context_with("s", profile.clone())).await;
        assert_eq!(next.request_overrides.max_tokens, Some(128_000));

        // A cut-off of that step can't escalate again: it goes to continuation.
        let state = ExecutionState::AfterLlm {
            response: Arc::new(
                (*reply(FinishReason::Length))
                    .clone()
                    .with_applied(GenerationOptions::NONE.with_max_tokens(128_000)),
            ),
            context: next,
        };
        assert!(matches!(
            mw.on_after_llm(state).await.unwrap(),
            ExecutionState::AfterLlm { .. }
        ));
    }

    #[tokio::test]
    async fn explicit_cap_override_wins_over_the_escalated_cap() {
        let mut mw = Run::default();
        let profile = profile_for(32_000, 128_000);
        after_llm(&mut mw, FinishReason::Length, &profile).await;
        let explicit = Arc::new(
            (*context_with("s", profile))
                .clone()
                .with_request_overrides(GenerationOptions::NONE.with_max_tokens(1_000)),
        );
        assert_eq!(
            step_start(&mut mw, explicit)
                .await
                .request_overrides
                .max_tokens,
            Some(1_000)
        );
    }

    #[tokio::test]
    async fn next_run_starts_at_the_initial_cap() {
        let mut mw = Run::default();
        let profile = profile_for(32_000, 128_000);
        after_llm(&mut mw, FinishReason::Length, &profile).await;

        // The next run gets its own instance.
        let mut next_run = Run::default();
        let next = step_start(&mut next_run, context_with("s", profile)).await;
        assert_eq!(next.request_overrides.max_tokens, None);
    }

    #[tokio::test]
    async fn escalation_happens_once_per_turn() {
        let mut mw = Run::default();
        let runtime = profile_for(32_000, 128_000);
        assert!(matches!(
            after_llm(&mut mw, FinishReason::Length, &runtime).await,
            ExecutionState::BeforeLlmCall { .. }
        ));

        // Second cut-off in the same turn passes through to the normal path.
        assert!(matches!(
            after_llm(&mut mw, FinishReason::Length, &runtime).await,
            ExecutionState::AfterLlm { .. }
        ));
    }

    #[tokio::test]
    async fn cutoff_at_the_ceiling_passes_through_without_spending_the_escalation() {
        let mut mw = Run::default();
        let runtime = profile_for(128_000, 128_000);
        assert!(matches!(
            after_llm(&mut mw, FinishReason::Length, &runtime).await,
            ExecutionState::AfterLlm { .. }
        ));
        // The escalation is still unspent, so later steps get no raised cap.
        let next = step_start(&mut mw, context_with("s", runtime)).await;
        assert_eq!(next.request_overrides.max_tokens, None);
    }

    // ── continuation (5.3) ──────────────────────────────────────────────

    async fn stop(
        mw: &mut Run,
        profile: &Arc<ModelProfile>,
        stop_type: StopType,
    ) -> ExecutionState {
        stop_with(mw, profile, GenerationOptions::NONE, stop_type).await
    }

    async fn stop_with(
        mw: &mut Run,
        profile: &Arc<ModelProfile>,
        applied: GenerationOptions,
        stop_type: StopType,
    ) -> ExecutionState {
        mw.on_stopped(stopped("s", stop_type, profile.clone(), applied))
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
        let mut mw = Run::default();
        let runtime = profile_for(128_000, 128_000);

        let context = recovered(stop(&mut mw, &runtime, StopType::ModelTokenLimit).await);

        assert_eq!(context.pending_notices.len(), 1);
        let notice = &context.pending_notices[0];
        assert_eq!(notice.source, NoticeSource::OutputLimit);
        assert!(
            notice.content.contains("128000 tokens"),
            "{}",
            notice.content
        );
        assert!(
            context.request_overrides.is_empty(),
            "no escalation, plain reply"
        );
    }

    #[tokio::test]
    async fn other_stop_types_pass_through_untouched() {
        let mut mw = Run::new(OutputLimitRecoveryMiddleware::new(OutputLimitConfig {
            max_continuations: 1,
            ..Default::default()
        }));
        let runtime = profile_for(128_000, 128_000);
        for stop_type in [StopType::ContextThreshold, StopType::ContentFilter] {
            let state = stop(&mut mw, &runtime, stop_type).await;
            assert!(matches!(state, ExecutionState::Stopped { .. }));
        }
        // Nothing was spent: the one continuation is still available.
        recovered(stop(&mut mw, &runtime, StopType::ModelTokenLimit).await);
    }

    #[tokio::test]
    async fn guidance_after_a_retained_partial_reply() {
        let mut mw = Run::default();
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
        let context = recovered(mw.on_stopped(state).await.unwrap());
        // The reply is sent unchanged; the guidance is queued to be stored after it.
        assert_eq!(context.messages.len(), 2);
        assert_eq!(context.messages[1].text(), "partial report");
        assert_eq!(context.pending_notices.len(), 1);
        assert!(
            context.pending_notices[0]
                .content
                .contains("Continue directly")
        );
    }

    #[tokio::test]
    async fn continuation_preserves_the_clamped_escalation_cap() {
        let rig = HookRig::new();
        let mut turn = OutputLimitTurn {
            config: OutputLimitConfig::default(),
            events: None,
            escalated: true,
            escalated_cap: Some(50_000),
            continuations: 0,
            last_cut_thinking_only: false,
        };
        let profile = profile_for(32_000, 128_000);
        let mut context = Arc::new(
            (*context_with("s", profile))
                .clone()
                .with_last_applied(GenerationOptions::NONE.with_max_tokens(50_000)),
        );
        let stop = Stop::new(StopType::ModelTokenLimit, "cut off");

        let verdict = turn
            .on_stopped(
                &mut HookCx::new(&mut context, &rig.runtime, &rig.changed),
                &stop,
            )
            .await
            .unwrap();

        assert_eq!(verdict, StopVerdict::Recover);
        assert_eq!(context.request_overrides.max_tokens, Some(50_000));
    }

    #[tokio::test]
    async fn successful_intermediate_replies_do_not_replenish_continuations() {
        let mut mw = Run::new(OutputLimitRecoveryMiddleware::new(OutputLimitConfig {
            max_continuations: 2,
            ..Default::default()
        }));
        let profile = profile_for(128_000, 128_000);
        for _ in 1..=2 {
            let context = recovered(stop(&mut mw, &profile, StopType::ModelTokenLimit).await);
            mw.on_after_llm(ExecutionState::AfterLlm {
                response: reply(FinishReason::ToolCalls),
                context,
            })
            .await
            .unwrap();
        }
        assert!(matches!(
            stop(&mut mw, &profile, StopType::ModelTokenLimit).await,
            ExecutionState::Stopped { .. }
        ));
    }

    #[tokio::test]
    async fn continuation_repeats_the_escalated_cap() {
        let mut mw = Run::default();
        let runtime = profile_for(32_000, 128_000);
        // Spend the escalation, then hit the ceiling and stop.
        after_llm(&mut mw, FinishReason::Length, &runtime).await;
        let at_ceiling = GenerationOptions::NONE.with_max_tokens(128_000);

        let context =
            recovered(stop_with(&mut mw, &runtime, at_ceiling, StopType::ModelTokenLimit).await);

        assert_eq!(context.request_overrides.max_tokens, Some(128_000));
    }

    #[tokio::test]
    async fn thinking_only_cutoff_lowers_effort_when_the_provider_honours_it() {
        let mut mw = Run::default();
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
        mw.on_after_llm(thinking_only).await.unwrap();

        let context = recovered(stop(&mut mw, &with_effort, StopType::ModelTokenLimit).await);
        assert_eq!(
            context.request_overrides.reasoning_effort,
            Some(ReasoningEffort::Medium)
        );

        // A provider that ignores effort gets no override.
        let mut unsupported = (*with_effort).clone();
        unsupported.capabilities.options.reasoning_effort = false;
        let context =
            recovered(stop(&mut mw, &Arc::new(unsupported), StopType::ModelTokenLimit).await);
        assert_eq!(context.request_overrides.reasoning_effort, None);
    }

    #[tokio::test]
    async fn mixed_cutoff_keeps_the_configured_effort() {
        let mut mw = Run::default();
        let runtime = profile_for(128_000, 128_000);
        // `reply` carries text, so it is not thinking-only.
        after_llm(&mut mw, FinishReason::Length, &runtime).await;

        let context = recovered(stop(&mut mw, &runtime, StopType::ModelTokenLimit).await);

        assert_eq!(context.request_overrides.reasoning_effort, None);
    }

    #[tokio::test]
    async fn instruction_stays_in_history_after_the_next_normal_reply() {
        let mut mw = Run::default();
        let runtime = profile_for(128_000, 128_000);
        let with_notice = recovered(stop(&mut mw, &runtime, StopType::ModelTokenLimit).await);
        // `stop` used session "s", the same one `after_llm` below replies for.
        let state = ExecutionState::AfterLlm {
            response: reply(FinishReason::Stop),
            context: with_notice.clone(),
        };

        let state = mw.on_after_llm(state).await.unwrap();

        let ExecutionState::AfterLlm { context, .. } = state else {
            panic!("expected AfterLlm, got {}", state.name());
        };
        // Nothing removes or rewrites the notice, and no second copy appears.
        assert_eq!(context.pending_notices, with_notice.pending_notices);
        assert_eq!(context.messages.len(), with_notice.messages.len());
    }

    #[tokio::test]
    async fn continuations_are_capped_per_turn() {
        let mut mw = Run::new(OutputLimitRecoveryMiddleware::new(OutputLimitConfig {
            max_continuations: 2,
            ..Default::default()
        }));
        let runtime = profile_for(128_000, 128_000);
        for _ in 0..2 {
            recovered(stop(&mut mw, &runtime, StopType::ModelTokenLimit).await);
        }

        let state = stop(&mut mw, &runtime, StopType::ModelTokenLimit).await;

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
        rx: &mut tokio::sync::broadcast::Receiver<querymt_agent_core::events::EventEnvelope>,
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
        Run,
        tokio::sync::broadcast::Receiver<querymt_agent_core::events::EventEnvelope>,
        crate::test_utils::TestAgent,
    ) {
        let agent = crate::test_utils::TestAgent::new().await;
        let rx = agent.config.event_sink.fanout().subscribe();
        let mw = OutputLimitRecoveryMiddleware::default();
        mw.bind_agent(&agent.config.tool_registry, &agent.config.event_sink);
        (Run::new(mw), rx, agent)
    }

    #[tokio::test]
    async fn escalation_publishes_a_durable_recovery_event() {
        let (mut mw, mut rx, _agent) = bound_middleware().await;
        let runtime = profile_for(32_000, 128_000);
        let response = Arc::new(
            LlmResponse::new(String::new(), Vec::new(), None, Some(FinishReason::Length))
                .with_message_id("m-1".into()),
        );
        let state = ExecutionState::AfterLlm {
            response,
            context: context_with("s", runtime),
        };

        mw.on_after_llm(state).await.unwrap();

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
        let (mut mw, mut rx, _agent) = bound_middleware().await;
        let runtime = profile_for(128_000, 128_000);

        for expected in 1..=2 {
            recovered(stop(&mut mw, &runtime, StopType::ModelTokenLimit).await);
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
        let mut capped = Run::new(capped);
        stop(&mut capped, &runtime, StopType::ModelTokenLimit).await;
        after_llm(&mut capped, FinishReason::Stop, &runtime).await;

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
        let (mut mw, _rx, agent) = bound_middleware().await;
        let session_id = agent.create_session().await;
        let runtime = profile_for(128_000, 128_000);
        let stopped = ExecutionState::Stopped {
            message: "cut off".into(),
            stop_type: StopType::ModelTokenLimit,
            context: Some(context_with(&session_id, runtime)),
        };

        mw.on_stopped(stopped).await.unwrap();

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
        let mut mw = Run::default();
        let runtime = profile_for(128_000, 128_000);
        recovered(stop(&mut mw, &runtime, StopType::ModelTokenLimit).await);
    }

    #[tokio::test]
    async fn disabled_escalation_and_normal_replies_pass_through() {
        let runtime = profile_for(32_000, 128_000);
        let mut off = Run::new(OutputLimitRecoveryMiddleware::new(OutputLimitConfig {
            escalate: false,
            ..Default::default()
        }));
        assert!(matches!(
            after_llm(&mut off, FinishReason::Length, &runtime).await,
            ExecutionState::AfterLlm { .. }
        ));

        let mut on = Run::default();
        assert!(matches!(
            after_llm(&mut on, FinishReason::Stop, &runtime).await,
            ExecutionState::AfterLlm { .. }
        ));
    }
}

//! Host glue for native repository-knowledge delivery.
//!
//! Resolves the session's effective checkout through the host registry and runs
//! the in-process adapters from
//! [`crate::knowledge::repository::adapters`]. Everything here is additive and
//! fail-open: a disabled stage, an unbound checkout, a blocking-task failure or
//! any selector error yields no contribution and never an error for the turn or
//! the tool call.

use crate::agent::agent_config::AgentConfig;
use crate::agent::execution_context::ExecutionContext;
use crate::hooks::HookContextContribution;
use crate::knowledge::repository::adapters::{
    IndexCall, PromptInput, orientation_contributions, post_index_contributions,
};
use serde_json::Value;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Longest automatic delivery may delay a prompt or a tool result. Selection is
/// local and normally takes milliseconds; a cold index or a busy refresh that
/// exceeds this skips the context instead of delaying the turn. Abandoned
/// blocking work finishes in the background and never touches the result.
pub(crate) const DELIVERY_TIMEOUT: Duration = Duration::from_millis(1500);
/// Minimum spacing between host diagnostics about skipped delivery.
const DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(30);

static LAST_DIAGNOSTIC: Mutex<Option<Instant>> = Mutex::new(None);

/// Log a skipped delivery at most once per interval so a persistently
/// unavailable index cannot flood the log on every tool call.
fn diagnose(reason: &str) {
    let mut last = LAST_DIAGNOSTIC
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    if last.is_some_and(|at| now.duration_since(at) < DIAGNOSTIC_INTERVAL) {
        tracing::debug!("repository knowledge delivery skipped: {reason}");
        return;
    }
    *last = Some(now);
    tracing::warn!("repository knowledge delivery skipped: {reason}");
}

const GET_TOOL: &str = "repo_knowledge_get";

/// Whether cards may name `repo_knowledge_get`: the tool is registered for
/// this agent and allowed by the session's tool policy. Otherwise cards keep
/// navigable relative citations without suggesting an unavailable call.
fn fetch_available(config: &AgentConfig, exec_ctx: &ExecutionContext) -> bool {
    config.tool_registry.find(GET_TOOL).is_some()
        && crate::agent::tools::is_tool_allowed_with(&exec_ctx.tool_config, GET_TOOL)
}

/// Whether the stable retrieval policy applies: repository knowledge is
/// enabled and both `repo_knowledge_query` and `repo_knowledge_get` are
/// registered and allowed by the tool policy. Explicit allowlists stay
/// authoritative; this never grants a tool.
pub(crate) fn policy_applies(config: &AgentConfig) -> bool {
    config.repository_knowledge().is_some()
        && crate::knowledge::repository::policy::should_attach(|name| {
            config.tool_registry.find(name).is_some()
                && crate::agent::tools::is_tool_allowed_with(&config.tool_config, name)
        })
}

/// Attach the one stable retrieval policy to a newly created session's stored
/// system prompt. Called once at creation, never on load/resume or per turn,
/// so existing prefixes are untouched and parts cannot accumulate. When the
/// feature is enabled but the retrieval tools are excluded, no instruction is
/// added and the mismatch is reported to the host log. Failures are logged and
/// leave the session as it was.
pub(crate) async fn attach_policy(config: &AgentConfig, session_id: &str) {
    let Some(_) = config.repository_knowledge() else {
        return;
    };
    if !policy_applies(config) {
        tracing::warn!(
            session_id,
            "repository knowledge is enabled but repo_knowledge_query/get are not usable; \
             retrieval guidance omitted (tool allowlists are authoritative)"
        );
        return;
    }
    let store = config.provider.history_store();
    let current = match store.get_session_llm_config(session_id).await {
        Ok(Some(current)) => current,
        _ => return,
    };
    let mut params = current
        .params
        .as_ref()
        .and_then(|json| serde_json::from_value::<querymt::LLMParams>(json.clone()).ok())
        .unwrap_or_default()
        .provider(&current.provider)
        .model(&current.model);
    if !crate::knowledge::repository::policy::append_once(&mut params.system) {
        return;
    }
    let result = async {
        let updated = store.create_or_get_llm_config(&params).await?;
        store.set_session_llm_config(session_id, updated.id).await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(session_id, %error, "failed to attach repository retrieval policy");
    }
}

/// Rebuild the turn budget's duplicate-suppression state from persisted card
/// provenance: cards still in the effective history (after the last
/// compaction) count as represented, and the compaction epoch is the number of
/// compactions so far. Nothing here depends on a diagnostics cache, so losing
/// one never causes reinjection. A history failure leaves the state as it was.
pub(crate) async fn restore_state(config: &AgentConfig, exec_ctx: &ExecutionContext) {
    use crate::model::MessagePart;
    let Some(registry) = config.repository_knowledge() else {
        return;
    };
    if !registry.settings().delivery.any_enabled() {
        return;
    }
    let Ok(Ok(history)) = tokio::time::timeout(
        DELIVERY_TIMEOUT,
        exec_ctx.state.store.get_history(&exec_ctx.session_id),
    )
    .await
    else {
        return;
    };
    let epoch = history
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter(|p| matches!(p, MessagePart::CompactionRequest { .. }))
        .count() as u64;
    let effective = crate::session::compaction::filter_to_effective_history(history);
    let contents: Vec<&str> = effective
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            MessagePart::HookContext { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    let mut guard = exec_ctx
        .delivery_budget
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .get_or_insert_with(|| {
            crate::knowledge::repository::budget::DeliveryBudget::new(&registry.settings().delivery)
        })
        .restore(contents, epoch);
}

/// Charge native contributions against the turn's shared budget, in the given
/// order. Contributions that do not fit are dropped whole (and so never count
/// as delivered); foreign hook context passes through untouched.
pub(crate) fn allocate(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    contributions: Vec<HookContextContribution>,
) -> Vec<HookContextContribution> {
    let Some(registry) = config.repository_knowledge() else {
        return contributions;
    };
    let mut guard = exec_ctx
        .delivery_budget
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let budget = guard.get_or_insert_with(|| {
        crate::knowledge::repository::budget::DeliveryBudget::new(&registry.settings().delivery)
    });
    // Counts matched/delivered/suppressed per stage; no card text is kept.
    crate::knowledge::repository::diagnostics::allocate_recorded(
        budget,
        registry.diagnostics(),
        contributions,
    )
}

/// Time one selection pass for a stage in the registry's local diagnostics.
fn record_selection(
    config: &AgentConfig,
    stage: crate::knowledge::repository::DeliveryStage,
    started: Instant,
) {
    if let Some(registry) = config.repository_knowledge() {
        registry
            .diagnostics()
            .record_selection(stage, started.elapsed());
    }
}

/// Orientation cards for an accepted prompt, when the host enabled that stage.
pub(crate) async fn orientation_contexts(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    prompt: &str,
    existing: &[HookContextContribution],
) -> Vec<HookContextContribution> {
    let Some(registry) = config.repository_knowledge() else {
        return Vec::new();
    };
    if !registry.settings().orientation_active() {
        return Vec::new();
    }
    let max_cards = registry.settings().delivery.effective_max_cards();
    restore_state(config, exec_ctx).await;
    // First task of the session: nothing from the user is stored yet. A
    // history failure reads as "not first", the conservative answer.
    let deadline = tokio::time::Instant::now() + DELIVERY_TIMEOUT;
    let history = tokio::time::timeout_at(
        deadline,
        exec_ctx.state.store.get_history(&exec_ctx.session_id),
    )
    .await;
    let first_task = match history {
        Ok(Ok(history)) => !history
            .iter()
            .any(|message| message.role == querymt::chat::ChatRole::User),
        _ => false,
    };
    let fetch = fetch_available(config, exec_ctx);
    let cwd = exec_ctx.cwd().map(|path| path.to_path_buf());
    let prompt = prompt.to_string();
    let existing = existing.to_vec();
    let started = Instant::now();
    let selected = run_bounded(deadline, move || {
        let service = registry.service_for(cwd.as_deref()).ok()?;
        Some(orientation_contributions(
            &service,
            PromptInput {
                prompt: &prompt,
                first_task,
                max_cards,
                fetch_available: fetch,
            },
            &existing,
        ))
    })
    .await;
    record_selection(
        config,
        crate::knowledge::repository::DeliveryStage::Orientation,
        started,
    );
    // Orientation is charged first, ahead of any index card of the turn.
    allocate(config, exec_ctx, selected)
}

/// What the post-tool stage needs to know about one finished call.
pub(crate) struct FinishedCall<'a> {
    pub tool_name: &'a str,
    pub tool_source: &'a str,
    pub execution_is_error: bool,
    pub result_is_error: bool,
    pub output_suppressed: bool,
    pub tool_use_id: &'a str,
    pub args: &'a Value,
}

/// Knowledge-for-path context for a successful built-in `index` call, when the
/// host enabled that stage.
pub(crate) async fn index_contexts(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    call: &FinishedCall<'_>,
    existing: &[HookContextContribution],
) -> Vec<HookContextContribution> {
    let Some(registry) = config.repository_knowledge() else {
        return Vec::new();
    };
    if !registry.settings().index_context_active() {
        return Vec::new();
    }
    let max_cards = registry.settings().delivery.effective_max_cards();
    let fetch = fetch_available(config, exec_ctx);
    let cwd = exec_ctx.cwd().map(|path| path.to_path_buf());
    let tool_name = call.tool_name.to_string();
    let tool_source = call.tool_source.to_string();
    let tool_use_id = call.tool_use_id.to_string();
    let args = call.args.clone();
    let (execution_is_error, result_is_error, output_suppressed) = (
        call.execution_is_error,
        call.result_is_error,
        call.output_suppressed,
    );
    let existing = existing.to_vec();
    // Ineligible calls are rejected before the checkout is touched.
    let probe = IndexCall {
        tool_name: &tool_name,
        tool_source: &tool_source,
        execution_is_error,
        result_is_error,
        output_suppressed,
        tool_use_id: &tool_use_id,
        args: &args,
        cwd: cwd.as_deref(),
    };
    if !probe.eligible() {
        return Vec::new();
    }
    let deadline = tokio::time::Instant::now() + DELIVERY_TIMEOUT;
    let started = Instant::now();
    let selected = run_bounded(deadline, move || {
        let service = registry.service_for(cwd.as_deref()).ok()?;
        let call = IndexCall {
            tool_name: &tool_name,
            tool_source: &tool_source,
            execution_is_error,
            result_is_error,
            output_suppressed,
            tool_use_id: &tool_use_id,
            args: &args,
            cwd: cwd.as_deref(),
        };
        Some(post_index_contributions(
            &service, &call, max_cards, fetch, &existing,
        ))
    })
    .await;
    record_selection(
        config,
        crate::knowledge::repository::DeliveryStage::IndexContext,
        started,
    );
    selected
}

/// Most recently indexed files re-evaluated after a compaction.
const RECENT_PATHS: usize = 3;

/// Cards to re-establish after a compaction, already charged to the new
/// epoch's budget. Call after the compaction boundary is stored: the
/// effective history then holds exactly what survived, so a retained card is
/// never duplicated and dropped ones are re-selected from current sources
/// (never from their old text). `before` is the pre-compaction effective
/// history, used only to find the active task and recently indexed paths.
pub(crate) async fn post_compaction_contexts(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    before: &[crate::model::AgentMessage],
) -> Vec<HookContextContribution> {
    use crate::model::MessagePart;
    let Some(registry) = config.repository_knowledge() else {
        return Vec::new();
    };
    let settings = registry.settings();
    if !settings.delivery.any_enabled() {
        return Vec::new();
    }
    let prompt = before
        .iter()
        .rev()
        .filter(|m| m.role == querymt::chat::ChatRole::User)
        .flat_map(|m| m.parts.iter())
        .find_map(|part| match part {
            MessagePart::Prompt { blocks } => {
                let text = crate::agent::utils::format_prompt_user_text_only(blocks);
                (!text.trim().is_empty()).then_some(text)
            }
            _ => None,
        });
    let mut recent_args: Vec<Value> = Vec::new();
    if settings.index_context_active() {
        for part in before.iter().rev().flat_map(|m| m.parts.iter().rev()) {
            if let MessagePart::ToolResult {
                tool_name: Some(name),
                tool_arguments: Some(arguments),
                is_error: false,
                ..
            } = part
                && name == crate::knowledge::repository::adapters::INDEX_TOOL_NAME
                && let Ok(args) = serde_json::from_str::<Value>(arguments)
            {
                recent_args.push(args);
                if recent_args.len() >= RECENT_PATHS * 2 {
                    break;
                }
            }
        }
    }
    // The new epoch starts with an empty allowance and the surviving history.
    *exec_ctx
        .delivery_budget
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    restore_state(config, exec_ctx).await;
    let surviving: Vec<HookContextContribution> =
        match exec_ctx.state.store.get_history(&exec_ctx.session_id).await {
            Ok(history) => crate::session::compaction::filter_to_effective_history(history)
                .iter()
                .flat_map(|m| m.parts.iter())
                .filter_map(|p| match p {
                    MessagePart::HookContext {
                        event_name,
                        handler_id,
                        tool_use_id,
                        content,
                    } => Some(HookContextContribution {
                        event_name: event_name.clone(),
                        handler_id: handler_id.clone(),
                        tool_use_id: tool_use_id.clone(),
                        content: content.clone(),
                    }),
                    _ => None,
                })
                .collect(),
            Err(_) => Vec::new(),
        };

    let orientation = settings.orientation_active();
    let index = settings.index_context_active();
    let max_cards = settings.delivery.effective_max_cards();
    let fetch = fetch_available(config, exec_ctx);
    let cwd = exec_ctx.cwd().map(|path| path.to_path_buf());
    let deadline = tokio::time::Instant::now() + DELIVERY_TIMEOUT;
    let selected = run_bounded(deadline, move || {
        let service = registry.service_for(cwd.as_deref()).ok()?;
        let mut out = Vec::new();
        if orientation && let Some(prompt) = prompt.as_deref() {
            out.extend(
                crate::knowledge::repository::adapters::recent_task_contributions(
                    &service, prompt, max_cards, fetch, &surviving,
                ),
            );
        }
        if index {
            let mut existing = surviving.clone();
            existing.extend(out.iter().cloned());
            out.extend(
                crate::knowledge::repository::adapters::recent_path_contributions(
                    &service,
                    &recent_args,
                    cwd.as_deref(),
                    max_cards,
                    fetch,
                    &existing,
                ),
            );
        }
        Some(out)
    })
    .await;
    allocate(config, exec_ctx, selected)
}

/// Run selection off the executor and give up at `deadline`. Every failure
/// mode (unbound or unavailable checkout, selector error, panic, timeout)
/// yields no contribution; none can fail the turn or the tool call.
pub(crate) async fn run_bounded<F>(
    deadline: tokio::time::Instant,
    work: F,
) -> Vec<HookContextContribution>
where
    F: FnOnce() -> Option<Vec<HookContextContribution>> + Send + 'static,
{
    let task = tokio::task::spawn_blocking(work);
    match tokio::time::timeout_at(deadline, task).await {
        Ok(Ok(Some(contributions))) => contributions,
        Ok(Ok(None)) => {
            diagnose("repository knowledge is unavailable for this checkout");
            Vec::new()
        }
        Ok(Err(error)) => {
            diagnose(&format!("task failed: {error}"));
            Vec::new()
        }
        Err(_) => {
            diagnose("timed out");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> HookContextContribution {
        HookContextContribution {
            event_name: "post_tool_use".into(),
            handler_id: "h".into(),
            tool_use_id: None,
            content: "c".into(),
        }
    }

    #[tokio::test]
    async fn slow_selection_is_abandoned_at_the_deadline() {
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(30);
        let out = run_bounded(deadline, || {
            std::thread::sleep(Duration::from_millis(400));
            Some(vec![card()])
        })
        .await;
        assert!(out.is_empty(), "late work must not contribute");
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "the caller must not wait for the slow work"
        );
    }

    async fn session_system(
        config: &AgentConfig,
        session_id: &str,
    ) -> Vec<querymt::params::SystemPrompt> {
        let stored = config
            .provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
            .unwrap()
            .unwrap();
        stored
            .params
            .as_ref()
            .and_then(|p| p.get("system"))
            .and_then(|s| s.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| serde_json::from_value(i.clone()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn config_with(tools_excluded: bool, enabled: bool) -> (AgentConfig, tempfile::TempDir) {
        use crate::agent::agent_config_builder::AgentConfigBuilder;
        use crate::config::RepositoryKnowledgeSettings;
        use crate::knowledge::repository::RepositoryKnowledgeRegistry;
        let (plugins, dir) = crate::test_utils::helpers::empty_plugin_registry().unwrap();
        let storage = std::sync::Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let llm = querymt::LLMParams::new()
            .provider("mock")
            .model("mock-model")
            .system("base one")
            .system("base two");
        let mut builder = AgentConfigBuilder::new(std::sync::Arc::new(plugins), storage, llm);
        if enabled {
            builder = builder.with_repository_knowledge(
                RepositoryKnowledgeRegistry::from_settings(&RepositoryKnowledgeSettings::enabled()),
            );
        }
        let mut config = builder.build();
        if tools_excluded {
            config
                .tool_config
                .denylist
                .insert("repo_knowledge_get".to_string());
        }
        (config, dir)
    }

    #[tokio::test]
    async fn policy_is_attached_once_after_an_unchanged_prefix() {
        let (config, _dir) = config_with(false, true).await;
        let session = config
            .provider
            .create_session(None, None, &config.execution_config_snapshot())
            .await
            .unwrap();
        let id = session.session().public_id.clone();
        let before = session_system(&config, &id).await;
        assert_eq!(before.len(), 2);

        attach_policy(&config, &id).await;
        let after = session_system(&config, &id).await;
        assert_eq!(after.len(), 3);
        assert_eq!(&after[..2], &before[..], "existing prefix is unchanged");
        assert_eq!(
            after[2].text(),
            crate::knowledge::repository::policy::RETRIEVAL_POLICY
        );

        // Repeating (e.g. a second creation path) never accumulates parts.
        attach_policy(&config, &id).await;
        assert_eq!(session_system(&config, &id).await, after);
    }

    #[tokio::test]
    async fn no_policy_when_disabled_or_retrieval_tools_are_excluded() {
        for (excluded, enabled) in [(false, false), (true, true)] {
            let (config, _dir) = config_with(excluded, enabled).await;
            let session = config
                .provider
                .create_session(None, None, &config.execution_config_snapshot())
                .await
                .unwrap();
            let id = session.session().public_id.clone();
            let before = session_system(&config, &id).await;
            attach_policy(&config, &id).await;
            assert_eq!(
                session_system(&config, &id).await,
                before,
                "excluded={excluded} enabled={enabled}: system prompt must be untouched"
            );
        }
    }

    #[tokio::test]
    async fn timely_selection_is_delivered_and_failures_yield_nothing() {
        let deadline = || tokio::time::Instant::now() + Duration::from_secs(5);
        assert_eq!(
            run_bounded(deadline(), || Some(vec![card()])).await.len(),
            1
        );
        assert!(run_bounded(deadline(), || None).await.is_empty());
        assert!(
            run_bounded(deadline(), || -> Option<Vec<_>> { panic!("selector bug") })
                .await
                .is_empty()
        );
    }
}

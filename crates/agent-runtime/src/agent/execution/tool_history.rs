//! Tool baseline and tool-change history (design D21).
//!
//! For a provider that declares `tool_updates`, the offered tools are recorded
//! in session history: a *baseline* (the whole list) before the first request
//! of a session, after a compaction and after a model change, and a *change*
//! (definitions added or redefined, names removed) whenever the tools offered
//! now differ from the baseline folded with the changes stored after it.
//!
//! A request then starts from the most recent baseline's tool list, so a tool
//! that appears or disappears mid-session never edits the head of the request
//! and the provider's prompt cache stays valid. Baselines are never sent as
//! messages; changes after the latest baseline are. Other providers get the
//! tools offered now and none of these records; the stored records stay in
//! history and are left out of their requests.
//!
//! In a [`ChatMessage`] both records are a tool-change part
//! ([`ToolChange`], with `baseline` set for a baseline), so this module
//! reads them from the same history the request is built from.

use crate::agent::execution_context::ExecutionContext;
use crate::middleware::ConversationContext;
use querymt::chat::{ChatInputPart, ChatMessage, ChatRole, Tool, ToolChange};
use querymt_agent_core::model::{AgentMessage, MessagePart};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The tool list a baseline recorded and what it was built for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Baseline {
    pub tools: Vec<Tool>,
    pub origin: String,
}

/// A history read for one request.
#[derive(Debug)]
pub(crate) struct SplitHistory {
    /// The most recent baseline, if any.
    pub baseline: Option<Baseline>,
    /// The tools offered after the baseline and the changes stored after it,
    /// ordered by name. Empty when there is no baseline.
    pub offered: Vec<Tool>,
    /// The history as the provider sees it: no baseline, and no tool change
    /// stored before the most recent baseline.
    pub sent: Vec<ChatMessage>,
}

/// The tool-change record of a message made only of such records.
fn tool_record(message: &ChatMessage) -> Option<&ToolChange> {
    let parts = message.input()?;
    if parts.is_empty() || !parts.iter().all(ChatInputPart::is_tool_change) {
        return None;
    }
    parts.iter().find_map(ChatInputPart::as_tool_change)
}

/// The origin tag of a baseline for the session's provider and model.
pub(crate) fn origin(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// Reads the baseline, the folded offered tools and the messages to send.
pub(crate) fn split(messages: &[ChatMessage]) -> SplitHistory {
    let latest = messages
        .iter()
        .rposition(|message| tool_record(message).is_some_and(|record| record.baseline));

    let baseline = latest
        .and_then(|index| tool_record(&messages[index]))
        .map(|record| Baseline {
            tools: record.added.clone(),
            origin: record.origin.clone().unwrap_or_default(),
        });

    let mut offered: BTreeMap<String, Tool> = BTreeMap::new();
    if let Some(baseline) = &baseline {
        for tool in &baseline.tools {
            offered.insert(tool.function.name.clone(), tool.clone());
        }
    }

    let mut sent = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        match tool_record(message) {
            None => sent.push(message.clone()),
            Some(record) if record.baseline => {}
            // A change before the latest baseline is already part of it, and
            // a change with no baseline has nothing to change: neither is sent.
            Some(record) if latest.is_some_and(|baseline_index| index > baseline_index) => {
                for tool in &record.added {
                    offered.insert(tool.function.name.clone(), tool.clone());
                }
                for name in &record.removed {
                    offered.remove(name);
                }
                sent.push(message.clone());
            }
            Some(_) => {}
        }
    }

    SplitHistory {
        baseline,
        offered: offered.into_values().collect(),
        sent,
    }
}

/// The history without baselines and tool changes, for a provider that does
/// not declare `tool_updates`. The records stay in the stored history.
pub(crate) fn without_tool_records(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|message| tool_record(message).is_none())
        .cloned()
        .collect()
}

/// What the tools offered now add, redefine and remove compared with
/// `offered`; `None` when nothing differs. Both lists are in name order when
/// `current` and `offered` are.
pub(crate) fn diff(offered: &[Tool], current: &[Tool]) -> Option<ToolChange> {
    let before: BTreeMap<&str, &Tool> = offered
        .iter()
        .map(|tool| (tool.function.name.as_str(), tool))
        .collect();
    let added: Vec<Tool> = current
        .iter()
        .filter(|tool| before.get(tool.function.name.as_str()) != Some(tool))
        .cloned()
        .collect();
    let now: std::collections::BTreeSet<&str> = current
        .iter()
        .map(|tool| tool.function.name.as_str())
        .collect();
    let removed: Vec<String> = before
        .keys()
        .filter(|name| !now.contains(**name))
        .map(|name| name.to_string())
        .collect();
    let change = ToolChange::new(added, removed);
    (!change.is_empty()).then_some(change)
}

/// What a model request is built from once the tool history is resolved.
pub(crate) struct ToolRequestState {
    /// The conversation, including any record stored for this request.
    pub context: Arc<ConversationContext>,
    /// The messages to send (see [`SplitHistory::sent`]).
    pub messages: Vec<ChatMessage>,
    /// The request's tool list.
    pub leading: Vec<Tool>,
}

/// Stores `part` as one user message after the conversation so far and
/// returns the context that includes it.
async fn store_record(
    exec_ctx: &ExecutionContext,
    context: &Arc<ConversationContext>,
    part: MessagePart,
) -> Result<Arc<ConversationContext>, anyhow::Error> {
    let mut message = AgentMessage::new(exec_ctx.session_id.clone(), ChatRole::User);
    message.parts.push(part);
    let chat_message = message
        .to_chat_message()
        .map_err(|error| anyhow::anyhow!("Invalid tool record: {error}"))?;
    exec_ctx.add_message(message).await?;

    let mut messages = context.messages.to_vec();
    messages.push(chat_message);
    let mut next = (**context).clone();
    next.messages = Arc::from(messages.into_boxed_slice());
    Ok(Arc::new(next))
}

/// Resolves the tool history for the next request over `context`.
///
/// `tools` are the tools offered now, ordered by name. With
/// `tool_updates`, stores a baseline when the history has none for this
/// provider and model, and a change when the offered tools differ from the
/// stored state; the record follows the conversation so far, so it comes after
/// any tool results. Nothing is stored while a tool call awaits its result, and
/// a change only after a user message. Without `tool_updates`, nothing is
/// stored.
pub(crate) async fn resolve(
    exec_ctx: &ExecutionContext,
    context: &Arc<ConversationContext>,
    tool_updates: bool,
    tools: &[Tool],
) -> Result<ToolRequestState, anyhow::Error> {
    if !tool_updates {
        return Ok(ToolRequestState {
            context: context.clone(),
            messages: without_tool_records(&context.messages),
            leading: tools.to_vec(),
        });
    }

    // The newest message that is not a tool record decides where a record may
    // go. A record is never stored between a tool call and its result: a
    // history reload moves the results into the first user message after the
    // call, which would then be the record. A call without a result is an
    // upstream fault that is answered elsewhere, so the request is built like
    // one for a provider without `tool_updates` and nothing is stored yet.
    let last_real = context
        .messages
        .iter()
        .rfind(|message| tool_record(message).is_none());
    if last_real.is_some_and(ChatMessage::has_tool_use) {
        return Ok(ToolRequestState {
            context: context.clone(),
            messages: without_tool_records(&context.messages),
            leading: tools.to_vec(),
        });
    }
    // The API accepts a tool change only directly after a user message, so one
    // waits for a request that follows a user message (a prompt or results).
    let change_allowed = last_real.is_some_and(|message| message.role == ChatRole::User);

    let wanted = origin(&context.provider, &context.model);
    let mut context = context.clone();
    let state = split(&context.messages);

    let needs_baseline = state
        .baseline
        .as_ref()
        .is_none_or(|baseline| baseline.origin != wanted);
    let offered = if needs_baseline {
        context = store_record(
            exec_ctx,
            &context,
            MessagePart::ToolBaseline {
                tools: tools.to_vec(),
                origin: wanted,
            },
        )
        .await?;
        tools.to_vec()
    } else {
        state.offered
    };

    if change_allowed && let Some(change) = diff(&offered, tools) {
        context = store_record(
            exec_ctx,
            &context,
            MessagePart::ToolChange {
                added: change.added,
                removed: change.removed,
            },
        )
        .await?;
    }

    let split = split(&context.messages);
    let leading = split
        .baseline
        .map(|baseline| baseline.tools)
        .unwrap_or_else(|| tools.to_vec());
    Ok(ToolRequestState {
        context,
        messages: split.sent,
        leading,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str) -> Tool {
        Tool {
            tool_type: "function".into(),
            function: querymt::chat::FunctionTool {
                name: name.into(),
                description: description.into(),
                parameters: serde_json::json!({"type": "object"}),
                strict: None,
            },
        }
    }

    fn user(text: &str) -> ChatMessage {
        ChatMessage::user().text(text).build()
    }

    fn baseline(tools: Vec<Tool>, origin: &str) -> ChatMessage {
        ChatMessage::from_tool_change(ToolChange::baseline(tools, origin))
    }

    fn change(added: Vec<Tool>, removed: &[&str]) -> ChatMessage {
        ChatMessage::from_tool_change(ToolChange::new(
            added,
            removed.iter().map(|name| name.to_string()).collect(),
        ))
    }

    fn names(tools: &[Tool]) -> Vec<&str> {
        tools.iter().map(|t| t.function.name.as_str()).collect()
    }

    #[test]
    fn history_without_records_is_sent_unchanged() {
        let messages = vec![user("a"), user("b")];
        let state = split(&messages);
        assert!(state.baseline.is_none() && state.offered.is_empty());
        assert_eq!(state.sent, messages);
    }

    #[test]
    fn baseline_is_not_sent_and_provides_the_tool_list() {
        let messages = vec![
            user("go"),
            baseline(vec![tool("a", "A"), tool("b", "B")], "p/m"),
        ];
        let state = split(&messages);
        assert_eq!(state.sent, vec![user("go")]);
        let baseline = state.baseline.expect("baseline");
        assert_eq!(names(&baseline.tools), ["a", "b"]);
        assert_eq!(baseline.origin, "p/m");
        assert_eq!(names(&state.offered), ["a", "b"]);
    }

    #[test]
    fn changes_after_the_baseline_are_sent_and_folded() {
        let messages = vec![
            user("go"),
            baseline(vec![tool("a", "A"), tool("b", "B")], "p/m"),
            change(vec![tool("c", "C"), tool("a", "A2")], &["b"]),
        ];
        let state = split(&messages);
        assert_eq!(state.sent.len(), 2, "the change is sent, the baseline not");
        assert!(state.sent[1].has_tool_change());
        assert_eq!(names(&state.offered), ["a", "c"]);
        let a = state
            .offered
            .iter()
            .find(|t| t.function.name == "a")
            .unwrap();
        assert_eq!(a.function.description, "A2");
        // The leading list is the baseline's, not the folded one.
        assert_eq!(names(&state.baseline.unwrap().tools), ["a", "b"]);
    }

    #[test]
    fn changes_before_the_latest_baseline_are_not_sent() {
        let messages = vec![
            user("go"),
            baseline(vec![tool("a", "A")], "p/m"),
            change(vec![tool("deploy", "D")], &[]),
            user("more"),
            baseline(vec![tool("a", "A"), tool("deploy", "D")], "q/n"),
        ];
        let state = split(&messages);
        assert_eq!(state.sent, vec![user("go"), user("more")]);
        assert_eq!(state.baseline.unwrap().origin, "q/n");
        assert_eq!(names(&state.offered), ["a", "deploy"]);
    }

    #[test]
    fn change_with_no_baseline_is_not_sent() {
        let messages = vec![user("go"), change(vec![tool("a", "A")], &[])];
        let state = split(&messages);
        assert_eq!(state.sent, vec![user("go")]);
        assert!(state.baseline.is_none());
    }

    #[test]
    fn records_are_left_out_for_a_provider_without_tool_updates() {
        let messages = vec![
            user("go"),
            baseline(vec![tool("a", "A")], "p/m"),
            change(vec![tool("b", "B")], &[]),
            user("more"),
        ];
        assert_eq!(
            without_tool_records(&messages),
            vec![user("go"), user("more")]
        );
    }

    #[test]
    fn diff_reports_added_redefined_and_removed_in_name_order() {
        let offered = vec![tool("a", "A"), tool("b", "B"), tool("c", "C")];
        let current = vec![
            tool("a", "A"),
            tool("b", "B2"),
            tool("d", "D"),
            tool("e", "E"),
        ];
        let change = diff(&offered, &current).expect("difference");
        assert_eq!(names(&change.added), ["b", "d", "e"]);
        assert_eq!(change.removed, vec!["c".to_string()]);
        assert!(!change.baseline);
    }

    #[test]
    fn diff_of_equal_lists_is_none() {
        let tools = vec![tool("a", "A"), tool("b", "B")];
        assert!(diff(&tools, &tools).is_none());
        assert!(diff(&[], &[]).is_none());
    }

    // ---- resolve: the stored flow ----

    use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};

    async fn agent() -> (TestAgent, ExecutionContext) {
        let agent =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let exec = agent.execution_context().await;
        (agent, exec)
    }

    fn context_for(
        exec: &ExecutionContext,
        provider: &str,
        model: &str,
        messages: Vec<ChatMessage>,
    ) -> Arc<ConversationContext> {
        let mut context = (*crate::test_utils::test_context(&exec.session_id, 0)).clone();
        context.provider = Arc::from(provider);
        context.model = Arc::from(model);
        context.messages = Arc::from(messages.into_boxed_slice());
        Arc::new(context)
    }

    /// Kinds of the stored tool records, in order: `baseline` or `change`.
    async fn stored_records(agent: &TestAgent, exec: &ExecutionContext) -> Vec<&'static str> {
        agent
            .config
            .provider
            .history_store()
            .get_history(&exec.session_id)
            .await
            .unwrap()
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                MessagePart::ToolBaseline { .. } => Some("baseline"),
                MessagePart::ToolChange { .. } => Some("change"),
                _ => None,
            })
            .collect()
    }

    fn tools_ab() -> Vec<Tool> {
        vec![tool("a", "A"), tool("b", "B")]
    }

    #[tokio::test]
    async fn first_request_stores_a_baseline_and_sends_the_tools_as_the_list() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);

        let state = resolve(&exec, &context, true, &tools_ab()).await.unwrap();

        assert_eq!(stored_records(&agent, &exec).await, ["baseline"]);
        assert_eq!(names(&state.leading), ["a", "b"]);
        assert_eq!(state.messages, vec![user("go")], "the baseline is not sent");
        assert_eq!(
            state.context.messages.len(),
            2,
            "the context holds the record"
        );
    }

    #[tokio::test]
    async fn unchanged_tools_store_nothing() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();

        let second = resolve(&exec, &first.context, true, &tools_ab())
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &exec).await, ["baseline"]);
        assert_eq!(second.messages, first.messages);
        assert_eq!(second.leading, first.leading);
    }

    #[tokio::test]
    async fn tool_added_between_requests_is_a_change_and_the_list_stays() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();

        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let second = resolve(&exec, &first.context, true, &now).await.unwrap();

        assert_eq!(stored_records(&agent, &exec).await, ["baseline", "change"]);
        assert_eq!(second.leading, first.leading, "the list does not change");
        let change = second.messages.last().unwrap().input().unwrap()[0]
            .as_tool_change()
            .expect("change is sent");
        assert_eq!(names(&change.added), ["deploy"]);
        assert!(change.removed.is_empty() && !change.baseline);
    }

    #[tokio::test]
    async fn tool_removed_between_requests_is_a_change() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();

        let second = resolve(&exec, &first.context, true, &[tool("a", "A")])
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &exec).await, ["baseline", "change"]);
        assert_eq!(second.leading, first.leading);
        let change = second.messages.last().unwrap().input().unwrap()[0]
            .as_tool_change()
            .unwrap();
        assert!(change.added.is_empty());
        assert_eq!(change.removed, vec!["b".to_string()]);
    }

    #[tokio::test]
    async fn changed_definition_is_a_change_and_is_then_remembered() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &[tool("skill", "old")])
            .await
            .unwrap();

        let redefined = [tool("skill", "new")];
        let second = resolve(&exec, &first.context, true, &redefined)
            .await
            .unwrap();
        let again = resolve(&exec, &second.context, true, &redefined)
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &exec).await, ["baseline", "change"]);
        assert_eq!(second.leading, first.leading);
        assert_eq!(again.messages, second.messages, "stored once");
    }

    #[tokio::test]
    async fn tool_change_follows_the_tool_results() {
        let (agent, exec) = agent().await;
        let results = ChatMessage::user()
            .tool_result(
                "toolu_1".into(),
                Some("a".into()),
                false,
                vec![querymt::chat::ToolResultPart::Text { text: "ok".into() }],
            )
            .build();
        let messages = vec![
            user("go"),
            ChatMessage::assistant()
                .tool_use("toolu_1", "a", serde_json::json!({}))
                .build(),
            results.clone(),
        ];
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", messages);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();

        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let second = resolve(&exec, &first.context, true, &now).await.unwrap();

        let sent = &second.messages;
        assert!(sent[sent.len() - 2].has_tool_result());
        assert!(sent[sent.len() - 1].has_tool_change());
        assert_eq!(stored_records(&agent, &exec).await, ["baseline", "change"]);
    }

    #[tokio::test]
    async fn nothing_is_stored_while_a_tool_call_awaits_its_result() {
        let (agent, exec) = agent().await;
        let messages = vec![
            user("go"),
            ChatMessage::assistant()
                .tool_use("toolu_1", "a", serde_json::json!({}))
                .build(),
        ];
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", messages);
        let first = resolve(&exec, &context, true, &[tool("a", "A")])
            .await
            .unwrap();
        let second = resolve(&exec, &first.context, true, &tools_ab())
            .await
            .unwrap();

        assert!(
            stored_records(&agent, &exec).await.is_empty(),
            "no baseline or change between call and result"
        );
        assert_eq!(second.messages.len(), 2);
        assert_eq!(names(&second.leading), ["a", "b"], "the current list");
    }

    #[tokio::test]
    async fn baseline_is_stored_once_the_result_has_arrived() {
        let (agent, exec) = agent().await;
        let call = ChatMessage::assistant()
            .tool_use("toolu_1", "a", serde_json::json!({}))
            .build();
        let pending = context_for(
            &exec,
            "anthropic",
            "claude-sonnet-5-5",
            vec![user("go"), call.clone()],
        );
        resolve(&exec, &pending, true, &tools_ab()).await.unwrap();
        assert!(stored_records(&agent, &exec).await.is_empty());

        let result = ChatMessage::user()
            .tool_result(
                "toolu_1".into(),
                Some("a".into()),
                false,
                vec![querymt::chat::ToolResultPart::Text { text: "ok".into() }],
            )
            .build();
        let answered = context_for(
            &exec,
            "anthropic",
            "claude-sonnet-5-5",
            vec![user("go"), call, result],
        );
        let state = resolve(&exec, &answered, true, &tools_ab()).await.unwrap();

        assert_eq!(stored_records(&agent, &exec).await, ["baseline"]);
        assert!(state.messages.last().unwrap().has_tool_result());
    }

    #[tokio::test]
    async fn change_waits_while_the_newest_message_is_an_assistant_reply() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();

        // The newest message is a reply with no tool call.
        let mut messages = first.context.messages.to_vec();
        messages.push(ChatMessage::assistant().text("done").build());
        let replied = context_for(&exec, "anthropic", "claude-sonnet-5-5", messages);
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let waiting = resolve(&exec, &replied, true, &now).await.unwrap();

        assert_eq!(stored_records(&agent, &exec).await, ["baseline"]);
        assert!(!waiting.messages.iter().any(ChatMessage::has_tool_change));

        // The next user message lets the change follow it.
        let mut messages = waiting.context.messages.to_vec();
        messages.push(user("next"));
        let prompted = context_for(&exec, "anthropic", "claude-sonnet-5-5", messages);
        let after = resolve(&exec, &prompted, true, &now).await.unwrap();

        assert_eq!(stored_records(&agent, &exec).await, ["baseline", "change"]);
        assert!(after.messages.last().unwrap().has_tool_change());
        assert_eq!(after.leading, first.leading);
    }

    #[tokio::test]
    async fn baseline_is_renewed_after_a_model_switch_and_old_changes_are_not_sent() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let second = resolve(&exec, &first.context, true, &now).await.unwrap();
        assert!(second.messages.last().unwrap().has_tool_change());

        let mut switched = (*second.context).clone();
        switched.model = Arc::from("claude-opus-4-8");
        let third = resolve(&exec, &Arc::new(switched), true, &now)
            .await
            .unwrap();

        assert_eq!(
            stored_records(&agent, &exec).await,
            ["baseline", "change", "baseline"]
        );
        assert_eq!(names(&third.leading), ["a", "b", "deploy"]);
        assert!(
            !third.messages.iter().any(ChatMessage::has_tool_change),
            "the earlier change is part of the new baseline"
        );
    }

    #[tokio::test]
    async fn baseline_is_renewed_after_compaction() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let second = resolve(&exec, &first.context, true, &now).await.unwrap();
        assert!(second.messages.last().unwrap().has_tool_change());

        // After compaction the effective history starts from the summary and
        // holds neither the baseline nor the change.
        let compacted = context_for(
            &exec,
            "anthropic",
            "claude-sonnet-5-5",
            vec![user("summary"), user("next")],
        );
        let third = resolve(&exec, &compacted, true, &now).await.unwrap();

        assert_eq!(
            stored_records(&agent, &exec).await,
            ["baseline", "change", "baseline"]
        );
        assert_eq!(names(&third.leading), ["a", "b", "deploy"]);
        assert!(!third.messages.iter().any(ChatMessage::has_tool_change));
        assert_eq!(third.messages.len(), 2);
    }

    #[tokio::test]
    async fn tool_list_and_change_survive_a_restart() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        // The user prompt must be stored too for the history to reload.
        let mut prompt = AgentMessage::new(exec.session_id.clone(), ChatRole::User);
        prompt.parts.push(MessagePart::Text {
            content: "go".into(),
        });
        exec.add_message(prompt).await.unwrap();
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let second = resolve(&exec, &first.context, true, &now).await.unwrap();

        // A new process reads the history back from the store.
        let (history, _, _, _) = exec
            .session_handle
            .history_with_notice_state()
            .await
            .unwrap();
        let reloaded = context_for(&exec, "anthropic", "claude-sonnet-5-5", history);
        let after = resolve(&exec, &reloaded, true, &now).await.unwrap();

        assert_eq!(after.leading, second.leading);
        assert_eq!(after.messages.len(), second.messages.len());
        assert!(after.messages.last().unwrap().has_tool_change());
        assert_eq!(stored_records(&agent, &exec).await, ["baseline", "change"]);
    }

    /// The prefix fingerprint of the request `state` describes.
    fn fingerprint(state: &ToolRequestState) -> crate::agent::execution::prefix::PrefixFingerprint {
        let leading = serde_json::to_vec(&state.leading).unwrap();
        crate::agent::execution::prefix::PrefixFingerprint::new(
            &Arc::from("anthropic"),
            &Arc::from("claude-sonnet-5-5"),
            querymt_agent_core::hash::RapidHash::new(&leading),
            querymt_agent_core::hash::RapidHash::new(b"system"),
            &state.messages,
        )
    }

    #[tokio::test]
    async fn tool_change_sent_as_a_message_reports_no_prefix_change() {
        use crate::agent::execution::prefix::{CachePrefixState, PrefixChange};
        let (_agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();
        let mut state = CachePrefixState::default();
        let started = std::time::Instant::now();
        state.begin(fingerprint(&first), started);
        state.commit(started);

        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let second = resolve(&exec, &first.context, true, &now).await.unwrap();

        assert_eq!(
            state.begin(fingerprint(&second), started).change,
            PrefixChange::Unchanged,
            "the leading list is the baseline's and the change is appended"
        );
    }

    #[tokio::test]
    async fn tool_list_after_undo_is_stored_again() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let second = resolve(&exec, &first.context, true, &now).await.unwrap();
        assert!(second.messages.last().unwrap().has_tool_change());

        // Undo removed the change message but `deploy` is still offered.
        let undone = context_for(
            &exec,
            "anthropic",
            "claude-sonnet-5-5",
            first.context.messages.to_vec(),
        );
        let third = resolve(&exec, &undone, true, &now).await.unwrap();

        let change = third.messages.last().unwrap().input().unwrap()[0]
            .as_tool_change()
            .expect("a new change is sent");
        assert_eq!(names(&change.added), ["deploy"]);
        assert_eq!(
            stored_records(&agent, &exec).await,
            ["baseline", "change", "change"]
        );
    }

    #[tokio::test]
    async fn provider_without_tool_updates_gets_the_current_list_and_stores_nothing() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "openai", "gpt-5", vec![user("go")]);

        let first = resolve(&exec, &context, false, &tools_ab()).await.unwrap();
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let second = resolve(&exec, &first.context, false, &now).await.unwrap();

        assert!(stored_records(&agent, &exec).await.is_empty());
        assert_eq!(names(&second.leading), ["a", "b", "deploy"]);
        assert_eq!(second.messages, vec![user("go")]);
    }

    #[tokio::test]
    async fn switching_to_a_provider_without_tool_updates_leaves_the_records_out() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let second = resolve(&exec, &first.context, true, &now).await.unwrap();
        assert!(second.messages.last().unwrap().has_tool_change());

        let mut switched = (*second.context).clone();
        switched.provider = Arc::from("openai");
        switched.model = Arc::from("gpt-5");
        let third = resolve(&exec, &Arc::new(switched), false, &now)
            .await
            .unwrap();

        assert_eq!(third.messages, vec![user("go")]);
        assert_eq!(names(&third.leading), ["a", "b", "deploy"]);
        assert_eq!(
            stored_records(&agent, &exec).await,
            ["baseline", "change"],
            "the stored records stay in history"
        );
    }

    #[tokio::test]
    async fn switching_away_and_back_reuses_the_baseline_built_for_that_model() {
        let (agent, exec) = agent().await;
        let context = context_for(&exec, "anthropic", "claude-sonnet-5-5", vec![user("go")]);
        let first = resolve(&exec, &context, true, &tools_ab()).await.unwrap();

        let mut away = (*first.context).clone();
        away.provider = Arc::from("openai");
        away.model = Arc::from("gpt-5");
        let away = resolve(&exec, &Arc::new(away), false, &tools_ab())
            .await
            .unwrap();

        let mut back = (*away.context).clone();
        back.provider = Arc::from("anthropic");
        back.model = Arc::from("claude-sonnet-5-5");
        let back = resolve(&exec, &Arc::new(back), true, &tools_ab())
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &exec).await, ["baseline"]);
        assert_eq!(back.leading, first.leading);
    }

    #[test]
    fn user_message_with_text_is_not_a_record() {
        let mut message = user("text");
        message.push_input_part(ChatInputPart::tool_change(ToolChange::new(
            vec![tool("a", "A")],
            Vec::new(),
        )));
        // A message that mixes text and a change is left alone.
        assert!(tool_record(&message).is_none());
        assert_eq!(without_tool_records(&[message]).len(), 1);
    }
}

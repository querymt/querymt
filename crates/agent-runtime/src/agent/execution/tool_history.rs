//! Tool baseline and tool-change history (design D21).
//!
//! For a provider that declares `tool_updates`, the offered tools are recorded
//! in session history: a *baseline* (the whole list) before the first request
//! of a session, after a compaction and after a model change, and a *change*
//! (definitions added or redefined, names removed) whenever the tools offered
//! now differ from the baseline folded with the changes stored after it.
//!
//! A request then starts from the baseline's tool list, so a tool that appears
//! or disappears mid-session never edits the head of the request and the
//! provider's prompt cache stays valid. Other providers get the tools offered
//! now and none of these records; the stored records stay in history and are
//! left out of their requests.
//!
//! The two records differ in kind. A change is conversation content: it is a
//! [`ChatMessage`] made of one tool-change part, sent at its position. A
//! baseline is a request's tool list and has no chat form: the run reads the
//! one in effect from the stored history when it loads it
//! ([`ConversationContext::tool_baseline`]), and the messages it holds contain
//! only the tool changes stored after that baseline.

use crate::agent::execution::turn::TurnEnv;
use crate::middleware::ConversationContext;
use querymt::chat::{ChatInputPart, ChatMessage, ChatRole, Tool, ToolChange};
use querymt_agent_core::model::{AgentMessage, MessagePart, ToolBaseline};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Whether `message` is a tool-change message: made only of tool-change
/// parts. A message that mixes one with other content is not; it is left in
/// place for the provider to refuse.
pub(crate) fn is_tool_change(message: &ChatMessage) -> bool {
    message
        .input()
        .is_some_and(|parts| !parts.is_empty() && parts.iter().all(ChatInputPart::is_tool_change))
}

/// The tool changes of the tool-change messages in `messages`, in order.
fn tool_changes(messages: &[ChatMessage]) -> impl Iterator<Item = &ToolChange> {
    messages
        .iter()
        .filter(|message| is_tool_change(message))
        .filter_map(ChatMessage::input)
        .flatten()
        .filter_map(ChatInputPart::as_tool_change)
}

/// The origin tag of a baseline for the session's provider and model.
pub(crate) fn origin(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// The tools the model can call after `baseline` and the tool changes in
/// `messages`, ordered by name: each change applied in order to the list.
pub(crate) fn offered(baseline: &[Tool], messages: &[ChatMessage]) -> Vec<Tool> {
    let mut offered: BTreeMap<&str, &Tool> = baseline
        .iter()
        .map(|tool| (tool.function.name.as_str(), tool))
        .collect();
    for change in tool_changes(messages) {
        for tool in &change.added {
            offered.insert(tool.function.name.as_str(), tool);
        }
        for name in &change.removed {
            offered.remove(name.as_str());
        }
    }
    offered.into_values().cloned().collect()
}

/// The history without tool-change messages: what a provider that does not
/// declare `tool_updates` is sent, and what remains once a new baseline has
/// taken the earlier changes in. The records stay in the stored history.
pub(crate) fn without_tool_changes(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|message| !is_tool_change(message))
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
    /// The messages to send: the conversation, with its tool-change messages
    /// only for a provider that takes them.
    pub messages: Vec<ChatMessage>,
    /// The request's tool list: the baseline's, or the tools offered now for
    /// a provider without `tool_updates`.
    pub leading: Vec<Tool>,
    /// The tools the model can call in this request: `leading` with the tool
    /// changes in `messages` applied.
    pub offered: Vec<Tool>,
}

/// Stores `baseline` after the conversation so far and returns the context it
/// is in effect for. The tool changes stored before it are part of its list,
/// so they leave the conversation; a later load of the history drops them the
/// same way.
async fn store_baseline(
    env: &TurnEnv,
    context: &Arc<ConversationContext>,
    baseline: Arc<ToolBaseline>,
) -> Result<Arc<ConversationContext>, anyhow::Error> {
    let mut message = AgentMessage::new(env.session_id.clone(), ChatRole::User);
    message.parts.push(MessagePart::ToolBaseline {
        tools: baseline.tools.clone(),
        origin: baseline.origin.clone(),
    });
    env.add_message(message).await?;

    let mut next = (**context).clone();
    if context.messages.iter().any(is_tool_change) {
        next.messages = Arc::from(without_tool_changes(&context.messages).into_boxed_slice());
    }
    next.tool_baseline = Some(baseline);
    Ok(Arc::new(next))
}

/// Stores `change` as one user message after the conversation so far and
/// returns the context that includes it.
async fn store_change(
    env: &TurnEnv,
    context: &Arc<ConversationContext>,
    change: ToolChange,
) -> Result<Arc<ConversationContext>, anyhow::Error> {
    let mut message = AgentMessage::new(env.session_id.clone(), ChatRole::User);
    message.parts.push(MessagePart::ToolChange {
        added: change.added,
        removed: change.removed,
    });
    let chat_message = message
        .to_chat_message()
        .map_err(|error| anyhow::anyhow!("Invalid tool change: {error}"))?;
    env.add_message(message).await?;

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
    env: &TurnEnv,
    context: &Arc<ConversationContext>,
    tool_updates: bool,
    tools: &[Tool],
) -> Result<ToolRequestState, anyhow::Error> {
    // A request for a provider without `tool_updates`: the tools offered now
    // as the list, and no tool-change message.
    let plain = || ToolRequestState {
        context: context.clone(),
        messages: without_tool_changes(&context.messages),
        leading: tools.to_vec(),
        offered: tools.to_vec(),
    };
    if !tool_updates {
        return Ok(plain());
    }

    // The newest message that is not a tool change decides where a record may
    // go. A record is never stored between a tool call and its result: a
    // history reload moves the results into the first user message after the
    // call, which would then be the record. A call without a result is an
    // upstream fault that is answered elsewhere, so the request is built like
    // one for a provider without `tool_updates` and nothing is stored yet.
    let last_real = context
        .messages
        .iter()
        .rfind(|message| !is_tool_change(message));
    if last_real.is_some_and(ChatMessage::has_tool_use) {
        return Ok(plain());
    }
    // The API accepts a tool change only directly after a user message, so one
    // waits for a request that follows a user message (a prompt or results).
    let change_allowed = last_real.is_some_and(|message| message.role == ChatRole::User);

    let wanted = origin(&context.provider, &context.model);
    let mut context = context.clone();
    let baseline = match context
        .tool_baseline
        .clone()
        .filter(|baseline| baseline.origin == wanted)
    {
        Some(baseline) => baseline,
        None => {
            let baseline = Arc::new(ToolBaseline {
                tools: tools.to_vec(),
                origin: wanted,
            });
            context = store_baseline(env, &context, baseline.clone()).await?;
            baseline
        }
    };

    if change_allowed
        && let Some(change) = diff(&offered(&baseline.tools, &context.messages), tools)
    {
        context = store_change(env, &context, change).await?;
    }

    Ok(ToolRequestState {
        messages: context.messages.to_vec(),
        leading: baseline.tools.clone(),
        offered: offered(&baseline.tools, &context.messages),
        context,
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

    fn change(added: Vec<Tool>, removed: &[&str]) -> ChatMessage {
        ChatMessage::from_tool_change(ToolChange::new(
            added,
            removed.iter().map(|name| name.to_string()).collect(),
        ))
    }

    fn names(tools: &[Tool]) -> Vec<&str> {
        tools.iter().map(|t| t.function.name.as_str()).collect()
    }

    // What a load of the stored history does with baselines and with changes
    // from before one is tested with `take_tool_baseline` in `agent-core`.

    #[test]
    fn history_without_changes_offers_the_baseline_list() {
        let baseline = vec![tool("a", "A"), tool("b", "B")];
        let messages = vec![user("a"), user("b")];
        assert_eq!(names(&offered(&baseline, &messages)), ["a", "b"]);
        assert!(offered(&[], &messages).is_empty());
        assert_eq!(without_tool_changes(&messages), messages);
    }

    #[test]
    fn changes_are_applied_in_order_to_the_baseline_list() {
        let baseline = vec![tool("a", "A"), tool("b", "B")];
        let messages = vec![
            user("go"),
            change(vec![tool("complete_task", "C"), tool("a", "A2")], &["b"]),
        ];
        let now = offered(&baseline, &messages);
        assert_eq!(names(&now), ["a", "complete_task"]);
        assert_eq!(now[0].function.description, "A2", "redefined");

        // A later change can take a tool away again.
        let messages = vec![
            user("go"),
            change(vec![tool("complete_task", "C")], &[]),
            user("more"),
            change(vec![], &["complete_task"]),
        ];
        assert_eq!(names(&offered(&baseline, &messages)), ["a", "b"]);
    }

    #[test]
    fn tools_can_be_offered_by_a_change_alone() {
        // The list a request leads with can be empty while tools are offered.
        let messages = vec![user("go"), change(vec![tool("late", "L")], &[])];
        assert_eq!(names(&offered(&[], &messages)), ["late"]);
    }

    #[test]
    fn tool_changes_are_left_out_for_a_provider_without_tool_updates() {
        let messages = vec![user("go"), change(vec![tool("b", "B")], &[]), user("more")];
        assert_eq!(
            without_tool_changes(&messages),
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
    }

    #[test]
    fn diff_of_equal_lists_is_none() {
        let tools = vec![tool("a", "A"), tool("b", "B")];
        assert!(diff(&tools, &tools).is_none());
        assert!(diff(&[], &[]).is_none());
    }

    // ---- resolve: the stored flow ----

    use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};

    async fn agent() -> (TestAgent, TurnEnv) {
        let agent =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let (exec, _turn) = agent.turn().await;
        (agent, exec)
    }

    fn context_for(
        exec: &TurnEnv,
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

    /// The same run a step later: `context` over `messages`. The baseline in
    /// effect is carried, as the loop carries it.
    fn continued(
        context: &Arc<ConversationContext>,
        messages: Vec<ChatMessage>,
    ) -> Arc<ConversationContext> {
        let mut next = (**context).clone();
        next.messages = Arc::from(messages.into_boxed_slice());
        Arc::new(next)
    }

    /// Kinds of the stored tool records, in order: `baseline` or `change`.
    async fn stored_records(agent: &TestAgent, exec: &TurnEnv) -> Vec<&'static str> {
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
        assert_eq!(names(&state.offered), ["a", "b"]);
        assert_eq!(state.messages, vec![user("go")], "the baseline is not sent");
        assert_eq!(
            state.context.messages.as_ref(),
            [user("go")],
            "a baseline is not a message"
        );
        let baseline = state.context.tool_baseline.as_ref().expect("in effect");
        assert_eq!(baseline.origin, "anthropic/claude-sonnet-5-5");
        assert_eq!(names(&baseline.tools), ["a", "b"]);
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
        assert!(change.removed.is_empty());
        assert_eq!(
            names(&second.offered),
            ["a", "b", "deploy"],
            "the model can call the added tool"
        );
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
        let replied = continued(&first.context, messages);
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let waiting = resolve(&exec, &replied, true, &now).await.unwrap();

        assert_eq!(stored_records(&agent, &exec).await, ["baseline"]);
        assert!(!waiting.messages.iter().any(ChatMessage::has_tool_change));
        assert_eq!(
            names(&waiting.offered),
            ["a", "b"],
            "not yet offered to the model"
        );

        // The next user message lets the change follow it.
        let mut messages = waiting.context.messages.to_vec();
        messages.push(user("next"));
        let prompted = continued(&waiting.context, messages);
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
        assert!(
            !third
                .context
                .messages
                .iter()
                .any(ChatMessage::has_tool_change),
            "and has left the conversation, as a reload would leave it out"
        );
        assert_eq!(
            third.context.tool_baseline.as_ref().unwrap().origin,
            "anthropic/claude-opus-4-8"
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
        // holds neither the baseline nor the change, so a load finds none.
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
        let view = exec.session_handle.history_view().await.unwrap();
        assert_eq!(
            view.messages,
            second.context.messages.as_ref(),
            "a load gives the messages the run held"
        );
        let mut reloaded =
            (*context_for(&exec, "anthropic", "claude-sonnet-5-5", view.messages)).clone();
        reloaded.tool_baseline = view.tool_baseline;
        let after = resolve(&exec, &Arc::new(reloaded), true, &now)
            .await
            .unwrap();

        assert_eq!(after.leading, second.leading);
        assert_eq!(after.offered, second.offered);
        assert_eq!(after.messages, second.messages);
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
        let undone = continued(&first.context, first.context.messages.to_vec());
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
        assert!(!is_tool_change(&message));
        assert_eq!(
            without_tool_changes(std::slice::from_ref(&message)).len(),
            1
        );
        assert!(offered(&[], &[message]).is_empty());
    }
}

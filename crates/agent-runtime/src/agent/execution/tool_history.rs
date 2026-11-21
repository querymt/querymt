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
//! Both records are stored messages like any other. The projection of a
//! request (`querymt_agent_core::projection::project_request`) decides what each becomes: a
//! baseline is the request's tool list and never a message, a change after the
//! baseline in effect is sent as a message when the provider takes them. This
//! module decides only when a record is stored.

use crate::agent::execution::turn::{Turn, TurnEnv};
use querymt::chat::{Tool, ToolChange};
use querymt_agent_core::model::{AgentMessage, MessagePart};
use querymt_agent_core::projection::tool_baseline;
use std::collections::BTreeMap;

/// The origin tag of a baseline for the session's provider and model.
pub(crate) fn origin(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

/// The tools the model can call after the baseline in effect and the tool
/// changes stored after it, ordered by name: each change applied in order to
/// the baseline's list. Without a baseline nothing is offered by the history.
pub(crate) fn offered(history: &[AgentMessage]) -> Vec<Tool> {
    let Some(baseline) = tool_baseline(history) else {
        return Vec::new();
    };
    let mut offered: BTreeMap<&str, &Tool> = baseline
        .tools
        .iter()
        .map(|tool| (tool.function.name.as_str(), tool))
        .collect();
    for part in history[baseline.at + 1..]
        .iter()
        .flat_map(|message| &message.parts)
    {
        if let MessagePart::ToolChange { added, removed } = part {
            for tool in added {
                offered.insert(tool.function.name.as_str(), tool);
            }
            for name in removed {
                offered.remove(name.as_str());
            }
        }
    }
    offered.into_values().cloned().collect()
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

/// Resolves the tool history for the next request of the run.
///
/// `tools` are the tools offered now, ordered by name and `origin` is the tag
/// of the session's provider and model. With `tool_updates`, stores a
/// baseline when the history has none for that origin, and a change when the
/// offered tools differ from the stored state; the record follows the
/// conversation so far, so it comes after any tool results. Nothing is stored
/// while a tool call awaits its result, and a change only after a user
/// message. Without `tool_updates`, nothing is stored.
///
/// Returns whether the request takes tool changes as messages. When it does,
/// the projection yields the baseline's list for it and [`offered`] the tools
/// the model can call; otherwise the request's list is `tools`.
pub(crate) async fn resolve(
    env: &TurnEnv,
    turn: &mut Turn,
    origin: &str,
    tool_updates: bool,
    tools: &[Tool],
) -> Result<bool, anyhow::Error> {
    if !tool_updates {
        return Ok(false);
    }

    // The newest message that is not a tool record decides where a record may
    // go. A record is never stored between a tool call and its result: a
    // history reload moves the results into the first user message after the
    // call, which would then be the record. A call without a result is an
    // upstream fault that is answered elsewhere, so the request is built like
    // one for a provider without `tool_updates` and nothing is stored yet.
    let last_real = turn
        .history
        .as_slice()
        .iter()
        .rfind(|message| !message.is_tool_record());
    if last_real.is_some_and(|message| !message.function_calls().is_empty()) {
        return Ok(false);
    }
    // The API accepts a tool change only directly after a user message, so one
    // waits for a request that follows a user message (a prompt or results).
    let change_allowed =
        last_real.is_some_and(|message| message.role == querymt::chat::ChatRole::User);

    if !turn
        .history
        .tool_baseline()
        .is_some_and(|baseline| baseline.origin == origin)
    {
        let mut message = AgentMessage::new(env.session_id.clone(), querymt::chat::ChatRole::User);
        message.parts.push(MessagePart::ToolBaseline {
            tools: tools.to_vec(),
            origin: origin.to_string(),
        });
        turn.history
            .commit(&env.session_handle, vec![message])
            .await?;
    }

    if change_allowed && let Some(change) = diff(&offered(turn.history.as_slice()), tools) {
        let mut message = AgentMessage::new(env.session_id.clone(), querymt::chat::ChatRole::User);
        message.parts.push(MessagePart::ToolChange {
            added: change.added,
            removed: change.removed,
        });
        turn.history
            .commit(&env.session_handle, vec![message])
            .await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};
    use querymt::chat::{ChatInputPart, ChatMessage, ChatRole};
    use querymt_agent_core::projection::{RequestTarget, project_request};

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

    fn stored(role: ChatRole, parts: Vec<MessagePart>) -> AgentMessage {
        let mut message = AgentMessage::new("s".into(), role);
        message.parts = parts;
        message
    }

    fn user(text: &str) -> AgentMessage {
        stored(
            ChatRole::User,
            vec![MessagePart::Text {
                content: text.into(),
            }],
        )
    }

    fn change(added: Vec<Tool>, removed: &[&str]) -> AgentMessage {
        stored(
            ChatRole::User,
            vec![MessagePart::ToolChange {
                added,
                removed: removed.iter().map(|name| name.to_string()).collect(),
            }],
        )
    }

    fn baseline(tools: Vec<Tool>) -> AgentMessage {
        stored(
            ChatRole::User,
            vec![MessagePart::ToolBaseline {
                tools,
                origin: "p/m".into(),
            }],
        )
    }

    fn names(tools: &[Tool]) -> Vec<&str> {
        tools.iter().map(|t| t.function.name.as_str()).collect()
    }

    #[test]
    fn history_without_changes_offers_the_baseline_list() {
        let history = vec![user("a"), baseline(vec![tool("a", "A"), tool("b", "B")])];
        assert_eq!(names(&offered(&history)), ["a", "b"]);
        assert!(offered(&[user("a")]).is_empty());
    }

    #[test]
    fn changes_are_applied_in_order_to_the_baseline_list() {
        let history = vec![
            baseline(vec![tool("a", "A"), tool("b", "B")]),
            user("go"),
            change(vec![tool("complete_task", "C"), tool("a", "A2")], &["b"]),
        ];
        let now = offered(&history);
        assert_eq!(names(&now), ["a", "complete_task"]);
        assert_eq!(now[0].function.description, "A2", "redefined");

        // A later change can take a tool away again.
        let history = vec![
            baseline(vec![tool("a", "A"), tool("b", "B")]),
            user("go"),
            change(vec![tool("complete_task", "C")], &[]),
            user("more"),
            change(vec![], &["complete_task"]),
        ];
        assert_eq!(names(&offered(&history)), ["a", "b"]);
    }

    #[test]
    fn changes_before_the_latest_baseline_are_part_of_its_list() {
        let history = vec![
            baseline(vec![tool("a", "A")]),
            change(vec![tool("old", "O")], &[]),
            baseline(vec![tool("a", "A"), tool("b", "B")]),
        ];
        assert_eq!(names(&offered(&history)), ["a", "b"]);
    }

    #[test]
    fn tools_can_be_offered_by_a_change_alone() {
        // The list a request leads with can be empty while tools are offered.
        let history = vec![baseline(Vec::new()), change(vec![tool("late", "L")], &[])];
        assert_eq!(names(&offered(&history)), ["late"]);
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

    const ANTHROPIC: &str = "anthropic/claude-sonnet-5-5";

    async fn run() -> (TestAgent, TurnEnv, Turn) {
        let agent =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let (env, turn) = agent.turn().await;
        (agent, env, turn)
    }

    fn user_for(env: &TurnEnv, text: &str) -> AgentMessage {
        let mut message = user(text);
        message.session_id = env.session_id.clone();
        message
    }

    /// Stores `messages` as the run's history so far.
    async fn seed(env: &TurnEnv, turn: &mut Turn, messages: Vec<AgentMessage>) {
        let messages = messages
            .into_iter()
            .map(|mut message| {
                message.session_id = env.session_id.clone();
                message
            })
            .collect();
        turn.history
            .commit(&env.session_handle, messages)
            .await
            .unwrap();
    }

    /// Kinds of the stored tool records, in order: `baseline` or `change`.
    async fn stored_records(agent: &TestAgent, env: &TurnEnv) -> Vec<&'static str> {
        agent
            .config
            .provider
            .history_store()
            .get_history(&env.session_id)
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

    /// The request a provider that takes tool changes would be sent.
    fn request(turn: &Turn, tool_changes: bool) -> (Vec<ChatMessage>, Option<Vec<Tool>>) {
        let projected = project_request(
            turn.history.as_slice(),
            &RequestTarget {
                output: None,
                max_prompt_bytes: None,
                tool_changes,
            },
        )
        .unwrap();
        (
            projected.messages,
            projected.baseline_tools.map(<[Tool]>::to_vec),
        )
    }

    fn sent_tool_change(messages: &[ChatMessage]) -> Option<ToolChange> {
        messages
            .last()
            .and_then(ChatMessage::input)
            .and_then(|parts| parts.first())
            .and_then(ChatInputPart::as_tool_change)
            .cloned()
    }

    #[tokio::test]
    async fn first_request_stores_a_baseline_and_sends_the_tools_as_the_list() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;

        let changes = resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();

        assert!(changes);
        assert_eq!(stored_records(&agent, &env).await, ["baseline"]);
        let (messages, leading) = request(&turn, true);
        assert_eq!(names(&leading.unwrap()), ["a", "b"]);
        assert_eq!(messages.len(), 1, "the baseline is not sent");
        assert_eq!(
            turn.history.tool_baseline().unwrap().origin,
            "anthropic/claude-sonnet-5-5"
        );
    }

    #[tokio::test]
    async fn unchanged_tools_store_nothing() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();
        let before = request(&turn, true);

        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &env).await, ["baseline"]);
        assert_eq!(request(&turn, true).0, before.0);
        assert_eq!(request(&turn, true).1, before.1);
    }

    #[tokio::test]
    async fn tool_added_between_requests_is_a_change_and_the_list_stays() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();
        let first = request(&turn, true).1;

        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        resolve(&env, &mut turn, ANTHROPIC, true, &now)
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &env).await, ["baseline", "change"]);
        let (messages, leading) = request(&turn, true);
        assert_eq!(leading, first, "the list does not change");
        let change = sent_tool_change(&messages).expect("change is sent");
        assert_eq!(names(&change.added), ["deploy"]);
        assert!(change.removed.is_empty());
        assert_eq!(
            names(&offered(turn.history.as_slice())),
            ["a", "b", "deploy"],
            "the model can call the added tool"
        );
    }

    #[tokio::test]
    async fn tool_removed_between_requests_is_a_change() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();

        resolve(&env, &mut turn, ANTHROPIC, true, &[tool("a", "A")])
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &env).await, ["baseline", "change"]);
        let change = sent_tool_change(&request(&turn, true).0).unwrap();
        assert!(change.added.is_empty());
        assert_eq!(change.removed, vec!["b".to_string()]);
    }

    #[tokio::test]
    async fn changed_definition_is_a_change_and_is_then_remembered() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &[tool("skill", "old")])
            .await
            .unwrap();

        let redefined = [tool("skill", "new")];
        resolve(&env, &mut turn, ANTHROPIC, true, &redefined)
            .await
            .unwrap();
        let second = request(&turn, true).0;
        resolve(&env, &mut turn, ANTHROPIC, true, &redefined)
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &env).await, ["baseline", "change"]);
        assert_eq!(request(&turn, true).0, second, "stored once");
    }

    fn calls_and_results() -> Vec<AgentMessage> {
        vec![
            user("go"),
            stored(
                ChatRole::Assistant,
                vec![MessagePart::ToolUse(querymt::ToolCall {
                    id: "toolu_1".into(),
                    call_type: "function".into(),
                    function: querymt::FunctionCall {
                        name: "a".into(),
                        arguments: "{}".into(),
                    },
                })],
            ),
        ]
    }

    fn results() -> AgentMessage {
        stored(
            ChatRole::User,
            vec![MessagePart::ToolResult {
                call_id: "toolu_1".into(),
                content: vec![querymt::chat::ToolResultPart::Text { text: "ok".into() }],
                is_error: false,
                tool_name: Some("a".into()),
                tool_arguments: None,
                compacted_at: None,
            }],
        )
    }

    #[tokio::test]
    async fn tool_change_follows_the_tool_results() {
        let (agent, env, mut turn) = run().await;
        let mut messages = calls_and_results();
        messages.push(results());
        seed(&env, &mut turn, messages).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();

        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        resolve(&env, &mut turn, ANTHROPIC, true, &now)
            .await
            .unwrap();

        let (sent, _) = request(&turn, true);
        assert!(sent[sent.len() - 2].has_tool_result());
        assert!(sent[sent.len() - 1].has_tool_change());
        assert_eq!(stored_records(&agent, &env).await, ["baseline", "change"]);
    }

    #[tokio::test]
    async fn nothing_is_stored_while_a_tool_call_awaits_its_result() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, calls_and_results()).await;

        let first = resolve(&env, &mut turn, ANTHROPIC, true, &[tool("a", "A")])
            .await
            .unwrap();
        let second = resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();

        assert!(!first && !second, "built like a request without changes");
        assert!(
            stored_records(&agent, &env).await.is_empty(),
            "no baseline or change between call and result"
        );
        assert_eq!(request(&turn, false).0.len(), 2);
    }

    #[tokio::test]
    async fn baseline_is_stored_once_the_result_has_arrived() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, calls_and_results()).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();
        assert!(stored_records(&agent, &env).await.is_empty());

        seed(&env, &mut turn, vec![results()]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &env).await, ["baseline"]);
        assert!(request(&turn, true).0.last().unwrap().has_tool_result());
    }

    #[tokio::test]
    async fn change_waits_while_the_newest_message_is_an_assistant_reply() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();

        // The newest message is a reply with no tool call.
        seed(
            &env,
            &mut turn,
            vec![stored(
                ChatRole::Assistant,
                vec![MessagePart::Text {
                    content: "done".into(),
                }],
            )],
        )
        .await;
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        resolve(&env, &mut turn, ANTHROPIC, true, &now)
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &env).await, ["baseline"]);
        assert!(
            !request(&turn, true)
                .0
                .iter()
                .any(ChatMessage::has_tool_change)
        );
        assert_eq!(
            names(&offered(turn.history.as_slice())),
            ["a", "b"],
            "not yet offered to the model"
        );

        // The next user message lets the change follow it.
        seed(&env, &mut turn, vec![user("next")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &now)
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &env).await, ["baseline", "change"]);
        assert!(request(&turn, true).0.last().unwrap().has_tool_change());
    }

    #[tokio::test]
    async fn baseline_is_renewed_after_a_model_switch_and_old_changes_are_not_sent() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        resolve(&env, &mut turn, ANTHROPIC, true, &now)
            .await
            .unwrap();
        assert!(request(&turn, true).0.last().unwrap().has_tool_change());

        resolve(&env, &mut turn, "anthropic/claude-opus-4-8", true, &now)
            .await
            .unwrap();

        assert_eq!(
            stored_records(&agent, &env).await,
            ["baseline", "change", "baseline"]
        );
        let (sent, leading) = request(&turn, true);
        assert_eq!(names(&leading.unwrap()), ["a", "b", "deploy"]);
        assert!(
            !sent.iter().any(ChatMessage::has_tool_change),
            "the earlier change is part of the new baseline"
        );
        assert_eq!(
            turn.history.tool_baseline().unwrap().origin,
            "anthropic/claude-opus-4-8"
        );
    }

    #[tokio::test]
    async fn baseline_is_renewed_after_compaction() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        resolve(&env, &mut turn, ANTHROPIC, true, &now)
            .await
            .unwrap();

        // After compaction the effective history starts from the summary and
        // holds neither the baseline nor the change, so a load finds none.
        turn.history
            .replace(vec![user_for(&env, "summary"), user_for(&env, "next")]);
        resolve(&env, &mut turn, ANTHROPIC, true, &now)
            .await
            .unwrap();

        assert_eq!(
            stored_records(&agent, &env).await,
            ["baseline", "change", "baseline"]
        );
        let (sent, leading) = request(&turn, true);
        assert_eq!(names(&leading.unwrap()), ["a", "b", "deploy"]);
        assert!(!sent.iter().any(ChatMessage::has_tool_change));
        assert_eq!(sent.len(), 2);
    }

    #[tokio::test]
    async fn tool_list_and_change_survive_a_restart() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        resolve(&env, &mut turn, ANTHROPIC, true, &now)
            .await
            .unwrap();
        let held = request(&turn, true);

        // A new process reads the history back from the store.
        let loaded = env
            .session_handle
            .get_effective_agent_history()
            .await
            .unwrap();
        let mut restarted = agent.turn().await.1;
        restarted.history.replace(loaded);
        resolve(&env, &mut restarted, ANTHROPIC, true, &now)
            .await
            .unwrap();

        assert_eq!(
            request(&restarted, true),
            held,
            "a load gives the request the run held"
        );
        assert_eq!(
            offered(restarted.history.as_slice()),
            offered(turn.history.as_slice())
        );
        assert_eq!(stored_records(&agent, &env).await, ["baseline", "change"]);
    }

    #[tokio::test]
    async fn tool_change_sent_as_a_message_reports_no_prefix_change() {
        use crate::agent::execution::prefix::{CachePrefixState, PrefixChange};
        let (_agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();
        let fingerprint = |turn: &Turn| {
            let (messages, leading) = request(turn, true);
            let leading = serde_json::to_vec(&leading.unwrap()).unwrap();
            crate::agent::execution::prefix::PrefixFingerprint::new(
                "anthropic",
                "claude-sonnet-5-5",
                querymt_agent_core::hash::RapidHash::new(&leading),
                querymt_agent_core::hash::RapidHash::new(b"system"),
                &messages,
            )
        };
        let mut state = CachePrefixState::default();
        let started = std::time::Instant::now();
        state.begin(fingerprint(&turn), started);
        state.commit(started);

        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        resolve(&env, &mut turn, ANTHROPIC, true, &now)
            .await
            .unwrap();

        assert_eq!(
            state.begin(fingerprint(&turn), started).change,
            PrefixChange::Unchanged,
            "the leading list is the baseline's and the change is appended"
        );
    }

    #[tokio::test]
    async fn provider_without_tool_updates_gets_the_current_list_and_stores_nothing() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;

        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        let first = resolve(&env, &mut turn, "openai/gpt-5", false, &tools_ab())
            .await
            .unwrap();
        let second = resolve(&env, &mut turn, "openai/gpt-5", false, &now)
            .await
            .unwrap();

        assert!(!first && !second);
        assert!(stored_records(&agent, &env).await.is_empty());
        let (sent, leading) = request(&turn, false);
        assert!(leading.is_none(), "the list is the tools offered now");
        assert_eq!(sent.len(), 1);
    }

    #[tokio::test]
    async fn switching_to_a_provider_without_tool_updates_leaves_the_records_out() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();
        let mut now = tools_ab();
        now.push(tool("deploy", "D"));
        resolve(&env, &mut turn, ANTHROPIC, true, &now)
            .await
            .unwrap();
        assert!(request(&turn, true).0.last().unwrap().has_tool_change());

        resolve(&env, &mut turn, "openai/gpt-5", false, &now)
            .await
            .unwrap();

        assert_eq!(request(&turn, false).0.len(), 1);
        assert_eq!(
            stored_records(&agent, &env).await,
            ["baseline", "change"],
            "the stored records stay in history"
        );
    }

    #[tokio::test]
    async fn switching_away_and_back_reuses_the_baseline_built_for_that_model() {
        let (agent, env, mut turn) = run().await;
        seed(&env, &mut turn, vec![user("go")]).await;
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();
        let first = request(&turn, true).1;

        resolve(&env, &mut turn, "openai/gpt-5", false, &tools_ab())
            .await
            .unwrap();
        resolve(&env, &mut turn, ANTHROPIC, true, &tools_ab())
            .await
            .unwrap();

        assert_eq!(stored_records(&agent, &env).await, ["baseline"]);
        assert_eq!(request(&turn, true).1, first);
    }
}

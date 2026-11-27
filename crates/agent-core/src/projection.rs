//! The one place a stored history becomes the messages of a request.
//!
//! [`project_request`] turns stored messages into what one model request, or
//! one compaction request, sends:
//!
//! - a notice becomes user text at its position;
//! - a tool baseline becomes the request's tool list and never a message;
//! - a tool change after the baseline in effect becomes a message when the
//!   provider takes tool changes as messages, and is left out otherwise; a tool
//!   change before that baseline is already part of its list and is left out;
//! - provider-native output is replayed only to the target it came from;
//! - prompt content is cut at the prompt size limit.
//!
//! Nothing else decides how a stored part is sent, so a request built in the
//! middle of a run equals one built from a fresh load of the session.

use crate::model::{AgentMessage, MessagePart, OutputTarget, PromptContentError};
use querymt::chat::{ChatMessage, Tool};
use std::borrow::Cow;

/// What a projection is built for.
pub struct RequestTarget<'a> {
    /// Provider, protocol, model and endpoint: decides native replay. `None`
    /// (a compaction request) replays nothing native.
    pub output: Option<&'a OutputTarget>,
    /// Longest prompt content sent, in bytes.
    pub max_prompt_bytes: Option<usize>,
    /// The provider takes tool changes as messages (`tool_updates`).
    pub tool_changes: bool,
}

/// The messages of one request, built from the stored history.
#[derive(Debug)]
pub struct ProjectedRequest<'a> {
    /// The conversation as the provider is sent it.
    pub messages: Vec<ChatMessage>,
    /// The tool list of the baseline in effect, when the provider takes tool
    /// changes and the history holds a baseline.
    pub baseline_tools: Option<&'a [Tool]>,
}

/// The tool baseline in effect in a history: a borrowed view of the most
/// recent stored one.
#[derive(Debug, Clone, Copy)]
pub struct ToolBaselineRef<'a> {
    /// Index of the message that holds it.
    pub at: usize,
    /// The offered tools, in the order the request lists them.
    pub tools: &'a [Tool],
    /// The provider and model it was built for, `provider/model`.
    pub origin: &'a str,
}

/// The most recent tool baseline in `history`. Every question about the
/// baseline in effect (the projection's, the tools offered, the origin check)
/// is answered from this one lookup.
pub fn tool_baseline(history: &[AgentMessage]) -> Option<ToolBaselineRef<'_>> {
    history.iter().enumerate().rev().find_map(|(at, message)| {
        message.parts.iter().rev().find_map(|part| match part {
            MessagePart::ToolBaseline { tools, origin } => {
                Some(ToolBaselineRef { at, tools, origin })
            }
            _ => None,
        })
    })
}

/// Project `history` for `target`.
///
/// # Errors
///
/// Fails when a stored prompt cannot be converted (for example an invalid
/// attachment).
pub fn project_request<'a>(
    history: &'a [AgentMessage],
    target: &RequestTarget<'_>,
) -> Result<ProjectedRequest<'a>, PromptContentError> {
    let baseline = tool_baseline(history);
    let baseline_at = baseline.map(|baseline| baseline.at);
    let mut messages = Vec::with_capacity(history.len() + 1);
    for (index, stored) in history.iter().enumerate() {
        let keep_changes = target.tool_changes && baseline_at.is_some_and(|at| index > at);
        let keep = |part: &MessagePart| match part {
            MessagePart::ToolBaseline { .. } => false,
            MessagePart::ToolChange { .. } => keep_changes,
            _ => true,
        };
        // Decide before copying: a stored tool record carries the full tool
        // schema list, and in production it is always a message of its own, so
        // it is either skipped or used as stored. Only a message that mixes a
        // dropped record with other parts needs a stripped copy.
        let stored: Cow<'_, AgentMessage> = if stored.parts.iter().all(keep) {
            Cow::Borrowed(stored)
        } else if !stored.parts.iter().any(keep) {
            continue;
        } else {
            let mut copy = stored.clone();
            copy.parts.retain(|part| keep(part));
            Cow::Owned(copy)
        };
        messages.push(
            stored.to_chat_message_with_output_target(target.output, target.max_prompt_bytes)?,
        );
    }
    Ok(ProjectedRequest {
        messages,
        baseline_tools: baseline
            .filter(|_| target.tool_changes)
            .map(|baseline| baseline.tools),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NoticeSource;
    use querymt::chat::{ChatInputPart, ChatRole};

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

    fn tool(name: &str) -> Tool {
        Tool {
            tool_type: "function".into(),
            function: querymt::chat::FunctionTool {
                name: name.into(),
                description: name.into(),
                parameters: serde_json::json!({"type": "object"}),
                strict: None,
            },
        }
    }

    fn baseline(names: &[&str]) -> AgentMessage {
        stored(
            ChatRole::User,
            vec![MessagePart::ToolBaseline {
                tools: names.iter().map(|name| tool(name)).collect(),
                origin: "p/m".into(),
            }],
        )
    }

    fn change(add: &[&str]) -> AgentMessage {
        stored(
            ChatRole::User,
            vec![MessagePart::ToolChange {
                added: add.iter().map(|name| tool(name)).collect(),
                removed: Vec::new(),
            }],
        )
    }

    fn target(tool_changes: bool) -> RequestTarget<'static> {
        RequestTarget {
            output: None,
            max_prompt_bytes: None,
            tool_changes,
        }
    }

    #[test]
    fn notice_is_sent_as_user_text() {
        let history = vec![
            user("go"),
            stored(
                ChatRole::User,
                vec![MessagePart::Notice {
                    source: NoticeSource::AgentMode,
                    content: "<mode>plan</mode>".into(),
                }],
            ),
        ];
        for tool_changes in [false, true] {
            let projected = project_request(&history, &target(tool_changes)).unwrap();
            assert_eq!(projected.messages.len(), 2);
            assert_eq!(projected.messages[1].role, ChatRole::User);
            assert_eq!(projected.messages[1].text(), "<mode>plan</mode>");
        }
    }

    #[test]
    fn baseline_is_the_tool_list_and_never_a_message() {
        let history = vec![user("go"), baseline(&["a", "b"]), user("more")];

        let with = project_request(&history, &target(true)).unwrap();
        assert_eq!(with.messages.len(), 2, "no message represents the baseline");
        let names: Vec<_> = with
            .baseline_tools
            .unwrap()
            .iter()
            .map(|tool| tool.function.name.as_str())
            .collect();
        assert_eq!(names, ["a", "b"]);

        let without = project_request(&history, &target(false)).unwrap();
        assert_eq!(without.messages.len(), 2);
        assert!(without.baseline_tools.is_none());
    }

    #[test]
    fn the_baseline_in_effect_is_the_most_recent_one() {
        let history = vec![baseline(&["a"]), user("go"), baseline(&["b"]), user("more")];
        let found = tool_baseline(&history).unwrap();
        assert_eq!(found.at, 2);
        assert_eq!(found.tools.len(), 1);
        assert_eq!(found.tools[0].function.name, "b");
        assert_eq!(found.origin, "p/m");
        assert!(tool_baseline(&[user("go")]).is_none());
    }

    #[test]
    fn tool_changes_follow_the_providers_declaration() {
        let history = vec![
            user("go"),
            change(&["before"]),
            baseline(&["a"]),
            user("more"),
            change(&["late"]),
        ];

        let taking = project_request(&history, &target(true)).unwrap();
        // The change before the baseline is part of its list; the one after
        // it is sent at its position.
        assert_eq!(taking.messages.len(), 3);
        assert!(
            taking.messages[2]
                .input()
                .is_some_and(|parts| { parts.iter().all(ChatInputPart::is_tool_change) })
        );

        let not_taking = project_request(&history, &target(false)).unwrap();
        assert_eq!(not_taking.messages.len(), 2);
    }

    #[test]
    fn a_stored_message_that_cannot_be_converted_fails_the_projection() {
        use crate::protocol::{ContentBlock, ImageContent};
        let invalid = stored(
            ChatRole::User,
            vec![MessagePart::Prompt {
                blocks: vec![ContentBlock::Image(ImageContent::new("%%%", "image/png"))],
            }],
        );

        assert!(project_request(&[invalid], &target(false)).is_err());
    }

    #[test]
    fn a_record_mixed_with_other_parts_is_stripped_and_the_rest_kept() {
        let text = || MessagePart::Text {
            content: "hello".into(),
        };
        // A baseline sharing a message with text: the baseline is always
        // dropped, the text stays.
        let with_baseline = stored(
            ChatRole::User,
            vec![
                MessagePart::ToolBaseline {
                    tools: vec![tool("a")],
                    origin: "p/m".into(),
                },
                text(),
            ],
        );
        for tool_changes in [false, true] {
            let history = vec![with_baseline.clone()];
            let projected = project_request(&history, &target(tool_changes)).unwrap();
            assert_eq!(projected.messages.len(), 1);
            assert_eq!(projected.messages[0].text(), "hello");
            // The stored history is never modified by the projection.
            assert_eq!(history[0].parts.len(), 2);
        }

        // A change after the baseline sharing a message with text: kept only
        // when the provider takes changes.
        let with_change = stored(
            ChatRole::User,
            vec![
                text(),
                MessagePart::ToolChange {
                    added: vec![tool("late")],
                    removed: Vec::new(),
                },
            ],
        );
        let history = vec![baseline(&["a"]), with_change];

        let without = project_request(&history, &target(false)).unwrap();
        assert_eq!(without.messages.len(), 1);
        assert_eq!(without.messages[0].text(), "hello");
        assert!(!without.messages[0].has_tool_change());

        let with = project_request(&history, &target(true)).unwrap();
        assert_eq!(with.messages.len(), 1);
        assert_eq!(with.messages[0].text(), "hello");
        assert!(with.messages[0].has_tool_change());
        assert_eq!(history[1].parts.len(), 2);
    }

    #[test]
    fn change_without_a_baseline_is_not_sent() {
        let history = vec![user("go"), change(&["late"])];
        let projected = project_request(&history, &target(true)).unwrap();
        assert_eq!(projected.messages.len(), 1);
        assert!(projected.baseline_tools.is_none());
    }

    #[test]
    fn a_baseline_has_no_chat_form() {
        // A message that still holds one projects to nothing a provider could
        // mistake for a tool change.
        let chat = baseline(&["a"]).to_chat_message().unwrap();
        assert!(!chat.has_tool_change());
        assert!(chat.input().unwrap().is_empty());
    }

    #[test]
    fn native_output_is_replayed_only_to_its_origin() {
        use querymt::chat::{
            ChatOutput, ChatOutputItem, ChatOutputProvenance, ChatReasoningItem, ChatReasoningPart,
        };
        let output = ChatOutput {
            items: vec![ChatOutputItem::Reasoning(ChatReasoningItem {
                id: Some("rs_1".into()),
                summary: Vec::new(),
                content: vec![ChatReasoningPart::text("thinking")],
                encrypted_content: Some("enc".into()),
                signature: Some("sig".into()),
                status: None,
                extensions: Default::default(),
            })],
            provenance: Some(ChatOutputProvenance {
                provider: "p".into(),
                protocol: "responses".into(),
                model: "m".into(),
                endpoint: "https://example.test".into(),
            }),
            ..ChatOutput::default()
        };
        let history = vec![
            user("go"),
            stored(ChatRole::Assistant, vec![MessagePart::Output { output }]),
        ];
        let reasoning = |request: &ProjectedRequest<'_>| match request.messages[1]
            .output()
            .unwrap()
            .items
            .first()
        {
            Some(ChatOutputItem::Reasoning(item)) => item.clone(),
            other => panic!("expected reasoning, got {other:?}"),
        };

        let origin = OutputTarget::new("p", "m")
            .with_protocol("responses")
            .with_endpoint("https://example.test");
        let native = project_request(
            &history,
            &RequestTarget {
                output: Some(&origin),
                ..target(false)
            },
        )
        .unwrap();
        assert_eq!(reasoning(&native).signature.as_deref(), Some("sig"));
        assert_eq!(reasoning(&native).encrypted_content.as_deref(), Some("enc"));

        let other = OutputTarget::new("p", "other")
            .with_protocol("responses")
            .with_endpoint("https://example.test");
        let portable = project_request(
            &history,
            &RequestTarget {
                output: Some(&other),
                ..target(false)
            },
        )
        .unwrap();
        assert!(reasoning(&portable).signature.is_none());
        assert!(reasoning(&portable).encrypted_content.is_none());
    }
}

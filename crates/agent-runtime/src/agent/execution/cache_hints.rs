//! Default prompt-cache hints on the messages of a request.

use querymt::chat::{CacheHint, ChatInputPart, ChatMessage};

/// Whether `message` is made only of tool changes: the provider places no
/// cache marker on it, so a hint there would be lost.
fn is_tool_change_only(message: &ChatMessage) -> bool {
    message
        .input()
        .is_some_and(|parts| !parts.is_empty() && parts.iter().all(ChatInputPart::is_tool_change))
}

/// Put the default cache hint on the last two conversation messages of a
/// request, in place; a hint already on a message keeps its own. A default
/// hint requests `ttl_seconds` (`None`: the provider's default lifetime).
///
/// A tool-change message carries no default hint and does not count as one of
/// the two. The positions are read from `messages` as they are, so a request a
/// context hook has restructured gets its hints in the right places.
///
/// A request with fewer than two conversation messages gets no hint.
pub(crate) fn apply_cache_breakpoints(messages: &mut [ChatMessage], ttl_seconds: Option<u64>) {
    let mut conversation = messages
        .iter_mut()
        .rev()
        .filter(|message| !is_tool_change_only(message));
    let (Some(last), Some(before)) = (conversation.next(), conversation.next()) else {
        return;
    };
    for message in [before, last] {
        if message.cache.is_none() {
            message.cache = Some(CacheHint::Ephemeral { ttl_seconds });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::ToolChange;

    fn text(text: &str) -> ChatMessage {
        ChatMessage::user().text(text).build()
    }

    fn change() -> ChatMessage {
        ChatMessage::from_tool_change(ToolChange::new(Vec::new(), vec!["x".into()]))
    }

    #[test]
    fn hints_go_on_the_last_two_conversation_messages() {
        let mut messages = vec![text("a"), text("b"), text("c"), change()];
        apply_cache_breakpoints(&mut messages, Some(60));
        let hinted: Vec<bool> = messages.iter().map(|m| m.cache.is_some()).collect();
        assert_eq!(hinted, [false, true, true, false]);
    }

    #[test]
    fn a_request_with_fewer_than_two_conversation_messages_gets_no_hint() {
        let mut messages = vec![text("a"), change()];
        apply_cache_breakpoints(&mut messages, None);
        assert!(messages.iter().all(|m| m.cache.is_none()));
    }

    #[test]
    fn a_hint_already_on_a_message_is_kept() {
        let mut own = text("b");
        own.cache = Some(CacheHint::Ephemeral {
            ttl_seconds: Some(5),
        });
        let mut messages = vec![text("a"), own];
        apply_cache_breakpoints(&mut messages, Some(60));
        assert_eq!(
            messages[1].cache,
            Some(CacheHint::Ephemeral {
                ttl_seconds: Some(5)
            })
        );
        assert!(messages[0].cache.is_some());
    }
}

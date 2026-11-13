/// A piece of context a hook handler contributes to the model conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookContextContribution {
    pub event_name: String,
    pub handler_id: String,
    pub tool_use_id: Option<String>,
    pub content: String,
}

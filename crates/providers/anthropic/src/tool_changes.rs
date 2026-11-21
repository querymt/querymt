//! Tool changes inside the conversation: `system` entries holding
//! `tool_addition` and `tool_removal` blocks (beta `inline-tools-2026-09-15`).
//!
//! The API accepts such an entry only directly after a `user` entry, and only
//! as the last entry or directly before an `assistant` entry. The canonical
//! history may hold a user entry after a tool-change message (a request-scoped
//! trailing message, or a prompt sent after a run that stopped before
//! answering), so [`place_tool_changes`] moves each entry after the run of
//! `user` entries that follows it.

use crate::config::Anthropic;
use crate::wire::request::{AnthropicMessage, MessageContent};
use querymt::chat::{ChatInputPart, ChatMessage, Tool};
use querymt::error::LLMError;
use serde_json::{Value, json};
use std::collections::HashMap;

/// Beta value that enables tool changes inside the conversation.
pub(crate) const INLINE_TOOLS_BETA: &str = "inline-tools-2026-09-15";

const SYSTEM_ROLE: &str = "system";

/// The strict tools in effect while a request is built: wire name and
/// converted schema, in the order they were sent. The API applies the strict
/// limits to the tools in effect (verified live: a removal or a redefinition
/// frees its slot, an added tool counts), so a tool change updates this set.
pub(crate) type StrictInUse = Vec<(String, Value)>;

/// Whether any message holds a tool change.
pub(crate) fn has_tool_change(messages: &[ChatMessage]) -> bool {
    messages.iter().any(ChatMessage::has_tool_change)
}

impl Anthropic {
    /// Whether the configured model accepts tool changes in the conversation.
    pub(crate) fn supports_tool_updates(&self) -> bool {
        self.parsed_model()
            .is_some_and(|model| model.supports_tool_updates())
    }

    /// Fails before anything is sent when a tool change meets a model that
    /// does not accept it.
    pub(crate) fn check_tool_changes(&self, messages: &[ChatMessage]) -> Result<(), LLMError> {
        if has_tool_change(messages) && !self.supports_tool_updates() {
            return Err(LLMError::InvalidRequest(format!(
                "Model {} does not accept tool changes inside the conversation",
                self.model
            )));
        }
        Ok(())
    }

    /// Wire-to-original names for the request's tools: the effective tool
    /// list plus every tool a tool change adds, so a response that calls a
    /// tool defined inside the conversation maps back to its original name.
    pub(crate) fn request_tool_names(
        &self,
        tools: Option<&[Tool]>,
        messages: &[ChatMessage],
    ) -> Result<HashMap<String, String>, LLMError> {
        let mut names = self.declared_tool_names(tools)?;
        let changes = messages
            .iter()
            .filter_map(ChatMessage::input)
            .flatten()
            .filter_map(ChatInputPart::as_tool_change);
        for change in changes {
            for tool in &change.added {
                let original = &tool.function.name;
                let wire = self.prefix_tool_name(original);
                if let Some(previous) = names.insert(wire.clone(), original.clone())
                    && previous != *original
                {
                    return Err(LLMError::InvalidRequest(format!(
                        "Tool names {previous:?} and {original:?} both encode as {wire:?}",
                    )));
                }
            }
        }
        Ok(names)
    }

    /// The `system` entry for a tool-change message: one `tool_addition`
    /// block per added or redefined tool, then one `tool_removal` block per
    /// removed tool, in the order given. `strict` holds the strict tools in
    /// effect before this entry. The request-wide strict limits count the
    /// tools in effect after the entry: those it removes or redefines no
    /// longer count, the strict ones it adds do. `strict` is updated to that
    /// state. An entry's strict flags depend only on the entries before it, so
    /// appending a change never alters the bytes of an earlier one.
    pub(crate) fn tool_change_entry(
        &self,
        message: &ChatMessage,
        strict: &mut StrictInUse,
    ) -> Result<AnthropicMessage, LLMError> {
        let mut content = Vec::new();
        for part in message.input().unwrap_or_default() {
            let ChatInputPart::ToolChange(change) = part else {
                return Err(LLMError::InvalidRequest(
                    "A tool-change part must be the only part of its message".into(),
                ));
            };
            if change.baseline {
                return Err(LLMError::InvalidRequest(
                    "A tool baseline is a request's tool list, not a message; it is never sent as one"
                        .into(),
                ));
            }
            let removed: Vec<String> = change
                .removed
                .iter()
                .map(|name| self.prefix_tool_name(name))
                .collect();
            let replaced: Vec<String> = change
                .added
                .iter()
                .map(|tool| self.prefix_tool_name(&tool.function.name))
                .collect();
            strict.retain(|(name, _)| !removed.contains(name) && !replaced.contains(name));
            let reserved: Vec<Value> = strict.iter().map(|(_, schema)| schema.clone()).collect();
            let built = self.build_tools_reserving(&change.added, &reserved);
            for tool in &built {
                if tool.strict == Some(true) {
                    strict.push((tool.name.clone(), tool.schema.as_ref().clone()));
                }
                content.push(MessageContent::ToolChange {
                    content_type: "tool_addition",
                    tool: json!({
                        "type": "tool_definition",
                        "definition": serde_json::to_value(tool)?,
                    }),
                });
            }
            for name in &change.removed {
                content.push(MessageContent::ToolChange {
                    content_type: "tool_removal",
                    tool: json!({
                        "type": "tool_reference",
                        "name": self.prefix_tool_name(name),
                    }),
                });
            }
        }
        if content.is_empty() {
            return Err(LLMError::InvalidRequest(
                "A tool-change message must add, redefine or remove a tool".into(),
            ));
        }
        Ok(AnthropicMessage {
            role: SYSTEM_ROLE,
            content,
        })
    }
}

/// Moves each tool-change entry after the run of `user` entries that follows
/// it, keeping the order of the tool-change entries.
///
/// Fails for a tool change that would be the first entry, or that would not
/// follow a `user` entry; the API rejects both.
pub(crate) fn place_tool_changes(
    entries: Vec<AnthropicMessage>,
) -> Result<Vec<AnthropicMessage>, LLMError> {
    if !entries.iter().any(|entry| entry.role == SYSTEM_ROLE) {
        return Ok(entries);
    }
    let mut placed = Vec::with_capacity(entries.len());
    let mut pending: Vec<AnthropicMessage> = Vec::new();
    for entry in entries {
        match entry.role {
            SYSTEM_ROLE => {
                if placed.is_empty() {
                    return Err(LLMError::InvalidRequest(
                        "A tool change cannot be the first message of a conversation".into(),
                    ));
                }
                pending.push(entry);
            }
            "assistant" => {
                flush(&mut placed, &mut pending)?;
                placed.push(entry);
            }
            _ => placed.push(entry),
        }
    }
    flush(&mut placed, &mut pending)?;
    Ok(placed)
}

fn flush(
    placed: &mut Vec<AnthropicMessage>,
    pending: &mut Vec<AnthropicMessage>,
) -> Result<(), LLMError> {
    if pending.is_empty() {
        return Ok(());
    }
    if placed.last().is_none_or(|last| last.role != "user") {
        return Err(LLMError::InvalidRequest(
            "A tool change must directly follow a user message".into(),
        ));
    }
    // Several tool changes can wait for the same spot. Two adjacent `system`
    // entries would leave the first without a `user` entry before it and
    // not directly before an `assistant` entry, so they are sent as one
    // entry whose blocks keep the order of the changes.
    let content = pending.drain(..).flat_map(|entry| entry.content).collect();
    placed.push(AnthropicMessage {
        role: SYSTEM_ROLE,
        content,
    });
    Ok(())
}

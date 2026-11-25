//! Live ACP `session/update` handling.

use crate::core::SessionCore;
use crate::entry::{
    AssistantMessage, CompactionEntry, CompactionState, EntryKind, PromptStatus, ReasoningPart,
    ToolCallEntry, UserMessage,
};
use crate::store::TurnHint;
use crate::view::{PlanView, SlashCommandView};
use querymt_agent_protocol_types::protocol::{
    AvailableCommand, AvailableCommandInput, CompactionStatus, CompactionSummaryChunk,
    CompactionUpdate, ContentBlock, ContentChunk, MaybeUndefined, Plan, PlanRemoved, PlanUpdate,
    PlanUpdateContent, SessionUpdate, ToolCall, ToolCallContent, ToolCallUpdate,
};
use querymt_agent_protocol_types::replay::{
    ACCOUNTING_META_KEY, CONTEXT_INVALIDATED_META_KEY, MODEL_META_KEY, QUERYMT_META_NAMESPACE,
    RUN_ID_META_KEY,
};
use querymt_agent_protocol_types::session::history::SummaryProvider;
use serde::Serialize;

/// The snake-case wire string of an ACP enum value.
fn wire_str<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        _ => String::new(),
    }
}

/// Merge `patch` into `target` key by key. Objects merge recursively, a
/// `null` removes the key and any other value replaces it.
fn merge_meta(
    target: &mut serde_json::Map<String, serde_json::Value>,
    patch: &serde_json::Map<String, serde_json::Value>,
) {
    for (key, value) in patch {
        match (target.get_mut(key), value) {
            (_, serde_json::Value::Null) => {
                target.remove(key);
            }
            (Some(serde_json::Value::Object(current)), serde_json::Value::Object(next)) => {
                merge_meta(current, next);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn content_values(content: &[ToolCallContent]) -> Vec<serde_json::Value> {
    content
        .iter()
        .filter_map(|item| serde_json::to_value(item).ok())
        .collect()
}

/// The slash commands of an `available_commands_update`, as the header
/// holds them: names without a leading `/`, entries with an empty name
/// dropped, the first of duplicate names kept. The input hint comes from the
/// unstructured input; an input kind this core does not know takes input
/// with an empty hint.
fn slash_commands(commands: &[AvailableCommand]) -> Vec<SlashCommandView> {
    let mut out: Vec<SlashCommandView> = Vec::with_capacity(commands.len());
    for command in commands {
        let name = command.name.trim_start_matches('/');
        if name.is_empty() || out.iter().any(|known| known.name == name) {
            continue;
        }
        let input_hint = match &command.input {
            Some(AvailableCommandInput::Unstructured(input)) => Some(input.hint.clone()),
            Some(_) => Some(String::new()),
            None => None,
        };
        out.push(SlashCommandView {
            name: name.to_owned(),
            description: command.description.clone(),
            input_hint,
        });
    }
    out
}

/// The model a `_meta.querymt.model` object names. A live update carries no
/// journal position, so the position is 0 (unknown).
fn selected_model(value: &serde_json::Value) -> Option<SummaryProvider> {
    Some(SummaryProvider {
        provider: value.get("provider")?.as_str()?.to_owned(),
        model: value.get("model")?.as_str()?.to_owned(),
        node: value
            .get("node")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        context_limit: value
            .get("contextLimit")
            .and_then(serde_json::Value::as_u64),
        position: 0,
    })
}

/// A string under `_meta.querymt.<key>` of a chunk.
fn querymt_meta<'a>(chunk: &'a ContentChunk, key: &str) -> Option<&'a str> {
    chunk
        .meta
        .as_ref()?
        .get(QUERYMT_META_NAMESPACE)?
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
}

/// Text of a chunk's content block; non-text blocks carry none.
fn chunk_text(chunk: &ContentChunk) -> Option<&str> {
    match &chunk.content {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }
}

/// The run id an update carries (`_meta.querymt.runId`), on the update kinds
/// that belong to a run.
fn run_id_of(update: &SessionUpdate) -> Option<String> {
    let meta = match update {
        SessionUpdate::UserMessageChunk(chunk)
        | SessionUpdate::AgentMessageChunk(chunk)
        | SessionUpdate::AgentThoughtChunk(chunk) => chunk.meta.as_ref(),
        SessionUpdate::ToolCall(call) => call.meta.as_ref(),
        SessionUpdate::ToolCallUpdate(call) => call.meta.as_ref(),
        SessionUpdate::CompactionUpdate(update) => match &update.meta {
            MaybeUndefined::Value(meta) => Some(meta),
            _ => None,
        },
        _ => None,
    }?;
    meta.get(QUERYMT_META_NAMESPACE)?
        .get(RUN_ID_META_KEY)?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

impl SessionCore {
    /// Apply one decoded update to the view. The update joins the turn of the
    /// run it carries, or of the run that is running.
    pub(crate) fn apply_update(&mut self, update: SessionUpdate) {
        self.store.update_run = run_id_of(&update);
        self.apply_update_inner(update);
        self.store.update_run = None;
    }

    fn apply_update_inner(&mut self, update: SessionUpdate) {
        self.observe_run(&update);
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => self.assistant_chunk(&chunk),
            SessionUpdate::UserMessageChunk(chunk) => self.user_chunk(&chunk),
            SessionUpdate::AgentThoughtChunk(chunk) => self.thought_chunk(&chunk),
            SessionUpdate::CompactionUpdate(update) => self.compaction_update(&update),
            SessionUpdate::CompactionSummaryChunk(chunk) => self.compaction_chunk(&chunk),
            SessionUpdate::UsageUpdate(usage) => self.usage_update(
                usage.used,
                usage.size,
                usage.cost.as_ref().map(|c| (c.amount, c.currency.as_str())),
                usage.meta.as_ref(),
            ),
            SessionUpdate::ConfigOptionUpdate(update) => {
                let options: Vec<serde_json::Value> = update
                    .config_options
                    .iter()
                    .filter_map(|option| serde_json::to_value(option).ok())
                    .collect();
                self.merge_config_options(options);
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.header.available_commands = slash_commands(&update.available_commands);
            }
            SessionUpdate::CurrentModeUpdate(update) => {
                self.header.current_mode = Some(update.current_mode_id.0.to_string());
            }
            SessionUpdate::SessionInfoUpdate(update) => {
                match update.title {
                    MaybeUndefined::Undefined => {}
                    MaybeUndefined::Null => self.header.title = None,
                    MaybeUndefined::Value(title) => self.header.title = Some(title),
                }
                let querymt = update
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get(QUERYMT_META_NAMESPACE));
                // The session's model changed: the last context measurement
                // does not apply to the new one until the next usage update,
                // and the header names the new model.
                if querymt
                    .and_then(|q| q.get(CONTEXT_INVALIDATED_META_KEY))
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                {
                    self.header.context = None;
                    if let Some(provider) = querymt
                        .and_then(|q| q.get(MODEL_META_KEY))
                        .and_then(selected_model)
                    {
                        self.header.provider = Some(provider);
                    }
                }
                self.set_token_usage(querymt);
                if let Some(accounting) = querymt
                    .and_then(|q| q.get(ACCOUNTING_META_KEY))
                    .and_then(|v| {
                        <querymt_agent_protocol_types::accounting::AccountingSnapshot as serde::Deserialize>::deserialize(v).ok()
                    })
                {
                    self.set_accounting(&accounting);
                }
            }
            SessionUpdate::Plan(plan) => self.set_plan(&plan),
            SessionUpdate::PlanUpdate(update) => self.plan_update(update),
            SessionUpdate::PlanRemoved(removed) => self.plan_removed(&removed),
            SessionUpdate::ToolCall(call) => {
                self.tool_call(&call);
                self.link_session_content(&call.tool_call_id.0, &call.content);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.tool_call_update(&update);
                if let Some(content) = &update.fields.content {
                    self.link_session_content(&update.tool_call_id.0, content);
                }
            }
            SessionUpdate::SubagentUpdate(update) => self.subagent_update(&update),
            SessionUpdate::CursorUpdate(update) => self.cursor_update(&update),
            _ => {}
        }
    }

    /// `tool_call` creates the entry, or replaces the state of a known id.
    fn tool_call(&mut self, call: &ToolCall) {
        let id = call.tool_call_id.0.to_string();
        let entry = ToolCallEntry {
            tool_call_id: id.clone(),
            title: call.title.clone(),
            kind: wire_str(&call.kind),
            status: wire_str(&call.status),
            raw_input: call.raw_input.clone(),
            raw_output: call.raw_output.clone(),
            content: content_values(&call.content),
            result_full_bytes: None,
            arguments_full_bytes: None,
            child_session_id: None,
        };
        if let Some(key) = self.store.idx.tool.get(&id).copied() {
            self.store.modify(key, |kind| match kind {
                EntryKind::ToolCall(existing) => {
                    let child = existing.child_session_id.take();
                    let mut next = entry;
                    next.child_session_id = child;
                    let changed = *existing != next;
                    *existing = next;
                    changed
                }
                _ => false,
            });
            return;
        }
        let key = self.store.push(
            format!("tool-{id}"),
            EntryKind::ToolCall(entry),
            TurnHint::Live,
        );
        self.store.idx.tool.insert(id, key);
    }

    /// `tool_call_update` patches the fields it carries; an unknown id creates
    /// the entry so a late update is not lost.
    fn tool_call_update(&mut self, update: &ToolCallUpdate) {
        let id = update.tool_call_id.0.to_string();
        let fields = &update.fields;
        if !self.store.idx.tool.contains_key(&id) {
            let key = self.store.push(
                format!("tool-{id}"),
                EntryKind::ToolCall(ToolCallEntry {
                    tool_call_id: id.clone(),
                    title: String::new(),
                    kind: wire_str(&querymt_agent_protocol_types::protocol::ToolKind::default()),
                    status: wire_str(
                        &querymt_agent_protocol_types::protocol::ToolCallStatus::default(),
                    ),
                    raw_input: None,
                    raw_output: None,
                    content: Vec::new(),
                    result_full_bytes: None,
                    arguments_full_bytes: None,
                    child_session_id: None,
                }),
                TurnHint::Live,
            );
            self.store.idx.tool.insert(id.clone(), key);
        }
        let Some(key) = self.store.idx.tool.get(&id).copied() else {
            return;
        };
        self.store.modify(key, |kind| {
            let EntryKind::ToolCall(call) = kind else {
                return false;
            };
            let before = call.clone();
            if let Some(value) = &fields.kind {
                call.kind = wire_str(value);
            }
            if let Some(value) = &fields.status {
                call.status = wire_str(value);
            }
            if let Some(value) = &fields.title {
                call.title.clone_from(value);
            }
            if let Some(value) = &fields.content {
                call.content = content_values(value);
            }
            if let Some(value) = &fields.raw_input {
                call.raw_input = Some(value.clone());
            }
            if let Some(value) = &fields.raw_output {
                call.raw_output = Some(value.clone());
            }
            *call != before
        });
    }

    /// A user chunk joins the message with its id. The agent's message for a
    /// pending prompt (same client prompt id) replaces the pending prompt: the
    /// pending prompt goes away and the message stands in its run's turn.
    fn user_chunk(&mut self, chunk: &ContentChunk) {
        let message_id = chunk.message_id.as_ref().map(|id| id.0.to_string());
        let prompt_id = querymt_meta(chunk, "client_prompt_id").map(str::to_owned);
        let block = chunk.content.clone();
        // More content of a message already in the view (the structured
        // blocks of one prompt share its message id).
        let existing = message_id
            .as_ref()
            .and_then(|id| self.store.idx.user.get(id).copied())
            .or_else(|| {
                prompt_id
                    .as_ref()
                    .and_then(|id| self.store.idx.prompt.get(id).copied())
                    .filter(|key| {
                        matches!(
                            self.store.kind(*key),
                            Some(EntryKind::UserMessage(user)) if user.status == PromptStatus::Sent
                        )
                    })
            });
        if let Some(key) = existing {
            let new_message_id = message_id.clone();
            self.store.modify(key, |kind| {
                let EntryKind::UserMessage(message) = kind else {
                    return false;
                };
                message.blocks.push(block);
                if new_message_id.is_some() {
                    message.message_id = new_message_id;
                }
                true
            });
            if let Some(id) = message_id {
                self.store.idx.user.insert(id, key);
            }
            return;
        }
        if let Some(id) = &prompt_id {
            self.drop_unsent_prompt(id);
        }
        let (entry_id, hint) = match &message_id {
            Some(id) => (format!("user-{id}"), TurnHint::Message(id.clone())),
            None => (
                format!("user-live-{}", self.store.next_anon()),
                TurnHint::Live,
            ),
        };
        let key = self.store.push(
            entry_id,
            EntryKind::UserMessage(UserMessage {
                message_id: message_id.clone(),
                // Only pending and failed prompts carry the client's id; the
                // agent's message is the same after a reload.
                client_prompt_id: None,
                blocks: vec![block],
                status: PromptStatus::Sent,
                error: None,
                at_ms: None,
            }),
            hint,
        );
        if let Some(id) = message_id {
            self.store.idx.user.insert(id, key);
        }
        if let Some(id) = prompt_id {
            self.store.idx.prompt.insert(id, key);
        }
    }

    /// Reasoning chunks merge per `(message id, part id)`; a chunk without a
    /// part id merges with the newest part of the same message.
    fn thought_chunk(&mut self, chunk: &ContentChunk) {
        let Some(text) = chunk_text(chunk) else {
            return;
        };
        let message_id = chunk.message_id.as_ref().map(|id| id.0.to_string());
        let msg_key = message_id.clone().unwrap_or_default();
        let part_id = querymt_meta(chunk, "reasoning_part_id").map(str::to_owned);
        let existing = match &part_id {
            Some(part) => self
                .store
                .idx
                .reasoning
                .get(&(msg_key.clone(), part.clone()))
                .copied(),
            None => self
                .store
                .idx
                .reasoning
                .range((msg_key.clone(), String::new())..)
                .take_while(|((message, _), _)| *message == msg_key)
                .map(|(_, key)| *key)
                .max(),
        };
        if let Some(key) = existing {
            self.store.modify(key, |kind| match kind {
                EntryKind::Reasoning(part) => {
                    part.text.push_str(text);
                    true
                }
                _ => false,
            });
            return;
        }
        let part = part_id.unwrap_or_default();
        let entry_id = format!("reasoning-{msg_key}-{part}");
        let key = self.store.push(
            entry_id,
            EntryKind::Reasoning(ReasoningPart {
                message_id,
                part_id: part.clone(),
                text: text.to_owned(),
            }),
            TurnHint::Live,
        );
        self.store.idx.reasoning.insert((msg_key, part), key);
    }

    /// Key of the compaction entry for `id`, creating it (which fixes its
    /// timeline position) when first seen.
    fn compaction_key(&mut self, id: &str, state: CompactionState) -> i64 {
        if let Some(key) = self.store.idx.compaction.get(id).copied() {
            return key;
        }
        let key = self.store.push(
            format!("compaction-{id}"),
            EntryKind::Compaction(CompactionEntry {
                compaction_id: id.to_owned(),
                state,
                other_status: None,
                summary: Vec::new(),
                error: None,
                meta: None,
            }),
            TurnHint::Live,
        );
        self.store.idx.compaction.insert(id.to_owned(), key);
        key
    }

    /// ACP upsert: the first update fixes the position; later ones patch.
    fn compaction_update(&mut self, update: &CompactionUpdate) {
        let id = update.compaction_id.0.to_string();
        let (state, other) = match &update.status {
            CompactionStatus::InProgress => (CompactionState::InProgress, None),
            CompactionStatus::Completed => (CompactionState::Completed, None),
            CompactionStatus::Failed => (CompactionState::Failed, None),
            CompactionStatus::Cancelled => (CompactionState::Cancelled, None),
            CompactionStatus::Other(raw) => (CompactionState::Other, Some(raw.clone())),
            _ => (CompactionState::Other, None),
        };
        let key = self.compaction_key(&id, state);
        self.store.modify(key, |kind| {
            let EntryKind::Compaction(entry) = kind else {
                return false;
            };
            let before = entry.clone();
            // A settled compaction is final: a later in_progress is ignored
            // but the other fields below still patch.
            if !(entry.state.is_settled() && state == CompactionState::InProgress) {
                entry.state = state;
                entry.other_status = other;
            }
            match &update.summary {
                MaybeUndefined::Undefined => {}
                MaybeUndefined::Null => entry.summary.clear(),
                MaybeUndefined::Value(blocks) => entry.summary.clone_from(blocks),
            }
            match &update.error {
                MaybeUndefined::Undefined => {}
                MaybeUndefined::Null => entry.error = None,
                MaybeUndefined::Value(text) => entry.error = Some(text.clone()),
            }
            match &update.meta {
                MaybeUndefined::Undefined => {}
                MaybeUndefined::Null => entry.meta = None,
                // Merged key by key, objects included, so a later update that
                // adds `tokensAfter` keeps the `tokensBefore` (and the run id)
                // an earlier one set.
                MaybeUndefined::Value(meta) => {
                    let mut merged = entry.meta.take().unwrap_or_default();
                    merge_meta(&mut merged, meta);
                    entry.meta = Some(merged);
                }
            }
            *entry != before
        });
    }

    /// Summary chunks append in receive order and create an in-progress
    /// compaction for a first-seen id.
    fn compaction_chunk(&mut self, chunk: &CompactionSummaryChunk) {
        let id = chunk.compaction_id.0.to_string();
        let key = self.compaction_key(&id, CompactionState::InProgress);
        let content = chunk.content.clone();
        self.store.modify(key, |kind| match kind {
            EntryKind::Compaction(entry) => {
                entry.summary.push(content);
                true
            }
            _ => false,
        });
    }

    /// Run state follows the newest agent activity: text streams, reasoning
    /// thinks, and an in-progress tool call is the active tool.
    fn observe_run(&mut self, update: &SessionUpdate) {
        use crate::view::RunState;
        // With state updates the agent decides whether a run is active;
        // session updates only refine a run that is.
        if self.state_updates_seen
            && (self.header.active_run_id.is_none()
                || self.header.run_state == RunState::WaitingForUser)
        {
            return;
        }
        match update {
            SessionUpdate::AgentMessageChunk(_) => self.set_run(RunState::Streaming),
            SessionUpdate::AgentThoughtChunk(_) => self.set_run(RunState::Thinking),
            SessionUpdate::ToolCall(call) if wire_str(&call.status) == "in_progress" => {
                self.header.run_state = RunState::ToolRunning;
                self.header.active_tool_call_id = Some(call.tool_call_id.0.to_string());
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.0.as_ref();
                let status = update.fields.status.as_ref().map(wire_str);
                match status.as_deref() {
                    Some("in_progress") => {
                        self.header.run_state = RunState::ToolRunning;
                        self.header.active_tool_call_id = Some(id.to_owned());
                    }
                    Some("completed" | "failed")
                        if self.header.active_tool_call_id.as_deref() == Some(id) =>
                    {
                        self.set_run(RunState::Thinking);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Replace config options with the same `id`; append new ones.
    pub(crate) fn merge_config_options(&mut self, options: Vec<serde_json::Value>) {
        for option in options {
            let id = option.get("id").cloned();
            let slot = id.as_ref().and_then(|id| {
                self.header
                    .config_options
                    .iter_mut()
                    .find(|current| current.get("id") == Some(id))
            });
            match slot {
                Some(current) => *current = option,
                None => self.header.config_options.push(option),
            }
        }
    }

    /// The legacy `plan` update replaces the whole plan.
    fn set_plan(&mut self, plan: &Plan) {
        self.header.plan = Some(PlanView {
            plan_id: None,
            entries: plan.entries.clone(),
            markdown: None,
        });
    }

    /// `plan_update` replaces the plan with the new content.
    fn plan_update(&mut self, update: PlanUpdate) {
        self.header.plan = match update.plan {
            PlanUpdateContent::Items(items) => Some(PlanView {
                plan_id: Some(items.plan_id.0.to_string()),
                entries: items.entries,
                markdown: None,
            }),
            PlanUpdateContent::Markdown(markdown) => Some(PlanView {
                plan_id: Some(markdown.plan_id.0.to_string()),
                entries: Vec::new(),
                markdown: Some(markdown.content),
            }),
            // A content kind this reducer does not know keeps the current plan.
            _ => self.header.plan.take(),
        };
    }

    /// `plan_removed` clears the plan it names (or any plan without an id).
    fn plan_removed(&mut self, removed: &PlanRemoved) {
        let id = removed.plan_id.0.as_ref();
        let matches = self
            .header
            .plan
            .as_ref()
            .is_some_and(|plan| plan.plan_id.as_deref().is_none_or(|current| current == id));
        if matches {
            self.header.plan = None;
        }
    }

    fn assistant_chunk(&mut self, chunk: &ContentChunk) {
        let Some(text) = chunk_text(chunk) else {
            return;
        };
        let message_id = chunk.message_id.as_ref().map(|id| id.0.to_string());
        let existing = match &message_id {
            Some(id) => self.store.idx.assistant.get(id).copied(),
            // A chunk with no id continues the newest entry when that is an
            // assistant message that also has no id.
            None => self.store.last_key().filter(|key| {
                matches!(
                    self.store.kind(*key),
                    Some(EntryKind::AssistantMessage(m)) if m.message_id.is_none()
                )
            }),
        };
        if let Some(key) = existing {
            self.store.modify(key, |kind| match kind {
                EntryKind::AssistantMessage(message) => {
                    message.text.push_str(text);
                    true
                }
                _ => false,
            });
            return;
        }
        let id = match &message_id {
            Some(id) => format!("assistant-{id}"),
            None => format!("assistant-live-{}", self.store.next_anon()),
        };
        let key = self.store.push(
            id,
            EntryKind::AssistantMessage(AssistantMessage {
                message_id: message_id.clone(),
                text: text.to_owned(),
            }),
            TurnHint::Live,
        );
        if let Some(message_id) = message_id {
            self.store.idx.assistant.insert(message_id, key);
        }
    }
}

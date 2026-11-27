//! Journal event to ACP update projection, shared by the agent and by clients.
//!
//! The agent replays a session's journal to a client as ACP updates, and a
//! client reducer rebuilds the same view from a load snapshot's events. Both
//! must project one event to the same updates, so the stateless rules live
//! here and neither side keeps a copy: tool kinds, the todo plan, reasoning
//! chunks, workspace announcements and compaction identity.

use crate::events::{
    AgentEventKind, ArchiveMerge, ArchiveRemoval, ReasoningPartStored, SlashCommandRecord,
};
use crate::protocol::{
    CompactionId, Content, ContentBlock, ContentChunk, CursorAnchor, CursorAnchorPosition,
    CursorCapabilities, CursorCompactionAnchor, CursorForkCapabilities, CursorMessageAnchor,
    CursorToolCallAnchor, CursorUpdate, MaybeUndefined, MessageId, Meta, Plan, PlanEntry,
    PlanEntryPriority, PlanEntryStatus, PlanId, PlanUpdate, PlanUpdateContent, SessionUpdate,
    TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};

/// Deterministic plan identity for QueryMT's todo list within a session.
pub const QUERYMT_TODO_PLAN_ID: &str = "querymt-todos";

/// A todo list as a plan-operations `plan_update` under
/// [`QUERYMT_TODO_PLAN_ID`]: the same update on the live, replay and
/// snapshot paths, so a plan keeps its identity across a reload.
pub fn todo_plan_update(entries: Vec<PlanEntry>) -> SessionUpdate {
    SessionUpdate::PlanUpdate(PlanUpdate::new(PlanUpdateContent::items(
        PlanId::from(QUERYMT_TODO_PLAN_ID.to_string()),
        entries,
    )))
}

/// Top-level `_meta` key of `session/list` entries (and of
/// `session_info_update` patches) holding a session's current plans: an array
/// of ACP plan content objects. Top level, because ACP v2 metadata patches
/// replace a top-level key whole.
pub const QUERYMT_PLANS_META_KEY: &str = "querymt/plans";

/// The [`QUERYMT_PLANS_META_KEY`] value for a todo list: one `items` plan
/// under [`QUERYMT_TODO_PLAN_ID`], built from the same plan content as
/// [`todo_plan_update`]. `None` when there are no entries, in which case the
/// key is omitted.
#[must_use]
pub fn todo_plans_meta(entries: Vec<PlanEntry>) -> Option<serde_json::Value> {
    if entries.is_empty() {
        return None;
    }
    let content = PlanUpdateContent::items(PlanId::from(QUERYMT_TODO_PLAN_ID.to_string()), entries);
    // Plan content is plain strings and enums: serializing it cannot fail.
    Some(serde_json::to_value([content]).expect("plan content serializes to JSON"))
}

/// The todo plan an event sets: the entries of a `todowrite` `ToolCallStart`
/// whose arguments are a todo list, or of a `TodoPlanRestored` after a
/// rewind. Empty when the list was emptied. `None` for every other event,
/// including a `todowrite` whose arguments are not a todo list.
///
/// The one place that decides what plan an event projects to; the journal,
/// the live translator, the materialized state and the announcer all use it.
#[must_use]
pub fn todo_plan_of(kind: &AgentEventKind) -> Option<Vec<PlanEntry>> {
    match kind {
        AgentEventKind::ToolCallStart {
            tool_name,
            arguments,
            ..
        } if is_todo_write_tool(tool_name) => todo_entries_from_arguments(arguments),
        AgentEventKind::TodoPlanRestored { entries } => Some(entries.clone()),
        _ => None,
    }
}

/// QueryMT extension namespace inside ACP's `_meta` escape hatch.
pub const QUERYMT_META_NAMESPACE: &str = "querymt";

/// `_meta.querymt` key holding the id of the run an update belongs to.
pub const RUN_ID_META_KEY: &str = "runId";

/// `_meta.querymt` key carrying the token breakdown of a usage update.
pub const USAGE_META_KEY: &str = "usage";

/// `_meta.querymt` key carrying the replacement accounting DTO.
pub const ACCOUNTING_META_KEY: &str = "accounting";

/// `_meta.querymt` key telling a client that its last context measurement no
/// longer applies (the session's model changed).
pub const CONTEXT_INVALIDATED_META_KEY: &str = "contextInvalidated";

/// `_meta.querymt` key naming the model a session switched to, next to
/// [`CONTEXT_INVALIDATED_META_KEY`]: `provider`, `model`, optional `node` and
/// optional `contextLimit`.
pub const MODEL_META_KEY: &str = "model";

/// `_meta.querymt` key on compaction updates holding the context size around
/// the compaction: `tokensBefore` and, once known, `tokensAfter`.
pub const COMPACTION_META_KEY: &str = "compaction";

/// Key of [`COMPACTION_META_KEY`]: the last context measurement before the
/// compaction started (the compaction's token estimate when there is none).
pub const COMPACTION_TOKENS_BEFORE_KEY: &str = "tokensBefore";

/// Key of [`COMPACTION_META_KEY`]: the context tokens after the compaction.
pub const COMPACTION_TOKENS_AFTER_KEY: &str = "tokensAfter";

/// `_meta.querymt` key on a skill problem `notice`: the complete current list,
/// an array of `{path, source, kind, message}` (empty when they were
/// resolved). Live-only; it is never part of a replay.
pub const SKILL_PROBLEMS_META_KEY: &str = "skillProblems";

/// `_meta.querymt` key holding the [`SlashCommandRecord`] of the invocation
/// an update belongs to: `name`, `arguments`, `source`, `effect` and, when the
/// prompt carried one, `clientPromptId`.
pub const SLASH_COMMAND_META_KEY: &str = "slashCommand";

/// `_meta.querymt` key holding the stable error code of a slash command that
/// was refused or failed, next to [`SLASH_COMMAND_META_KEY`].
pub const SLASH_COMMAND_ERROR_META_KEY: &str = "slashCommandError";

/// The wire form of a [`SlashCommandRecord`] (camelCase keys).
fn slash_command_meta_value(record: &SlashCommandRecord) -> serde_json::Value {
    let mut value = serde_json::json!({
        "name": record.name,
        "arguments": record.arguments,
        "source": record.source,
        "effect": record.effect,
    });
    if let (Some(id), Some(map)) = (&record.client_prompt_id, value.as_object_mut()) {
        map.insert("clientPromptId".to_owned(), id.clone().into());
    }
    value
}

/// Run `edit` on the `_meta.querymt` object of `meta`, creating it (or
/// replacing a value that is not an object) when needed.
fn with_querymt_namespace(
    meta: &mut Meta,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) {
    let slot = meta
        .entry(QUERYMT_META_NAMESPACE.to_owned())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let serde_json::Value::Object(map) = slot {
        edit(map);
    } else {
        let mut map = serde_json::Map::new();
        edit(&mut map);
        *slot = serde_json::Value::Object(map);
    }
}

/// Merge `_meta.querymt.runId` into `meta`, keeping every other key.
fn merge_run_id(meta: &mut Meta, run_id: &str) {
    with_querymt_namespace(meta, |namespace| {
        namespace.insert(RUN_ID_META_KEY.to_owned(), run_id.into());
    });
}

/// Merge the known compaction token counts into `meta.querymt.compaction`,
/// keeping every other key (a count that is not known is not written).
fn merge_compaction_tokens(meta: &mut Meta, before: Option<u64>, after: Option<u64>) {
    if before.is_none() && after.is_none() {
        return;
    }
    with_querymt_namespace(meta, |namespace| {
        let slot = namespace
            .entry(COMPACTION_META_KEY.to_owned())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if !slot.is_object() {
            *slot = serde_json::Value::Object(serde_json::Map::new());
        }
        if let serde_json::Value::Object(counts) = slot {
            if let Some(before) = before {
                counts.insert(COMPACTION_TOKENS_BEFORE_KEY.to_owned(), before.into());
            }
            if let Some(after) = after {
                counts.insert(COMPACTION_TOKENS_AFTER_KEY.to_owned(), after.into());
            }
        }
    });
}

/// Stamp the token counts of a compaction on a `compaction_update`:
/// `_meta.querymt.compaction.{tokensBefore, tokensAfter}`, merged next to the
/// existing keys. Other update kinds are left unchanged, and a count that is
/// `None` is omitted. The live translator, the history replay and a load
/// snapshot's projection all call this, so the three agree.
pub fn stamp_compaction_tokens(
    update: &mut SessionUpdate,
    before: Option<u64>,
    after: Option<u64>,
) {
    if let SessionUpdate::CompactionUpdate(compaction) = update {
        let mut meta = match std::mem::take(&mut compaction.meta) {
            MaybeUndefined::Value(meta) => meta,
            _ => Meta::new(),
        };
        merge_compaction_tokens(&mut meta, before, after);
        compaction.meta = if meta.is_empty() {
            MaybeUndefined::Undefined
        } else {
            MaybeUndefined::Value(meta)
        };
    }
}

/// Stamp the record of a slash command on the update that delivers its
/// outcome: `_meta.querymt.slashCommand`, and `slashCommandError` when `error`
/// is given, merged next to the existing keys. Only user message chunks, agent
/// message chunks and compaction updates carry one; other kinds are left
/// unchanged. Live delivery, replay and the session view all call this, so
/// they agree.
pub fn stamp_slash_command(
    update: &mut SessionUpdate,
    record: &SlashCommandRecord,
    error: Option<&str>,
) {
    let stamp = |meta: &mut Meta| {
        with_querymt_namespace(meta, |namespace| {
            namespace.insert(
                SLASH_COMMAND_META_KEY.to_owned(),
                slash_command_meta_value(record),
            );
            if let Some(error) = error {
                namespace.insert(SLASH_COMMAND_ERROR_META_KEY.to_owned(), error.into());
            }
        });
    };
    match update {
        SessionUpdate::UserMessageChunk(chunk) | SessionUpdate::AgentMessageChunk(chunk) => {
            stamp(chunk.meta.get_or_insert_with(Meta::new));
        }
        SessionUpdate::CompactionUpdate(compaction) => {
            let mut meta = match std::mem::take(&mut compaction.meta) {
                MaybeUndefined::Value(meta) => meta,
                _ => Meta::new(),
            };
            stamp(&mut meta);
            compaction.meta = MaybeUndefined::Value(meta);
        }
        _ => {}
    }
}

/// One agent message update, stamped with the command record when there is one.
fn agent_message_update(
    chunk: ContentChunk,
    record: Option<&SlashCommandRecord>,
    error: Option<&str>,
) -> Vec<SessionUpdate> {
    let mut update = SessionUpdate::AgentMessageChunk(chunk);
    if let Some(record) = record {
        stamp_slash_command(&mut update, record, error);
    }
    vec![update]
}

/// `tokensBefore` of a compaction that starts now: the last context
/// measurement, or the compaction's own token estimate when there is none.
pub fn compaction_tokens_before(last_measurement: Option<u64>, token_estimate: u32) -> u64 {
    last_measurement.unwrap_or(u64::from(token_estimate))
}

/// Stamp the id of the run an update belongs to on the update kinds that
/// belong to a run: user, assistant and thought chunks, tool calls and their
/// updates, and compaction updates. Other kinds are left unchanged. The id is
/// merged into `_meta.querymt` next to existing keys (reasoning part id,
/// client prompt id).
pub fn stamp_run_id(update: &mut SessionUpdate, run_id: &str) {
    match update {
        SessionUpdate::UserMessageChunk(chunk)
        | SessionUpdate::AgentMessageChunk(chunk)
        | SessionUpdate::AgentThoughtChunk(chunk) => {
            merge_run_id(chunk.meta.get_or_insert_with(Meta::new), run_id)
        }
        SessionUpdate::ToolCall(call) => {
            merge_run_id(call.meta.get_or_insert_with(Meta::new), run_id)
        }
        SessionUpdate::ToolCallUpdate(call) => {
            merge_run_id(call.meta.get_or_insert_with(Meta::new), run_id)
        }
        SessionUpdate::CompactionUpdate(compaction) => {
            let mut meta = match std::mem::take(&mut compaction.meta) {
                MaybeUndefined::Value(meta) => meta,
                _ => Meta::new(),
            };
            merge_run_id(&mut meta, run_id);
            compaction.meta = MaybeUndefined::Value(meta);
        }
        _ => {}
    }
}

/// Tool kind by tool name, as the agent reports it.
pub fn tool_kind_for_tool(name: &str) -> ToolKind {
    match name {
        "search_text" | "mdq" => ToolKind::Search,
        "write_file" | "edit" | "multiedit" | "accept_delegation_result" => ToolKind::Edit,
        "delete_file" => ToolKind::Delete,
        "shell" => ToolKind::Execute,
        "web_fetch" | "browse" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// Names the todo-list tool is journaled under (native and MCP).
pub const TODO_WRITE_TOOL_NAMES: [&str; 2] = ["todowrite", "mcp_todowrite"];

/// True for the todo-list tool, which projects to a plan and not a tool call.
pub fn is_todo_write_tool(tool_name: &str) -> bool {
    TODO_WRITE_TOOL_NAMES.contains(&tool_name)
}

/// The plan entries of a `todowrite` call. `None` when the arguments are not
/// a todo list; entries with an unknown status (and `cancelled`, which ACP
/// plans cannot express) are left out.
pub fn todo_entries_from_arguments(arguments: &str) -> Option<Vec<PlanEntry>> {
    let parsed: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let todos = parsed.get("todos")?.as_array()?;
    let mut entries = Vec::with_capacity(todos.len());

    for todo in todos {
        let content = todo.get("content")?.as_str()?.to_string();
        let Some(status) = todo
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(todo_status_to_plan_status)
        else {
            continue;
        };
        let priority = todo
            .get("priority")
            .and_then(serde_json::Value::as_str)
            .and_then(todo_priority_to_plan_priority)
            .unwrap_or(PlanEntryPriority::Medium);
        entries.push(PlanEntry::new(content, priority, status));
    }
    Some(entries)
}

fn todo_priority_to_plan_priority(priority: &str) -> Option<PlanEntryPriority> {
    match priority {
        "high" => Some(PlanEntryPriority::High),
        "medium" => Some(PlanEntryPriority::Medium),
        "low" => Some(PlanEntryPriority::Low),
        _ => None,
    }
}

fn todo_status_to_plan_status(status: &str) -> Option<PlanEntryStatus> {
    match status {
        "pending" => Some(PlanEntryStatus::Pending),
        "in_progress" => Some(PlanEntryStatus::InProgress),
        "completed" => Some(PlanEntryStatus::Completed),
        // ACP plans do not support a cancelled state; omit these entries.
        _ => None,
    }
}

fn text_block(content: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(content.to_owned()))
}

/// A reasoning chunk; a non-empty `part_id` is carried in `_meta` so a client
/// can tell reasoning parts of one message apart.
pub fn thought_chunk(content: &str, message_id: &str, part_id: Option<&str>) -> ContentChunk {
    let chunk =
        ContentChunk::new(text_block(content)).message_id(MessageId::from(message_id.to_owned()));
    match part_id.filter(|part_id| !part_id.is_empty()) {
        Some(part_id) => chunk.meta(serde_json::Map::from_iter([(
            QUERYMT_META_NAMESPACE.to_owned(),
            serde_json::json!({ "reasoning_part_id": part_id }),
        )])),
        None => chunk,
    }
}

/// What a stored assistant message stands for: its reasoning parts (only when
/// it has an id to attach them to), then its text.
pub fn stored_summary_updates(
    content: &str,
    message_id: Option<&str>,
    reasoning_parts: &[ReasoningPartStored],
) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    if let Some(message_id) = message_id {
        for part in reasoning_parts.iter().filter(|part| !part.text.is_empty()) {
            updates.push(SessionUpdate::AgentThoughtChunk(thought_chunk(
                &part.text,
                message_id,
                Some(&part.id),
            )));
        }
    }
    if !content.is_empty() {
        updates.push(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(text_block(content))
                .message_id(message_id.map(|id| MessageId::from(id.to_owned()))),
        ));
    }
    updates
}

/// Announcement of an archived workspace. The `message_id` is derived from the
/// workspace so live delivery and replay agree on identity.
///
/// `removal` is the outcome of a removal attempted together with the archive;
/// without it the text points at `/worktree remove`, and with it the text
/// states what happened instead of asking the user to run it again.
///
/// `merged` is the merge into the source checkout that preceded the archive
/// (`/worktree merge --archive`); the text then states its outcome.
pub fn worktree_archived_chunk(
    workspace_id: &str,
    by_session_id: Option<&str>,
    removal: Option<&ArchiveRemoval>,
    merged: Option<&ArchiveMerge>,
) -> ContentChunk {
    let subject = match merged {
        None => format!("This session's workspace {workspace_id} was archived"),
        Some(merge) => format!(
            "This session's workspace {workspace_id} {}, then archived",
            merge_outcome_text(merge)
        ),
    };
    let text = match removal {
        None => format!(
            "{subject}. This session is now read-only; run `/worktree remove` to delete the \
             checkout."
        ),
        Some(ArchiveRemoval::Removed) => {
            format!("{subject} and removed. This session is now read-only.")
        }
        Some(ArchiveRemoval::Refused { code, message }) => format!(
            "{subject}, but removal was refused ({code}): {message}. This session is now \
             read-only."
        ),
    };
    let mut details = serde_json::json!({ "workspaceId": workspace_id });
    if let Some(by) = by_session_id {
        details["bySessionId"] = by.into();
    }
    if let Some(merge) = merged {
        details["merged"] = serde_json::to_value(merge).unwrap_or_default();
    }
    ContentChunk::new(text_block(&text))
        .message_id(MessageId::from(format!("worktree-archived-{workspace_id}")))
        .meta(serde_json::Map::from_iter([(
            QUERYMT_META_NAMESPACE.to_owned(),
            serde_json::json!({ "worktreeArchived": details }),
        )]))
}

/// How a merge outcome reads inside the archived update.
fn merge_outcome_text(merge: &ArchiveMerge) -> String {
    let branch = merge
        .branch
        .as_deref()
        .map(|branch| format!(" on {branch}"))
        .unwrap_or_default();
    match (merge.outcome.as_str(), merge.commit.as_deref()) {
        ("merged", Some(commit)) => {
            format!("was merged into its source as commit {commit}{branch}")
        }
        ("already_merged", Some(commit)) => {
            format!("was already merged into its source as commit {commit}{branch}")
        }
        ("no_changes", _) => "had nothing to merge: its source already contains the changes".into(),
        (other, _) => format!("was merged into its source ({other})"),
    }
}

/// Reply to a mutating native `/worktree` command. The `message_id` is
/// derived from the per-prompt `id` so live delivery and replay agree on
/// identity.
pub fn worktree_command_chunk(
    id: &str,
    command: &str,
    code: Option<&str>,
    archived: Option<bool>,
    text: &str,
) -> ContentChunk {
    let mut details = serde_json::json!({ "command": command });
    if let Some(code) = code {
        details["code"] = code.into();
    }
    if let Some(archived) = archived {
        details["archived"] = archived.into();
    }
    ContentChunk::new(text_block(text))
        .message_id(MessageId::from(format!("worktree-cmd-{id}")))
        .meta(serde_json::Map::from_iter([(
            QUERYMT_META_NAMESPACE.to_owned(),
            serde_json::json!({ "worktreeCommand": details }),
        )]))
}

/// Announcement of an ownership handoff. The `message_id` is derived from the
/// workspace and successor so live delivery and replay agree on identity.
pub fn worktree_handoff_chunk(
    workspace_id: &str,
    successor_session_id: &str,
    summary_seeded: bool,
) -> ContentChunk {
    let text = format!(
        "This session's workspace {workspace_id} was handed off to session \
         {successor_session_id}. This session is now read-only."
    );
    ContentChunk::new(text_block(&text))
        .message_id(MessageId::from(format!(
            "worktree-handoff-{workspace_id}-{successor_session_id}"
        )))
        .meta(serde_json::Map::from_iter([(
            QUERYMT_META_NAMESPACE.to_owned(),
            serde_json::json!({
                "worktreeHandoff": {
                    "workspaceId": workspace_id,
                    "successorSessionId": successor_session_id,
                    "summarySeeded": summary_seeded,
                }
            }),
        )]))
}

/// The stateless, replay-safe projection of one event: conversation, tool and
/// workspace-announcement content.
///
/// Streaming text deltas and per-block user prompts are live-only (replay
/// reconstructs them from stored messages), and the stateful kinds (mode and
/// config, metadata, usage, notices, compactions) are projected by their
/// owners. A stored assistant message is projected here as text only; use
/// [`stored_summary_updates`] to include its reasoning parts.
pub fn replayable_updates(kind: &AgentEventKind) -> Vec<SessionUpdate> {
    match kind {
        AgentEventKind::PromptReceived {
            content,
            message_id,
            slash_command,
        } => {
            let mut update = SessionUpdate::UserMessageChunk(
                ContentChunk::new(text_block(content))
                    .message_id(message_id.clone().map(MessageId::from)),
            );
            if let Some(record) = slash_command {
                stamp_slash_command(&mut update, record, None);
            }
            vec![update]
        }
        AgentEventKind::AssistantMessageStored {
            content,
            message_id,
            ..
        } => {
            if content.is_empty() {
                return Vec::new();
            }
            vec![SessionUpdate::AgentMessageChunk(
                ContentChunk::new(text_block(content))
                    .message_id(message_id.clone().map(MessageId::from)),
            )]
        }
        AgentEventKind::AssistantThinkingDelta {
            content,
            message_id,
            part_id,
        } => {
            if content.is_empty() {
                return Vec::new();
            }
            vec![SessionUpdate::AgentThoughtChunk(thought_chunk(
                content,
                message_id,
                part_id.as_deref(),
            ))]
        }
        AgentEventKind::ToolCallStart {
            tool_call_id,
            tool_name,
            arguments,
        } => {
            if is_todo_write_tool(tool_name) {
                // Ungated contexts receive the legacy full-replacement plan;
                // plan-operations connections intercept todowrite earlier.
                return todo_plan_of(kind)
                    .map(|entries| vec![SessionUpdate::Plan(Plan::new(entries))])
                    .unwrap_or_default();
            }
            let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
            vec![SessionUpdate::ToolCall(
                ToolCall::new(
                    ToolCallId::from(tool_call_id.clone()),
                    format!("Run {tool_name}"),
                )
                .kind(tool_kind_for_tool(tool_name))
                .status(ToolCallStatus::InProgress)
                .raw_input(args),
            )]
        }
        AgentEventKind::ToolCallEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            if is_todo_write_tool(tool_name) {
                return Vec::new();
            }
            let status = if *is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            let raw_output = serde_json::from_str(result).ok();
            vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::from(tool_call_id.clone()),
                ToolCallUpdateFields::new()
                    .kind(tool_kind_for_tool(tool_name))
                    .status(status)
                    .title(format!("Run {tool_name}"))
                    .content(vec![ToolCallContent::Content(Content::new(text_block(
                        result,
                    )))])
                    .raw_output(raw_output),
            ))]
        }
        AgentEventKind::WorktreeCommandOutput {
            text,
            code,
            slash_command,
        } => agent_message_update(
            ContentChunk::new(text_block(text)),
            slash_command.as_ref(),
            code.as_deref(),
        ),
        AgentEventKind::SlashCommandReply {
            slash_command,
            text,
            code,
        } => agent_message_update(
            ContentChunk::new(text_block(text)),
            Some(slash_command),
            code.as_deref(),
        ),
        AgentEventKind::WorktreeCommandCompleted {
            id,
            command,
            code,
            archived,
            text,
            slash_command,
        } => agent_message_update(
            worktree_command_chunk(id, command, code.as_deref(), *archived, text),
            slash_command.as_ref(),
            code.as_deref(),
        ),
        AgentEventKind::WorktreeOwnershipTransferred {
            workspace_id,
            successor_session_id,
            summary_seeded,
            slash_command,
        } => agent_message_update(
            worktree_handoff_chunk(workspace_id, successor_session_id, *summary_seeded),
            slash_command.as_ref(),
            None,
        ),
        AgentEventKind::WorktreeArchived {
            workspace_id,
            by_session_id,
            removal,
            merged,
            slash_command,
        } => agent_message_update(
            worktree_archived_chunk(
                workspace_id,
                by_session_id.as_deref(),
                removal.as_ref(),
                merged.as_ref(),
            ),
            slash_command.as_ref(),
            None,
        ),
        _ => Vec::new(),
    }
}

/// Identity of a compaction recorded before events carried an id, derived from
/// its position in the journal.
pub fn legacy_compaction_id(position: i64) -> CompactionId {
    CompactionId::from(format!("querymt-compaction-legacy-{position}"))
}

/// Hands out compaction identities while a journal is folded in order.
///
/// An event with an explicit id keeps it. An id-less start and the id-less end
/// that follows it are one compaction and get one derived identity; an id-less
/// end without a start gets its own. An explicit id or a failure closes the
/// open start.
///
/// It also follows the context measurements of the journal, so each
/// compaction's `tokensBefore` is the last measurement before its start
/// ([`compaction_tokens_before`]). Call [`observe`](Self::observe) with every
/// event, after handling it.
#[derive(Debug, Default)]
pub struct CompactionIds {
    open_legacy: Option<CompactionId>,
    legacy_count: usize,
    /// The last context measurement seen: a request end or a finished
    /// compaction (the same measurements the agent's usage projection takes).
    last_measurement: Option<u64>,
    /// `tokensBefore` of the compactions that started and did not settle.
    open_tokens_before: std::collections::BTreeMap<String, u64>,
    /// The slash command record of the compactions that started and did not
    /// settle, so the terminal update can carry it.
    open_records: std::collections::BTreeMap<String, SlashCommandRecord>,
}

impl CompactionIds {
    /// Take the context measurement `kind` reports, if it is one.
    pub fn observe(&mut self, kind: &AgentEventKind) {
        match kind {
            AgentEventKind::LlmRequestEnd { context_tokens, .. } => {
                self.last_measurement = Some(*context_tokens);
            }
            AgentEventKind::CompactionEnd {
                context_tokens: Some(tokens),
                ..
            } => self.last_measurement = Some(*tokens),
            _ => {}
        }
    }

    /// `tokensBefore` of the compaction `id` that starts now, remembered
    /// until it settles.
    pub fn start_tokens(&mut self, id: &CompactionId, token_estimate: u32) -> u64 {
        let before = compaction_tokens_before(self.last_measurement, token_estimate);
        self.open_tokens_before.insert(id.0.to_string(), before);
        before
    }

    /// `tokensBefore` remembered for `id`, forgotten once its compaction
    /// settles (end or failure).
    pub fn take_tokens_before(&mut self, id: &CompactionId) -> Option<u64> {
        self.open_tokens_before.remove(&*id.0)
    }

    /// Remember the record of the compaction `id` that starts now (none for a
    /// compaction the agent started on its own), until it settles.
    pub fn remember_record(&mut self, id: &CompactionId, record: Option<&SlashCommandRecord>) {
        if let Some(record) = record {
            self.open_records.insert(id.0.to_string(), record.clone());
        }
    }

    /// The record remembered for `id`, forgotten once its compaction settles
    /// (end or failure).
    pub fn take_record(&mut self, id: &CompactionId) -> Option<SlashCommandRecord> {
        self.open_records.remove(&*id.0)
    }

    fn next_legacy(&mut self, seq: i64) -> CompactionId {
        self.legacy_count += 1;
        let count = i64::try_from(self.legacy_count).unwrap_or(i64::MAX);
        legacy_compaction_id(seq.max(count))
    }

    /// Identity of a `compaction_start` at `seq`.
    pub fn start(&mut self, explicit: Option<&str>, seq: i64) -> CompactionId {
        match explicit.filter(|id| !id.is_empty()) {
            Some(id) => {
                self.open_legacy = None;
                CompactionId::from(id.to_owned())
            }
            None => {
                let id = self.next_legacy(seq);
                self.open_legacy = Some(id.clone());
                id
            }
        }
    }

    /// Identity of a `compaction_end` at `seq`; it closes the open start.
    pub fn end(&mut self, explicit: Option<&str>, seq: i64) -> CompactionId {
        let open = self.open_legacy.take();
        match explicit.filter(|id| !id.is_empty()) {
            Some(id) => CompactionId::from(id.to_owned()),
            None => open.unwrap_or_else(|| self.next_legacy(seq)),
        }
    }

    /// A failed compaction closes the open start.
    pub fn failed(&mut self) {
        self.open_legacy = None;
    }
}

/// Prefix of a cursor id anchored after a user or agent message.
const CURSOR_MESSAGE_PREFIX: &str = "m:";
/// Prefix of a cursor id anchored after an agent thought.
const CURSOR_THOUGHT_PREFIX: &str = "t:";
/// Prefix of a cursor id anchored after a tool call.
const CURSOR_TOOL_CALL_PREFIX: &str = "c:";
/// Prefix of a cursor id anchored after a compaction.
const CURSOR_COMPACTION_PREFIX: &str = "k:";

/// A timeline entity a session cursor is anchored after, as the agent knows
/// it when it announces the cursor. Carries the entity kind that the cursor
/// id alone does not (user and agent messages share one id prefix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorEntity {
    /// After a user message.
    UserMessage(String),
    /// After an agent message.
    AgentMessage(String),
    /// After an agent thought.
    AgentThought(String),
    /// After a tool call.
    ToolCall(String),
    /// After a compaction.
    Compaction(String),
}

impl CursorEntity {
    /// The cursor this entity's `after` position is announced as.
    pub fn target(&self) -> CursorTarget {
        match self {
            Self::UserMessage(id) | Self::AgentMessage(id) => CursorTarget::Message(id.clone()),
            Self::AgentThought(id) => CursorTarget::Thought(id.clone()),
            Self::ToolCall(id) => CursorTarget::ToolCall(id.clone()),
            Self::Compaction(id) => CursorTarget::Compaction(id.clone()),
        }
    }

    /// The cursor id of this entity's `after` position.
    pub fn cursor_id(&self) -> String {
        self.target().encode()
    }
}

/// What a cursor id identifies: the anchored entity's durable id, without the
/// message kind. The id is a pure function of the anchor, so replay, live
/// delivery and a restarted agent derive the same one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorTarget {
    /// `m:<message_id>`: a user or agent message.
    Message(String),
    /// `t:<message_id>`: an agent thought.
    Thought(String),
    /// `c:<tool_call_id>`: a tool call.
    ToolCall(String),
    /// `k:<compaction_id>`: a compaction.
    Compaction(String),
}

impl CursorTarget {
    /// The wire cursor id.
    pub fn encode(&self) -> String {
        let (prefix, id) = match self {
            Self::Message(id) => (CURSOR_MESSAGE_PREFIX, id),
            Self::Thought(id) => (CURSOR_THOUGHT_PREFIX, id),
            Self::ToolCall(id) => (CURSOR_TOOL_CALL_PREFIX, id),
            Self::Compaction(id) => (CURSOR_COMPACTION_PREFIX, id),
        };
        format!("{prefix}{id}")
    }

    /// Parse a wire cursor id. `None` for an unknown prefix or an empty id.
    pub fn decode(cursor_id: &str) -> Option<Self> {
        let parse = |prefix: &str, build: fn(String) -> Self| {
            cursor_id
                .strip_prefix(prefix)
                .filter(|id| !id.is_empty())
                .map(|id| build(id.to_owned()))
        };
        parse(CURSOR_MESSAGE_PREFIX, Self::Message)
            .or_else(|| parse(CURSOR_THOUGHT_PREFIX, Self::Thought))
            .or_else(|| parse(CURSOR_TOOL_CALL_PREFIX, Self::ToolCall))
            .or_else(|| parse(CURSOR_COMPACTION_PREFIX, Self::Compaction))
    }

    /// Whether `session/fork` may use this cursor: only message anchors.
    /// Fixed for a cursor's lifetime.
    pub fn is_forkable(&self) -> bool {
        matches!(self, Self::Message(_))
    }

    /// The message a forkable cursor forks after, `None` for the other kinds.
    pub fn forkable_message_id(&self) -> Option<&str> {
        match self {
            Self::Message(id) => Some(id),
            _ => None,
        }
    }
}

/// The message, thought and tool-call entities whose terminal update `kind`
/// produces, in timeline order: the entities a cursor is announced after.
///
/// Live delivery and history replay both call this on the same events, so
/// they announce the same cursors. Compactions are not here: their identity
/// depends on the event's stream position (see the shared compaction
/// identity), which the callers hold.
///
/// An entity without a message id (legacy events) has no durable anchor and
/// yields nothing. A thought is anchored only where replay sends one (a
/// stored reasoning part with text), and an agent message only when it has
/// text, so every cursor follows an entity the client was actually sent.
pub fn message_cursor_entities(kind: &AgentEventKind) -> Vec<CursorEntity> {
    match kind {
        AgentEventKind::PromptReceived {
            message_id: Some(message_id),
            ..
        } => vec![CursorEntity::UserMessage(message_id.clone())],
        AgentEventKind::AssistantMessageStored {
            content,
            reasoning_parts,
            message_id: Some(message_id),
            ..
        } => {
            let mut entities = Vec::new();
            if reasoning_parts.iter().any(|part| !part.text.is_empty()) {
                entities.push(CursorEntity::AgentThought(message_id.clone()));
            }
            if !content.is_empty() {
                entities.push(CursorEntity::AgentMessage(message_id.clone()));
            }
            entities
        }
        AgentEventKind::ToolCallEnd {
            tool_call_id,
            tool_name,
            ..
        } if !is_todo_write_tool(tool_name) => vec![CursorEntity::ToolCall(tool_call_id.clone())],
        _ => Vec::new(),
    }
}

/// The entities whose terminal update `kind` produces, including the
/// compaction a `compaction_end` completes.
///
/// `compaction` is the identity of the compaction `kind` completes, which
/// only the caller can resolve (live events carry it, a journal fold derives
/// it with [`CompactionIds`]). Pass `None` for a client that was not sent
/// compaction updates, so no cursor follows an update it never received.
/// Every cursor producer (live, replay, snapshot, history page) goes through
/// this one rule, so they announce the same cursors.
pub fn event_cursor_entities(
    kind: &AgentEventKind,
    compaction: Option<&CompactionId>,
) -> Vec<CursorEntity> {
    let mut entities = message_cursor_entities(kind);
    if let (AgentEventKind::CompactionEnd { .. }, Some(id)) = (kind, compaction) {
        entities.push(CursorEntity::Compaction(id.0.to_string()));
    }
    entities
}

/// Derives the cursors of an ordered journal, each announced once.
///
/// It owns the compaction identity of the fold, so an id-less (legacy)
/// compaction keeps one identity from its start to its end. Feed it every
/// event in order through [`observe`](Self::observe). A window or page is its
/// own fold: its cursors are the ones of the events it holds.
#[derive(Debug, Default)]
pub struct CursorTracker {
    announce_compactions: bool,
    compaction_ids: CompactionIds,
    announced: std::collections::HashSet<String>,
}

impl CursorTracker {
    /// `announce_compactions`: the client was sent compaction updates, so
    /// completed compactions get a cursor too.
    pub fn new(announce_compactions: bool) -> Self {
        Self {
            announce_compactions,
            ..Self::default()
        }
    }

    /// The cursors `kind` (the event at `seq`) announces for the first time.
    pub fn observe(&mut self, seq: i64, kind: &AgentEventKind) -> Vec<CursorEntity> {
        let completed = match kind {
            AgentEventKind::CompactionStart { compaction_id, .. } => {
                self.compaction_ids.start(compaction_id.as_deref(), seq);
                None
            }
            AgentEventKind::CompactionEnd { compaction_id, .. } => {
                Some(self.compaction_ids.end(compaction_id.as_deref(), seq))
            }
            AgentEventKind::CompactionFailed { .. } => {
                self.compaction_ids.failed();
                None
            }
            _ => None,
        };
        let completed = completed.filter(|_| self.announce_compactions);
        let mut entities = event_cursor_entities(kind, completed.as_ref());
        entities.retain(|entity| self.announced.insert(entity.cursor_id()));
        entities
    }
}

/// The `cursor_update`s of a session's events, in order: what a load
/// snapshot or a history page carries as `cursorUpdates`.
pub fn session_cursor_updates<'a>(
    events: impl IntoIterator<Item = &'a crate::events::AgentEvent>,
    announce_compactions: bool,
) -> Vec<CursorUpdate> {
    let mut tracker = CursorTracker::new(announce_compactions);
    events
        .into_iter()
        .flat_map(|event| tracker.observe(event.seq, &event.kind))
        .map(|entity| cursor_update_body(&entity))
        .collect()
}

/// The `cursor_update` announcing the position right after `entity`.
///
/// Carries the anchor (the first update for a cursor must) and
/// `capabilities.fork` on message cursors only.
pub fn cursor_update(entity: &CursorEntity) -> SessionUpdate {
    SessionUpdate::CursorUpdate(cursor_update_body(entity))
}

/// The payload of [`cursor_update`], without the `session_update` tag: the
/// shape `cursorUpdates` lists in load snapshots and history pages carry.
pub fn cursor_update_body(entity: &CursorEntity) -> CursorUpdate {
    let position = CursorAnchorPosition::After;
    let anchor = match entity {
        CursorEntity::UserMessage(id) => {
            CursorAnchor::UserMessage(CursorMessageAnchor::new(id.clone(), position))
        }
        CursorEntity::AgentMessage(id) => {
            CursorAnchor::AgentMessage(CursorMessageAnchor::new(id.clone(), position))
        }
        CursorEntity::AgentThought(id) => {
            CursorAnchor::AgentThought(CursorMessageAnchor::new(id.clone(), position))
        }
        CursorEntity::ToolCall(id) => {
            CursorAnchor::ToolCall(CursorToolCallAnchor::new(id.clone(), position))
        }
        CursorEntity::Compaction(id) => {
            CursorAnchor::Compaction(CursorCompactionAnchor::new(id.clone(), position))
        }
    };
    let mut update = CursorUpdate::new(entity.cursor_id()).anchor(anchor);
    if entity.target().is_forkable() {
        update = update.capabilities(CursorCapabilities::new().fork(CursorForkCapabilities::new()));
    }
    update
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(entity: &CursorEntity) -> CursorUpdate {
        match cursor_update(entity) {
            SessionUpdate::CursorUpdate(update) => update,
            other => panic!("expected a cursor update, got {other:?}"),
        }
    }

    #[test]
    fn cursor_ids_round_trip_through_the_codec() {
        for target in [
            CursorTarget::Message("msg-1".into()),
            CursorTarget::Thought("msg-1".into()),
            CursorTarget::ToolCall("call:with:colons".into()),
            CursorTarget::Compaction("cmp-9".into()),
        ] {
            assert_eq!(CursorTarget::decode(&target.encode()), Some(target));
        }
        assert_eq!(CursorEntity::UserMessage("a".into()).cursor_id(), "m:a");
        assert_eq!(CursorEntity::AgentMessage("a".into()).cursor_id(), "m:a");
        assert_eq!(CursorEntity::AgentThought("a".into()).cursor_id(), "t:a");
        assert_eq!(CursorEntity::ToolCall("a".into()).cursor_id(), "c:a");
        assert_eq!(CursorEntity::Compaction("a".into()).cursor_id(), "k:a");
    }

    #[test]
    fn malformed_cursor_ids_do_not_decode() {
        for bad in ["", "m:", "x:abc", "abc", "M:abc"] {
            assert_eq!(CursorTarget::decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn message_cursors_are_forkable() {
        for entity in [
            CursorEntity::UserMessage("u1".into()),
            CursorEntity::AgentMessage("a1".into()),
        ] {
            let update = cursor(&entity);
            let MaybeUndefined::Value(capabilities) = &update.capabilities else {
                panic!("message cursor must carry capabilities: {update:?}");
            };
            assert!(capabilities.fork.is_some());
            assert!(entity.target().is_forkable());
            assert!(matches!(
                update.anchor,
                Some(CursorAnchor::UserMessage(_) | CursorAnchor::AgentMessage(_))
            ));
        }
        let target = CursorTarget::decode("m:a1").unwrap();
        assert_eq!(target.forkable_message_id(), Some("a1"));
    }

    #[test]
    fn tool_call_cursors_are_not_forkable() {
        for entity in [
            CursorEntity::ToolCall("call-1".into()),
            CursorEntity::AgentThought("m1".into()),
            CursorEntity::Compaction("k1".into()),
        ] {
            let update = cursor(&entity);
            assert!(
                matches!(update.capabilities, MaybeUndefined::Undefined),
                "{entity:?} must carry no fork capability: {update:?}"
            );
            assert!(!entity.target().is_forkable());
            assert_eq!(entity.target().forkable_message_id(), None);
            assert!(update.anchor.is_some());
        }
    }

    #[test]
    fn cursor_anchors_sit_after_their_entity() {
        let update = cursor(&CursorEntity::ToolCall("call-1".into()));
        let Some(CursorAnchor::ToolCall(anchor)) = update.anchor else {
            panic!("tool call anchor expected");
        };
        assert_eq!(&*anchor.tool_call_id.0, "call-1");
        assert_eq!(anchor.position, CursorAnchorPosition::After);
    }

    #[test]
    fn id_less_start_and_end_are_one_compaction() {
        let mut ids = CompactionIds::default();
        let start = ids.start(None, 5);
        let end = ids.end(None, 9);
        assert_eq!(start, end);
        assert_eq!(start, legacy_compaction_id(5));
    }

    #[test]
    fn id_less_end_without_a_start_gets_its_own_identity() {
        let mut ids = CompactionIds::default();
        assert_eq!(ids.end(None, 9), legacy_compaction_id(9));
    }

    #[test]
    fn explicit_ids_are_kept_and_close_the_open_start() {
        let mut ids = CompactionIds::default();
        ids.start(None, 1);
        assert_eq!(
            ids.start(Some("c1"), 2),
            CompactionId::from("c1".to_owned())
        );
        // the legacy start was closed: an id-less end is a new compaction
        assert_ne!(ids.end(None, 3), legacy_compaction_id(1));
    }

    #[test]
    fn a_failure_closes_the_open_start() {
        let mut ids = CompactionIds::default();
        ids.start(None, 1);
        ids.failed();
        assert_ne!(ids.end(None, 4), legacy_compaction_id(1));
    }

    fn prompt(id: &str) -> AgentEventKind {
        AgentEventKind::PromptReceived {
            slash_command: None,
            content: "hi".into(),
            message_id: Some(id.into()),
        }
    }

    fn compaction_start(id: Option<&str>) -> AgentEventKind {
        AgentEventKind::CompactionStart {
            slash_command: None,
            token_estimate: 10,
            compaction_id: id.map(str::to_owned),
        }
    }

    fn record(name: &str, arguments: &str) -> SlashCommandRecord {
        SlashCommandRecord {
            name: name.into(),
            arguments: arguments.into(),
            source: crate::events::SlashCommandSource::Builtin,
            effect: crate::events::SlashCommandEffect::Action,
            client_prompt_id: Some("c1".into()),
        }
    }

    #[test]
    fn replayed_worktree_publish_reply_keeps_its_record() {
        let reply = AgentEventKind::WorktreeCommandCompleted {
            id: "c1".into(),
            command: "publish".into(),
            code: None,
            archived: None,
            text: "published".into(),
            slash_command: Some(record("worktree", "publish")),
        };
        let value = serde_json::to_value(chunk_of(&reply)).unwrap();
        assert_eq!(
            value["_meta"][QUERYMT_META_NAMESPACE][SLASH_COMMAND_META_KEY],
            serde_json::json!({
                "name": "worktree",
                "arguments": "publish",
                "source": "builtin",
                "effect": "action",
                "clientPromptId": "c1",
            })
        );
        assert!(
            value["_meta"][QUERYMT_META_NAMESPACE]
                .get(SLASH_COMMAND_ERROR_META_KEY)
                .is_none()
        );
        // The worktree metadata stays next to the record.
        assert_eq!(
            value["_meta"][QUERYMT_META_NAMESPACE]["worktreeCommand"],
            serde_json::json!({ "command": "publish" })
        );

        let refused = AgentEventKind::WorktreeCommandOutput {
            text: "busy".into(),
            code: Some("owner_active".into()),
            slash_command: Some(record("worktree", "archive")),
        };
        let value = serde_json::to_value(chunk_of(&refused)).unwrap();
        assert_eq!(
            value["_meta"][QUERYMT_META_NAMESPACE][SLASH_COMMAND_ERROR_META_KEY],
            "owner_active"
        );
    }

    #[test]
    fn history_without_records_replays_unchanged() {
        // Journal JSON written before records existed has no `slash_command`.
        let prompt: AgentEventKind = serde_json::from_value(serde_json::json!({
            "type": "prompt_received",
            "data": { "content": "hi", "message_id": "m1" }
        }))
        .unwrap();
        let reply: AgentEventKind = serde_json::from_value(serde_json::json!({
            "type": "worktree_command_completed",
            "data": { "id": "c1", "command": "publish", "text": "published" }
        }))
        .unwrap();
        let archived: AgentEventKind = serde_json::from_value(serde_json::json!({
            "type": "worktree_archived",
            "data": { "workspace_id": "w1" }
        }))
        .unwrap();
        for kind in [prompt, reply, archived] {
            let updates = replayable_updates(&kind);
            assert_eq!(updates.len(), 1, "{kind:?}");
            let value = serde_json::to_value(&updates[0]).unwrap();
            assert!(
                value["_meta"][QUERYMT_META_NAMESPACE]
                    .get(SLASH_COMMAND_META_KEY)
                    .is_none(),
                "{value}"
            );
        }
        let prompt_value = serde_json::to_value(
            &replayable_updates(&AgentEventKind::PromptReceived {
                content: "hi".into(),
                message_id: Some("m1".into()),
                slash_command: None,
            })[0],
        )
        .unwrap();
        assert!(prompt_value.get("_meta").is_none(), "{prompt_value}");
    }

    #[test]
    fn replayed_manual_compaction_keeps_its_record() {
        let mut ids = CompactionIds::default();
        let id = ids.start(Some("k1"), 1);
        ids.remember_record(&id, Some(&record("compact", "keep the API decisions")));
        let end = ids.end(Some("k1"), 2);
        let mut update = compaction_update("k1");
        let taken = ids.take_record(&end).expect("the record was remembered");
        stamp_slash_command(&mut update, &taken, None);
        assert_eq!(
            meta_of(&update).unwrap()[QUERYMT_META_NAMESPACE][SLASH_COMMAND_META_KEY]["arguments"],
            "keep the API decisions"
        );
        // Settled: nothing is kept, and an automatic compaction has no record.
        assert!(ids.take_record(&end).is_none());
        let auto = ids.start(Some("k2"), 3);
        ids.remember_record(&auto, None);
        assert!(ids.take_record(&auto).is_none());
    }

    fn compaction_end(id: Option<&str>) -> AgentEventKind {
        AgentEventKind::CompactionEnd {
            summary: "s".into(),
            summary_len: 1,
            compaction_id: id.map(str::to_owned),
            context_tokens: None,
        }
    }

    #[test]
    fn tracker_announces_each_cursor_once() {
        let mut tracker = CursorTracker::new(true);
        let first = tracker.observe(1, &prompt("m1"));
        assert_eq!(first, vec![CursorEntity::UserMessage("m1".into())]);
        assert!(tracker.observe(2, &prompt("m1")).is_empty());
    }

    #[test]
    fn tracker_names_a_completed_compaction() {
        let mut tracker = CursorTracker::new(true);
        assert!(tracker.observe(1, &compaction_start(Some("c1"))).is_empty());
        assert_eq!(
            tracker.observe(2, &compaction_end(Some("c1"))),
            vec![CursorEntity::Compaction("c1".into())]
        );
    }

    #[test]
    fn tracker_gives_an_id_less_compaction_its_start_identity() {
        let mut tracker = CursorTracker::new(true);
        tracker.observe(5, &compaction_start(None));
        let ended = tracker.observe(9, &compaction_end(None));
        assert_eq!(
            ended,
            vec![CursorEntity::Compaction(
                legacy_compaction_id(5).0.to_string()
            )]
        );
    }

    #[test]
    fn tracker_without_compaction_updates_announces_no_compaction() {
        let mut tracker = CursorTracker::new(false);
        tracker.observe(1, &compaction_start(Some("c1")));
        assert!(tracker.observe(2, &compaction_end(Some("c1"))).is_empty());
        assert_eq!(tracker.observe(3, &prompt("m1")).len(), 1);
    }

    #[test]
    fn a_failed_compaction_has_no_cursor() {
        let mut tracker = CursorTracker::new(true);
        tracker.observe(1, &compaction_start(Some("c1")));
        let failed = AgentEventKind::CompactionFailed {
            compaction_id: "c1".into(),
            reason: "no".into(),
            cancelled: false,
        };
        assert!(tracker.observe(2, &failed).is_empty());
    }

    #[test]
    fn cursor_update_body_carries_no_session_update_tag() {
        let body = cursor_update_body(&CursorEntity::UserMessage("m1".into()));
        let value = serde_json::to_value(&body).expect("a cursor update serializes");
        assert_eq!(value["cursorId"], "m:m1");
        assert!(value.get("sessionUpdate").is_none(), "{value}");
        let SessionUpdate::CursorUpdate(wrapped) =
            cursor_update(&CursorEntity::UserMessage("m1".into()))
        else {
            panic!("cursor_update wraps a cursor update");
        };
        assert_eq!(wrapped, body);
    }

    #[test]
    fn empty_explicit_id_counts_as_absent() {
        let mut ids = CompactionIds::default();
        assert_eq!(ids.start(Some(""), 7), legacy_compaction_id(7));
    }

    #[test]
    fn todowrite_projects_to_a_plan_and_no_tool_call() {
        let start = AgentEventKind::ToolCallStart {
            tool_call_id: "t".into(),
            tool_name: "todowrite".into(),
            arguments: r#"{"todos":[{"content":"a","status":"pending","priority":"high"},{"content":"b","status":"cancelled"}]}"#.into(),
        };
        let updates = replayable_updates(&start);
        assert_eq!(updates.len(), 1);
        let SessionUpdate::Plan(plan) = &updates[0] else {
            panic!("expected a plan, got {:?}", updates[0]);
        };
        assert_eq!(plan.entries.len(), 1);
        let end = AgentEventKind::ToolCallEnd {
            tool_call_id: "t".into(),
            tool_name: "todowrite".into(),
            result: "ok".into(),
            is_error: false,
        };
        assert!(replayable_updates(&end).is_empty());
    }

    #[test]
    fn todo_plan_update_uses_the_shared_plan_id() {
        let entries =
            todo_entries_from_arguments(r#"{"todos":[{"content":"a","status":"pending"}]}"#)
                .unwrap();
        let SessionUpdate::PlanUpdate(update) = todo_plan_update(entries) else {
            panic!("expected a plan update");
        };
        let PlanUpdateContent::Items(items) = &update.plan else {
            panic!("expected an item-based plan");
        };
        assert_eq!(items.plan_id.0.as_ref(), QUERYMT_TODO_PLAN_ID);
        assert_eq!(items.entries.len(), 1);
    }

    #[test]
    fn todo_plans_meta_serialises_one_items_plan() {
        let entries = todo_entries_from_arguments(
            r#"{"todos":[{"content":"Add table","status":"completed","priority":"high"}]}"#,
        )
        .unwrap();
        let value = todo_plans_meta(entries).expect("non-empty entries");
        assert_eq!(
            value,
            serde_json::json!([{
                "type": "items",
                "planId": "querymt-todos",
                "entries": [{
                    "content": "Add table",
                    "priority": "high",
                    "status": "completed"
                }]
            }])
        );
    }

    fn tool_start(tool_name: &str, arguments: &str) -> AgentEventKind {
        AgentEventKind::ToolCallStart {
            tool_call_id: "call".into(),
            tool_name: tool_name.into(),
            arguments: arguments.into(),
        }
    }

    #[test]
    fn todo_plan_of_projects_todowrite_calls_under_both_tool_names() {
        let args = r#"{"todos":[{"content":"a","status":"pending"}]}"#;
        for tool_name in TODO_WRITE_TOOL_NAMES {
            let entries = todo_plan_of(&tool_start(tool_name, args)).expect("a todo plan");
            assert_eq!(entries.len(), 1, "{tool_name}");
        }
    }

    #[test]
    fn todo_plan_of_projects_a_restored_plan_as_is() {
        let entries =
            todo_entries_from_arguments(r#"{"todos":[{"content":"a","status":"pending"}]}"#)
                .unwrap();
        assert_eq!(
            todo_plan_of(&AgentEventKind::TodoPlanRestored {
                entries: entries.clone()
            }),
            Some(entries)
        );
        assert_eq!(
            todo_plan_of(&AgentEventKind::TodoPlanRestored {
                entries: Vec::new()
            }),
            Some(Vec::new())
        );
    }

    #[test]
    fn todo_plan_of_is_empty_for_an_emptied_list_and_none_otherwise() {
        assert_eq!(
            todo_plan_of(&tool_start("todowrite", r#"{"todos":[]}"#)),
            Some(Vec::new())
        );
        // Not a todo list, not the todo tool, not a tool call.
        assert!(todo_plan_of(&tool_start("todowrite", "not json")).is_none());
        assert!(todo_plan_of(&tool_start("todowrite", r#"{"nothing":true}"#)).is_none());
        assert!(todo_plan_of(&tool_start("shell", r#"{"todos":[]}"#)).is_none());
        assert!(todo_plan_of(&AgentEventKind::Cancelled).is_none());
    }

    #[test]
    fn todo_plans_meta_is_none_for_empty_entries() {
        assert!(todo_plans_meta(Vec::new()).is_none());
    }

    #[test]
    fn todo_plans_meta_and_plan_update_agree_on_entries() {
        let args = r#"{"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"cancelled"},{"content":"c","status":"pending","priority":"low"}]}"#;
        let SessionUpdate::PlanUpdate(update) =
            todo_plan_update(todo_entries_from_arguments(args).unwrap())
        else {
            panic!("expected a plan update");
        };
        let expected = serde_json::to_value([update.plan]).unwrap();
        let listed = todo_plans_meta(todo_entries_from_arguments(args).unwrap()).unwrap();
        assert_eq!(listed, expected);
        assert_eq!(listed[0]["entries"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn worktree_events_project_to_agent_messages() {
        let archived = AgentEventKind::WorktreeArchived {
            slash_command: None,
            workspace_id: "w1".into(),
            by_session_id: None,
            removal: None,
            merged: None,
        };
        let updates = replayable_updates(&archived);
        assert!(matches!(
            updates.as_slice(),
            [SessionUpdate::AgentMessageChunk(_)]
        ));
    }

    fn chunk_of(kind: &AgentEventKind) -> ContentChunk {
        let mut updates = replayable_updates(kind);
        let Some(SessionUpdate::AgentMessageChunk(chunk)) = updates.pop() else {
            panic!("expected an agent message chunk");
        };
        chunk
    }

    fn chunk_text(chunk: &ContentChunk) -> String {
        let value = serde_json::to_value(&chunk.content).unwrap();
        value["text"].as_str().unwrap().to_owned()
    }

    #[test]
    fn mutating_command_reply_is_replayed() {
        let refused = AgentEventKind::WorktreeCommandCompleted {
            slash_command: None,
            id: "c1".into(),
            command: "remove".into(),
            code: Some("unpreserved_content".into()),
            archived: Some(true),
            text: "refused: a.txt".into(),
        };
        let chunk = chunk_of(&refused);
        assert_eq!(chunk_text(&chunk), "refused: a.txt");
        let value = serde_json::to_value(&chunk).unwrap();
        assert_eq!(value["messageId"], "worktree-cmd-c1");
        assert_eq!(
            value["_meta"][QUERYMT_META_NAMESPACE]["worktreeCommand"],
            serde_json::json!({
                "command": "remove",
                "code": "unpreserved_content",
                "archived": true
            })
        );

        // A success has neither `code` nor `archived`.
        let ok = AgentEventKind::WorktreeCommandCompleted {
            slash_command: None,
            id: "c2".into(),
            command: "publish".into(),
            code: None,
            archived: None,
            text: "published".into(),
        };
        let value = serde_json::to_value(chunk_of(&ok)).unwrap();
        assert_eq!(value["messageId"], "worktree-cmd-c2");
        assert_eq!(
            value["_meta"][QUERYMT_META_NAMESPACE]["worktreeCommand"],
            serde_json::json!({ "command": "publish" })
        );
    }

    #[test]
    fn archived_update_replayed_with_the_removal_outcome() {
        let archived = |removal| AgentEventKind::WorktreeArchived {
            slash_command: None,
            workspace_id: "w1".into(),
            by_session_id: None,
            removal,
            merged: None,
        };

        let plain = chunk_text(&chunk_of(&archived(None)));
        assert!(plain.contains("run `/worktree remove`"), "{plain}");

        let removed = chunk_text(&chunk_of(&archived(Some(ArchiveRemoval::Removed))));
        assert_eq!(
            removed,
            "This session's workspace w1 was archived and removed. This session is now read-only."
        );

        let refused = chunk_text(&chunk_of(&archived(Some(ArchiveRemoval::Refused {
            code: "unpreserved_content".into(),
            message: "3 uncommitted".into(),
        }))));
        assert_eq!(
            refused,
            "This session's workspace w1 was archived, but removal was refused \
             (unpreserved_content): 3 uncommitted. This session is now read-only."
        );
        assert!(
            !refused.contains("run `/worktree remove`"),
            "removal was attempted: {refused}"
        );

        // The identity stays the workspace-derived id in every case.
        let value =
            serde_json::to_value(chunk_of(&archived(Some(ArchiveRemoval::Removed)))).unwrap();
        assert_eq!(value["messageId"], "worktree-archived-w1");
    }

    #[test]
    fn archived_update_replayed_with_the_merge_outcome() {
        let archived = |removal, merged| AgentEventKind::WorktreeArchived {
            slash_command: None,
            workspace_id: "w1".into(),
            by_session_id: None,
            removal,
            merged,
        };
        let merge = |outcome: &str, commit: Option<&str>, branch: Option<&str>| ArchiveMerge {
            outcome: outcome.into(),
            commit: commit.map(Into::into),
            branch: branch.map(Into::into),
        };

        let merged = chunk_text(&chunk_of(&archived(
            None,
            Some(merge("merged", Some("abc123"), Some("main"))),
        )));
        assert_eq!(
            merged,
            "This session's workspace w1 was merged into its source as commit abc123 on main, \
             then archived. This session is now read-only; run `/worktree remove` to delete \
             the checkout."
        );

        let removed = chunk_text(&chunk_of(&archived(
            Some(ArchiveRemoval::Removed),
            Some(merge("merged", Some("abc123"), None)),
        )));
        assert_eq!(
            removed,
            "This session's workspace w1 was merged into its source as commit abc123, then \
             archived and removed. This session is now read-only."
        );

        let refused = chunk_text(&chunk_of(&archived(
            Some(ArchiveRemoval::Refused {
                code: "unpreserved_content".into(),
                message: "1 uncommitted".into(),
            }),
            Some(merge("already_merged", Some("abc123"), None)),
        )));
        assert!(refused.contains("was already merged into its source as commit abc123"));
        assert!(refused.contains("removal was refused (unpreserved_content)"));

        let nothing = chunk_text(&chunk_of(&archived(
            None,
            Some(merge("no_changes", None, None)),
        )));
        assert!(nothing.contains("had nothing to merge"), "{nothing}");

        let value = serde_json::to_value(chunk_of(&archived(
            None,
            Some(merge("merged", Some("abc123"), Some("main"))),
        )))
        .unwrap();
        assert_eq!(value["messageId"], "worktree-archived-w1");
        assert_eq!(
            value["_meta"][QUERYMT_META_NAMESPACE]["worktreeArchived"]["merged"],
            serde_json::json!({"outcome": "merged", "commit": "abc123", "branch": "main"})
        );
    }

    fn compaction_update(id: &str) -> SessionUpdate {
        SessionUpdate::CompactionUpdate(crate::protocol::CompactionUpdate::new(
            CompactionId::from(id.to_owned()),
            crate::protocol::CompactionStatus::Completed,
        ))
    }

    fn meta_of(update: &SessionUpdate) -> Option<serde_json::Value> {
        let SessionUpdate::CompactionUpdate(c) = update else {
            return None;
        };
        match &c.meta {
            MaybeUndefined::Value(meta) => Some(serde_json::Value::Object(meta.clone())),
            _ => None,
        }
    }

    #[test]
    fn compaction_counts_merge_next_to_the_run_id() {
        let mut update = compaction_update("c1");
        stamp_run_id(&mut update, "r1");
        stamp_compaction_tokens(&mut update, Some(150_000), Some(20_000));
        assert_eq!(
            meta_of(&update),
            Some(serde_json::json!({ "querymt": {
                "runId": "r1",
                "compaction": { "tokensBefore": 150_000, "tokensAfter": 20_000 }
            } }))
        );
    }

    #[test]
    fn an_unknown_compaction_count_is_omitted() {
        let mut update = compaction_update("c1");
        stamp_compaction_tokens(&mut update, Some(7), None);
        assert_eq!(
            meta_of(&update),
            Some(serde_json::json!({ "querymt": { "compaction": { "tokensBefore": 7 } } }))
        );
        // Nothing known: no meta at all.
        let mut bare = compaction_update("c2");
        stamp_compaction_tokens(&mut bare, None, None);
        assert_eq!(meta_of(&bare), None);
    }

    #[test]
    fn tokens_before_is_the_last_measurement_or_the_estimate() {
        assert_eq!(compaction_tokens_before(Some(150_000), 90_000), 150_000);
        assert_eq!(compaction_tokens_before(None, 90_000), 90_000);
    }
}

//! Inputs that come from the client rather than the agent: optimistic
//! prompts, prompt outcomes and the run state they drive.

use crate::core::SessionCore;
use crate::entry::{EntryKind, PromptStatus, UserMessage};
use crate::input::UndoFrame;
use crate::store::TurnHint;
use crate::view::RunState;
use querymt_agent_protocol_types::delegation_update::DelegationUpdateNotification;
use querymt_agent_protocol_types::protocol::{ContentBlock, StopReason};
use querymt_agent_protocol_types::runtime_status::{SessionRuntimePhase, SessionRuntimeStatus};
use querymt_agent_protocol_types::session::history::ToolContentPart;
use querymt_agent_protocol_types::session::history::{
    InputDelivery, PendingInputState, PendingInputSummary,
};
use querymt_agent_protocol_types::session::input_state::{
    SessionInputDelivery, SessionInputState, SessionInputStateNotification,
};

impl SessionCore {
    /// Move the run state, unless the run already ended in a terminal state
    /// that only a new prompt may leave.
    pub(crate) fn set_run(&mut self, state: RunState) {
        self.header.run_state = state;
        if state != RunState::ToolRunning {
            self.header.active_tool_call_id = None;
        }
    }

    /// A runtime observation. An idle runtime ends a stale run (the active
    /// tool is no longer reported; tool entries keep their last status); a
    /// busy runtime moves an idle view to the matching run state.
    pub(crate) fn runtime_observed(&mut self, status: &SessionRuntimeStatus) {
        let idle = status.phase == SessionRuntimePhase::Idle;
        self.header.runtime = Some(status.clone());
        if idle {
            if !matches!(
                self.header.run_state,
                RunState::Cancelled | RunState::Failed
            ) {
                self.set_run(RunState::Idle);
            } else {
                self.header.active_tool_call_id = None;
            }
        } else if self.header.run_state == RunState::Idle {
            self.set_run(match status.phase {
                SessionRuntimePhase::Starting => RunState::Submitting,
                SessionRuntimePhase::Model => RunState::Thinking,
                _ => RunState::Idle,
            });
        }
    }

    /// Accepted and queued inputs are pending (upserted by id); applied,
    /// started and discarded inputs are no longer pending.
    pub(crate) fn input_state(&mut self, note: &SessionInputStateNotification) {
        let pending = &mut self.header.pending_inputs;
        let position = pending.iter().position(|p| p.input_id == note.input_id);
        let state = match note.state {
            SessionInputState::Accepted => PendingInputState::Accepted,
            SessionInputState::Queued => PendingInputState::Queued,
            SessionInputState::Applied
            | SessionInputState::Started
            | SessionInputState::Discarded => {
                if let Some(at) = position {
                    pending.remove(at);
                }
                return;
            }
        };
        let delivery = match note.delivery {
            SessionInputDelivery::Steer => InputDelivery::Steer,
            SessionInputDelivery::Queue => InputDelivery::Queue,
        };
        match position {
            Some(at) => {
                let current = &mut pending[at];
                current.delivery = delivery;
                current.state = state;
                current.run_id.clone_from(&note.run_id);
                current.position = note.position.or(current.position);
            }
            None => pending.push(PendingInputSummary {
                input_id: note.input_id.clone(),
                delivery,
                state,
                run_id: note.run_id.clone(),
                position: note.position,
                blocks: Vec::new(),
                created_at: 0,
            }),
        }
    }

    /// Upsert the delegation by id and link its tool call to the child session.
    pub(crate) fn delegation_update(&mut self, update: &DelegationUpdateNotification) {
        let delegations = &mut self.header.delegations;
        match delegations
            .iter_mut()
            .find(|d| d.delegation_id == update.delegation_id)
        {
            Some(current) => *current = update.clone(),
            None => delegations.push(update.clone()),
        }
        let (Some(tool_id), Some(child)) = (&update.tool_call_id, &update.child_session_id) else {
            return;
        };
        self.link_child_session(tool_id, child);
    }

    /// Link a tool call to the child session it spawned.
    pub(crate) fn link_child_session(&mut self, tool_id: &str, child: &str) {
        if let Some(key) = self.store.idx.tool.get(tool_id).copied() {
            self.store.modify(key, |kind| match kind {
                EntryKind::ToolCall(call) if call.child_session_id.as_deref() != Some(child) => {
                    call.child_session_id = Some(child.to_owned());
                    true
                }
                _ => false,
            });
        }
    }

    /// Full tool content replaces the truncated prefix; only that tool call's
    /// turn changes.
    pub(crate) fn tool_content(
        &mut self,
        tool_call_id: &str,
        part: ToolContentPart,
        content: &str,
    ) {
        let Some(key) = self.store.idx.tool.get(tool_call_id).copied() else {
            return;
        };
        let value = serde_json::from_str::<serde_json::Value>(content)
            .unwrap_or_else(|_| serde_json::Value::String(content.to_owned()));
        self.store.modify(key, |kind| {
            let EntryKind::ToolCall(call) = kind else {
                return false;
            };
            match part {
                ToolContentPart::Result => {
                    call.raw_output = Some(value);
                    call.result_full_bytes = None;
                }
                ToolContentPart::Arguments => {
                    call.raw_input = Some(value);
                    call.arguments_full_bytes = None;
                }
            }
            true
        });
    }

    /// The undo stack is replaced as a whole.
    pub(crate) fn undo_stack(&mut self, frames: &[UndoFrame]) {
        self.header.undo_stack = frames.to_vec();
    }

    /// A prompt the client just submitted appears at once, keyed by its
    /// client prompt id.
    pub(crate) fn prompt_optimistic(
        &mut self,
        client_prompt_id: &str,
        blocks: &[ContentBlock],
        at_ms: i64,
    ) {
        if self.store.idx.prompt.contains_key(client_prompt_id) {
            return;
        }
        let key = self.store.push(
            format!("user-{client_prompt_id}"),
            EntryKind::UserMessage(UserMessage {
                message_id: None,
                client_prompt_id: Some(client_prompt_id.to_owned()),
                blocks: blocks.to_vec(),
                status: PromptStatus::Pending,
                error: None,
                at_ms: Some(at_ms),
            }),
            TurnHint::Prompt(client_prompt_id.to_owned()),
        );
        self.store
            .idx
            .prompt
            .insert(client_prompt_id.to_owned(), key);
        self.header.last_error = None;
        self.set_run(RunState::Submitting);
    }

    /// A failed prompt is marked failed with its message; nothing is attached.
    pub(crate) fn prompt_failed(&mut self, client_prompt_id: &str, message: &str) {
        if let Some(key) = self.store.idx.prompt.get(client_prompt_id).copied() {
            self.store.modify(key, |kind| match kind {
                EntryKind::UserMessage(user) => {
                    user.status = PromptStatus::Failed;
                    user.error = Some(message.to_owned());
                    true
                }
                _ => false,
            });
        }
        self.header.last_error = Some(message.to_owned());
        self.set_run(RunState::Failed);
    }

    /// The `session/prompt` request returned with a stop reason.
    pub(crate) fn prompt_completed(&mut self, stop_reason: &StopReason) {
        let text = match serde_json::to_value(stop_reason) {
            Ok(serde_json::Value::String(text)) => text,
            _ => String::new(),
        };
        let cancelled = text == "cancelled";
        self.header.last_stop_reason = Some(text);
        self.set_run(if cancelled {
            RunState::Cancelled
        } else {
            RunState::Idle
        });
    }
}

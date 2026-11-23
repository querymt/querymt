//! Inputs that come from the client rather than the agent: optimistic
//! prompts, prompt outcomes and the run state they drive.

use crate::core::SessionCore;
use crate::entry::{EntryKind, PromptStatus, UserMessage};
use crate::input::UndoFrame;
use crate::store::TurnHint;
use crate::view::RunState;
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

    /// A runtime observation. It decides the run state only while the agent
    /// sends no state updates; the status itself is always kept.
    pub(crate) fn runtime_observed(&mut self, status: &SessionRuntimeStatus) {
        self.header.runtime = Some(status.clone());
        if !self.state_updates_seen {
            self.apply_runtime_phase(status);
        }
    }

    /// Take the run state from a runtime status. An idle runtime ends a stale
    /// run (the active tool is no longer reported; tool entries keep their
    /// last status); every other known phase is an active run, so an idle view
    /// moves to the matching run state.
    pub(crate) fn apply_runtime_phase(&mut self, status: &SessionRuntimeStatus) {
        let idle = status.phase == SessionRuntimePhase::Idle;
        self.header.active_run_id = status.active_run_id.clone();
        self.store.active_run = status.active_run_id.clone();
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
                SessionRuntimePhase::Waiting => RunState::WaitingForUser,
                SessionRuntimePhase::Unknown | SessionRuntimePhase::Idle => RunState::Idle,
                // model, tools, closing, cancel requested
                _ => RunState::Thinking,
            });
        }
    }

    /// A `querymt/session/stateUpdate`: whether a run is active, which run,
    /// whether it waits for the user, and how it ended. Once one arrives the
    /// agent decides the run state; session updates only refine it.
    pub(crate) fn state_update(
        &mut self,
        notification: &serde_json::Value,
        index: u32,
        report: &mut crate::view::ChangeReport,
    ) {
        let params = notification.get("params").unwrap_or(notification);
        let mut reject = |message: String| {
            report.errors.push(crate::view::InputError {
                input_index: index,
                seq: None,
                message,
            });
        };
        let text = |key: &str| params.get(key).and_then(serde_json::Value::as_str);
        if let Some(session) = text("sessionId")
            && session != self.header.session_id
        {
            report.ignored += 1;
            return;
        }
        let (Some(state), Some(run_id)) = (text("state"), text("runId")) else {
            reject("state update has no `state` or `runId`".to_owned());
            return;
        };
        match state {
            "running" | "requires_action" => {
                self.header.active_run_id = Some(run_id.to_owned());
                self.store.active_run = Some(run_id.to_owned());
                if state == "requires_action" {
                    self.set_run(RunState::WaitingForUser);
                } else if !matches!(
                    self.header.run_state,
                    RunState::Thinking | RunState::Streaming | RunState::ToolRunning
                ) {
                    self.set_run(RunState::Thinking);
                }
            }
            "idle" => {
                // The end of an older run does not end the one running now.
                if self
                    .header
                    .active_run_id
                    .as_deref()
                    .is_some_and(|active| active != run_id)
                {
                    report.ignored += 1;
                    return;
                }
                self.header.active_run_id = None;
                self.store.active_run = None;
                let stop = text("stopReason").unwrap_or("end_turn").to_owned();
                let cancelled = stop == "cancelled";
                self.header.last_stop_reason = Some(stop);
                self.set_run(if cancelled {
                    RunState::Cancelled
                } else {
                    RunState::Idle
                });
            }
            other => {
                reject(format!("unknown state `{other}`"));
                return;
            }
        }
        self.state_updates_seen = true;
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
            format!("user-pending-{client_prompt_id}"),
            EntryKind::UserMessage(UserMessage {
                message_id: None,
                client_prompt_id: Some(client_prompt_id.to_owned()),
                blocks: blocks.to_vec(),
                status: PromptStatus::Pending,
                error: None,
                at_ms: Some(at_ms),
            }),
            TurnHint::Pending(client_prompt_id.to_owned()),
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
    ///
    /// The message id the response names links a pending prompt whose echo
    /// carried no client prompt id: the agent's user message stands for it.
    /// The stop reason decides the run state only while the agent sends no
    /// state updates.
    pub(crate) fn prompt_completed(
        &mut self,
        client_prompt_id: Option<&str>,
        message_id: Option<&str>,
        stop_reason: &StopReason,
    ) {
        if let (Some(prompt), Some(message)) = (client_prompt_id, message_id)
            && let Some(user) = self.store.idx.user.get(message).copied()
            && self
                .store
                .idx
                .prompt
                .get(prompt)
                .is_some_and(|key| *key != user)
        {
            self.drop_unsent_prompt(prompt);
        }
        if self.state_updates_seen {
            return;
        }
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

    /// Remove a client-local prompt (pending or failed) and its turn. The
    /// agent's own message for the same client prompt id stands for it: a
    /// prompt marked failed that the agent did receive must not stay behind.
    pub(crate) fn drop_unsent_prompt(&mut self, client_prompt_id: &str) {
        let Some(key) = self.store.idx.prompt.get(client_prompt_id).copied() else {
            return;
        };
        let unsent = matches!(
            self.store.kind(key),
            Some(EntryKind::UserMessage(user))
                if matches!(user.status, PromptStatus::Pending | PromptStatus::Failed)
        );
        if unsent {
            self.store.idx.prompt.remove(client_prompt_id);
            self.store.remove(key);
        }
    }
}

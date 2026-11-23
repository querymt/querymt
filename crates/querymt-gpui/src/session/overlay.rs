//! Client-only session state the reducer deliberately leaves out (design D3):
//! notices anchored to turns, load and fetch state, modes, access
//! and steer/queue inputs the agent has not confirmed yet. Active time is the
//! core's (`header.active_time`), not kept here.

#![allow(dead_code)]

use std::collections::HashMap;

use super::{ContentFetch, PendingInput};

/// A client-side note in the transcript. It follows the turn that was last
/// when it was pushed, so it keeps its place when older pages arrive.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub key: String,
    pub text: String,
    pub error: bool,
    /// Turn id the note follows; `None`: it was pushed before any turn.
    pub anchor: Option<String>,
}

#[derive(Default)]
pub struct Overlay {
    pub notices: Vec<Notice>,
    notice_counter: u64,
    // Plain load, history, modes, access and option state (`loading`,
    // `error`, `history_loading`, `modes`, `access`, `pending_options`,
    // `undo_pending`, ...) are public fields of `SessionView` itself: they
    // carry no logic, so wrapping them here would only add indirection.
    /// Progress of fetching a truncated tool call's full content, by tool call id.
    pub fetches: HashMap<String, ContentFetch>,
    /// Steer and queue inputs submitted here that the agent has not listed
    /// yet, in submission order.
    inputs: Vec<PendingInput>,
    /// Queued inputs whose removal is in flight.
    discarding: std::collections::HashSet<String>,
    /// Lifecycle states that arrived before the submit response re-keyed the
    /// input: input id → (delivery, state).
    pub input_notes: HashMap<String, (String, String)>,
}

impl Overlay {
    /// Add a note after `anchor` (the last turn id, if any).
    pub fn push_notice(&mut self, text: impl Into<String>, error: bool, anchor: Option<String>) {
        self.notice_counter += 1;
        self.notices.push(Notice {
            key: format!("notice-{}", self.notice_counter),
            text: text.into(),
            error,
            anchor,
        });
    }

    /// Settle the inputs to match `running`.
    pub fn sync_inputs(&mut self, running: bool) {
        if !running {
            // Nothing can still be waiting once the session is idle; only an
            // input whose request is still in flight stays.
            self.inputs.retain(|i| i.state == "sending");
            self.input_notes.clear();
        }
    }

    /// Track a message sent mid-run until the agent applies or starts it.
    pub fn add_input(&mut self, input: PendingInput) {
        self.inputs.retain(|i| i.input_id != input.input_id);
        self.inputs.push(input);
    }

    /// Re-key an input when the agent assigns its own input id.
    pub fn rekey_input(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        if let Some(input) = self.inputs.iter_mut().find(|i| i.input_id == from) {
            input.input_id = to.into();
        }
    }

    /// Update (or drop, once terminal) an input from a lifecycle state:
    /// `accepted`, `queued`, `applied`, `started`, `discarded`, `failed`.
    pub fn set_input_state(&mut self, input_id: &str, delivery: &str, state: &str) {
        if matches!(state, "applied" | "started" | "discarded" | "failed") {
            self.inputs.retain(|i| i.input_id != input_id);
            return;
        }
        if let Some(input) = self.inputs.iter_mut().find(|i| i.input_id == input_id) {
            input.state = state.into();
            if !delivery.is_empty() {
                input.delivery = delivery.into();
            }
        }
    }

    /// A lifecycle notification: applied at once when this input is known,
    /// otherwise kept until the submit response gives it its agent id.
    pub fn note_input_state(&mut self, input_id: &str, delivery: &str, state: &str) {
        if self.inputs.iter().any(|i| i.input_id == input_id) {
            self.set_input_state(input_id, delivery, state);
        } else {
            self.input_notes
                .insert(input_id.into(), (delivery.into(), state.into()));
        }
    }

    /// Apply the buffered state of `input_id` (or of its former id), if any.
    pub fn apply_input_notes(&mut self, input_id: &str, former_id: &str) {
        if let Some((delivery, state)) = self
            .input_notes
            .remove(input_id)
            .or_else(|| self.input_notes.remove(former_id))
        {
            self.set_input_state(input_id, &delivery, &state);
        }
    }

    /// Mark a queued input's removal as in flight (or not).
    pub fn set_discarding(&mut self, input_id: &str, discarding: bool) {
        if discarding {
            self.discarding.insert(input_id.to_owned());
        } else {
            self.discarding.remove(input_id);
        }
    }

    pub fn has_input(&self, input_id: &str) -> bool {
        self.inputs.iter().any(|i| i.input_id == input_id)
    }

    /// The core's pending inputs first, then the ones only this client knows.
    /// An input both list shows once, in the core's state with the text the
    /// client kept (the agent's early notification carries none).
    pub fn merge_inputs(&self, core: Vec<PendingInput>) -> Vec<PendingInput> {
        let mut merged: Vec<PendingInput> = core
            .into_iter()
            .map(|mut input| {
                if let Some(local) = self.inputs.iter().find(|i| i.input_id == input.input_id)
                    && input.text.is_empty()
                    && input.attachments == 0
                {
                    input.text.clone_from(&local.text);
                    input.attachments = local.attachments;
                }
                input
            })
            .collect();
        for local in &self.inputs {
            if !merged.iter().any(|i| i.input_id == local.input_id) {
                merged.push(local.clone());
            }
        }
        for input in &mut merged {
            input.discard_pending = self.discarding.contains(&input.input_id);
        }
        merged
    }
}

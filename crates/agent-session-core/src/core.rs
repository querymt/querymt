//! `SessionCore`: the single writer of the session view.

use crate::SCHEMA_VERSION;
use crate::bounded::{DebugEntry, DebugLog, DebugRing, SeenLive};
use crate::decode::{Decoded, decode_notification};
use crate::entry::{Entry, Turn};
use crate::input::SessionInput;
use crate::store::{Store, TurnRef};
use crate::view::{ChangeReport, InputError, SessionHeader};

/// The session view and the state needed to keep it current.
///
/// Every input goes through [`SessionCore::apply`]; the accessors only read.
#[derive(Debug)]
pub struct SessionCore {
    pub(crate) header: SessionHeader,
    pub(crate) store: Store,
    pub(crate) debug: DebugRing,
    pub(crate) seen: SeenLive,
    /// History positions of events already applied from a snapshot or page.
    pub(crate) seqs: std::collections::BTreeSet<i64>,
    /// The agent sent a state update: from then on state updates decide
    /// whether a run is active, and prompt outcomes and runtime observations
    /// no longer do.
    pub(crate) state_updates_seen: bool,
    /// The active run's start is on the host's clock (a snapshot's run start
    /// or a state update's `atMs`). A start taken from the client's clock is
    /// never paired with a host time, nor the reverse.
    pub(crate) active_start_is_host: bool,
}

// A client may hydrate on a background thread and move the view to its UI
// thread, so the core must stay `Send + 'static` (no `Rc`, no `RefCell`).
const _: fn() = || {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<SessionCore>();
};

impl SessionCore {
    /// An empty view for `session_id`.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            header: SessionHeader {
                session_id: session_id.into(),
                schema_version: SCHEMA_VERSION,
                ..SessionHeader::default()
            },
            store: Store::new(),
            debug: DebugRing::default(),
            seen: SeenLive::default(),
            seqs: std::collections::BTreeSet::new(),
            state_updates_seen: false,
            active_start_is_host: false,
        }
    }

    /// Apply a batch in order and report what changed.
    ///
    /// Never panics and never aborts the batch: a bad input becomes an entry
    /// in [`ChangeReport::errors`] and leaves the view unchanged.
    pub fn apply(&mut self, batch: &[SessionInput]) -> ChangeReport {
        let header_before = self.header.clone();
        let mut report = ChangeReport::default();
        for (index, input) in batch.iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            self.debug.push(describe(input));
            self.apply_one(index, input, &mut report);
        }
        // Entities arrive in any order: place cursors against the final view.
        self.refresh_cursor_placement();
        let changes = self.store.take_changes();
        report.added = changes.added;
        report.updated = changes.updated;
        report.removed = changes.removed;
        report.header_changed = self.header != header_before;
        report
    }

    fn apply_one(&mut self, index: u32, input: &SessionInput, report: &mut ChangeReport) {
        match input {
            SessionInput::AcpNotification {
                notification,
                replayed,
                ..
            } => {
                let hash = SeenLive::hash(&notification.to_string());
                if *replayed && self.seen.contains(hash) {
                    report.duplicates += 1;
                    return;
                }
                match decode_notification(notification) {
                    Decoded::Unknown => report.unknown += 1,
                    Decoded::Error(message) => report.errors.push(InputError {
                        input_index: index,
                        seq: None,
                        message,
                    }),
                    Decoded::Value(note) if note.session_id != self.header.session_id => {
                        report.ignored += 1;
                    }
                    Decoded::Value(note) => {
                        // Only live notifications count as already applied.
                        if !*replayed {
                            self.seen.remember(hash);
                        }
                        self.apply_update(note.update);
                    }
                }
            }
            SessionInput::SnapshotLoaded { response } => {
                self.snapshot_loaded(response, index, report);
            }
            SessionInput::HistoryPage { response } => {
                self.history_page(response, index, report);
            }
            SessionInput::PromptOptimistic {
                client_prompt_id,
                blocks,
                at_ms,
            } => self.prompt_optimistic(client_prompt_id, blocks, *at_ms),
            SessionInput::PromptFailed {
                client_prompt_id,
                message,
            } => self.prompt_failed(client_prompt_id, message),
            SessionInput::PromptCompleted {
                client_prompt_id,
                message_id,
                stop_reason,
                ..
            } => {
                self.prompt_completed(
                    client_prompt_id.as_deref(),
                    message_id.as_deref(),
                    stop_reason,
                );
            }
            SessionInput::StateUpdate {
                notification,
                at_ms,
            } => {
                self.state_update(notification, *at_ms, index, report);
            }
            SessionInput::RuntimeObserved { status } => self.runtime_observed(status),
            SessionInput::ToolContent {
                tool_call_id,
                part,
                content,
            } => self.tool_content(tool_call_id, *part, content),
            SessionInput::InputState { notification } => self.input_state(notification),
            SessionInput::UndoStack { frames } => self.undo_stack(frames),
            SessionInput::ConfigOptions { options } => {
                self.merge_config_options(options.clone());
            }
        }
    }

    /// The session header (everything except the turns).
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Turn ids in history order.
    pub fn turn_ids(&self) -> Vec<String> {
        self.store.turn_ids()
    }

    /// Turn ids in history order, borrowed. Copies nothing.
    pub fn turn_order(&self) -> impl ExactSizeIterator<Item = &str> {
        self.store.turn_order()
    }

    /// One turn read in place, or `None` for an unknown id. Copies no entry
    /// content; the borrow ends before the next `apply`.
    pub fn turn_ref(&self, id: &str) -> Option<TurnRef<'_>> {
        self.store.turn_ref(id)
    }

    /// One entry by id, or `None` for an unknown id.
    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.store.entry(id)
    }

    /// One turn, or `None` for an unknown id.
    pub fn turn(&self, id: &str) -> Option<Turn> {
        self.store.turn(id)
    }

    /// The turns for `ids`, skipping unknown ids.
    pub fn turns<S: AsRef<str>>(&self, ids: &[S]) -> Vec<Turn> {
        ids.iter()
            .filter_map(|id| self.store.turn(id.as_ref()))
            .collect()
    }

    /// The bounded log of applied inputs.
    pub fn debug_log(&self) -> DebugLog {
        self.debug.snapshot()
    }
}

/// Kind, short summary and message id of an input, for the debug log.
fn describe(input: &SessionInput) -> DebugEntry {
    let (kind, summary, message_id) = match input {
        SessionInput::SnapshotLoaded { .. } => ("snapshot_loaded", String::new(), None),
        SessionInput::AcpNotification { notification, .. } => {
            let params = notification.get("params").unwrap_or(notification);
            let update = params.get("update").unwrap_or(params);
            let kind = update
                .get("sessionUpdate")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let message_id = update
                .get("messageId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            ("acp_notification", kind.to_owned(), message_id)
        }
        SessionInput::HistoryPage { .. } => ("history_page", String::new(), None),
        SessionInput::PromptOptimistic {
            client_prompt_id, ..
        } => ("prompt_optimistic", client_prompt_id.clone(), None),
        SessionInput::PromptCompleted { message_id, .. } => {
            ("prompt_completed", String::new(), message_id.clone())
        }
        SessionInput::StateUpdate { notification, .. } => (
            "state_update",
            notification
                .get("state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            None,
        ),
        SessionInput::PromptFailed {
            client_prompt_id, ..
        } => ("prompt_failed", client_prompt_id.clone(), None),
        SessionInput::RuntimeObserved { .. } => ("runtime_observed", String::new(), None),
        SessionInput::InputState { notification } => {
            ("input_state", notification.input_id.clone(), None)
        }
        SessionInput::UndoStack { frames } => {
            ("undo_stack", format!("{} frames", frames.len()), None)
        }
        SessionInput::ConfigOptions { options } => {
            ("config_options", format!("{} options", options.len()), None)
        }
        SessionInput::ToolContent { tool_call_id, .. } => {
            ("tool_content", tool_call_id.clone(), None)
        }
    };
    DebugEntry {
        kind: kind.to_owned(),
        summary,
        message_id,
    }
}

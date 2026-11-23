//! Item store: entries keyed by a stable position, grouped into turns.
//!
//! One `Entry` is one store item. A turn is one agent run (`run-<runId>`):
//! an entry joins the turn of the run id it carries, or of the run that is
//! running, and an agent that reports no run ids at all gets a turn at each
//! user message and compaction instead. Client-local pending prompts are
//! their own `pending-<clientPromptId>` turns after the last run turn.
//! Every change takes a revision from one view-wide counter, so a client can
//! cache per turn or per entry.

use crate::entry::{Entry, EntryKind, Turn};
use std::collections::{BTreeMap, VecDeque};

/// Prefix of the ids of client-local pending prompt turns.
pub(crate) const PENDING_PREFIX: &str = "pending-";

/// How a new entry finds its turn when no run applies, and where a turn it
/// opens gets its id from.
#[derive(Debug, Clone)]
pub(crate) enum TurnHint {
    /// Client-local pending prompt (client prompt id): its own turn at the end.
    Pending(String),
    /// An agent message (message id): a turn it opens is named after it.
    Message(String),
    /// Nothing to name a turn after; the history position or a counter does.
    Live,
}

/// Lookup maps so one update finds its entry without scanning.
#[derive(Debug, Default)]
pub(crate) struct Indexes {
    pub tool: BTreeMap<String, i64>,
    pub assistant: BTreeMap<String, i64>,
    pub user: BTreeMap<String, i64>,
    pub prompt: BTreeMap<String, i64>,
    pub compaction: BTreeMap<String, i64>,
    pub reasoning: BTreeMap<(String, String), i64>,
}

#[derive(Debug)]
struct Slot {
    turn: String,
    entry: Entry,
}

#[derive(Debug)]
struct TurnState {
    revision: u64,
    keys: Vec<i64>,
}

/// Turns and entries touched since the last [`Store::take_changes`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StoreChanges {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Store {
    items: BTreeMap<i64, Slot>,
    turns: BTreeMap<String, TurnState>,
    order: VecDeque<String>,
    /// Lowest key in use (keys grow downward when a page is prepended).
    lo: i64,
    /// Next key for an appended entry.
    hi: i64,
    rev: u64,
    anon: u64,
    pub idx: Indexes,
    /// History position of the event being replayed; live hints use it for
    /// turn ids while it is set.
    pub seq_hint: Option<i64>,
    /// The run the session is running (from state updates, the runtime
    /// status, or `RunStarted` while a journal is replayed).
    pub active_run: Option<String>,
    /// The run id the update being applied carries; it wins over `active_run`.
    pub update_run: Option<String>,
    added: Vec<String>,
    updated: Vec<String>,
    removed: Vec<String>,
}

fn push_unique(list: &mut Vec<String>, id: &str) {
    if !list.iter().any(|existing| existing == id) {
        list.push(id.to_owned());
    }
}

impl Store {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A store that continues another store's revision counter.
    pub(crate) fn continuing(rev: u64, anon: u64) -> Self {
        Self {
            rev,
            anon,
            ..Self::default()
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.rev
    }

    pub(crate) fn anon_counter(&self) -> u64 {
        self.anon
    }

    /// A deterministic counter for ids the agent did not provide.
    pub(crate) fn next_anon(&mut self) -> u64 {
        self.anon += 1;
        self.anon
    }

    /// An id for a turn that no run names: after the message that opens it,
    /// else after the history position, else from a counter.
    fn fallback_turn_id(&mut self, hint: &TurnHint) -> String {
        let base = match hint {
            TurnHint::Message(id) => format!("turn-{id}"),
            TurnHint::Pending(id) => format!("{PENDING_PREFIX}{id}"),
            TurnHint::Live => match self.seq_hint {
                Some(seq) => format!("turn-{seq}"),
                None => format!("turn-live-{}", self.next_anon()),
            },
        };
        if !self.turns.contains_key(&base) {
            return base;
        }
        loop {
            let candidate = format!("{base}-{}", self.next_anon());
            if !self.turns.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    fn is_pending(id: &str) -> bool {
        id.starts_with(PENDING_PREFIX)
    }

    /// Number of pending prompt turns at the end of the order.
    fn trailing_pending(&self) -> usize {
        self.order
            .iter()
            .rev()
            .take_while(|id| Self::is_pending(id))
            .count()
    }

    /// The newest turn that is not a pending prompt.
    fn last_run_turn(&self) -> Option<&String> {
        self.order.iter().rev().find(|id| !Self::is_pending(id))
    }

    /// Create a turn: pending prompts go last, every other turn goes before
    /// the pending ones.
    fn open_turn(&mut self, id: String) {
        self.turns.insert(
            id.clone(),
            TurnState {
                revision: self.rev,
                keys: Vec::new(),
            },
        );
        if Self::is_pending(&id) {
            self.order.push_back(id.clone());
        } else {
            let at = self.order.len() - self.trailing_pending();
            self.order.insert(at, id.clone());
        }
        push_unique(&mut self.added, &id);
    }

    /// Append an entry and return its key.
    ///
    /// It joins the turn of the run it carries, or of the running run; the
    /// turn is opened by its first entry. Without any run (an agent that
    /// reports none) a user message or a compaction opens a turn and other
    /// entries join the last one.
    pub(crate) fn push(&mut self, id: String, kind: EntryKind, hint: TurnHint) -> i64 {
        let opens_turn = matches!(kind, EntryKind::UserMessage(_) | EntryKind::Compaction(_));
        let key = self.hi;
        self.hi += 1;
        self.rev += 1;
        let run = self.update_run.clone().or_else(|| self.active_run.clone());
        let target = match (&hint, run) {
            (TurnHint::Pending(_), _) => self.fallback_turn_id(&hint),
            (_, Some(run)) => format!("run-{run}"),
            (_, None) => match self.last_run_turn() {
                Some(last) if !opens_turn => last.clone(),
                _ => self.fallback_turn_id(&hint),
            },
        };
        let turn_id = if self.turns.contains_key(&target) {
            push_unique(&mut self.updated, &target);
            target
        } else {
            self.open_turn(target.clone());
            target
        };
        if let Some(turn) = self.turns.get_mut(&turn_id) {
            turn.revision = self.rev;
            turn.keys.push(key);
        }
        self.items.insert(
            key,
            Slot {
                turn: turn_id,
                entry: Entry {
                    id,
                    revision: self.rev,
                    kind,
                },
            },
        );
        key
    }

    /// Change an entry in place. `change` returns `true` when it changed
    /// something; only then do the entry and its turn get a new revision.
    /// Returns `false` when the key is unknown.
    pub(crate) fn modify(&mut self, key: i64, change: impl FnOnce(&mut EntryKind) -> bool) -> bool {
        let Some(slot) = self.items.get_mut(&key) else {
            return false;
        };
        if change(&mut slot.entry.kind) {
            self.rev += 1;
            slot.entry.revision = self.rev;
            if let Some(turn) = self.turns.get_mut(&slot.turn) {
                turn.revision = self.rev;
            }
            push_unique(&mut self.updated, &slot.turn);
        }
        true
    }

    pub(crate) fn kind(&self, key: i64) -> Option<&EntryKind> {
        self.items.get(&key).map(|slot| &slot.entry.kind)
    }

    /// Key of the newest entry of the newest run turn.
    pub(crate) fn last_key(&self) -> Option<i64> {
        let turn = self.turns.get(self.last_run_turn()?)?;
        turn.keys.last().copied()
    }

    /// Remove one entry; its turn goes with it when it becomes empty. The
    /// caller fixes the lookup maps.
    pub(crate) fn remove(&mut self, key: i64) {
        let Some(slot) = self.items.remove(&key) else {
            return;
        };
        let turn_id = slot.turn;
        let empty = match self.turns.get_mut(&turn_id) {
            Some(turn) => {
                turn.keys.retain(|k| *k != key);
                self.rev += 1;
                turn.revision = self.rev;
                turn.keys.is_empty()
            }
            None => return,
        };
        if !empty {
            push_unique(&mut self.updated, &turn_id);
            return;
        }
        self.turns.remove(&turn_id);
        self.order.retain(|id| *id != turn_id);
        self.updated.retain(|id| *id != turn_id);
        if self.added.contains(&turn_id) {
            // Never reported: nothing to remove for the client.
            self.added.retain(|id| *id != turn_id);
        } else {
            push_unique(&mut self.removed, &turn_id);
        }
    }

    /// Every turn, oldest first.
    pub(crate) fn all_turns(&self) -> Vec<Turn> {
        self.order.iter().filter_map(|id| self.turn(id)).collect()
    }

    /// Ids of the client-local pending turns, in order.
    pub(crate) fn pending_turn_ids(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| Self::is_pending(id))
            .cloned()
            .collect()
    }

    pub(crate) fn turn_ids(&self) -> Vec<String> {
        self.order.iter().cloned().collect()
    }

    pub(crate) fn turn(&self, id: &str) -> Option<Turn> {
        let state = self.turns.get(id)?;
        let entries = state
            .keys
            .iter()
            .filter_map(|key| self.items.get(key))
            .map(|slot| slot.entry.clone())
            .collect();
        Some(Turn {
            id: id.to_owned(),
            revision: state.revision,
            entries,
        })
    }

    /// Put every turn of `other` (built from an older page) in front of this
    /// store's turns. Existing entries and index slots win; the page's turns
    /// are reported as added.
    pub(crate) fn prepend(&mut self, mut other: Store) {
        let shift = self.lo - other.hi;
        for (key, slot) in std::mem::take(&mut other.items) {
            self.items.insert(key + shift, slot);
        }
        for (id, mut state) in std::mem::take(&mut other.turns) {
            for key in &mut state.keys {
                *key += shift;
            }
            self.turns.insert(id, state);
        }
        for id in std::mem::take(&mut other.order).into_iter().rev() {
            self.order.push_front(id);
        }
        self.lo -= other.hi;
        fn merge<K: Ord>(into: &mut BTreeMap<K, i64>, from: BTreeMap<K, i64>, shift: i64) {
            for (key, value) in from {
                into.entry(key).or_insert(value + shift);
            }
        }
        merge(&mut self.idx.tool, other.idx.tool, shift);
        merge(&mut self.idx.assistant, other.idx.assistant, shift);
        merge(&mut self.idx.user, other.idx.user, shift);
        merge(&mut self.idx.prompt, other.idx.prompt, shift);
        merge(&mut self.idx.compaction, other.idx.compaction, shift);
        merge(&mut self.idx.reasoning, other.idx.reasoning, shift);
        self.rev = self.rev.max(other.rev);
        self.anon = self.anon.max(other.anon);
        for id in other.added {
            push_unique(&mut self.added, &id);
        }
    }

    /// Mark this freshly built store as the replacement of one that held
    /// `old`: new ids are added, gone ids removed, and a turn with the same
    /// id keeps its revisions and is not reported when its content (entry ids
    /// and kinds) is unchanged.
    pub(crate) fn finish_rebuild(&mut self, old: &[Turn]) {
        let mut added = Vec::new();
        let mut updated = Vec::new();
        let order: Vec<String> = self.order.iter().cloned().collect();
        for id in &order {
            let Some(before) = old.iter().find(|turn| turn.id == *id) else {
                added.push(id.clone());
                continue;
            };
            let unchanged = self.turns.get(id).is_some_and(|state| {
                state.keys.len() == before.entries.len()
                    && state
                        .keys
                        .iter()
                        .zip(&before.entries)
                        .all(|(key, old_entry)| {
                            self.items.get(key).is_some_and(|slot| {
                                slot.entry.id == old_entry.id && slot.entry.kind == old_entry.kind
                            })
                        })
            });
            if !unchanged {
                updated.push(id.clone());
                continue;
            }
            // Same content: keep the revisions a client may have cached.
            if let Some(state) = self.turns.get_mut(id) {
                state.revision = before.revision;
                for (key, old_entry) in state.keys.iter().zip(&before.entries) {
                    if let Some(slot) = self.items.get_mut(key) {
                        slot.entry.revision = old_entry.revision;
                    }
                }
            }
        }
        self.added = added;
        self.updated = updated;
        self.removed = old
            .iter()
            .filter(|turn| !self.turns.contains_key(&turn.id))
            .map(|turn| turn.id.clone())
            .collect();
    }

    /// Take the turn ids changed since the last call. A turn added and then
    /// updated in the same window is reported as added only.
    pub(crate) fn take_changes(&mut self) -> StoreChanges {
        let added = std::mem::take(&mut self.added);
        let mut updated = std::mem::take(&mut self.updated);
        let removed = std::mem::take(&mut self.removed);
        updated.retain(|id| !added.contains(id) && self.turns.contains_key(id));
        StoreChanges {
            added,
            updated,
            removed,
        }
    }
}

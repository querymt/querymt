//! Item store: entries keyed by a stable position, grouped into turns.
//!
//! One `Entry` is one store item. A turn starts at a user message or a
//! compaction boundary and holds every following entry until the next start.
//! Every change takes a revision from one view-wide counter, so a client can
//! cache per turn or per entry.

use crate::entry::{Entry, EntryKind, Turn};
use std::collections::{BTreeMap, VecDeque};

/// Where a new turn's id comes from.
#[derive(Debug, Clone)]
pub(crate) enum TurnHint {
    /// Client prompt id of an optimistic prompt.
    Prompt(String),
    /// A live notification with no history position.
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

    fn turn_id_for(&mut self, hint: TurnHint) -> String {
        let base = match hint {
            TurnHint::Prompt(id) => format!("turn-{id}"),
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

    /// Append an entry. User messages and compactions open a new turn; other
    /// entries join the last turn (or open one when the store is empty).
    /// Returns the entry's key.
    pub(crate) fn push(&mut self, id: String, kind: EntryKind, hint: TurnHint) -> i64 {
        let opens_turn = matches!(kind, EntryKind::UserMessage(_) | EntryKind::Compaction(_));
        let key = self.hi;
        self.hi += 1;
        self.rev += 1;
        let turn_id = match self.order.back() {
            Some(last) if !opens_turn => {
                let last = last.clone();
                push_unique(&mut self.updated, &last);
                last
            }
            _ => {
                let new_id = self.turn_id_for(hint);
                self.turns.insert(
                    new_id.clone(),
                    TurnState {
                        revision: self.rev,
                        keys: Vec::new(),
                    },
                );
                self.order.push_back(new_id.clone());
                push_unique(&mut self.added, &new_id);
                new_id
            }
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

    /// Key of the newest entry of the newest turn.
    pub(crate) fn last_key(&self) -> Option<i64> {
        let turn = self.turns.get(self.order.back()?)?;
        turn.keys.last().copied()
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
    /// `old_ids`: ids in both are updated, new ids added, gone ids removed.
    pub(crate) fn finish_rebuild(&mut self, old_ids: &[String]) {
        let mut added = Vec::new();
        let mut updated = Vec::new();
        for id in &self.order {
            if old_ids.contains(id) {
                updated.push(id.clone());
            } else {
                added.push(id.clone());
            }
        }
        self.added = added;
        self.updated = updated;
        self.removed = old_ids
            .iter()
            .filter(|id| !self.turns.contains_key(*id))
            .cloned()
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

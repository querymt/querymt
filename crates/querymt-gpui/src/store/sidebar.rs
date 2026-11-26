//! The sidebar's session lists (desktop `RecentSessionRail`): pinned and
//! open sessions in a stable, tab-like order. Sessions that appear while the
//! app runs go on top; the rest keep their place however their activity
//! changes, and the user can drag them around. The order is saved.
//!
//! Also what needs the user's attention there: sessions waiting on a
//! permission or a question, and agents that failed or lost their connection.

use std::collections::{HashMap, HashSet};

use gpui_kit::*;

use super::{AgentsStore, ConnState, SessionSummary};

/// Which sidebar list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SideList {
    Pinned,
    Open,
}

/// Placed positions kept per list beyond the sessions present: room for
/// sessions of a disconnected agent to come back to their place.
const KEEP: usize = 256;
/// Present sessions given a fixed place in the open list; the rest follow by
/// activity, far below anything shown.
const HEAD: usize = 200;

/// `agent/session`.
pub fn tab_key(s: &SessionSummary) -> String {
    format!("{}/{}", s.agent_id, s.id)
}

/// One session offered to [`arrange`].
pub struct Candidate {
    pub key: String,
    pub updated_at: String,
    /// Its agent is still loading the catalog: it is not new, just arriving.
    pub settling: bool,
}

/// Bring `order` up to date with `items` and return the keys of `items` in
/// tab order. New sessions (not `known`, agent settled) go on top, newest
/// first; sessions that left while their agent was settled drop out, so they
/// count as new if they come back. `settled(key)`: the key's agent has its
/// catalog loaded. Returns whether `order` changed.
pub fn arrange(
    order: &mut Vec<String>,
    known: &mut HashSet<String>,
    items: &[Candidate],
    settled: impl Fn(&str) -> bool,
) -> (Vec<String>, bool) {
    let before = order.clone();
    let present: HashMap<&str, &Candidate> = items.iter().map(|c| (c.key.as_str(), c)).collect();

    // Gone while its agent was there to list it: forget the place.
    order.retain(|k| {
        let keep = present.contains_key(k.as_str()) || !settled(k);
        if !keep {
            known.remove(k);
        }
        keep
    });
    known.retain(|k| present.contains_key(k.as_str()) || !settled(k));

    // New on top, newest first.
    let in_order: HashSet<&str> = order.iter().map(String::as_str).collect();
    let mut fresh: Vec<&Candidate> = items
        .iter()
        .filter(|c| !c.settling && !known.contains(&c.key))
        .filter(|c| !in_order.contains(c.key.as_str()))
        .collect();
    drop(in_order);
    fresh.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    for (ix, c) in fresh.iter().enumerate() {
        order.insert(ix, c.key.clone());
    }
    known.extend(items.iter().map(|c| c.key.clone()));

    // Placed sessions in their order, then the rest by activity.
    let placed: HashSet<&str> = order.iter().map(String::as_str).collect();
    let mut rest: Vec<&Candidate> = items
        .iter()
        .filter(|c| !placed.contains(c.key.as_str()))
        .collect();
    rest.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let mut out: Vec<String> = order
        .iter()
        .filter(|k| present.contains_key(k.as_str()))
        .cloned()
        .collect();
    out.extend(rest.iter().map(|c| c.key.clone()));

    // Fix the place of what is shown (the head), so activity cannot move it.
    let placed_present = out.iter().filter(|k| placed.contains(k.as_str())).count();
    if placed_present < HEAD.min(out.len()) {
        let add = out
            .iter()
            .filter(|k| !placed.contains(k.as_str()))
            .take(HEAD.min(out.len()) - placed_present)
            .cloned()
            .collect::<Vec<_>>();
        order.extend(add);
    }
    order.truncate(KEEP);
    let changed = *order != before;
    (out, changed)
}

/// Put `key` just before `target`, or just after it with `after`.
/// Returns whether the order changed.
pub fn move_key(
    order: &mut Vec<String>,
    shown: &[String],
    key: &str,
    target: &str,
    after: bool,
) -> bool {
    if key == target || !shown.iter().any(|k| k == key) {
        return false;
    }
    // Shown rows all have a place (see `arrange`); keep any others.
    let mut next: Vec<String> = shown.iter().filter(|k| *k != key).cloned().collect();
    let Some(at) = next.iter().position(|k| k == target) else {
        return false;
    };
    next.insert(at + usize::from(after), key.to_string());
    if next == shown {
        return false;
    }
    let others: Vec<String> = order
        .iter()
        .filter(|k| !shown.contains(k))
        .cloned()
        .collect();
    next.extend(others);
    *order = next;
    order.truncate(KEEP);
    true
}

impl super::Prefs {
    pub fn sidebar_order(&self, list: SideList) -> &Vec<String> {
        match list {
            SideList::Pinned => &self.sidebar_pinned_order,
            SideList::Open => &self.sidebar_open_order,
        }
    }

    fn sidebar_order_mut(&mut self, list: SideList) -> &mut Vec<String> {
        match list {
            SideList::Pinned => &mut self.sidebar_pinned_order,
            SideList::Open => &mut self.sidebar_open_order,
        }
    }
}

/// Sidebar state that is not saved: the sessions already seen this run.
#[derive(Default)]
pub struct SidebarState {
    known: HashMap<SideList, HashSet<String>>,
    /// [`AgentsStore::sidebar_fingerprint`] at the last sync.
    fingerprint: Option<u64>,
}

impl AgentsStore {
    fn sidebar_candidates(&self, list: SideList) -> Vec<&SessionSummary> {
        let show_pinned = self.prefs.show_pinned_sidebar;
        self.all_sessions()
            .into_iter()
            .filter(|s| match list {
                SideList::Pinned => show_pinned && s.root && s.pinned && !s.archived,
                SideList::Open => s.root && s.open && !(show_pinned && s.pinned),
            })
            .collect()
    }

    /// The agent of `agent` has listed all its sessions.
    fn catalog_settled(&self, agent: &str) -> bool {
        self.runtime
            .get(agent)
            .is_some_and(|r| r.is_connected() && r.sessions_loaded && !r.catalog_loading)
    }

    /// What the orders depend on: which sessions are listed where, and
    /// which agents have settled. Cheap, so a frame can check it.
    fn sidebar_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.prefs.show_pinned_sidebar.hash(&mut h);
        for c in &self.configs {
            (c.id.as_str(), self.catalog_settled(&c.id)).hash(&mut h);
        }
        for s in self.all_sessions() {
            (s.agent_id.as_str(), s.id.as_str()).hash(&mut h);
            (s.root, s.open, s.pinned, s.archived).hash(&mut h);
        }
        h.finish()
    }

    /// Update the saved orders with sessions that came and went. Saves when
    /// an order changed; does nothing when no session came or went.
    pub fn sync_sidebar_order(&mut self) {
        let fingerprint = self.sidebar_fingerprint();
        if self.sidebar.fingerprint == Some(fingerprint) {
            return;
        }
        self.sidebar.fingerprint = Some(fingerprint);
        let mut changed = false;
        for list in [SideList::Pinned, SideList::Open] {
            let candidates: Vec<Candidate> = self
                .sidebar_candidates(list)
                .into_iter()
                .map(|s| Candidate {
                    key: tab_key(s),
                    updated_at: s.updated_at.clone(),
                    settling: !self.catalog_settled(&s.agent_id),
                })
                .collect();
            let settled_agents: HashSet<String> = self
                .configs
                .iter()
                .filter(|c| self.catalog_settled(&c.id))
                .map(|c| c.id.clone())
                .collect();
            let mut order = self.prefs.sidebar_order(list).clone();
            let mut known = self.sidebar.known.remove(&list).unwrap_or_default();
            let (_, moved) = arrange(&mut order, &mut known, &candidates, |k| {
                k.split_once('/')
                    .is_some_and(|(agent, _)| settled_agents.contains(agent))
            });
            drop(candidates);
            self.sidebar.known.insert(list, known);
            if moved {
                *self.prefs.sidebar_order_mut(list) = order;
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }

    /// The sessions of `list` in tab order, as last synced.
    pub fn sidebar_sessions(&self, list: SideList) -> Vec<&SessionSummary> {
        let items: Vec<&SessionSummary> = self.sidebar_candidates(list);
        let order = self.prefs.sidebar_order(list);
        // By `(agent, session)`: no key is formatted per session per frame.
        let rank: HashMap<(&str, &str), usize> = order
            .iter()
            .enumerate()
            .filter_map(|(ix, k)| Some((k.split_once('/')?, ix)))
            .collect();
        let place = |s: &SessionSummary| rank.get(&(s.agent_id.as_str(), s.id.as_str())).copied();
        let mut ix: Vec<usize> = (0..items.len()).collect();
        ix.sort_by(|&a, &b| match (place(items[a]), place(items[b])) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => items[b].updated_at.cmp(&items[a].updated_at),
        });
        ix.into_iter().map(|i| items[i]).collect()
    }

    /// Drop `key` before `target` in `list`, or after it with `after`.
    pub fn move_sidebar_session(
        &mut self,
        list: SideList,
        key: &str,
        target: &str,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        let shown: Vec<String> = self
            .sidebar_sessions(list)
            .into_iter()
            .map(tab_key)
            .collect();
        let mut order = self.prefs.sidebar_order(list).clone();
        if move_key(&mut order, &shown, key, target, after) {
            self.update_prefs(|p| *p.sidebar_order_mut(list) = order, cx);
        }
    }

    /// Where `key` sits in `list`: `(index, rows)`, or `None` when not there.
    pub fn sidebar_place(&self, list: SideList, key: &str) -> Option<(usize, usize)> {
        let shown = self.sidebar_sessions(list);
        let ix = shown.iter().position(|s| tab_key(s) == key)?;
        Some((ix, shown.len()))
    }

    /// Move `key` to the top of `list`, or to its bottom with `!top`.
    pub fn move_sidebar_session_to_end(
        &mut self,
        list: SideList,
        key: &str,
        top: bool,
        cx: &mut Context<Self>,
    ) {
        let shown = self.sidebar_sessions(list);
        let edge = if top { shown.first() } else { shown.last() };
        let Some(target) = edge.map(|s| tab_key(s)) else {
            return;
        };
        self.move_sidebar_session(list, key, &target, !top, cx);
    }

    /// Root sessions (`agent/session`) with a permission or question waiting.
    /// A delegate's request marks the session it belongs to, when known.
    pub fn sessions_waiting(&self) -> HashSet<String> {
        self.requests
            .iter()
            .filter(|r| r.is_pending())
            .filter_map(|r| {
                let mut id = r.session_id.clone()?;
                // Walk up to the root through known parents.
                for _ in 0..16 {
                    match self
                        .session_summary(&r.agent_id, &id)
                        .and_then(|s| s.parent.clone().filter(|_| !s.root))
                    {
                        Some(parent) => id = parent,
                        None => break,
                    }
                }
                Some(format!("{}/{id}", r.agent_id))
            })
            .collect()
    }

    /// Agents in trouble, with why: failed, or connection lost and retrying
    /// (desktop `agentsNeedingAttention`).
    pub fn agents_needing_attention(&self) -> Vec<(String, String)> {
        self.configs
            .iter()
            .filter_map(|c| {
                let rt = self.runtime.get(&c.id)?;
                let why = match &rt.state {
                    ConnState::Failed(err) if err.is_empty() => "failed".to_string(),
                    ConnState::Failed(err) => format!("failed: {err}"),
                    ConnState::Connecting if rt.reconnect_attempt > 0 => {
                        "connection lost, reconnecting".to_string()
                    }
                    _ => return None,
                };
                Some((c.name.clone(), why))
            })
            .collect()
    }
}

#[cfg(test)]
impl AgentsStore {
    /// Deliver request `method` from agent `agent`, as its connection would.
    pub(crate) fn request_for_test(
        &mut self,
        agent: &str,
        id: u64,
        method: &str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let generation = self.runtime.get(agent).map_or(0, |rt| rt.generation);
        let incoming = crate::rpc::Incoming::Request {
            id: serde_json::json!(id),
            method: method.into(),
            params,
        };
        self.on_incoming(agent, generation, incoming, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{Candidate, arrange, move_key};
    use std::collections::HashSet;

    fn c(key: &str, updated: &str) -> Candidate {
        Candidate {
            key: key.into(),
            updated_at: updated.into(),
            settling: false,
        }
    }

    #[test]
    fn keeps_places_and_puts_new_on_top() {
        let (mut order, mut known) = (Vec::new(), HashSet::new());
        // First listing while the agent settles: by activity, then fixed.
        let first = [
            Candidate {
                settling: true,
                ..c("a/1", "2026-01-01")
            },
            Candidate {
                settling: true,
                ..c("a/2", "2026-01-03")
            },
            Candidate {
                settling: true,
                ..c("a/3", "2026-01-02")
            },
        ];
        let (shown, _) = arrange(&mut order, &mut known, &first, |_| true);
        assert_eq!(shown, ["a/2", "a/3", "a/1"]);

        // Activity no longer moves them; a new one goes on top.
        let next = [
            c("a/1", "2026-02-01"),
            c("a/2", "2026-01-03"),
            c("a/3", "2026-01-02"),
            c("a/4", "2026-01-05"),
        ];
        let (shown, changed) = arrange(&mut order, &mut known, &next, |_| true);
        assert!(changed);
        assert_eq!(shown, ["a/4", "a/2", "a/3", "a/1"]);

        // Closed, then reopened: new again.
        let without = [c("a/1", "x"), c("a/2", "x"), c("a/4", "x")];
        arrange(&mut order, &mut known, &without, |_| true);
        let back = [c("a/1", "x"), c("a/2", "x"), c("a/3", "x"), c("a/4", "x")];
        let (shown, _) = arrange(&mut order, &mut known, &back, |_| true);
        assert_eq!(shown, ["a/3", "a/4", "a/2", "a/1"]);
    }

    #[test]
    fn an_agent_away_keeps_its_places() {
        let (mut order, mut known) = (Vec::new(), HashSet::new());
        let all = [c("a/1", "3"), c("b/1", "2"), c("a/2", "1")];
        arrange(&mut order, &mut known, &all, |_| true);
        // `b` disconnects (not settled): its session keeps its place.
        let only_a = [c("a/1", "3"), c("a/2", "1")];
        arrange(&mut order, &mut known, &only_a, |k| k.starts_with("a/"));
        let (shown, _) = arrange(&mut order, &mut known, &all, |_| true);
        assert_eq!(shown, ["a/1", "b/1", "a/2"]);
    }

    #[test]
    fn drops_before_or_after_the_target() {
        let shown: Vec<String> = ["1", "2", "3", "4"].map(String::from).to_vec();
        let mut order = shown.clone();
        assert!(move_key(&mut order, &shown, "1", "3", true));
        assert_eq!(order, ["2", "3", "1", "4"]);
        let shown = order.clone();
        assert!(move_key(&mut order, &shown, "4", "2", false));
        assert_eq!(order, ["4", "2", "3", "1"]);
        // Already there, or onto itself: nothing moves.
        let shown = order.clone();
        assert!(!move_key(&mut order, &shown, "4", "2", false));
        assert!(!move_key(&mut order, &shown, "2", "4", true));
        assert!(!move_key(&mut order, &shown, "4", "4", true));
    }
}

//! Slash commands advertised before the session view exists (design D3).
//!
//! `launch` creates its view only after `session/new` and the config round
//! trips, and the agent advertises its commands right after answering
//! `session/new`. Those updates arrive while no view matches, so the latest
//! `available_commands_update` of each such session is kept here until a view
//! for it is created.

use std::collections::VecDeque;

use serde_json::Value;

use super::AgentsStore;
use crate::session::SessionView;

/// Sessions held at once; the oldest insertion is dropped beyond this.
const CAPACITY: usize = 32;

/// The latest `available_commands_update` params per `(agent, session)`.
/// Each update replaces the whole list, so only the latest one matters.
#[derive(Default)]
pub(crate) struct EarlyCommands {
    /// Oldest first.
    entries: VecDeque<((String, String), Value)>,
}

impl EarlyCommands {
    /// Keep `params` (a `session/update` notification's params) when it is an
    /// `available_commands_update`; any other update is not buffered, since
    /// replaying config or transcript updates before a load could undo
    /// options or duplicate content.
    pub(crate) fn note(&mut self, agent_id: &str, session_id: &str, params: &Value) {
        let kind = params
            .pointer("/update/sessionUpdate")
            .and_then(Value::as_str);
        if kind != Some("available_commands_update") || session_id.is_empty() {
            return;
        }
        let key = (agent_id.to_owned(), session_id.to_owned());
        self.entries.retain(|(k, _)| *k != key);
        self.entries.push_back((key, params.clone()));
        while self.entries.len() > CAPACITY {
            self.entries.pop_front();
        }
    }

    /// Remove and return the update held for a session.
    pub(crate) fn take(&mut self, agent_id: &str, session_id: &str) -> Option<Value> {
        let at = self
            .entries
            .iter()
            .position(|((a, s), _)| a == agent_id && s == session_id)?;
        self.entries.remove(at).map(|(_, params)| params)
    }

    /// Forget everything held for an agent (its connection ended).
    pub(crate) fn clear_agent(&mut self, agent_id: &str) {
        self.entries.retain(|((a, _), _)| a != agent_id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

impl AgentsStore {
    /// Route one `session/update` of `agent_id`: to the open session's view
    /// when it is the update's session (returns `true`: a flush is due),
    /// otherwise keep it when it advertises commands.
    pub(super) fn deliver_session_update(&mut self, agent_id: &str, params: &Value) -> bool {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(view) = self
            .session
            .as_mut()
            .filter(|v| v.agent_id == agent_id && v.session_id == session_id)
        {
            view.queue_session_update(params);
            return true;
        }
        self.early_commands.note(agent_id, session_id, params);
        false
    }

    /// Hand a view that was just created the commands advertised for its
    /// session so far. Call before feeding the view anything else, so a later
    /// load or update applies on top of them.
    pub(super) fn take_early_commands(&mut self, view: &mut SessionView) {
        if let Some(params) = self.early_commands.take(&view.agent_id, &view.session_id) {
            view.queue_session_update(&params);
        }
    }
}

#[cfg(test)]
mod tests {
    use querymt_agent_session_core::testing::{TraceSpec, generate, snapshot_response};
    use serde_json::json;

    use super::*;
    use crate::store::launch::peer_fixture;

    fn commands(session: &str, names: &[&str]) -> Value {
        let list: Vec<Value> = names
            .iter()
            .map(|n| json!({ "name": n, "description": format!("{n} command") }))
            .collect();
        json!({ "sessionId": session, "update": {
            "sessionUpdate": "available_commands_update", "availableCommands": list } })
    }

    fn names(view: &SessionView) -> Vec<&str> {
        view.slash_commands
            .iter()
            .map(|c| c.name.as_str())
            .collect()
    }

    /// What `launch` does when the new session's view is created.
    fn launch_view(store: &mut AgentsStore, session: &str) -> SessionView {
        let mut view = SessionView::new("a", session, "/w", "first prompt");
        store.take_early_commands(&mut view);
        view.prompt_optimistic("c1", &[json!({ "type": "text", "text": "first prompt" })]);
        view
    }

    #[test]
    fn commands_sent_before_the_launch_view_are_applied() {
        let mut store = peer_fixture();
        store.session = None;
        // `session/new` answered; the agent advertises before the view exists.
        assert!(!store.deliver_session_update("a", &commands("new1", &["compact", "worktree"])));
        let view = launch_view(&mut store, "new1");
        assert_eq!(names(&view), ["compact", "worktree"]);
        assert_eq!(store.early_commands.len(), 0, "the view took the entry");
    }

    #[test]
    fn the_latest_early_update_wins() {
        let mut store = peer_fixture();
        store.session = None;
        store.deliver_session_update("a", &commands("new1", &["compact", "worktree"]));
        store.deliver_session_update("a", &commands("new1", &["compact"]));
        assert_eq!(store.early_commands.len(), 1);
        assert_eq!(names(&launch_view(&mut store, "new1")), ["compact"]);
    }

    #[test]
    fn early_commands_stay_with_their_session_and_agent() {
        let mut store = peer_fixture();
        store.session = None;
        store.deliver_session_update("a", &commands("s1", &["compact"]));
        store.deliver_session_update("b", &commands("s1", &["other-agent"]));
        assert_eq!(names(&launch_view(&mut store, "s2")), Vec::<&str>::new());
        assert_eq!(names(&launch_view(&mut store, "s1")), ["compact"]);
    }

    #[test]
    fn only_commands_are_buffered() {
        let mut store = peer_fixture();
        store.session = None;
        store.deliver_session_update(
            "a",
            &json!({ "sessionId": "new1", "update": {
                "sessionUpdate": "agent_message_chunk", "messageId": "m1",
                "content": { "type": "text", "text": "hello" } } }),
        );
        store.deliver_session_update(
            "a",
            &json!({ "sessionId": "new1", "update": {
                "sessionUpdate": "current_mode_update", "currentModeId": "plan" } }),
        );
        assert_eq!(store.early_commands.len(), 0);
        let view = launch_view(&mut store, "new1");
        assert!(view.slash_commands.is_empty());
    }

    #[test]
    fn early_commands_are_bounded() {
        let mut store = peer_fixture();
        store.session = None;
        for n in 0..40 {
            store.deliver_session_update("a", &commands(&format!("s{n}"), &["compact"]));
        }
        assert_eq!(store.early_commands.len(), CAPACITY);
        // The oldest insertions were dropped, the newest kept.
        assert!(names(&launch_view(&mut store, "s0")).is_empty());
        assert!(names(&launch_view(&mut store, "s7")).is_empty());
        assert_eq!(names(&launch_view(&mut store, "s8")), ["compact"]);
        assert_eq!(names(&launch_view(&mut store, "s39")), ["compact"]);
    }

    #[test]
    fn a_new_connection_forgets_the_agents_early_commands() {
        let mut store = peer_fixture();
        store.session = None;
        store.deliver_session_update("a", &commands("s1", &["compact"]));
        store.deliver_session_update("b", &commands("s2", &["compact"]));
        store.early_commands.clear_agent("a");
        assert!(names(&launch_view(&mut store, "s1")).is_empty());
        let mut view = SessionView::new("b", "s2", "/w", "t");
        store.take_early_commands(&mut view);
        view.flush();
        assert_eq!(names(&view), ["compact"], "other agents keep theirs");
    }

    #[test]
    fn a_view_for_the_session_gets_updates_directly() {
        let mut store = peer_fixture();
        store.session = Some(SessionView::new("a", "s1", "/w", "t"));
        assert!(store.deliver_session_update("a", &commands("s1", &["compact"])));
        assert_eq!(store.early_commands.len(), 0);
        // Another session's update is buffered, not given to the open view.
        assert!(!store.deliver_session_update("a", &commands("s2", &["other"])));
        assert_eq!(store.early_commands.len(), 1);
        let view = store.session.as_mut().unwrap();
        view.flush();
        assert_eq!(names(view), ["compact"]);
    }

    #[test]
    fn early_commands_survive_a_load_response() {
        let mut store = peer_fixture();
        store.session = None;
        // Opened, the agent advertises while the load is in flight, then the
        // load answers with a snapshot.
        let mut view = SessionView::new("a", "s1", "/w", "t");
        store.take_early_commands(&mut view);
        view.loading = true;
        store.session = Some(view);
        store.deliver_session_update("a", &commands("s1", &["compact", "worktree"]));
        let events = generate(TraceSpec::small(1)).events;
        let view = store.session.as_mut().unwrap();
        view.loading = false;
        view.load(snapshot_response(&events, json!({})));
        assert_eq!(names(view), ["compact", "worktree"]);
        assert!(!view.items.is_empty(), "the snapshot was applied");
        // And an advertisement that arrived before the view was opened is
        // applied first, so the load does not drop it either.
        store.session = None;
        store.deliver_session_update("a", &commands("s3", &["compact"]));
        let mut view = SessionView::new("a", "s3", "/w", "t");
        store.take_early_commands(&mut view);
        view.load(snapshot_response(&events, json!({})));
        assert_eq!(names(&view), ["compact"]);
    }

    /// The same two cases through the background path (a response over
    /// `HYDRATE_BYTES`), which builds its own core.
    #[test]
    fn early_commands_survive_a_background_load() {
        let mut store = peer_fixture();
        store.session = None;
        let events = generate(TraceSpec::small(1)).events;
        let load = |view: &mut SessionView| {
            let job = view
                .begin_load(snapshot_response(&events, json!({})))
                .unwrap_or_else(|_| panic!("a view with no turns hydrates"));
            let hydrated = std::thread::spawn(move || job.hydrate()).join().unwrap();
            view.finish_load(hydrated);
        };

        // Advertised while the load is in flight, before the response.
        let mut view = SessionView::new("a", "s1", "/w", "t");
        store.take_early_commands(&mut view);
        view.loading = true;
        store.session = Some(view);
        store.deliver_session_update("a", &commands("s1", &["compact", "worktree"]));
        let view = store.session.as_mut().unwrap();
        view.loading = false;
        load(view);
        assert_eq!(names(view), ["compact", "worktree"]);
        assert!(!view.items.is_empty(), "the snapshot was applied");

        // Advertised before the view was opened.
        store.session = None;
        store.deliver_session_update("a", &commands("s3", &["compact"]));
        let mut view = SessionView::new("a", "s3", "/w", "t");
        store.take_early_commands(&mut view);
        load(&mut view);
        assert_eq!(names(&view), ["compact"]);
    }
}

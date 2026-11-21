//! Delegate routing of a session (desktop `DelegateModelDialog` and the
//! store's `delegateAssignments*`): which model, and on which mesh node, each
//! delegate of the session's profile uses the next time it starts, and its
//! reasoning effort (`_querymt/session/delegateModels`,
//! `_querymt/session/setDelegateModel`).

use gpui_kit::*;
use serde_json::{Value, json};

use super::{AgentsStore, Model, model_key};

pub const GET: &str = "querymt/session/delegateModels";
pub const SET: &str = "querymt/session/setDelegateModel";
/// Recent models kept per delegate.
const RECENT_LIMIT: usize = 5;

/// Reasoning efforts a delegate can use; `None` inherits the parent session.
pub const REASONING: [(&str, &str); 5] = [
    ("auto", "Auto"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("max", "Max"),
];

/// A delegate's model on a node (`DelegateModelOverride`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub model_id: String,
    pub node_id: Option<String>,
}

impl Route {
    pub fn key(&self) -> String {
        model_key(&self.model_id, self.node_id.as_deref())
    }

    pub fn of(model: &Model) -> Self {
        Self {
            model_id: model.id.clone(),
            node_id: model.node_id.clone(),
        }
    }

    fn parse(v: Option<&Value>) -> Option<Self> {
        let v = v?;
        Some(Self {
            model_id: v.get("model_id")?.as_str()?.to_owned(),
            node_id: v.get("node_id").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

/// A delegate of the session's profile.
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    pub agent_id: String,
    pub name: String,
    pub description: String,
    /// The session's override; `None`: the profile default.
    pub route: Option<Route>,
    pub default_model: Option<String>,
    pub reasoning: Option<String>,
}

/// A saved route for a delegate the profile no longer has.
#[derive(Clone, Debug, PartialEq)]
pub struct Orphan {
    pub agent_id: String,
    pub route: Option<Route>,
    pub reasoning: Option<String>,
}

/// `DelegateAssignmentsInfo`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Routes {
    pub profile_id: String,
    pub revision: Option<u64>,
    /// Saved with the session; otherwise for this run only.
    pub durable: bool,
    /// False for a delegated child: its parent configures routing.
    pub editable: bool,
    pub reasoning_supported: bool,
    pub assignments: Vec<Assignment>,
    pub orphans: Vec<Orphan>,
}

impl Routes {
    pub fn parse(v: &Value) -> Option<Self> {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let list = |k: &str| {
            v.get(k)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        Some(Self {
            profile_id: s(v, "profile_id").unwrap_or_default(),
            revision: v.get("revision").and_then(Value::as_u64),
            durable: v.get("durable").and_then(Value::as_bool) == Some(true),
            editable: v.get("editable").and_then(Value::as_bool) != Some(false),
            reasoning_supported: v.get("reasoning_effort_supported").and_then(Value::as_bool)
                == Some(true),
            assignments: list("assignments")
                .iter()
                .filter_map(|a| {
                    let agent_id = s(a, "agent_id")?;
                    Some(Assignment {
                        name: s(a, "name").unwrap_or_else(|| agent_id.clone()),
                        description: s(a, "description").unwrap_or_default(),
                        route: Route::parse(a.get("model")),
                        default_model: s(a, "configured_default_model_id"),
                        reasoning: s(a, "reasoning_effort"),
                        agent_id,
                    })
                })
                .collect(),
            orphans: list("orphaned_overrides")
                .iter()
                .filter_map(|a| {
                    Some(Orphan {
                        agent_id: s(a, "agent_id")?,
                        route: Route::parse(a.get("model")),
                        reasoning: s(a, "reasoning_effort"),
                    })
                })
                .collect(),
        })
    }

    /// The route and reasoning saved for `agent_id`, as a delegate or orphan.
    fn current(&self, agent_id: &str) -> Option<(Option<&Route>, Option<&str>)> {
        self.assignments
            .iter()
            .find(|a| a.agent_id == agent_id)
            .map(|a| (a.route.as_ref(), a.reasoning.as_deref()))
            .or_else(|| {
                self.orphans
                    .iter()
                    .find(|o| o.agent_id == agent_id)
                    .map(|o| (o.route.as_ref(), o.reasoning.as_deref()))
            })
    }
}

/// Delegate routing of one session.
#[derive(Clone, Debug, Default)]
pub struct DelegateState {
    pub routes: Option<Routes>,
    pub loading: bool,
    pub error: Option<String>,
    /// Delegate whose route is being saved.
    pub pending: Option<String>,
    /// The routes changed elsewhere since they were read.
    pub conflict: bool,
    /// Requests started; a response older than the latest is dropped.
    generation: u64,
}

impl DelegateState {
    /// Configurable delegates, or something to retry or clean up.
    pub fn has_routes(&self) -> bool {
        self.error.is_some()
            || self
                .routes
                .as_ref()
                .is_some_and(|r| !r.assignments.is_empty() || !r.orphans.is_empty())
    }
}

/// What to change; `reasoning: None` leaves the reasoning as it is.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteChange {
    pub route: Option<Route>,
    pub reasoning: Option<Option<String>>,
}

fn set_params(session: &str, agent: &str, change: &RouteChange, revision: Option<u64>) -> Value {
    let mut params = json!({
        "session_id": session,
        "agent_id": agent,
        "model_id": change.route.as_ref().map(|r| r.model_id.clone()),
        "node_id": change.route.as_ref().and_then(|r| r.node_id.clone()),
        "expected_revision": revision,
    });
    if let Some(reasoning) = &change.reasoning {
        params["reasoning_effort"] = json!(reasoning);
    }
    params
}

/// The agent refused because the routes changed elsewhere.
fn is_conflict(error: &crate::rpc::RpcError) -> bool {
    error
        .raw()
        .and_then(|e| e.pointer("/data/code"))
        .and_then(Value::as_str)
        == Some("delegate_assignment_conflict")
}

fn recent_key(owner: &str, profile: &str, delegate: &str) -> String {
    format!("{owner}\u{0}{profile}\u{0}{delegate}")
}

impl AgentsStore {
    pub fn supports_delegate_routing(&self, agent_id: &str) -> bool {
        self.runtime(agent_id)
            .is_some_and(|rt| rt.supports_method(GET) && rt.supports_method(SET))
    }

    /// Delegate routing of the open session.
    pub fn delegate_state(&self) -> Option<&DelegateState> {
        let view = self.session.as_ref()?;
        self.runtime(&view.agent_id)?
            .delegates
            .get(&view.session_id)
    }

    fn delegate_state_mut(&mut self, agent_id: &str, session_id: &str) -> &mut DelegateState {
        self.runtime
            .entry(agent_id.to_owned())
            .or_default()
            .delegates
            .entry(session_id.to_owned())
            .or_default()
    }

    /// Read the open session's delegate routes.
    pub fn refresh_delegates(&mut self, cx: &mut Context<Self>) {
        let Some((agent, session)) = self
            .session
            .as_ref()
            .filter(|v| !v.loading)
            .map(|v| (v.agent_id.clone(), v.session_id.clone()))
        else {
            return;
        };
        if !self.supports_delegate_routing(&agent) {
            return;
        }
        let Some(rpc) = self.runtime(&agent).and_then(|rt| rt.rpc.clone()) else {
            return;
        };
        let state = self.delegate_state_mut(&agent, &session);
        state.generation += 1;
        state.loading = true;
        let generation = state.generation;
        cx.notify();
        let request = rpc.request(&format!("_{GET}"), json!({ "session_id": session }));
        cx.spawn(async move |this, cx| {
            let result = request.await.and_then(|v| {
                if v.get("session_id").and_then(Value::as_str) != Some(session.as_str()) {
                    return Err("The agent returned delegate routes for another session.".into());
                }
                Routes::parse(&v).ok_or_else(|| "The agent returned unreadable routes.".into())
            });
            let _ = this.update(cx, |s, cx| {
                let state = s.delegate_state_mut(&agent, &session);
                if state.generation != generation {
                    return;
                }
                state.loading = false;
                match result {
                    Ok(routes) => {
                        // Never step back to an older revision.
                        let stale = matches!(
                            (state.routes.as_ref().and_then(|r| r.revision), routes.revision),
                            (Some(have), Some(got)) if got < have
                        );
                        if !stale {
                            state.routes = Some(routes);
                        }
                        state.error = None;
                    }
                    Err(error) => state.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Route `delegate` of the open session. Unchanged choices are not sent.
    pub fn set_delegate_route(
        &mut self,
        delegate: &str,
        change: RouteChange,
        cx: &mut Context<Self>,
    ) {
        let Some((agent, session)) = self
            .session
            .as_ref()
            .map(|v| (v.agent_id.clone(), v.session_id.clone()))
        else {
            return;
        };
        let Some(rpc) = self.runtime(&agent).and_then(|rt| rt.rpc.clone()) else {
            return;
        };
        let state = self.delegate_state_mut(&agent, &session);
        let Some(routes) = state.routes.clone().filter(|r| r.editable) else {
            return;
        };
        if state.pending.is_some() || (change.reasoning.is_some() && !routes.reasoning_supported) {
            return;
        }
        let (route, reasoning) = routes.current(delegate).unwrap_or((None, None));
        let explicit_pick = change.route.is_some()
            && change.reasoning.is_none()
            && routes.assignments.iter().any(|a| a.agent_id == delegate);
        let unchanged = route == change.route.as_ref()
            && change
                .reasoning
                .as_ref()
                .is_none_or(|r| r.as_deref() == reasoning);
        if unchanged {
            if explicit_pick && let Some(route) = &change.route {
                self.remember_delegate_model(&agent, &routes.profile_id, delegate, &route.key());
            }
            return;
        }
        state.pending = Some(delegate.to_owned());
        state.conflict = false;
        state.error = None;
        cx.notify();
        let params = set_params(&session, delegate, &change, routes.revision);
        let request = rpc.request_detailed(&format!("_{SET}"), params);
        let delegate = delegate.to_owned();
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |s, cx| {
                let state = s.delegate_state_mut(&agent, &session);
                state.pending = None;
                match &result {
                    Ok(v)
                        if v.get("agent_id").and_then(Value::as_str) == Some(delegate.as_str()) =>
                    {
                        let confirmed = Route::parse(v.get("model"));
                        if explicit_pick
                            && confirmed == change.route
                            && let Some(route) = &confirmed
                        {
                            s.remember_delegate_model(
                                &agent,
                                &routes.profile_id,
                                &delegate,
                                &route.key(),
                            );
                        }
                    }
                    Ok(_) => {
                        state.error = Some("The agent confirmed a different delegate.".into());
                    }
                    Err(error) if is_conflict(error) => state.conflict = true,
                    Err(error) => {
                        state.error = Some(format!("Could not update the route: {error}"))
                    }
                }
                // Read back the agent's view either way: revision and sources.
                s.refresh_delegates(cx);
            });
        })
        .detach();
    }

    /// Routes of the open session, as if the agent had answered.
    #[cfg(test)]
    pub fn set_delegate_routes_for_test(&mut self, v: &Value) {
        let view = self.session.as_ref().expect("a session is open");
        let (agent, session) = (view.agent_id.clone(), view.session_id.clone());
        self.delegate_state_mut(&agent, &session).routes = Routes::parse(v);
    }

    pub fn dismiss_delegate_conflict(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.session.as_ref() {
            let (agent, session) = (view.agent_id.clone(), view.session_id.clone());
            self.delegate_state_mut(&agent, &session).conflict = false;
            cx.notify();
        }
    }

    fn remember_delegate_model(&mut self, owner: &str, profile: &str, delegate: &str, key: &str) {
        let recent = self
            .prefs
            .recent_delegate_models
            .entry(recent_key(owner, profile, delegate))
            .or_default();
        recent.retain(|k| k != key);
        recent.insert(0, key.to_owned());
        recent.truncate(RECENT_LIMIT);
        self.save();
    }

    /// Models recently routed to `delegate`, still in the agent's catalog.
    pub fn recent_delegate_models(
        &self,
        owner: &str,
        profile: &str,
        delegate: &str,
    ) -> Vec<&Model> {
        let models = self.models(owner);
        self.prefs
            .recent_delegate_models
            .get(&recent_key(owner, profile, delegate))
            .into_iter()
            .flatten()
            .filter_map(|key| models.iter().find(|m| &m.key() == key))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Route, RouteChange, Routes, set_params};
    use serde_json::{Value, json};

    fn routes() -> Value {
        serde_json::from_str(
            r#"{"version":1,"reasoning_effort_supported":true,"session_id":"s","profile_id":"dev",
                "revision":4,"durable":true,"editable":true,
                "assignments":[{"agent_id":"coder","name":"Coder","description":"Writes code",
                  "model":{"model_id":"codex/m","node_id":"thor"},"source":"override",
                  "configured_default_model_id":"anthropic/c","reasoning_effort":null}],
                "orphaned_overrides":[{"agent_id":"old","model":null,"reasoning_effort":"high"}]}"#,
        )
        .unwrap()
    }

    #[gpui_kit::test]
    fn reads_routes_saves_a_pick_and_reports_conflicts(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let (sender, requests) = async_channel::unbounded::<String>();
        let rpc = crate::rpc::RpcClient::new(sender);
        let store = cx.new(|_| {
            let mut store = crate::store::automations::schedule_ui_fixture();
            let rt = store.runtime.get_mut("test-agent").unwrap();
            rt.rpc = Some(rpc.clone());
            rt.capabilities = Some(json!({ "methods": [super::GET, super::SET] }));
            store.session = Some(crate::session::SessionView::new(
                "test-agent",
                "s",
                "/w",
                "t",
            ));
            store
        });
        let next = || -> Value { serde_json::from_str(&requests.try_recv().unwrap()).unwrap() };
        let reply = |request: &Value, body: Value| {
            let mut message = json!({ "id": request["id"] });
            let key = if body.get("code").is_some() {
                "error"
            } else {
                "result"
            };
            message[key] = body;
            rpc.handle(&message.to_string());
        };

        cx.update(|cx| store.update(cx, |s, cx| s.refresh_delegates(cx)));
        cx.run_until_parked();
        let get = next();
        assert_eq!(get["method"], "_querymt/session/delegateModels");
        reply(&get, routes());
        cx.run_until_parked();
        cx.update(|cx| {
            let state = store.read(cx).delegate_state().unwrap();
            assert!(state.has_routes() && !state.loading);
        });

        // Picking a model sends it with the revision that was read.
        let pick = RouteChange {
            route: Some(Route {
                model_id: "anthropic/c".into(),
                node_id: None,
            }),
            reasoning: None,
        };
        cx.update(|cx| store.update(cx, |s, cx| s.set_delegate_route("coder", pick, cx)));
        cx.run_until_parked();
        let set = next();
        assert_eq!(set["method"], "_querymt/session/setDelegateModel");
        assert_eq!(set["params"]["model_id"], "anthropic/c");
        assert_eq!(set["params"]["expected_revision"], 4);
        cx.update(|cx| assert!(store.read(cx).delegate_state().unwrap().pending.is_some()));
        reply(
            &set,
            json!({ "code": -32020, "message": "conflict",
                    "data": { "code": "delegate_assignment_conflict" } }),
        );
        cx.run_until_parked();
        cx.update(|cx| {
            let state = store.read(cx).delegate_state().unwrap();
            assert!(state.conflict && state.pending.is_none());
        });
        // The routes are read again after a conflict.
        assert_eq!(next()["method"], "_querymt/session/delegateModels");
    }

    #[test]
    fn parses_assignments_and_orphans() {
        let r = Routes::parse(&routes()).unwrap();
        assert_eq!(r.revision, Some(4));
        assert!(r.durable && r.editable && r.reasoning_supported);
        let coder = &r.assignments[0];
        assert_eq!(
            coder.route,
            Some(Route {
                model_id: "codex/m".into(),
                node_id: Some("thor".into())
            })
        );
        assert_eq!(coder.default_model.as_deref(), Some("anthropic/c"));
        assert_eq!(coder.reasoning, None);
        assert_eq!(r.orphans[0].reasoning.as_deref(), Some("high"));
        assert_eq!(r.current("old"), Some((None, Some("high"))));
    }

    #[test]
    fn set_sends_null_model_to_reset_and_omits_untouched_reasoning() {
        let reset = RouteChange {
            route: None,
            reasoning: None,
        };
        let p = set_params("s", "coder", &reset, Some(4));
        assert_eq!(p["model_id"], Value::Null);
        assert_eq!(p["expected_revision"], 4);
        assert!(p.get("reasoning_effort").is_none());
        let inherit = RouteChange {
            route: None,
            reasoning: Some(None),
        };
        let p = set_params("s", "coder", &inherit, None);
        assert_eq!(p["reasoning_effort"], Value::Null);
        assert!(p.as_object().unwrap().contains_key("reasoning_effort"));
        let pick = RouteChange {
            route: Some(Route {
                model_id: "m".into(),
                node_id: None,
            }),
            reasoning: Some(Some("low".into())),
        };
        let p = set_params("s", "coder", &pick, None);
        assert_eq!(p["model_id"], "m");
        assert_eq!(p["reasoning_effort"], json!("low"));
    }
}

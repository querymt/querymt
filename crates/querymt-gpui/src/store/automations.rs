//! ACP schedule catalog and mutations, scoped to one agent connection.

use std::collections::BTreeSet;

use super::{AgentRuntime, AgentsStore, Context, Deserialize, RpcClient, Task, Value, json};

const LIST: &str = "querymt/schedules/list";
pub(crate) const CREATE: &str = "querymt/schedules/create";

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Schedule {
    pub(crate) public_id: String,
    pub(crate) task_public_id: String,
    pub(crate) session_public_id: String,
    pub(crate) node_id: Option<String>,
    pub(crate) trigger: Value,
    pub(crate) state: String,
    pub(crate) last_run_at: Option<String>,
    pub(crate) next_run_at: Option<String>,
    pub(crate) run_count: u32,
    pub(crate) consecutive_failures: u32,
    pub(crate) max_runs: Option<u32>,
    pub(crate) max_runtime_seconds: u64,
    pub(crate) updated_at: String,
}

impl Schedule {
    pub(crate) fn key(&self) -> (Option<String>, String) {
        (self.node_id.clone(), self.public_id.clone())
    }

    fn action_params(&self) -> Value {
        let mut params = json!({ "schedule_public_id": self.public_id });
        if let Some(node) = &self.node_id {
            params["node_id"] = json!(node);
        }
        params
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScheduleAction {
    Pause,
    Resume,
    Run,
    Delete,
}

impl ScheduleAction {
    pub(crate) fn method(self) -> &'static str {
        match self {
            Self::Pause => "querymt/schedules/pause",
            Self::Resume => "querymt/schedules/resume",
            Self::Run => "querymt/schedules/trigger",
            Self::Delete => "querymt/schedules/delete",
        }
    }
}

#[derive(Default)]
pub(crate) struct ScheduleCatalog {
    pub(crate) items: Vec<Schedule>,
    pub(crate) loaded: bool,
    pub(crate) loading: bool,
    pub(crate) error: Option<String>,
    pub(crate) actions: super::HashMap<(Option<String>, String), ScheduleActionState>,
    reload: bool,
    // Remember remote targets used in this connection even if they are not in the session catalog.
    remote_targets: BTreeSet<String>,
}

pub(crate) enum ScheduleActionState {
    Pending(ScheduleAction),
    Failed(String),
}

#[derive(Deserialize)]
struct ScheduleList {
    schedules: Vec<Schedule>,
}

pub(crate) enum ScheduleTarget {
    Workspace(String),
    Session { id: String, node: Option<String> },
}

pub(crate) struct NewSchedule {
    pub(crate) target: ScheduleTarget,
    pub(crate) prompt: String,
    pub(crate) interval_seconds: u64,
    pub(crate) max_runs: Option<u32>,
    pub(crate) max_steps: Option<u32>,
    pub(crate) max_cost_usd: Option<f64>,
}

impl NewSchedule {
    fn params(&self, session_id: &str) -> Value {
        let mut params = json!({
            "session_id": session_id,
            "prompt": self.prompt,
            "trigger": { "type": "interval", "seconds": self.interval_seconds }
        });
        if let ScheduleTarget::Session {
            node: Some(node), ..
        } = &self.target
        {
            params["node_id"] = json!(node);
        }
        if let Some(max_runs) = self.max_runs {
            params["max_runs"] = json!(max_runs);
        }
        if let Some(max_steps) = self.max_steps {
            params["max_steps"] = json!(max_steps);
        }
        if let Some(max_cost_usd) = self.max_cost_usd {
            params["max_cost_usd"] = json!(max_cost_usd);
        }
        params
    }
}

async fn list(rpc: &RpcClient, node: Option<&str>) -> Result<Vec<Schedule>, String> {
    let params = node.map_or_else(|| json!({}), |node| json!({ "node_id": node }));
    let response = rpc.request("_querymt/schedules/list", params).await?;
    let mut list: ScheduleList = serde_json::from_value(response)
        .map_err(|err| format!("Invalid automation list: {err}"))?;
    // Routing follows the request's scope, not fields repeated in a reply.
    for schedule in &mut list.schedules {
        schedule.node_id = node.map(str::to_owned);
    }
    Ok(list.schedules)
}

impl ScheduleCatalog {
    fn apply_list(&mut self, node: Option<&str>, result: Result<Vec<Schedule>, String>) {
        match result {
            Ok(items) => {
                self.items.retain(|s| s.node_id.as_deref() != node);
                self.items.extend(items);
                self.loaded = true;
            }
            Err(error) => {
                let message = self.error.get_or_insert_with(String::new);
                if !message.is_empty() {
                    message.push('\n');
                }
                match node {
                    Some(node) => message.push_str(&format!("Peer {node}: {error}")),
                    None => message.push_str(&error),
                }
            }
        }
    }
}

impl AgentRuntime {
    pub(crate) fn supports_schedules(&self) -> bool {
        self.is_connected() && self.supports_method(LIST)
    }

    pub(crate) fn supports_remote_schedules(&self) -> bool {
        self.capabilities
            .as_ref()
            .and_then(|caps| caps.pointer("/features/remote_schedules"))
            .and_then(Value::as_bool)
            == Some(true)
    }
}

impl AgentsStore {
    pub(crate) fn refresh_schedules(&mut self, id: &str, cx: &mut Context<Self>) {
        let included = self.prefs.remote_node_ids();
        let Some(rt) = self.runtime.get_mut(id).filter(|r| r.supports_schedules()) else {
            return;
        };
        let Some(rpc) = rt.rpc.clone() else { return };
        if rt.schedules.loading {
            rt.schedules.reload = true;
            return;
        }
        rt.schedules.loading = true;
        rt.schedules.error = None;
        let nodes: BTreeSet<_> = if rt.supports_remote_schedules() {
            rt.mesh_nodes
                .iter()
                .map(|node| &node.id)
                .chain(
                    rt.sessions
                        .iter()
                        .filter_map(|s| s.remote.as_ref().map(|node| &node.node_id)),
                )
                .filter(|node| included.contains(node))
                .chain(&rt.schedules.remote_targets)
                .cloned()
                .collect()
        } else {
            BTreeSet::new()
        };
        let (id, generation) = (id.to_owned(), rt.generation);
        cx.notify();
        cx.spawn(async move |this, cx| {
            // A slow or offline peer must not block the local list from appearing.
            let local = list(&rpc, None).await;
            let _ = this.update(cx, |store, cx| {
                let Some(rt) = store.live(&id, generation) else {
                    return;
                };
                let catalog = &mut rt.schedules;
                catalog
                    .items
                    .retain(|s| s.node_id.as_ref().is_none_or(|node| nodes.contains(node)));
                catalog.apply_list(None, local);
                cx.notify();
            });
            let results = futures::future::join_all(nodes.into_iter().map(|node| {
                let rpc = rpc.clone();
                async move {
                    let result = list(&rpc, Some(&node)).await;
                    (node, result)
                }
            }))
            .await;
            let _ = this.update(cx, |store, cx| {
                let Some(rt) = store.live(&id, generation) else {
                    return;
                };
                let catalog = &mut rt.schedules;
                catalog.loading = false;
                for (node, result) in results {
                    catalog.apply_list(Some(&node), result);
                }
                let present: std::collections::HashSet<_> =
                    catalog.items.iter().map(Schedule::key).collect();
                catalog.actions.retain(|key, state| {
                    matches!(state, ScheduleActionState::Pending(_)) || present.contains(key)
                });
                if std::mem::take(&mut catalog.reload) {
                    store.refresh_schedules(&id, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn run_schedule_action(
        &mut self,
        id: &str,
        schedule: &Schedule,
        action: ScheduleAction,
        cx: &mut Context<Self>,
    ) {
        let method = action.method();
        let Some(rt) = self
            .runtime
            .get_mut(id)
            .filter(|r| r.supports_schedules() && r.supports_method(method))
        else {
            return;
        };
        let Some(rpc) = rt.rpc.clone() else { return };
        let key = schedule.key();
        if matches!(
            rt.schedules.actions.get(&key),
            Some(ScheduleActionState::Pending(_))
        ) {
            return;
        }
        rt.schedules
            .actions
            .insert(key.clone(), ScheduleActionState::Pending(action));
        let params = schedule.action_params();
        let (id, generation) = (id.to_owned(), rt.generation);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = rpc
                .request(&format!("_{method}"), params)
                .await
                .and_then(|value| {
                    if value.get("success").and_then(Value::as_bool) == Some(true) {
                        Ok(())
                    } else {
                        Err(
                            "The agent did not complete the action. Refresh before retrying."
                                .into(),
                        )
                    }
                });
            let _ = this.update(cx, |store, cx| {
                let Some(rt) = store.live(&id, generation) else {
                    return;
                };
                match result {
                    Ok(()) => {
                        rt.schedules.actions.remove(&key);
                        if matches!(action, ScheduleAction::Delete) {
                            rt.schedules.items.retain(|s| s.key() != key);
                        }
                        store.refresh_schedules(&id, cx);
                    }
                    Err(error) => {
                        rt.schedules
                            .actions
                            .insert(key, ScheduleActionState::Failed(error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn create_schedule(
        &mut self,
        id: &str,
        request: NewSchedule,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let Some(rt) = self
            .runtime
            .get(id)
            .filter(|r| r.supports_schedules() && r.supports_method(CREATE))
        else {
            return Task::ready(Err(
                "The agent is not connected or cannot create automations.".into(),
            ));
        };
        let Some(rpc) = rt.rpc.clone() else {
            return Task::ready(Err("The agent is not connected.".into()));
        };
        if matches!(
            request.target,
            ScheduleTarget::Session { node: Some(_), .. }
        ) && !rt.supports_remote_schedules()
        {
            return Task::ready(Err(
                "This agent does not support scheduling on mesh peers.".into()
            ));
        }
        let (id, generation, can_delete) = (id.to_owned(), rt.generation, rt.can_delete_sessions);
        let profile = self.launch_profile(&id).to_owned();
        let node = match &request.target {
            ScheduleTarget::Session { node, .. } => node.clone(),
            ScheduleTarget::Workspace(_) => None,
        };
        cx.spawn(async move |this, cx| {
            let result = create(&rpc, request, &profile, can_delete).await;
            let _ = this.update(cx, |store, cx| {
                if let Some(rt) = store.live(&id, generation) {
                    if let Some(node) = node {
                        rt.schedules.remote_targets.insert(node);
                    }
                    store.refresh_schedules(&id, cx);
                    if result.is_ok() {
                        store.schedule_sessions_refresh(&id, super::Duration::ZERO, cx);
                    }
                }
            });
            result
        })
    }
}

async fn create(
    rpc: &RpcClient,
    request: NewSchedule,
    profile: &str,
    can_delete: bool,
) -> Result<(), String> {
    let (session, created) = match &request.target {
        ScheduleTarget::Session { id, .. } => (id.clone(), false),
        ScheduleTarget::Workspace(path) => {
            let result = rpc
                .request("session/new", super::new_session_params(path, profile))
                .await
                .map_err(|err| format!("Could not create the background session: {err}"))?;
            let id = result
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or("The agent did not return an ID for the background session.")?;
            (id.to_owned(), true)
        }
    };
    match rpc
        .request_detailed("_querymt/schedules/create", request.params(&session))
        .await
    {
        Ok(response)
            if response
                .pointer("/schedule/public_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty()) =>
        {
            Ok(())
        }
        Ok(_) => Err(
            "The agent did not return an automation ID. Check Automations before retrying.".into(),
        ),
        Err(error) => {
            // A lost response does not tell us whether the schedule was created.
            if error.raw().is_none() {
                return Err(format!(
                    "Creation could not be confirmed: {error}. Check Automations before retrying."
                ));
            }
            let mut message = format!("Could not create the automation: {error}");
            if created && (!can_delete || !discard_unused_session(rpc, &session).await) {
                message.push_str(&format!(
                    ". Background session {session} was kept. Check Automations before retrying."
                ));
            }
            Err(message)
        }
    }
}

async fn discard_unused_session(rpc: &RpcClient, session: &str) -> bool {
    // The server may have committed the schedule before returning an error.
    let result = rpc
        .request("_querymt/schedules/list", json!({ "session_id": session }))
        .await;
    let unused = result
        .ok()
        .and_then(|response| serde_json::from_value::<ScheduleList>(response).ok())
        .is_some_and(|list| list.schedules.is_empty());
    unused
        && rpc
            .request("session/delete", json!({ "sessionId": session }))
            .await
            .is_ok()
}

#[cfg(test)]
pub(crate) fn schedule_ui_fixture() -> AgentsStore {
    use super::{AgentConfig, ConnState, Endpoint, HashMap};

    let id = "test-agent".to_owned();
    let config = AgentConfig {
        id: id.clone(),
        name: "Test agent".into(),
        endpoint: Endpoint::Process {
            command: String::new(),
        },
        auto_start: false,
        host_managed: false,
    };
    let runtime = AgentRuntime {
        state: ConnState::Connected,
        capabilities: Some(json!({ "methods": [LIST, CREATE] })),
        schedules: ScheduleCatalog {
            loaded: true,
            ..Default::default()
        },
        ..Default::default()
    };
    AgentsStore {
        configs: vec![config],
        prefs: Default::default(),
        launch: Default::default(),
        session: None,
        requests: Vec::new(),
        runtime: HashMap::from([(id, runtime)]),
        first_run: false,
        _launch_task: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{NewSchedule, RpcClient, ScheduleTarget, Value, create, json};

    #[test]
    fn new_workspace_creates_session_before_submitting_schedule() {
        let (sender, requests) = async_channel::unbounded();
        let rpc = RpcClient::new(sender);
        let draft = NewSchedule {
            target: ScheduleTarget::Workspace("/workspace".into()),
            prompt: "Prepare the weekly report".into(),
            interval_seconds: 3600,
            max_runs: Some(1),
            max_steps: None,
            max_cost_usd: None,
        };
        let agent = async {
            let request: Value = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
            assert_eq!(request["method"], "session/new");
            assert_eq!(request["params"]["cwd"], "/workspace");
            rpc.handle(
                &json!({ "id": request["id"], "result": { "sessionId": "new-session" } })
                    .to_string(),
            );

            let request: Value = serde_json::from_str(&requests.recv().await.unwrap()).unwrap();
            assert_eq!(request["method"], "_querymt/schedules/create");
            assert_eq!(request["params"]["session_id"], "new-session");
            assert_eq!(request["params"]["max_runs"], 1);
            rpc.handle(&json!({ "id": request["id"], "result": { "schedule": { "public_id": "schedule-1" } } }).to_string());
        };
        let (result, ()) = futures::executor::block_on(async {
            futures::join!(create(&rpc, draft, "default", true), agent)
        });
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn create_payload_uses_interval_limits_and_remote_owner() {
        let draft = NewSchedule {
            target: ScheduleTarget::Session {
                id: "s".into(),
                node: Some("peer".into()),
            },
            prompt: "Prepare the weekly report".into(),
            interval_seconds: 3600,
            max_runs: Some(2),
            max_steps: None,
            max_cost_usd: Some(0.5),
        };
        assert_eq!(
            draft.params("s"),
            json!({
                "session_id": "s", "node_id": "peer", "prompt": "Prepare the weekly report",
                "trigger": { "type": "interval", "seconds": 3600 }, "max_runs": 2, "max_cost_usd": 0.5
            })
        );
    }
}

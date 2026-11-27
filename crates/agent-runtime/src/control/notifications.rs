use serde::{Deserialize, Serialize};
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshNodesChangedNotification {
    pub peer_id: String,
    pub change: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshJoinedNotification {
    pub peer_id: String,
    pub transport: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshPeerExpiredNotification {
    pub peer_id: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsChangedNotification {
    pub reason: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulesChangedNotification {
    pub node_id: Option<String>,
    pub session_id: Option<String>,
    pub schedule_public_id: String,
    pub change: String,
    pub schedule: Option<crate::control::schedules::ScheduleInfo>,
}

pub use querymt_agent_protocol_types::session::input_state::{
    SESSION_INPUT_STATE_VERSION, SessionInputDelivery, SessionInputState,
    SessionInputStateNotification,
};

/// Params of the `querymt/session/plansChanged` notification: a session's
/// current plans, the value `session/list` reports under `_meta["querymt/plans"]`
/// (`[]` when the session has none).
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPlansChangedNotification {
    pub session_id: String,
    #[serde(default)]
    #[typeshare(serialized_as = "Array<any>")]
    pub plans: Vec<serde_json::Value>,
}

/// The `plansChanged` notification for a session whose todo plan is `entries`
/// (empty for an emptied list).
pub(crate) fn session_plans_changed_notification(
    session_id: &str,
    entries: Vec<crate::acp::protocol::PlanEntry>,
) -> Option<crate::acp::protocol::ExtNotification> {
    let plans = match querymt_agent_core::replay::todo_plans_meta(entries) {
        Some(serde_json::Value::Array(plans)) => plans,
        _ => Vec::new(),
    };
    let params = SessionPlansChangedNotification {
        session_id: session_id.to_string(),
        plans,
    };
    let raw = serde_json::value::to_raw_value(&params)
        .inspect_err(|error| {
            tracing::error!(%error, "failed to serialize the plansChanged params");
        })
        .ok()?;
    Some(crate::acp::protocol::ExtNotification::new(
        crate::acp::shared::QMT_NOTIFICATION_SESSION_PLANS_CHANGED,
        std::sync::Arc::from(raw),
    ))
}

/// Announce todo-plan changes to one connection as
/// `querymt/session/plansChanged`, for the sessions the connection does not
/// otherwise follow (a session list sees every session; only the active
/// session's updates reach a connection).
///
/// Watches the agent's event stream: when a journaled `todowrite` call changes
/// a session's plans compared with what this connection was last told, the new
/// plans go to `deliver`. Events are published after they are journaled, so a
/// `session/list` already reports the plans when the client hears about them.
/// Each connection runs its own announcer, so a slow connection only delays
/// itself and the last-announced state dies with the connection.
///
/// Returns when `cancel` fires, the stream closes or `deliver` returns `false`
/// (the connection is gone).
///
/// If the announcer falls behind the stream, it drops what was queued (the
/// store already reflects it) and asks `plan_source` for the plans of the
/// sessions it announced before, announcing those that changed meanwhile. A
/// session it never announced is not covered: the session list reports its
/// plans, and its next `todowrite` is announced. Without a `plan_source`, or
/// when it cannot answer, the announced state is forgotten instead, so the
/// next change of each session is announced even if it equals an old value.
///
/// `deliver` is a plain `FnMut` returning a future, not an `AsyncFnMut`, so
/// that the returned future can be proven `Send` and spawned.
pub async fn announce_session_plans<F, Fut>(
    mut events: tokio::sync::broadcast::Receiver<
        std::sync::Arc<querymt_agent_core::events::EventEnvelope>,
    >,
    cancel: tokio_util::sync::CancellationToken,
    plan_source: Option<std::sync::Arc<dyn crate::session::projection::ViewStore>>,
    mut deliver: F,
) where
    F: FnMut(crate::acp::protocol::ExtNotification) -> Fut,
    Fut: std::future::Future<Output = bool> + Send,
{
    use tokio::sync::broadcast::error::RecvError;

    let mut announced: std::collections::HashMap<String, Vec<crate::acp::protocol::PlanEntry>> =
        std::collections::HashMap::new();
    loop {
        let event = tokio::select! {
            () = cancel.cancelled() => break,
            received = events.recv() => match received {
                Ok(event) => event,
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        "plans announcer fell behind the event stream; reconciling with the stored plans"
                    );
                    // Replaying the retained events after the stored plans
                    // would announce older plans again.
                    events = events.resubscribe();
                    if !reconcile_announced(plan_source.as_deref(), &mut announced, &mut deliver).await {
                        break;
                    }
                    continue;
                }
                Err(RecvError::Closed) => break,
            },
        };
        // Only journaled events change what `session/list` reports, plus the
        // rewind hint, which is emitted after the stored plan was recomputed.
        if !event.is_durable()
            && !matches!(
                event.kind(),
                querymt_agent_core::events::AgentEventKind::TodoPlanRestored { .. }
            )
        {
            continue;
        }
        let Some(entries) = querymt_agent_core::replay::todo_plan_of(event.kind()) else {
            continue;
        };
        let session_id = event.session_id();
        if announced.get(session_id) == Some(&entries) {
            continue;
        }
        announced.insert(session_id.to_owned(), entries.clone());
        let Some(notification) = session_plans_changed_notification(session_id, entries) else {
            continue;
        };
        if !deliver(notification).await {
            break;
        }
    }
}

/// After the announcer fell behind: announce the plans of the sessions it
/// announced before that differ from the stored ones. Forgets the announced
/// state when the store cannot answer. Returns `false` when `deliver` did.
async fn reconcile_announced<F, Fut>(
    plan_source: Option<&dyn crate::session::projection::ViewStore>,
    announced: &mut std::collections::HashMap<String, Vec<crate::acp::protocol::PlanEntry>>,
    deliver: &mut F,
) -> bool
where
    F: FnMut(crate::acp::protocol::ExtNotification) -> Fut,
    Fut: std::future::Future<Output = bool> + Send,
{
    let session_ids: Vec<String> = announced.keys().cloned().collect();
    let stored = match plan_source {
        Some(store) if !session_ids.is_empty() => {
            store.get_session_todo_plans(&session_ids).await.ok()
        }
        _ => None,
    };
    let Some(mut stored) = stored else {
        announced.clear();
        return true;
    };
    for session_id in session_ids {
        let entries = stored.remove(&session_id).unwrap_or_default();
        if announced.get(&session_id) == Some(&entries) {
            continue;
        }
        announced.insert(session_id.clone(), entries.clone());
        let Some(notification) = session_plans_changed_notification(&session_id, entries) else {
            continue;
        };
        if !deliver(notification).await {
            return false;
        }
    }
    true
}

pub(crate) async fn emit_session_pins_changed_notification(
    agent: &crate::LocalAgentHandle,
    session_id: &str,
) {
    agent
        .notify_all_clients(
            crate::acp::shared::QMT_NOTIFICATION_SESSION_PINS_CHANGED,
            &serde_json::json!({ "session_id": session_id }),
        )
        .await;
}

/// An MCP server that was left out of a session because its authorization is
/// missing or could not be checked. The session itself still started.
///
/// Carried by the `McpServerAuthRequired` event, by session responses
/// (`_meta.querymt.skippedMcpServers`) and, with the session id, by the
/// `querymt/session/mcpAuthRequired` notification.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedMcpServer {
    pub server_name: String,
    /// Provider id to log in with (`mcp:<server>`).
    pub provider_id: String,
    /// Why authorization could not be used, when it is not simply "no login
    /// yet" (e.g. the authorization server is unreachable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SkippedMcpServer {
    /// No usable credentials: the user has to log in.
    pub fn auth_required(server: &str) -> Self {
        Self {
            server_name: server.to_string(),
            provider_id: crate::mcp_auth::provider_id(server),
            error: None,
        }
    }

    /// Authorization could not be checked; logging in may not help until the
    /// underlying problem is fixed.
    pub fn auth_unavailable(server: &str, reason: &str) -> Self {
        Self {
            error: Some(reason.to_string()),
            ..Self::auth_required(server)
        }
    }
}

pub const MCP_AUTH_REQUIRED_VERSION: u32 = 1;

/// Params of the `querymt/session/mcpAuthRequired` notification.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuthRequiredNotification {
    pub version: u32,
    pub session_id: String,
    pub server_name: String,
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod mcp_auth_tests {
    use super::*;

    #[test]
    fn auth_required_params_keep_their_wire_format() {
        let n = McpAuthRequiredNotification {
            version: MCP_AUTH_REQUIRED_VERSION,
            session_id: "s1".into(),
            server_name: "linear".into(),
            provider_id: "mcp:linear".into(),
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            serde_json::json!({
                "version": 1, "session_id": "s1",
                "server_name": "linear", "provider_id": "mcp:linear",
            })
        );
        let n = McpAuthRequiredNotification {
            error: Some("idp unreachable".into()),
            ..n
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            serde_json::json!({
                "version": 1, "session_id": "s1",
                "server_name": "linear", "provider_id": "mcp:linear",
                "error": "idp unreachable",
            })
        );
    }

    #[test]
    fn skipped_server_wire_format() {
        assert_eq!(
            serde_json::to_value(SkippedMcpServer::auth_required("linear")).unwrap(),
            serde_json::json!({"server_name": "linear", "provider_id": "mcp:linear"})
        );
        assert_eq!(
            serde_json::to_value(SkippedMcpServer::auth_unavailable("corp", "down")).unwrap(),
            serde_json::json!({
                "server_name": "corp", "provider_id": "mcp:corp", "error": "down"
            })
        );
    }
}

#[cfg(test)]
mod plans_changed_tests {
    use super::*;
    use crate::test_utils::TestAgent;
    use querymt_agent_core::events::AgentEventKind;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn todowrite(arguments: &str) -> AgentEventKind {
        AgentEventKind::ToolCallStart {
            tool_call_id: "call".into(),
            tool_name: "todowrite".into(),
            arguments: arguments.into(),
        }
    }

    fn todos(items: &[(&str, &str)]) -> String {
        let todos: Vec<_> = items
            .iter()
            .map(|(content, status)| serde_json::json!({"content": content, "status": status}))
            .collect();
        serde_json::json!({ "todos": todos }).to_string()
    }

    /// An announcer on the agent's event stream, delivering into a channel.
    struct Announced {
        rx: mpsc::UnboundedReceiver<SessionPlansChangedNotification>,
        cancel: CancellationToken,
    }

    impl Announced {
        fn start(fixture: &TestAgent) -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            let cancel = CancellationToken::new();
            tokio::spawn(announce_session_plans(
                fixture.config.event_sink.fanout().subscribe(),
                cancel.clone(),
                None,
                move |notification| {
                    assert_eq!(
                        notification.method.as_ref(),
                        crate::acp::shared::QMT_NOTIFICATION_SESSION_PLANS_CHANGED
                    );
                    let params = serde_json::from_str(notification.params.get()).unwrap();
                    let delivered = tx.send(params).is_ok();
                    async move { delivered }
                },
            ));
            Self { rx, cancel }
        }

        async fn next(&mut self) -> SessionPlansChangedNotification {
            tokio::time::timeout(Duration::from_secs(2), self.rx.recv())
                .await
                .expect("a plansChanged notification")
                .expect("the announcer is running")
        }
    }

    impl Drop for Announced {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    #[tokio::test]
    async fn changed_plans_are_announced_once_and_repeats_are_skipped() {
        let fixture = TestAgent::new().await;
        let mut announced = Announced::start(&fixture);

        let two = todos(&[("a", "completed"), ("b", "pending")]);
        fixture.config.emit_event("s1", todowrite(&two));
        let first = announced.next().await;
        assert_eq!(first.session_id, "s1");
        assert_eq!(first.plans.len(), 1);
        assert_eq!(first.plans[0]["planId"], "querymt-todos");
        assert_eq!(first.plans[0]["entries"].as_array().unwrap().len(), 2);

        // The same plans again, then a different list: events are delivered in
        // order, so the next announcement being the different list proves the
        // repeat produced none.
        fixture.config.emit_event("s1", todowrite(&two));
        fixture
            .config
            .emit_event("s1", todowrite(&todos(&[("a", "completed")])));
        let next = announced.next().await;
        assert_eq!(next.plans[0]["entries"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn each_session_is_announced_independently() {
        let fixture = TestAgent::new().await;
        let mut announced = Announced::start(&fixture);
        let same = todos(&[("a", "pending")]);
        fixture.config.emit_event("s1", todowrite(&same));
        fixture.config.emit_event("s2", todowrite(&same));
        // Sessions have separate ordered streams, so their order is open.
        let mut sessions = [
            announced.next().await.session_id,
            announced.next().await.session_id,
        ];
        sessions.sort();
        assert_eq!(sessions, ["s1", "s2"]);
    }

    #[tokio::test]
    async fn emptying_the_list_announces_empty_plans() {
        let fixture = TestAgent::new().await;
        let mut announced = Announced::start(&fixture);
        fixture
            .config
            .emit_event("s1", todowrite(&todos(&[("a", "pending")])));
        assert_eq!(announced.next().await.plans.len(), 1);

        fixture.config.emit_event("s1", todowrite(&todos(&[])));
        let emptied = announced.next().await;
        assert_eq!(emptied.session_id, "s1");
        assert!(emptied.plans.is_empty());
    }

    #[tokio::test]
    async fn events_that_are_not_a_todo_list_announce_nothing() {
        let fixture = TestAgent::new().await;
        let mut announced = Announced::start(&fixture);
        for bad in ["not json", r#"{"nothing":true}"#] {
            fixture.config.emit_event("s1", todowrite(bad));
        }
        fixture.config.emit_event("s1", AgentEventKind::Cancelled);
        // Ephemeral events never change the list, whatever they carry.
        fixture
            .config
            .event_sink
            .emit_ephemeral("s1", todowrite(&todos(&[("ephemeral", "pending")])));
        // A real change behind them is the first announcement.
        fixture
            .config
            .emit_event("s1", todowrite(&todos(&[("real", "pending")])));
        let first = announced.next().await;
        assert_eq!(first.plans[0]["entries"][0]["content"], "real");
    }

    #[tokio::test]
    async fn a_rewind_is_announced_even_though_its_event_is_ephemeral() {
        let fixture = TestAgent::new().await;
        let mut announced = Announced::start(&fixture);
        let before = todos(&[("a", "completed"), ("b", "pending")]);
        fixture.config.emit_event("s1", todowrite(&before));
        assert_eq!(
            announced.next().await.plans[0]["entries"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let restored =
            querymt_agent_core::replay::todo_entries_from_arguments(&todos(&[("a", "completed")]))
                .unwrap();
        fixture.config.event_sink.emit_ephemeral(
            "s1",
            AgentEventKind::TodoPlanRestored {
                entries: restored.clone(),
            },
        );
        let announcement = announced.next().await;
        assert_eq!(announcement.session_id, "s1");
        assert_eq!(
            announcement.plans[0]["entries"].as_array().unwrap().len(),
            1
        );

        // The same rewind again changes nothing.
        fixture
            .config
            .event_sink
            .emit_ephemeral("s1", AgentEventKind::TodoPlanRestored { entries: restored });
        fixture
            .config
            .emit_event("s1", todowrite(&todos(&[("z", "pending")])));
        assert_eq!(
            announced.next().await.plans[0]["entries"][0]["content"],
            "z"
        );
    }

    #[tokio::test]
    async fn announcer_stops_when_delivery_fails_or_it_is_cancelled() {
        let fixture = TestAgent::new().await;
        let events = fixture.config.event_sink.fanout().subscribe();
        let failing = tokio::spawn(announce_session_plans(
            events,
            CancellationToken::new(),
            None,
            |_| async { false },
        ));
        fixture
            .config
            .emit_event("s1", todowrite(&todos(&[("a", "pending")])));
        tokio::time::timeout(Duration::from_secs(2), failing)
            .await
            .expect("stops after a failed delivery")
            .unwrap();

        let cancel = CancellationToken::new();
        let cancelled = tokio::spawn(announce_session_plans(
            fixture.config.event_sink.fanout().subscribe(),
            cancel.clone(),
            None,
            |_| async { true },
        ));
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), cancelled)
            .await
            .expect("stops when cancelled")
            .unwrap();
    }

    type Notifications = mpsc::UnboundedReceiver<SessionPlansChangedNotification>;

    /// An announcer held inside its first delivery until the returned gate is
    /// released, so the event stream can overflow while it waits.
    fn start_held(
        fixture: &TestAgent,
        plan_source: Option<std::sync::Arc<dyn crate::session::projection::ViewStore>>,
    ) -> (Notifications, std::sync::Arc<tokio::sync::Notify>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let mut held = false;
        tokio::spawn(announce_session_plans(
            fixture.config.event_sink.fanout().subscribe(),
            CancellationToken::new(),
            plan_source,
            move |notification| {
                let params = serde_json::from_str(notification.params.get()).unwrap();
                let delivered = tx.send(params).is_ok();
                let hold = !std::mem::replace(&mut held, true);
                let gate = gate.clone();
                async move {
                    if hold {
                        gate.notified().await;
                    }
                    delivered
                }
            },
        ));
        (rx, release)
    }

    async fn next_of(rx: &mut Notifications) -> SessionPlansChangedNotification {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("a plansChanged notification")
            .expect("the announcer is running")
    }

    /// Overflow the live stream so a receiver that is not reading lags. The
    /// noise goes to a session that has journaled events: only then is an
    /// ephemeral event published at once.
    fn overflow_stream(fixture: &TestAgent, session: &str) {
        for _ in 0..=querymt_agent_services::event_fanout::LIVE_EVENT_CAPACITY {
            fixture
                .config
                .event_sink
                .emit_ephemeral(session, AgentEventKind::Cancelled);
        }
    }

    async fn session_with_plan(fixture: &TestAgent, items: &[(&str, &str)]) -> String {
        let session = fixture
            .config
            .provider
            .history_store()
            .create_session(Some("plans".into()), None, None, None)
            .await
            .unwrap()
            .public_id;
        fixture
            .config
            .emit_event_persisted(&session, todowrite(&todos(items)))
            .await
            .unwrap();
        session
    }

    #[tokio::test]
    async fn lag_announces_the_plan_that_changed_meanwhile_and_not_the_replayed_past() {
        let fixture = TestAgent::new().await;
        let (mut rx, release) = start_held(&fixture, fixture.config.storage.view_store());
        let session = session_with_plan(&fixture, &[("a", "pending")]).await;
        let first = next_of(&mut rx).await;
        assert_eq!(first.session_id, session);

        // While the announcer is held: two more lists, then an overflow.
        for list in [&[("b", "pending")][..], &[("c", "pending")][..]] {
            fixture
                .config
                .emit_event_persisted(&session, todowrite(&todos(list)))
                .await
                .unwrap();
        }
        overflow_stream(&fixture, &session);
        release.notify_one();

        let caught_up = next_of(&mut rx).await;
        assert_eq!(caught_up.session_id, session);
        assert_eq!(caught_up.plans[0]["entries"][0]["content"], "c");

        // Nothing of the skipped past is announced afterwards: the next
        // announcement is the next real change.
        fixture
            .config
            .emit_event_persisted(&session, todowrite(&todos(&[("d", "pending")])))
            .await
            .unwrap();
        assert_eq!(
            next_of(&mut rx).await.plans[0]["entries"][0]["content"],
            "d"
        );
    }

    #[tokio::test]
    async fn lag_announces_a_plan_emptied_meanwhile() {
        let fixture = TestAgent::new().await;
        let (mut rx, release) = start_held(&fixture, fixture.config.storage.view_store());
        let session = session_with_plan(&fixture, &[("a", "pending")]).await;
        next_of(&mut rx).await;

        fixture
            .config
            .emit_event_persisted(&session, todowrite(&todos(&[])))
            .await
            .unwrap();
        overflow_stream(&fixture, &session);
        release.notify_one();

        let emptied = next_of(&mut rx).await;
        assert_eq!(emptied.session_id, session);
        assert!(emptied.plans.is_empty());
    }

    #[tokio::test]
    async fn lag_without_a_plan_source_forgets_what_was_announced() {
        let fixture = TestAgent::new().await;
        let (mut rx, release) = start_held(&fixture, None);
        let session = session_with_plan(&fixture, &[("a", "pending")]).await;
        next_of(&mut rx).await;

        overflow_stream(&fixture, &session);
        release.notify_one();
        // Give the announcer time to observe the lag before the next write.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The same list is announced again: the announced state was dropped.
        fixture
            .config
            .emit_event_persisted(&session, todowrite(&todos(&[("a", "pending")])))
            .await
            .unwrap();
        let again = next_of(&mut rx).await;
        assert_eq!(again.session_id, session);
        assert_eq!(again.plans[0]["entries"][0]["content"], "a");
    }

    #[tokio::test]
    async fn active_connection_patch_and_notification_carry_the_same_plans() {
        use crate::acp::shared::{AcpLiveEventTranslator, AcpSessionUpdateCapabilities};

        let fixture = TestAgent::new().await;
        let mut events = fixture.handle.subscribe_events();
        let mut announced = Announced::start(&fixture);
        let mut active = AcpLiveEventTranslator::new();
        active.set_capabilities(AcpSessionUpdateCapabilities::default());

        fixture
            .config
            .emit_event("s1", todowrite(&todos(&[("a", "pending")])));
        let envelope = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event published")
            .unwrap();
        let patched = active
            .translate_updates(&envelope)
            .into_iter()
            .filter_map(|update| serde_json::to_value(update).ok())
            .find(|update| update["sessionUpdate"] == "session_info_update")
            .expect("the active connection gets session_info_update");

        let announced = announced.next().await;
        assert_eq!(
            patched["_meta"]["querymt/plans"],
            serde_json::Value::Array(announced.plans),
            "patch and notification carry the same plans"
        );
    }

    #[tokio::test]
    async fn capability_is_advertised() {
        let fixture = TestAgent::new().await;
        let capabilities = crate::control::capabilities::get_capabilities(&fixture.handle);
        assert!(
            capabilities
                .notifications
                .iter()
                .any(|method| method == "querymt/session/plansChanged")
        );
    }
}

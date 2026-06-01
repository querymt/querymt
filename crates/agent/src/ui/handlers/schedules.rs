use super::super::ServerState;
use super::super::connection::{send_error, send_message};
use super::super::messages::{ScheduleInfo, UiServerMessage};
use crate::session::domain_schedule::Schedule;
use tokio::sync::mpsc;

/// Parameters for creating a new schedule, bundled to stay within clippy's
/// argument-count limit.
pub struct CreateScheduleParams<'a> {
    pub session_id: &'a str,
    pub node_id: Option<&'a str>,
    pub prompt: &'a str,
    pub trigger_json: &'a serde_json::Value,
    pub max_steps: Option<u32>,
    pub max_cost_usd: Option<f64>,
    pub max_runs: Option<u32>,
}

/// Convert a domain `Schedule` to a UI `ScheduleInfo` DTO.
fn schedule_to_info(s: &Schedule, node_id: Option<&str>) -> ScheduleInfo {
    let fmt = &time::format_description::well_known::Rfc3339;
    ScheduleInfo {
        public_id: s.public_id.clone(),
        task_public_id: s.task_public_id.clone(),
        session_public_id: s.session_public_id.clone(),
        node_id: node_id.map(ToOwned::to_owned),
        trigger: serde_json::to_value(&s.trigger).unwrap_or_default(),
        state: s.state.to_string(),
        last_run_at: s.last_run_at.and_then(|t| t.format(fmt).ok()),
        next_run_at: s.next_run_at.and_then(|t| t.format(fmt).ok()),
        run_count: s.run_count,
        consecutive_failures: s.consecutive_failures,
        max_runs: s.config.max_runs,
        max_runtime_seconds: s.config.max_runtime_seconds,
        created_at: s.created_at.format(fmt).unwrap_or_default(),
        updated_at: s.updated_at.format(fmt).unwrap_or_default(),
    }
}

/// Resolve the `session_public_id` that owns a local schedule.
async fn resolve_session_id(state: &ServerState, schedule_public_id: &str) -> Option<String> {
    state
        .agent
        .get_schedule(schedule_public_id)
        .await
        .ok()
        .flatten()
        .map(|s| s.session_public_id)
}

#[cfg(feature = "remote")]
async fn find_node_manager(
    state: &ServerState,
    node_id: &str,
) -> Result<kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>, String> {
    state
        .agent
        .find_node_manager(node_id)
        .await
        .map_err(|e| e.to_string())
}

/// Handle `ListSchedules` — list schedules for a session or all.
pub async fn handle_list_schedules(
    state: &ServerState,
    session_id: Option<&str>,
    node_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "remote")]
    if let Some(node_id) = node_id {
        let result = match find_node_manager(state, node_id).await {
            Ok(node_manager) => state
                .agent
                .list_remote_schedules(&node_manager, session_id.map(ToOwned::to_owned))
                .await
                .map(|r| r.schedules),
            Err(e) => {
                let _ = send_error(tx, format!("Failed to find remote node: {e}")).await;
                return;
            }
        };

        match result {
            Ok(schedules) => {
                let infos: Vec<ScheduleInfo> = schedules
                    .iter()
                    .map(|schedule| schedule_to_info(schedule, Some(node_id)))
                    .collect();
                let _ = send_message(
                    tx,
                    UiServerMessage::ScheduleList {
                        schedules: infos,
                        node_id: Some(node_id.to_string()),
                    },
                )
                .await;
            }
            Err(e) => {
                let _ = send_error(tx, format!("Failed to list remote schedules: {e}")).await;
            }
        }
        return;
    }

    match state.agent.list_schedules(session_id).await {
        Ok(schedules) => {
            let infos: Vec<ScheduleInfo> = schedules
                .iter()
                .map(|schedule| schedule_to_info(schedule, None))
                .collect();
            let _ = send_message(
                tx,
                UiServerMessage::ScheduleList {
                    schedules: infos,
                    node_id: None,
                },
            )
            .await;
        }
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list schedules: {e}")).await;
        }
    }
}

/// Handle `CreateSchedule` — create a recurring task + schedule.
pub async fn handle_create_schedule(
    state: &ServerState,
    params: &CreateScheduleParams<'_>,
    tx: &mpsc::Sender<String>,
) {
    use crate::session::domain_schedule::ScheduleTrigger;

    let trigger: ScheduleTrigger = match serde_json::from_value(params.trigger_json.clone()) {
        Ok(t) => t,
        Err(e) => {
            let _ = send_message(
                tx,
                UiServerMessage::ScheduleCreatedResult {
                    success: false,
                    schedule_public_id: None,
                    node_id: params.node_id.map(ToOwned::to_owned),
                    message: Some(format!("Invalid trigger configuration: {e}")),
                },
            )
            .await;
            return;
        }
    };

    #[cfg(feature = "remote")]
    let result = if let Some(node_id) = params.node_id {
        match find_node_manager(state, node_id).await {
            Ok(node_manager) => state
                .agent
                .create_remote_schedule(
                    &node_manager,
                    crate::agent::remote::CreateRemoteSchedule {
                        session_id: params.session_id.to_string(),
                        prompt: params.prompt.to_string(),
                        trigger,
                        max_steps: params.max_steps,
                        max_cost_usd: params.max_cost_usd,
                        max_runs: params.max_runs,
                    },
                )
                .await
                .map(|r| r.schedule_public_id),
            Err(e) => Err(agent_client_protocol::Error::internal_error().data(e)),
        }
    } else {
        state
            .agent
            .create_scheduled_task(
                params.session_id,
                params.prompt,
                trigger,
                params.max_steps,
                params.max_cost_usd,
                params.max_runs,
            )
            .await
    };

    #[cfg(not(feature = "remote"))]
    let result = state
        .agent
        .create_scheduled_task(
            params.session_id,
            params.prompt,
            trigger,
            params.max_steps,
            params.max_cost_usd,
            params.max_runs,
        )
        .await;

    match result {
        Ok(schedule_public_id) => {
            let _ = send_message(
                tx,
                UiServerMessage::ScheduleCreatedResult {
                    success: true,
                    schedule_public_id: Some(schedule_public_id),
                    node_id: params.node_id.map(ToOwned::to_owned),
                    message: None,
                },
            )
            .await;
            handle_list_schedules(state, Some(params.session_id), params.node_id, tx).await;
        }
        Err(e) => {
            let _ = send_message(
                tx,
                UiServerMessage::ScheduleCreatedResult {
                    success: false,
                    schedule_public_id: None,
                    node_id: params.node_id.map(ToOwned::to_owned),
                    message: Some(format!("Failed to create schedule: {e}")),
                },
            )
            .await;
        }
    }
}

pub async fn handle_pause_schedule(
    state: &ServerState,
    schedule_public_id: &str,
    session_id: Option<&str>,
    node_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    handle_schedule_action(state, schedule_public_id, session_id, node_id, "pause", tx).await;
}

pub async fn handle_resume_schedule(
    state: &ServerState,
    schedule_public_id: &str,
    session_id: Option<&str>,
    node_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    handle_schedule_action(state, schedule_public_id, session_id, node_id, "resume", tx).await;
}

pub async fn handle_trigger_schedule(
    state: &ServerState,
    schedule_public_id: &str,
    session_id: Option<&str>,
    node_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    handle_schedule_action(
        state,
        schedule_public_id,
        session_id,
        node_id,
        "trigger",
        tx,
    )
    .await;
}

pub async fn handle_delete_schedule(
    state: &ServerState,
    schedule_public_id: &str,
    session_id: Option<&str>,
    node_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    handle_schedule_action(state, schedule_public_id, session_id, node_id, "delete", tx).await;
}

async fn handle_schedule_action(
    state: &ServerState,
    schedule_public_id: &str,
    session_id: Option<&str>,
    node_id: Option<&str>,
    action: &str,
    tx: &mpsc::Sender<String>,
) {
    let refresh_session_id = if node_id.is_some() {
        session_id.map(ToOwned::to_owned)
    } else if let Some(session_id) = session_id {
        Some(session_id.to_string())
    } else {
        resolve_session_id(state, schedule_public_id).await
    };

    #[cfg(feature = "remote")]
    let result = if let Some(node_id) = node_id {
        match find_node_manager(state, node_id).await {
            Ok(node_manager) => match action {
                "pause" => {
                    state
                        .agent
                        .pause_remote_schedule(&node_manager, schedule_public_id.to_string())
                        .await
                }
                "resume" => {
                    state
                        .agent
                        .resume_remote_schedule(&node_manager, schedule_public_id.to_string())
                        .await
                }
                "trigger" => {
                    state
                        .agent
                        .trigger_remote_schedule(&node_manager, schedule_public_id.to_string())
                        .await
                }
                "delete" => {
                    state
                        .agent
                        .delete_remote_schedule(&node_manager, schedule_public_id.to_string())
                        .await
                }
                _ => Ok(()),
            },
            Err(e) => Err(agent_client_protocol::Error::internal_error().data(e)),
        }
    } else {
        run_local_action(state, schedule_public_id, action).await
    };

    #[cfg(not(feature = "remote"))]
    let result = run_local_action(state, schedule_public_id, action).await;

    let (success, message) = match result {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };

    let _ = send_message(
        tx,
        UiServerMessage::ScheduleActionResult {
            success,
            schedule_public_id: schedule_public_id.to_string(),
            action: action.to_string(),
            node_id: node_id.map(ToOwned::to_owned),
            message,
        },
    )
    .await;

    if success {
        handle_list_schedules(state, refresh_session_id.as_deref(), node_id, tx).await;
    }
}

async fn run_local_action(
    state: &ServerState,
    schedule_public_id: &str,
    action: &str,
) -> Result<(), agent_client_protocol::Error> {
    match action {
        "pause" => state.agent.pause_schedule(schedule_public_id).await,
        "resume" => state.agent.resume_schedule(schedule_public_id).await,
        "trigger" => state.agent.trigger_schedule_now(schedule_public_id).await,
        "delete" => state.agent.delete_schedule(schedule_public_id).await,
        _ => Ok(()),
    }
}

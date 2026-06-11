use super::super::ServerState;
use super::super::connection::{send_error, send_message};
use super::super::messages::UiServerMessage;
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

/// Handle `ListSchedules` — list schedules for a session or all.
pub async fn handle_list_schedules(
    state: &ServerState,
    session_id: Option<&str>,
    node_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    let request = crate::control::schedules::ListSchedulesControlRequest {
        node_id: node_id.map(ToOwned::to_owned),
        session_id: session_id.map(ToOwned::to_owned),
    };
    match crate::control::schedules::list_schedules(&state.agent, request).await {
        Ok(response) => {
            let _ = send_message(
                tx,
                UiServerMessage::ScheduleList {
                    schedules: response.schedules,
                    session_id: response.session_id,
                    node_id: response.node_id,
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

    let request = crate::control::schedules::CreateScheduleControlRequest {
        node_id: params.node_id.map(ToOwned::to_owned),
        session_id: params.session_id.to_string(),
        prompt: params.prompt.to_string(),
        trigger,
        max_steps: params.max_steps,
        max_cost_usd: params.max_cost_usd,
        max_runs: params.max_runs,
    };

    match crate::control::schedules::create_schedule(&state.agent, request).await {
        Ok(schedule) => {
            let _ = send_message(
                tx,
                UiServerMessage::ScheduleCreatedResult {
                    success: true,
                    schedule_public_id: Some(schedule.public_id),
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
        match session_id {
            Some(session_id) => Some(session_id.to_string()),
            None => {
                let _ = send_message(
                    tx,
                    UiServerMessage::ScheduleActionResult {
                        success: false,
                        schedule_public_id: schedule_public_id.to_string(),
                        action: action.to_string(),
                        node_id: node_id.map(ToOwned::to_owned),
                        message: Some("Remote schedule actions require session_id".to_string()),
                    },
                )
                .await;
                return;
            }
        }
    } else if let Some(session_id) = session_id {
        Some(session_id.to_string())
    } else {
        resolve_session_id(state, schedule_public_id).await
    };

    let request = crate::control::schedules::ScheduleActionControlRequest {
        node_id: node_id.map(ToOwned::to_owned),
        schedule_public_id: schedule_public_id.to_string(),
    };

    let (success, message) =
        match crate::control::schedules::schedule_action(&state.agent, request, action).await {
            Ok(_) => (true, None),
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

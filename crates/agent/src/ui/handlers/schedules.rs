//! Handler for schedule management requests.

use super::super::ServerState;
use super::super::connection::{send_error, send_message};
use super::super::messages::{ScheduleInfo, UiServerMessage};
use crate::session::domain_schedule::Schedule;
use tokio::sync::mpsc;

/// Convert a domain `Schedule` to a UI `ScheduleInfo` DTO.
fn schedule_to_info(s: &Schedule) -> ScheduleInfo {
    let fmt = &time::format_description::well_known::Rfc3339;
    ScheduleInfo {
        public_id: s.public_id.clone(),
        task_public_id: s.task_public_id.clone(),
        session_public_id: s.session_public_id.clone(),
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

/// Handle `ListSchedules` — list schedules for a session or all.
pub async fn handle_list_schedules(
    state: &ServerState,
    session_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    match state.agent.list_schedules(session_id).await {
        Ok(schedules) => {
            let infos: Vec<ScheduleInfo> = schedules.iter().map(schedule_to_info).collect();
            let _ = send_message(tx, UiServerMessage::ScheduleList { schedules: infos }).await;
        }
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list schedules: {}", e)).await;
        }
    }
}

/// Handle `CreateSchedule` — create a recurring task + schedule.
///
/// Creates a `Task` row first (so FK constraints are satisfied), then creates
/// the `Schedule` referencing it and registers it with the SchedulerActor.
pub async fn handle_create_schedule(
    state: &ServerState,
    session_id: &str,
    prompt: &str,
    trigger_json: &serde_json::Value,
    max_steps: Option<u32>,
    max_cost_usd: Option<f64>,
    max_runs: Option<u32>,
    tx: &mpsc::Sender<String>,
) {
    use crate::session::domain_schedule::ScheduleTrigger;

    // Parse the trigger from JSON
    let trigger: ScheduleTrigger = match serde_json::from_value(trigger_json.clone()) {
        Ok(t) => t,
        Err(e) => {
            let _ = send_message(
                tx,
                UiServerMessage::ScheduleCreatedResult {
                    success: false,
                    schedule_public_id: None,
                    message: Some(format!("Invalid trigger configuration: {}", e)),
                },
            )
            .await;
            return;
        }
    };

    match state
        .agent
        .create_scheduled_task(
            session_id,
            prompt,
            trigger,
            max_steps,
            max_cost_usd,
            max_runs,
        )
        .await
    {
        Ok(schedule_public_id) => {
            let _ = send_message(
                tx,
                UiServerMessage::ScheduleCreatedResult {
                    success: true,
                    schedule_public_id: Some(schedule_public_id),
                    message: None,
                },
            )
            .await;
            // Send updated list
            handle_list_schedules(state, Some(session_id), tx).await;
        }
        Err(e) => {
            let _ = send_message(
                tx,
                UiServerMessage::ScheduleCreatedResult {
                    success: false,
                    schedule_public_id: None,
                    message: Some(format!("Failed to create schedule: {}", e)),
                },
            )
            .await;
        }
    }
}

/// Handle `PauseSchedule`.
pub async fn handle_pause_schedule(
    state: &ServerState,
    schedule_public_id: &str,
    tx: &mpsc::Sender<String>,
) {
    let (success, message) = match state.agent.pause_schedule(schedule_public_id).await {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    let _ = send_message(
        tx,
        UiServerMessage::ScheduleActionResult {
            success,
            schedule_public_id: schedule_public_id.to_string(),
            action: "pause".to_string(),
            message,
        },
    )
    .await;
    if success {
        handle_list_schedules(state, None, tx).await;
    }
}

/// Handle `ResumeSchedule`.
pub async fn handle_resume_schedule(
    state: &ServerState,
    schedule_public_id: &str,
    tx: &mpsc::Sender<String>,
) {
    let (success, message) = match state.agent.resume_schedule(schedule_public_id).await {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    let _ = send_message(
        tx,
        UiServerMessage::ScheduleActionResult {
            success,
            schedule_public_id: schedule_public_id.to_string(),
            action: "resume".to_string(),
            message,
        },
    )
    .await;
    if success {
        handle_list_schedules(state, None, tx).await;
    }
}

/// Handle `TriggerSchedule` — fire immediately.
pub async fn handle_trigger_schedule(
    state: &ServerState,
    schedule_public_id: &str,
    tx: &mpsc::Sender<String>,
) {
    let (success, message) = match state.agent.trigger_schedule_now(schedule_public_id).await {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    let _ = send_message(
        tx,
        UiServerMessage::ScheduleActionResult {
            success,
            schedule_public_id: schedule_public_id.to_string(),
            action: "trigger".to_string(),
            message,
        },
    )
    .await;
    if success {
        handle_list_schedules(state, None, tx).await;
    }
}

/// Handle `DeleteSchedule`.
pub async fn handle_delete_schedule(
    state: &ServerState,
    schedule_public_id: &str,
    tx: &mpsc::Sender<String>,
) {
    let (success, message) = match state.agent.delete_schedule(schedule_public_id).await {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    let _ = send_message(
        tx,
        UiServerMessage::ScheduleActionResult {
            success,
            schedule_public_id: schedule_public_id.to_string(),
            action: "delete".to_string(),
            message,
        },
    )
    .await;
    if success {
        handle_list_schedules(state, None, tx).await;
    }
}

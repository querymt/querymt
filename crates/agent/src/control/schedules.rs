use crate::LocalAgentHandle;
use crate::session::domain_schedule::{Schedule, ScheduleTrigger};
use agent_client_protocol::Error;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleInfo {
    pub public_id: String,
    pub task_public_id: String,
    pub session_public_id: String,
    #[serde(default)]
    pub node_id: Option<String>,
    #[typeshare(serialized_as = "any")]
    pub trigger: serde_json::Value,
    pub state: String,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub run_count: u32,
    pub consecutive_failures: u32,
    pub max_runs: Option<u32>,
    #[typeshare(serialized_as = "number")]
    pub max_runtime_seconds: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[typeshare]
#[derive(Debug, Clone, Deserialize)]
pub struct CreateScheduleControlRequest {
    #[serde(default)]
    pub node_id: Option<String>,
    pub session_id: String,
    pub prompt: String,
    #[typeshare(serialized_as = "any")]
    pub trigger: ScheduleTrigger,
    #[serde(default)]
    pub max_steps: Option<u32>,
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    #[serde(default)]
    pub max_runs: Option<u32>,
}

#[typeshare]
#[derive(Debug, Clone, Deserialize)]
pub struct ListSchedulesControlRequest {
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[typeshare]
#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleActionControlRequest {
    #[serde(default)]
    pub node_id: Option<String>,
    pub schedule_public_id: String,
}

#[typeshare]
#[derive(Debug, Clone, Deserialize)]
pub struct GetScheduleControlRequest {
    #[serde(default)]
    pub node_id: Option<String>,
    pub schedule_public_id: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleListInfo {
    pub node_id: Option<String>,
    pub session_id: Option<String>,
    pub schedules: Vec<ScheduleInfo>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleActionResult {
    pub success: bool,
    pub node_id: Option<String>,
    pub schedule_public_id: String,
    pub action: String,
}

pub fn schedule_to_info(schedule: &Schedule, node_id: Option<&str>) -> ScheduleInfo {
    let fmt = &time::format_description::well_known::Rfc3339;
    ScheduleInfo {
        public_id: schedule.public_id.clone(),
        task_public_id: schedule.task_public_id.clone(),
        session_public_id: schedule.session_public_id.clone(),
        node_id: node_id.map(ToOwned::to_owned),
        trigger: serde_json::to_value(&schedule.trigger).unwrap_or_default(),
        state: schedule.state.to_string(),
        last_run_at: schedule.last_run_at.and_then(|t| t.format(fmt).ok()),
        next_run_at: schedule.next_run_at.and_then(|t| t.format(fmt).ok()),
        run_count: schedule.run_count,
        consecutive_failures: schedule.consecutive_failures,
        max_runs: schedule.config.max_runs,
        max_runtime_seconds: schedule.config.max_runtime_seconds,
        created_at: schedule.created_at.format(fmt).unwrap_or_default(),
        updated_at: schedule.updated_at.format(fmt).unwrap_or_default(),
    }
}

fn schedule_change_name(action: &str) -> &str {
    match action {
        "pause" | "resume" => "updated",
        "trigger" => "triggered",
        "delete" => "deleted",
        _ => "updated",
    }
}

async fn emit_schedule_changed_notification(
    agent: &LocalAgentHandle,
    notification: crate::control::notifications::SchedulesChangedNotification,
) {
    let Some(bridge) = agent.bridge.lock().ok().and_then(|guard| guard.clone()) else {
        return;
    };

    let params = match serde_json::value::RawValue::from_string(
        serde_json::to_string(&notification).unwrap_or_else(|_| "null".to_string()),
    ) {
        Ok(raw) => std::sync::Arc::from(raw),
        Err(_) => return,
    };

    let _ = bridge
        .notify_ext(crate::acp::protocol::ExtNotification::new(
            crate::acp::shared::QMT_NOTIFICATION_SCHEDULES_CHANGED,
            params,
        ))
        .await;
}

pub async fn create_schedule(
    agent: &LocalAgentHandle,
    request: CreateScheduleControlRequest,
) -> Result<ScheduleInfo, Error> {
    #[cfg(feature = "remote")]
    if let Some(node_id) = request.node_id.clone() {
        let nm_ref = agent.find_node_manager(&node_id).await?;
        let response = agent
            .create_remote_schedule(
                &nm_ref,
                crate::agent::remote::CreateRemoteSchedule {
                    session_id: request.session_id.clone(),
                    prompt: request.prompt,
                    trigger: request.trigger,
                    max_steps: request.max_steps,
                    max_cost_usd: request.max_cost_usd,
                    max_runs: request.max_runs,
                },
            )
            .await?;
        let schedule = agent
            .list_remote_schedules(&nm_ref, Some(request.session_id.clone()))
            .await?
            .schedules
            .into_iter()
            .find(|schedule| schedule.public_id == response.schedule_public_id)
            .ok_or_else(|| {
                Error::internal_error().data(serde_json::json!({
                    "error": "created remote schedule was not returned by the owning node",
                    "schedulePublicId": response.schedule_public_id,
                    "nodeId": node_id,
                }))
            })?;
        let schedule_info = schedule_to_info(&schedule, Some(&node_id));
        emit_schedule_changed_notification(
            agent,
            crate::control::notifications::SchedulesChangedNotification {
                node_id: Some(node_id),
                session_id: Some(schedule_info.session_public_id.clone()),
                schedule_public_id: schedule_info.public_id.clone(),
                change: "created".to_string(),
                schedule: Some(schedule_info.clone()),
            },
        )
        .await;
        return Ok(schedule_info);
    }

    #[cfg(feature = "remote")]
    if let Some(remote_node_id) = agent
        .config
        .provider
        .history_store()
        .get_session_provider_node_id(&request.session_id)
        .await
        .map_err(|e| Error::internal_error().data(serde_json::json!({"error": e.to_string()})))?
    {
        return Err(Error::invalid_params().data(serde_json::json!({
            "error": "Remote sessions require nodeId for schedule creation",
            "reason": "missing_node_id",
            "sessionId": request.session_id,
            "nodeId": remote_node_id,
        })));
    }

    let schedule_public_id = agent
        .create_scheduled_task(
            &request.session_id,
            &request.prompt,
            request.trigger,
            request.max_steps,
            request.max_cost_usd,
            request.max_runs,
        )
        .await?;
    let schedule = agent
        .get_schedule(&schedule_public_id)
        .await?
        .ok_or_else(|| {
            Error::internal_error().data(serde_json::json!({
                "error": "created local schedule was not returned by the scheduler",
                "schedulePublicId": schedule_public_id,
            }))
        })?;
    let schedule_info = schedule_to_info(&schedule, None);
    emit_schedule_changed_notification(
        agent,
        crate::control::notifications::SchedulesChangedNotification {
            node_id: None,
            session_id: Some(schedule_info.session_public_id.clone()),
            schedule_public_id: schedule_info.public_id.clone(),
            change: "created".to_string(),
            schedule: Some(schedule_info.clone()),
        },
    )
    .await;
    Ok(schedule_info)
}

pub async fn get_schedule(
    agent: &LocalAgentHandle,
    request: GetScheduleControlRequest,
) -> Result<ScheduleInfo, Error> {
    #[cfg(feature = "remote")]
    if let Some(node_id) = request.node_id.clone() {
        let nm_ref = agent.find_node_manager(&node_id).await?;
        let schedule = agent
            .list_remote_schedules(&nm_ref, None)
            .await?
            .schedules
            .into_iter()
            .find(|schedule| schedule.public_id == request.schedule_public_id)
            .ok_or_else(|| {
                Error::from(crate::error::AgentError::ScheduleNotFound {
                    schedule_public_id: request.schedule_public_id.clone(),
                })
            })?;
        return Ok(schedule_to_info(&schedule, Some(&node_id)));
    }

    let schedule = agent
        .get_schedule(&request.schedule_public_id)
        .await?
        .ok_or_else(|| {
            Error::from(crate::error::AgentError::ScheduleNotFound {
                schedule_public_id: request.schedule_public_id.clone(),
            })
        })?;
    Ok(schedule_to_info(&schedule, None))
}

pub async fn list_schedules(
    agent: &LocalAgentHandle,
    request: ListSchedulesControlRequest,
) -> Result<ScheduleListInfo, Error> {
    #[cfg(feature = "remote")]
    if let Some(node_id) = request.node_id.clone() {
        let nm_ref = agent.find_node_manager(&node_id).await?;
        let schedules = agent
            .list_remote_schedules(&nm_ref, request.session_id.clone())
            .await?
            .schedules
            .into_iter()
            .map(|schedule| schedule_to_info(&schedule, Some(&node_id)))
            .collect();
        return Ok(ScheduleListInfo {
            node_id: Some(node_id),
            session_id: request.session_id,
            schedules,
        });
    }

    let schedules = agent
        .list_schedules(request.session_id.as_deref())
        .await?
        .into_iter()
        .map(|schedule| schedule_to_info(&schedule, None))
        .collect();
    Ok(ScheduleListInfo {
        node_id: None,
        session_id: request.session_id,
        schedules,
    })
}

pub async fn schedule_action(
    agent: &LocalAgentHandle,
    request: ScheduleActionControlRequest,
    action: &str,
) -> Result<ScheduleActionResult, Error> {
    #[cfg(feature = "remote")]
    if let Some(node_id) = request.node_id.clone() {
        let nm_ref = agent.find_node_manager(&node_id).await?;
        match action {
            "pause" => {
                agent
                    .pause_remote_schedule(&nm_ref, request.schedule_public_id.clone())
                    .await?
            }
            "resume" => {
                agent
                    .resume_remote_schedule(&nm_ref, request.schedule_public_id.clone())
                    .await?
            }
            "trigger" => {
                agent
                    .trigger_remote_schedule(&nm_ref, request.schedule_public_id.clone())
                    .await?
            }
            "delete" => {
                agent
                    .delete_remote_schedule(&nm_ref, request.schedule_public_id.clone())
                    .await?
            }
            _ => return Err(Error::method_not_found()),
        }
        let result = ScheduleActionResult {
            success: true,
            node_id: Some(node_id.clone()),
            schedule_public_id: request.schedule_public_id.clone(),
            action: action.to_string(),
        };
        emit_schedule_changed_notification(
            agent,
            crate::control::notifications::SchedulesChangedNotification {
                node_id: Some(node_id),
                session_id: None,
                schedule_public_id: request.schedule_public_id,
                change: schedule_change_name(action).to_string(),
                schedule: None,
            },
        )
        .await;
        return Ok(result);
    }

    match action {
        "pause" => agent.pause_schedule(&request.schedule_public_id).await?,
        "resume" => agent.resume_schedule(&request.schedule_public_id).await?,
        "trigger" => {
            agent
                .trigger_schedule_now(&request.schedule_public_id)
                .await?
        }
        "delete" => agent.delete_schedule(&request.schedule_public_id).await?,
        _ => return Err(Error::method_not_found()),
    }

    let result = ScheduleActionResult {
        success: true,
        node_id: None,
        schedule_public_id: request.schedule_public_id.clone(),
        action: action.to_string(),
    };
    emit_schedule_changed_notification(
        agent,
        crate::control::notifications::SchedulesChangedNotification {
            node_id: None,
            session_id: None,
            schedule_public_id: request.schedule_public_id,
            change: schedule_change_name(action).to_string(),
            schedule: None,
        },
    )
    .await;
    Ok(result)
}

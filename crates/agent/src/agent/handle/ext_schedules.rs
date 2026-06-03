use super::utils::ext_json_response;
use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_schedule_create(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CreateScheduleReq {
            #[serde(default)]
            node_id: Option<String>,
            session_id: String,
            prompt: String,
            trigger: crate::session::domain_schedule::ScheduleTrigger,
            #[serde(default)]
            max_steps: Option<u32>,
            #[serde(default)]
            max_cost_usd: Option<f64>,
            #[serde(default)]
            max_runs: Option<u32>,
        }

        let parsed: CreateScheduleReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        if let Some(node_id) = parsed.node_id.clone() {
            let nm_ref = self.find_node_manager(&node_id).await?;
            let response = self
                .create_remote_schedule(
                    &nm_ref,
                    crate::agent::remote::CreateRemoteSchedule {
                        session_id: parsed.session_id,
                        prompt: parsed.prompt,
                        trigger: parsed.trigger,
                        max_steps: parsed.max_steps,
                        max_cost_usd: parsed.max_cost_usd,
                        max_runs: parsed.max_runs,
                    },
                )
                .await?;
            return ext_json_response(&serde_json::json!({
                "schedulePublicId": response.schedule_public_id,
                "nodeId": node_id,
            }));
        }

        let schedule_public_id = self
            .create_scheduled_task(
                &parsed.session_id,
                &parsed.prompt,
                parsed.trigger,
                parsed.max_steps,
                parsed.max_cost_usd,
                parsed.max_runs,
            )
            .await?;
        ext_json_response(&serde_json::json!({
            "schedulePublicId": schedule_public_id,
            "nodeId": parsed.node_id,
        }))
    }

    pub(super) async fn handle_ext_schedule_list(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ListSchedulesReq {
            #[serde(default)]
            node_id: Option<String>,
            #[serde(default)]
            session_id: Option<String>,
        }

        let parsed: ListSchedulesReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        if let Some(node_id) = parsed.node_id.clone() {
            let nm_ref = self.find_node_manager(&node_id).await?;
            let response = self
                .list_remote_schedules(&nm_ref, parsed.session_id)
                .await?;
            return ext_json_response(&serde_json::json!({
                "nodeId": node_id,
                "schedules": response.schedules,
            }));
        }

        let schedules = self.list_schedules(parsed.session_id.as_deref()).await?;
        ext_json_response(&serde_json::json!({
            "nodeId": parsed.node_id,
            "schedules": schedules,
        }))
    }

    pub(super) async fn handle_ext_schedule_pause(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        self.handle_ext_schedule_action(req, "pause").await
    }

    pub(super) async fn handle_ext_schedule_resume(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        self.handle_ext_schedule_action(req, "resume").await
    }

    pub(super) async fn handle_ext_schedule_trigger(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        self.handle_ext_schedule_action(req, "trigger").await
    }

    pub(super) async fn handle_ext_schedule_delete(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        self.handle_ext_schedule_action(req, "delete").await
    }

    async fn handle_ext_schedule_action(
        &self,
        req: ExtRequest,
        action: &str,
    ) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ActionReq {
            #[serde(default)]
            node_id: Option<String>,
            schedule_public_id: String,
        }

        let parsed: ActionReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        if let Some(node_id) = parsed.node_id.clone() {
            let nm_ref = self.find_node_manager(&node_id).await?;
            match action {
                "pause" => {
                    self.pause_remote_schedule(&nm_ref, parsed.schedule_public_id.clone())
                        .await?
                }
                "resume" => {
                    self.resume_remote_schedule(&nm_ref, parsed.schedule_public_id.clone())
                        .await?
                }
                "trigger" => {
                    self.trigger_remote_schedule(&nm_ref, parsed.schedule_public_id.clone())
                        .await?
                }
                "delete" => {
                    self.delete_remote_schedule(&nm_ref, parsed.schedule_public_id.clone())
                        .await?
                }
                _ => return Err(Error::method_not_found()),
            }
            return ext_json_response(&serde_json::json!({
                "success": true,
                "nodeId": node_id,
                "schedulePublicId": parsed.schedule_public_id,
            }));
        }

        match action {
            "pause" => self.pause_schedule(&parsed.schedule_public_id).await?,
            "resume" => self.resume_schedule(&parsed.schedule_public_id).await?,
            "trigger" => {
                self.trigger_schedule_now(&parsed.schedule_public_id)
                    .await?
            }
            "delete" => self.delete_schedule(&parsed.schedule_public_id).await?,
            _ => return Err(Error::method_not_found()),
        }

        ext_json_response(&serde_json::json!({
            "success": true,
            "nodeId": parsed.node_id,
            "schedulePublicId": parsed.schedule_public_id,
        }))
    }
}

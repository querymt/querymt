use super::utils::ext_json_response;
use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_schedule_create(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::schedules::CreateScheduleControlRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::schedules::create_schedule(self, parsed).await?;
        ext_json_response(&serde_json::json!({ "schedule": response }))
    }

    pub(super) async fn handle_ext_schedule_list(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::schedules::ListSchedulesControlRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::schedules::list_schedules(self, parsed).await?;
        ext_json_response(&response)
    }

    pub(super) async fn handle_ext_schedule_get(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::schedules::GetScheduleControlRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::schedules::get_schedule(self, parsed).await?;
        ext_json_response(&serde_json::json!({ "schedule": response }))
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
        let parsed: crate::control::schedules::ScheduleActionControlRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::schedules::schedule_action(self, parsed, action).await?;
        ext_json_response(&response)
    }
}

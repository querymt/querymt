use agent_client_protocol::schema::v1 as acp;
use clap::ValueEnum;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum PermissionPolicy {
    #[default]
    AllowOnce,
    RejectOnce,
    Cancel,
}

pub fn permission_response(
    request: &acp::RequestPermissionRequest,
    policy: PermissionPolicy,
) -> (acp::RequestPermissionResponse, Value) {
    let wanted = match policy {
        PermissionPolicy::AllowOnce => Some(acp::PermissionOptionKind::AllowOnce),
        PermissionPolicy::RejectOnce => Some(acp::PermissionOptionKind::RejectOnce),
        PermissionPolicy::Cancel => None,
    };
    let selected = wanted.and_then(|kind| {
        request
            .options
            .iter()
            .find(|option| option.kind == kind)
            .or_else(|| request.options.first())
    });
    match selected {
        Some(option) if wanted.is_some() => (
            acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Selected(
                acp::SelectedPermissionOutcome::new(option.option_id.clone()),
            )),
            json!({
                "type": "permission",
                "sessionId": request.session_id.to_string(),
                "optionId": option.option_id.to_string(),
                "kind": match option.kind {
                    acp::PermissionOptionKind::AllowOnce => "allowOnce",
                    acp::PermissionOptionKind::AllowAlways => "allowAlways",
                    acp::PermissionOptionKind::RejectOnce => "rejectOnce",
                    acp::PermissionOptionKind::RejectAlways => "rejectAlways",
                    _ => "other",
                },
                "name": option.name,
            }),
        ),
        _ => (
            acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled),
            json!({
                "type": "permission",
                "sessionId": request.session_id.to_string(),
                "cancelled": true,
            }),
        ),
    }
}

pub fn elicitation_cancel_response() -> acp::CreateElicitationResponse {
    acp::CreateElicitationResponse::new(acp::ElicitationAction::Cancel)
}

pub fn elicitation_event(id: &Value, request: &acp::CreateElicitationRequest) -> Value {
    let session_id = request_session_id(request);
    let (message, schema) = match &request.mode {
        acp::ElicitationMode::Form(form) => (
            request.message.clone(),
            serde_json::to_value(&form.requested_schema).unwrap_or(Value::Null),
        ),
        acp::ElicitationMode::Url(url) => (request.message.clone(), json!({ "url": url.url })),
        _ => (request.message.clone(), Value::Null),
    };
    json!({
        "type": "elicitation",
        "id": id,
        "sessionId": session_id,
        "message": message,
        "requestedSchema": schema,
        "cancelled": true,
    })
}

fn request_session_id(request: &acp::CreateElicitationRequest) -> Option<String> {
    let scope = match &request.mode {
        acp::ElicitationMode::Form(form) => Some(&form.scope),
        acp::ElicitationMode::Url(url) => Some(&url.scope),
        _ => None,
    }?;
    match scope {
        acp::ElicitationScope::Session(session) => Some(session.session_id.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_allow_once_selects_matching_option() {
        let request = acp::RequestPermissionRequest::new(
            "session-1",
            acp::ToolCallUpdate::new("tool-1", acp::ToolCallUpdateFields::new()),
            vec![
                acp::PermissionOption::new(
                    "reject",
                    "Reject",
                    acp::PermissionOptionKind::RejectOnce,
                ),
                acp::PermissionOption::new("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
            ],
        );
        let (response, event) = permission_response(&request, PermissionPolicy::AllowOnce);
        match response.outcome {
            acp::RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(selected.option_id.to_string(), "allow");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(event["optionId"], "allow");
        assert_eq!(event["kind"], "allowOnce");
    }

    #[test]
    fn permission_cancel_does_not_select_always() {
        let request = acp::RequestPermissionRequest::new(
            "session-1",
            acp::ToolCallUpdate::new("tool-1", acp::ToolCallUpdateFields::new()),
            vec![acp::PermissionOption::new(
                "reject-always",
                "Reject always",
                acp::PermissionOptionKind::RejectAlways,
            )],
        );
        let (response, event) = permission_response(&request, PermissionPolicy::Cancel);
        assert!(matches!(
            response.outcome,
            acp::RequestPermissionOutcome::Cancelled
        ));
        assert_eq!(event["cancelled"], true);
    }
}

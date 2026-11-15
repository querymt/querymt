use super::*;

pub(super) fn format_error_chain(err: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in err.chain() {
        let message = cause.to_string();
        if parts.last() != Some(&message) {
            parts.push(message);
        }
    }
    parts.join(": ")
}

pub(super) fn format_prefixed_error_chain(prefix: &str, err: &anyhow::Error) -> String {
    format!("{prefix}: {}", format_error_chain(err))
}

/// Helper to build an `ExtResponse` from a serializable value.
pub(super) fn ext_json_response<T: serde::Serialize>(
    value: &T,
) -> Result<ExtResponse, agent_client_protocol::Error> {
    let json = serde_json::to_string(value).map_err(|e| {
        agent_client_protocol::Error::from(crate::error::AgentError::Serialization(e.to_string()))
    })?;
    let raw = serde_json::value::RawValue::from_string(json).map_err(|e| {
        agent_client_protocol::Error::from(crate::error::AgentError::Serialization(e.to_string()))
    })?;
    Ok(ExtResponse::new(Arc::from(raw)))
}

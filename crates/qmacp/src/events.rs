use serde_json::{Value, json};

pub fn observe_assistant_text(buffer: &mut String, event: &Value) {
    if event.get("type").and_then(Value::as_str) != Some("text") {
        return;
    }
    if event.get("role").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    if let Some(text) = event.get("text").and_then(Value::as_str) {
        buffer.push_str(text);
    }
}

pub fn compact_input_state(params: &Value) -> Option<Value> {
    let session_id = params
        .get("session_id")
        .or_else(|| params.get("sessionId"))?
        .clone();
    let input_id = params
        .get("input_id")
        .or_else(|| params.get("inputId"))?
        .clone();
    let mut event = serde_json::Map::from_iter([
        ("type".to_string(), json!("input_state")),
        (
            "version".to_string(),
            params.get("version").cloned().unwrap_or_else(|| json!(1)),
        ),
        ("sessionId".to_string(), session_id),
        ("inputId".to_string(), input_id),
        (
            "delivery".to_string(),
            params.get("delivery").cloned().unwrap_or(Value::Null),
        ),
        (
            "state".to_string(),
            params.get("state").cloned().unwrap_or(Value::Null),
        ),
    ]);
    for (output_key, input_keys) in [
        ("runId", ["run_id", "runId"]),
        ("latencyMs", ["latency_ms", "latencyMs"]),
    ] {
        if let Some(value) = input_keys.iter().find_map(|key| params.get(*key)).cloned() {
            event.insert(output_key.to_string(), value);
        }
    }
    for key in ["position", "boundary", "reason"] {
        if let Some(value) = params.get(key).cloned() {
            event.insert(key.to_string(), value);
        }
    }
    Some(Value::Object(event))
}

/// Delegation workflow state is independent of its dispatch tool's status.
pub fn compact_delegation_update(params: &Value) -> Option<Value> {
    let fields = params.as_object()?;
    for key in ["sessionId", "delegationId", "state"] {
        fields.get(key)?.as_str()?;
    }
    let mut event = fields.clone();
    event.insert("type".to_string(), json!("delegation"));
    Some(Value::Object(event))
}

pub fn compact_session_update(session_id: &Value, update: &Value) -> Option<Value> {
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let session_id = session_id.clone();
    match kind {
        "agent_message_chunk" | "user_message_chunk" => Some(json!({
            "type": "text",
            "sessionId": session_id,
            "role": if kind.starts_with("user") { "user" } else { "assistant" },
            "text": content_text(update.get("content").unwrap_or(&Value::Null)),
        })),
        "agent_thought_chunk" => None,
        "tool_call" | "tool_call_update" => Some(json!({
            "type": "tool",
            "sessionId": session_id,
            "id": first_string(update, &["toolCallId", "tool_call_id", "id"]),
            "name": first_string(update, &["title", "kind", "name"]),
            "status": first_string(update, &["status"]).unwrap_or_else(|| {
                if kind == "tool_call" {
                    "started".to_string()
                } else {
                    "updated".to_string()
                }
            }),
        })),
        "current_mode_update" => Some(json!({
            "type": "mode",
            "sessionId": session_id,
            "mode": first_string(update, &["currentModeId", "current_mode_id", "mode"]),
        })),
        "plan" => Some(json!({
            "type": "plan",
            "sessionId": session_id,
            "entries": update.get("entries").cloned().unwrap_or(Value::Null),
        })),
        "available_commands_update" => None,
        _ => Some(json!({
            "type": "update",
            "sessionId": session_id,
            "kind": kind,
        })),
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(content_text).collect::<Vec<_>>().join(""),
        Value::Object(_) => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match value.get(*key)? {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_input_state_notification() {
        let event = compact_input_state(&json!({
            "version": 1,
            "session_id": "s1",
            "input_id": "i1",
            "delivery": "steer",
            "state": "applied",
            "run_id": "r1",
            "boundary": "after_tools",
            "latency_ms": 42
        }))
        .unwrap();
        assert_eq!(event["type"], "input_state");
        assert_eq!(event["sessionId"], "s1");
        assert_eq!(event["inputId"], "i1");
        assert_eq!(event["state"], "applied");
        assert_eq!(event["runId"], "r1");
        assert_eq!(event["latencyMs"], 42);
        assert!(event.get("position").is_none());
    }

    #[test]
    fn compact_discarded_queue_notification() {
        let event = compact_input_state(&json!({
            "version": 1,
            "session_id": "s1",
            "input_id": "i2",
            "delivery": "queue",
            "state": "discarded",
            "reason": "removed_by_user"
        }))
        .unwrap();
        assert_eq!(event["delivery"], "queue");
        assert_eq!(event["state"], "discarded");
        assert_eq!(event["reason"], "removed_by_user");
    }

    #[test]
    fn compact_agent_text_chunk() {
        let event = compact_session_update(
            &json!("s1"),
            &json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "hello" }
            }),
        )
        .unwrap();
        assert_eq!(event["type"], "text");
        assert_eq!(event["role"], "assistant");
        assert_eq!(event["text"], "hello");
    }

    #[test]
    fn skips_thought_chunks() {
        assert!(
            compact_session_update(
                &json!("s1"),
                &json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "secret" }
                }),
            )
            .is_none()
        );
    }

    #[test]
    fn compact_tool_and_mode() {
        let tool = compact_session_update(
            &json!("s1"),
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "t1",
                "title": "bash",
                "status": "pending"
            }),
        )
        .unwrap();
        assert_eq!(tool["type"], "tool");
        assert_eq!(tool["id"], "t1");
        assert_eq!(tool["name"], "bash");

        let mode = compact_session_update(
            &json!("s1"),
            &json!({
                "sessionUpdate": "current_mode_update",
                "currentModeId": "plan"
            }),
        )
        .unwrap();
        assert_eq!(mode["mode"], "plan");
    }

    #[test]
    fn observe_assistant_text_concatenates_chunks() {
        let mut text = String::new();
        observe_assistant_text(
            &mut text,
            &json!({"type":"text","role":"assistant","text":"STE"}),
        );
        observe_assistant_text(
            &mut text,
            &json!({"type":"text","role":"user","text":"ignore"}),
        );
        observe_assistant_text(
            &mut text,
            &json!({"type":"text","role":"assistant","text":"ERED"}),
        );
        assert_eq!(text, "STEERED");
    }
}

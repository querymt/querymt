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

/// A delegated child session as the agent reports it in a `subagent_update`,
/// reported separately from the status of the tool call that dispatched it.
///
/// `parent` is the session the update was sent on (a load snapshot's
/// `subagentUpdates` entries do not carry it, so the caller supplies it).
/// The update is a patch: a field the agent left out is left out here too,
/// and `cancellable` is only reported when the update carries `capabilities`.
pub fn compact_subagent_update(parent: Option<&Value>, update: &Value) -> Option<Value> {
    let child = update.get("sessionId")?.as_str()?;
    let mut event = serde_json::Map::from_iter([
        ("type".to_string(), json!("delegation")),
        ("childSessionId".to_string(), json!(child)),
    ]);
    if let Some(parent) = parent.filter(|parent| !parent.is_null()) {
        event.insert("sessionId".to_string(), parent.clone());
    }
    for key in ["title", "description"] {
        if let Some(text) = update.get(key).and_then(Value::as_str) {
            event.insert(key.to_string(), json!(text));
        }
    }
    if let Some(state) = update["state"]["state"].as_str() {
        event.insert("state".to_string(), json!(state));
    }
    if let Some(capabilities) = update.get("capabilities") {
        event.insert(
            "cancellable".to_string(),
            json!(capabilities.get("cancel").is_some_and(Value::is_object)),
        );
    }
    let querymt = &update["_meta"]["querymt"];
    for (output_key, meta_key) in [
        ("stage", "stage"),
        ("delegationId", "delegationId"),
        ("modelId", "modelId"),
        ("providerNodeId", "providerNodeId"),
        ("requestedAt", "requestedAt"),
        ("forkedAt", "forkedAt"),
        ("finishedAt", "finishedAt"),
        ("updatedAt", "updatedAt"),
    ] {
        if let Some(value) = querymt.get(meta_key).filter(|value| !value.is_null()) {
            event.insert(output_key.to_string(), value.clone());
        }
    }
    Some(Value::Object(event))
}

/// The child session a tool call's `session` content points at.
fn tool_child_session(update: &Value) -> Option<String> {
    update
        .get("content")?
        .as_array()?
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("session"))?
        .get("sessionId")?
        .as_str()
        .map(str::to_owned)
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
        "tool_call" | "tool_call_update" => {
            let mut event = json!({
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
            });
            // A delegate tool call that points at the child it started.
            if let Some(child) = tool_child_session(update) {
                event["childSessionId"] = json!(child);
            }
            Some(event)
        }
        "subagent_update" => compact_subagent_update(Some(&session_id), update),
        // The id is opaque: it is only echoed back to `fork`.
        "cursor_update" => Some(json!({
            "type": "cursor",
            "sessionId": session_id,
            "cursorId": first_string(update, &["cursorId", "cursor_id"]),
            "anchor": update.get("anchor").cloned().unwrap_or(Value::Null),
            "forkable": update["capabilities"]["fork"].is_object(),
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
    fn compact_subagent_update_keeps_patch_semantics() {
        let running = compact_session_update(
            &json!("parent"),
            &json!({
                "sessionUpdate": "subagent_update", "sessionId": "c1",
                "title": "coder", "description": "fix the build",
                "state": {"state": "running"}, "capabilities": {"cancel": {}},
                "_meta": {"querymt": {"stage": "verifying", "modelId": "gpt-5", "delegationId": "d1"}},
            }),
        )
        .unwrap();
        assert_eq!(running["type"], "delegation");
        assert_eq!(running["sessionId"], "parent");
        assert_eq!(running["childSessionId"], "c1");
        assert_eq!(running["state"], "running");
        assert_eq!(running["stage"], "verifying");
        assert_eq!(running["cancellable"], true);
        assert_eq!(running["modelId"], "gpt-5");

        // A patch that only changes the state leaves the other fields out.
        let idle = compact_session_update(
            &json!("parent"),
            &json!({
                "sessionUpdate": "subagent_update", "sessionId": "c1",
                "state": {"state": "idle"}, "capabilities": null,
            }),
        )
        .unwrap();
        assert_eq!(idle["state"], "idle");
        assert_eq!(idle["cancellable"], false);
        assert!(idle.get("title").is_none() && idle.get("stage").is_none());
    }

    #[test]
    fn compact_tool_names_the_child_session() {
        let tool = compact_session_update(
            &json!("parent"),
            &json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "t1",
                "content": [{"type": "session", "sessionId": "c1"}],
            }),
        )
        .unwrap();
        assert_eq!(tool["childSessionId"], "c1");
        let plain = compact_session_update(
            &json!("parent"),
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": "t2", "status": "failed"}),
        )
        .unwrap();
        assert!(plain.get("childSessionId").is_none());
    }

    #[test]
    fn compact_cursor_update_reports_forkability() {
        let forkable = compact_session_update(
            &json!("s"),
            &json!({
                "sessionUpdate": "cursor_update", "cursorId": "c-1",
                "anchor": {"type": "user_message", "messageId": "m1", "position": "after"},
                "capabilities": {"fork": {}},
            }),
        )
        .unwrap();
        assert_eq!(forkable["type"], "cursor");
        assert_eq!(forkable["cursorId"], "c-1");
        assert_eq!(forkable["forkable"], true);
        let plain = compact_session_update(
            &json!("s"),
            &json!({"sessionUpdate": "cursor_update", "cursorId": "c-2", "anchor": {}}),
        )
        .unwrap();
        assert_eq!(plain["forkable"], false);
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

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::events;

const SESSION_LOAD_SNAPSHOT_META_KEY: &str = "querymt/sessionLoadSnapshot.v1";
const INSPECT_TEXT_LIMIT: usize = 4_000;
const FIND_PAGE_CAP: usize = 20;

pub fn resolve_cwd(cwd: Option<PathBuf>) -> Result<PathBuf> {
    let path = cwd.unwrap_or(std::env::current_dir().context("current directory")?);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .context("current directory")?
            .join(path)
    };
    if !path.is_absolute() {
        bail!("cwd must be an absolute path: {}", path.display());
    }
    Ok(path)
}

pub fn resolve_optional_cwd(cwd: Option<PathBuf>) -> Result<Option<PathBuf>> {
    cwd.map(|cwd| resolve_cwd(Some(cwd))).transpose()
}

pub fn profile_meta(profile_id: &str) -> Map<String, Value> {
    let mut meta = Map::new();
    meta.insert("querymt".to_string(), json!({ "profile_id": profile_id }));
    meta
}

/// A usable self-execution phase requires an available backend observation.
pub fn observed_phase(runtime: &Value) -> Option<&str> {
    let phase = runtime.get("phase")?.as_str()?;
    match runtime.pointer("/observation/availability")?.as_str()? {
        "live"
            if matches!(
                phase,
                "idle"
                    | "starting"
                    | "model"
                    | "tools"
                    | "waiting"
                    | "closing"
                    | "cancel_requested"
            ) =>
        {
            Some(phase)
        }
        "inactive" if phase == "idle" => Some(phase),
        _ => None,
    }
}

pub fn session_summary(session: &acp::SessionInfo) -> Value {
    compact_session_summary(session)
}

pub fn compact_session_summary(session: &acp::SessionInfo) -> Value {
    let meta = session
        .meta
        .as_ref()
        .and_then(|meta| serde_json::to_value(meta).ok())
        .unwrap_or(Value::Null);
    let runtime = meta.get("runtimeStatus").cloned().unwrap_or(Value::Null);
    let phase = runtime
        .get("phase")
        .filter(|phase| !phase.is_null())
        .map(|_| observed_phase(&runtime).unwrap_or("unknown"));
    let mut summary = json!({
        "sessionId": session.session_id.to_string(),
        "cwd": path_string(&session.cwd),
        "title": session.title,
        "updatedAt": session.updated_at,
        "phase": phase,
        "messageCount": meta.get("messageCount").cloned(),
        "runtime": runtime,
    });
    let querymt = meta.get("querymt");
    let lifecycle = querymt
        .and_then(|querymt| querymt.get("lifecycle"))
        .filter(|lifecycle| !lifecycle.is_null());
    let pin = querymt
        .and_then(|querymt| querymt.get("pin"))
        .filter(|pin| !pin.is_null());
    if lifecycle.is_some() || pin.is_some() {
        let mut known_metadata = Map::new();
        if let Some(lifecycle) = lifecycle {
            known_metadata.insert("lifecycle".to_string(), lifecycle.clone());
        }
        if let Some(pin) = pin {
            known_metadata.insert("pin".to_string(), pin.clone());
        }
        summary["_meta"] = json!({ "querymt": known_metadata });
    }
    summary
}

pub fn config_status(
    session_id: &str,
    modes: Option<&acp::SessionModeState>,
    raw: &Value,
) -> Value {
    let mut options = match config_values_from_raw(raw) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    if !options.contains_key("model")
        && let Some(model) = model_from_snapshot(raw)
    {
        options.insert("model".to_string(), json!(model));
    }
    let options = Value::Object(options);
    json!({
        "sessionId": session_id,
        "mode": modes.map(|state| state.current_mode_id.to_string()),
        "profile": options.get("profile").cloned(),
        "model": options.get("model").cloned(),
        "reasoningEffort": options.get("reasoning_effort").cloned(),
        "config": options,
    })
}

pub fn model_from_snapshot(raw: &Value) -> Option<String> {
    let events = session_load_snapshot(raw)
        .and_then(|snapshot| snapshot.get("audit"))
        .and_then(|audit| audit.get("events"))
        .and_then(Value::as_array)?;
    events.iter().rev().find_map(|event| {
        let kind = event.get("kind")?;
        if kind.get("type").and_then(Value::as_str) != Some("provider_changed") {
            return None;
        }
        let data = kind.get("data")?;
        let model = data.get("model").and_then(Value::as_str)?;
        match data.get("provider").and_then(Value::as_str) {
            Some(provider) if !provider.is_empty() => Some(format!("{provider}/{model}")),
            _ => Some(model.to_string()),
        }
    })
}

pub fn config_values_from_raw(raw: &Value) -> Value {
    let items = raw
        .get("configOptions")
        .or_else(|| raw.get("config_options"))
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| raw.as_array().cloned())
        .unwrap_or_default();
    let mut values = Map::new();
    for option in items {
        let Some(id) = option.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(current) = current_config_value(&option) {
            values.insert(id.to_string(), current);
        }
    }
    Value::Object(values)
}

fn current_config_value(option: &Value) -> Option<Value> {
    option
        .get("currentValue")
        .cloned()
        .or_else(|| option.get("value").cloned())
        .or_else(|| option.pointer("/kind/currentValue").cloned())
        .or_else(|| option.pointer("/select/currentValue").cloned())
        .filter(|value| !value.is_null())
}

pub fn compact_inspect(
    session_id: &str,
    cwd: &Path,
    modes: Option<&acp::SessionModeState>,
    load_value: &Value,
    message_limit: Option<usize>,
    include_tools: bool,
) -> Value {
    let snapshot = session_load_snapshot(load_value);
    let mut messages = compact_messages(snapshot);
    if let Some(limit) = message_limit.filter(|limit| *limit > 0)
        && messages.len() > limit
    {
        messages = messages.split_off(messages.len() - limit);
    }
    json!({
        "sessionId": session_id,
        "cwd": path_string(cwd),
        "status": config_status(session_id, modes, load_value),
        "messages": messages,
        "tools": if include_tools {
            Value::Array(compact_tools(snapshot))
        } else {
            Value::Array(Vec::new())
        },
        "delegations": compact_delegations(snapshot),
        "errors": compact_errors(snapshot),
    })
}

fn compact_delegations(snapshot: Option<&Value>) -> Vec<Value> {
    snapshot
        .and_then(|snapshot| snapshot.get("delegationUpdates"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(events::compact_delegation_update)
        .collect()
}

fn session_load_snapshot(response: &Value) -> Option<&Value> {
    response
        .get("_meta")
        .or_else(|| response.get("meta"))
        .and_then(|meta| meta.get(SESSION_LOAD_SNAPSHOT_META_KEY))
}

fn compact_messages(snapshot: Option<&Value>) -> Vec<Value> {
    let Some(events) = snapshot
        .and_then(|snapshot| snapshot.get("audit"))
        .and_then(|audit| audit.get("events"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut messages = Vec::new();
    for event in events {
        let Some(kind) = event.get("kind") else {
            continue;
        };
        let Some(kind_type) = kind.get("type").and_then(Value::as_str) else {
            continue;
        };
        let data = kind.get("data").unwrap_or(&Value::Null);
        match kind_type {
            "prompt_received" => messages.push(json!({
                "role": "user",
                "messageId": string_field(data, "message_id"),
                "text": truncate_text(&content_text(data.get("content").unwrap_or(&Value::Null))),
            })),
            "assistant_message_stored" => messages.push(json!({
                "role": "assistant",
                "messageId": string_field(data, "message_id"),
                "text": truncate_text(&string_field(data, "content").unwrap_or_default()),
                "thinking": string_field(data, "thinking").filter(|text| !text.is_empty()),
            })),
            _ => {}
        }
    }
    messages
}

fn compact_tools(snapshot: Option<&Value>) -> Vec<Value> {
    let Some(events) = snapshot
        .and_then(|snapshot| snapshot.get("audit"))
        .and_then(|audit| audit.get("events"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    events
        .iter()
        .filter_map(|event| {
            let kind = event.get("kind")?;
            let kind_type = kind.get("type").and_then(Value::as_str)?;
            let data = kind.get("data").unwrap_or(&Value::Null);
            match kind_type {
                "tool_call_start" => Some(json!({
                    "id": string_field(data, "tool_call_id"),
                    "name": string_field(data, "tool_name").unwrap_or_else(|| "tool".to_string()),
                    "status": "started",
                    "arguments": data.get("arguments").or_else(|| data.get("input")).cloned(),
                })),
                "tool_call_end" => Some(json!({
                    "id": string_field(data, "tool_call_id"),
                    "name": string_field(data, "tool_name").unwrap_or_else(|| "tool".to_string()),
                    "status": if data.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                        "error"
                    } else {
                        "ok"
                    },
                    "result": truncate_opt(string_field(data, "result")
                        .or_else(|| string_field(data, "output"))
                        .or_else(|| string_field(data, "content"))),
                })),
                _ => None,
            }
        })
        .collect()
}

fn compact_errors(snapshot: Option<&Value>) -> Vec<Value> {
    let Some(events) = snapshot
        .and_then(|snapshot| snapshot.get("audit"))
        .and_then(|audit| audit.get("events"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    events
        .iter()
        .filter_map(|event| {
            let kind = event.get("kind")?;
            let kind_type = kind.get("type").and_then(Value::as_str)?;
            if kind_type != "error" && kind_type != "cancelled" {
                return None;
            }
            Some(json!({
                "type": kind_type,
                "data": kind.get("data").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect()
}

pub fn matches_query_and_phase(session: &Value, query: &str, phase: Option<&str>) -> bool {
    if let Some(phase) = phase {
        let current = session
            .get("phase")
            .and_then(Value::as_str)
            .or_else(|| session.get("runtime").and_then(observed_phase))
            .unwrap_or("");
        if !current.eq_ignore_ascii_case(phase) {
            return false;
        }
    }
    let query = query.to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    for key in ["sessionId", "title", "cwd", "phase"] {
        if session
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| value.to_ascii_lowercase().contains(&query))
        {
            return true;
        }
    }
    false
}

pub fn filter_models(payload: Value, query: Option<&str>, provider: Option<&str>) -> Value {
    let mut root = payload;
    let Some(models) = root.get_mut("models").and_then(Value::as_array_mut) else {
        return root;
    };
    models.retain(|model| {
        let provider_ok = provider.is_none_or(|wanted| {
            model
                .get("provider")
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case(wanted))
        });
        let query_ok = query.is_none_or(|wanted| {
            let wanted = wanted.to_ascii_lowercase();
            ["id", "label", "model", "provider"].iter().any(|key| {
                model
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.to_ascii_lowercase().contains(&wanted))
            })
        });
        provider_ok && query_ok
    });
    root
}

pub fn find_page_cap() -> usize {
    FIND_PAGE_CAP
}

pub fn last_assistant_text(inspect: &Value) -> Option<String> {
    inspect
        .get("messages")
        .and_then(Value::as_array)?
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .and_then(|message| message.get("text").and_then(Value::as_str))
        .map(str::to_string)
        .filter(|text| !text.is_empty())
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn string_field(data: &Value, key: &str) -> Option<String> {
    match data.get(key)? {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => serde_json::to_string(other).ok(),
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| item.as_str().map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(_) => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn truncate_text(text: &str) -> String {
    if text.chars().count() <= INSPECT_TEXT_LIMIT {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(INSPECT_TEXT_LIMIT).collect();
        format!("{truncated}…")
    }
}

fn truncate_opt(text: Option<String>) -> Option<String> {
    text.map(|text| truncate_text(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_inspect_extracts_user_and_tool_events() {
        let load = json!({
            "_meta": {
                "querymt/sessionLoadSnapshot.v1": {
                    "audit": {
                        "events": [
                            {"kind": {"type": "prompt_received", "data": {
                                "message_id": "m1",
                                "content": [{"type": "text", "text": "hello"}]
                            }}},
                            {"kind": {"type": "tool_call_end", "data": {
                                "tool_call_id": "t1",
                                "tool_name": "read",
                                "result": "ok"
                            }}},
                            {"kind": {"type": "assistant_message_stored", "data": {
                                "message_id": "m2",
                                "content": "done"
                            }}}
                        ]
                    }
                }
            }
        });
        let compact = compact_inspect("s1", Path::new("/repo"), None, &load, None, true);
        assert_eq!(compact["messages"][0]["role"], "user");
        assert_eq!(compact["messages"][0]["text"], "hello");
        assert_eq!(compact["tools"][0]["name"], "read");
        assert_eq!(compact["messages"][1]["text"], "done");
    }

    #[test]
    fn find_matches_id_title_or_cwd() {
        let session = json!({
            "sessionId": "abc-123",
            "title": "Fix build",
            "cwd": "/tmp/project"
        });
        assert!(matches_query_and_phase(&session, "abc", None));
        assert!(matches_query_and_phase(&session, "build", None));
        assert!(matches_query_and_phase(&session, "project", None));
        assert!(!matches_query_and_phase(&session, "missing", None));
    }

    #[test]
    fn find_can_filter_by_phase() {
        let session = json!({
            "sessionId": "abc-123",
            "title": "Fix build",
            "cwd": "/tmp/project",
            "phase": "tools"
        });
        assert!(matches_query_and_phase(&session, "build", Some("tools")));
        assert!(!matches_query_and_phase(&session, "build", Some("idle")));
    }

    #[test]
    fn last_assistant_text_uses_newest_assistant_message() {
        let inspect = json!({
            "messages": [
                {"role":"user","text":"hi"},
                {"role":"assistant","text":"first"},
                {"role":"assistant","text":"STEERED"}
            ]
        });
        assert_eq!(last_assistant_text(&inspect).as_deref(), Some("STEERED"));
    }

    #[test]
    fn model_from_snapshot_uses_latest_provider_changed() {
        let raw = json!({
            "_meta": {
                "querymt/sessionLoadSnapshot.v1": {
                    "audit": {
                        "events": [
                            {"kind": {"type": "provider_changed", "data": {
                                "provider": "anthropic",
                                "model": "claude-sonnet-4-5-20250929"
                            }}},
                            {"kind": {"type": "provider_changed", "data": {
                                "provider": "xai",
                                "model": "grok-4.5"
                            }}}
                        ]
                    }
                }
            }
        });
        assert_eq!(model_from_snapshot(&raw).as_deref(), Some("xai/grok-4.5"));
        let status = config_status("s1", None, &raw);
        assert_eq!(status["model"], "xai/grok-4.5");
        assert_eq!(status["config"]["model"], "xai/grok-4.5");
    }

    #[test]
    fn config_values_read_model_from_raw_select() {
        let raw = json!({
            "configOptions": [
                {
                    "id": "profile",
                    "name": "Profile",
                    "type": "select",
                    "currentValue": "default"
                },
                {
                    "id": "model",
                    "name": "Model",
                    "type": "select",
                    "currentValue": "xai/grok-4.5",
                    "options": [{"value": "xai/grok-4.5", "name": "grok-4.5"}]
                }
            ]
        });
        let values = config_values_from_raw(&raw);
        assert_eq!(values["profile"], "default");
        assert_eq!(values["model"], "xai/grok-4.5");
    }

    #[test]
    fn session_summary_preserves_known_lifecycle_and_pin_metadata_only() {
        let lifecycle = json!({
            "resolved": true,
            "archived": false,
            "resolved_at": "2025-01-02T03:04:05Z",
            "archived_at": null
        });
        let pin = json!({
            "pinned": true,
            "pinned_at": "2025-01-03T03:04:05Z"
        });
        let meta = serde_json::from_value::<Map<String, Value>>(json!({
            "querymt": { "lifecycle": lifecycle, "pin": pin }
        }))
        .unwrap();
        let session = acp::SessionInfo::new("session-1", "/tmp").meta(meta);
        let summary = compact_session_summary(&session);
        assert_eq!(summary["_meta"]["querymt"]["lifecycle"]["resolved"], true);
        assert_eq!(
            summary["_meta"]["querymt"]["lifecycle"]["resolved_at"],
            "2025-01-02T03:04:05Z"
        );
        assert_eq!(summary["_meta"]["querymt"]["pin"]["pinned"], true);
        assert_eq!(
            summary["_meta"]["querymt"]["pin"]["pinned_at"],
            "2025-01-03T03:04:05Z"
        );

        let pin_only_meta = serde_json::from_value::<Map<String, Value>>(json!({
            "querymt": { "pin": { "pinned": false, "pinned_at": null } }
        }))
        .unwrap();
        let pin_only = acp::SessionInfo::new("session-2", "/tmp").meta(pin_only_meta);
        let pin_only_summary = compact_session_summary(&pin_only);
        assert_eq!(pin_only_summary["_meta"]["querymt"]["pin"]["pinned"], false);
        assert!(
            pin_only_summary["_meta"]["querymt"]
                .get("lifecycle")
                .is_none()
        );

        let legacy = acp::SessionInfo::new("session-3", "/tmp");
        assert!(compact_session_summary(&legacy).get("_meta").is_none());

        let meta = serde_json::from_value::<Map<String, Value>>(json!({
            "querymt": { "lifecycle": null },
            "messageCount": 7,
            "runtimeStatus": { "phase": "idle" }
        }))
        .unwrap();
        let session = acp::SessionInfo::new("session-3", "/tmp").meta(meta);
        let summary = compact_session_summary(&session);
        assert!(summary.get("_meta").is_none());
        assert_eq!(summary["messageCount"], 7);
        assert_eq!(summary["runtime"]["phase"], "idle");
        assert_eq!(summary["title"], json!(session.title));

        let meta = serde_json::from_value::<Map<String, Value>>(json!({
            "querymt": {
                "lifecycle": null,
                "pin": {
                    "pinned": true,
                    "pinned_at": "2025-01-04T03:04:05Z"
                }
            },
            "messageCount": 8
        }))
        .unwrap();
        let session = acp::SessionInfo::new("session-4", "/tmp").meta(meta);
        let summary = compact_session_summary(&session);
        assert_eq!(summary["_meta"]["querymt"]["pin"]["pinned"], true);
        assert!(summary["_meta"]["querymt"].get("lifecycle").is_none());
        assert_eq!(summary["messageCount"], 8);

        let null_pin_meta = serde_json::from_value::<Map<String, Value>>(json!({
            "querymt": { "pin": null }
        }))
        .unwrap();
        let null_pin = acp::SessionInfo::new("session-5", "/tmp").meta(null_pin_meta);
        assert!(compact_session_summary(&null_pin).get("_meta").is_none());

        let lifecycle_with_null_pin_meta = serde_json::from_value::<Map<String, Value>>(json!({
            "querymt": { "lifecycle": lifecycle.clone(), "pin": null }
        }))
        .unwrap();
        let lifecycle_with_null_pin =
            acp::SessionInfo::new("session-6", "/tmp").meta(lifecycle_with_null_pin_meta);
        let summary = compact_session_summary(&lifecycle_with_null_pin);
        assert_eq!(summary["_meta"]["querymt"]["lifecycle"], lifecycle);
        assert!(summary["_meta"]["querymt"].get("pin").is_none());
    }
}

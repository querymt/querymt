use std::path::{Path, PathBuf};

use agent_client_protocol::v1::schema as acp;
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::{client::AcpClient, error::CliError, events};
use std::time::Duration;

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    clap::ValueEnum,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Auto,
    Prompt,
    Steer,
    Queue,
}

pub struct OpenedSession {
    pub status: Value,
}

const SESSION_LOAD_SNAPSHOT_META_KEY: &str = "querymt/sessionLoadSnapshot.v1";
const INSPECT_TEXT_LIMIT: usize = 4_000;
const FIND_PAGE_CAP: usize = 20;

/// Check only the absolute path syntax; QueryMT validates it on the server host.
pub fn is_absolute_server_cwd(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return false;
    };
    path.starts_with('/')
        || matches!(path.as_bytes(), [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic())
        || path.strip_prefix(r"\\").is_some_and(|path| {
            let mut parts = path.split(['\\', '/']);
            parts.next().is_some_and(|part| !part.is_empty())
                && parts.next().is_some_and(|part| !part.is_empty())
        })
}

pub fn resolve_cwd(cwd: Option<PathBuf>) -> Result<PathBuf> {
    match resolve_optional_cwd(cwd)? {
        Some(path) => Ok(path),
        None => std::env::current_dir().context("current directory"),
    }
}

pub fn resolve_optional_cwd(cwd: Option<PathBuf>) -> Result<Option<PathBuf>> {
    validate_cwd(cwd.as_deref(), false).map_err(anyhow::Error::msg)?;
    Ok(cwd)
}

/// cwd for a load/resume of an existing session. An omitted cwd is sent empty,
/// which QueryMT treats as "no cwd" and answers with the session's stored cwd.
pub fn existing_session_cwd(cwd: Option<PathBuf>) -> Result<PathBuf> {
    Ok(resolve_optional_cwd(cwd)?.unwrap_or_default())
}

pub fn profile_meta(profile_id: &str) -> Map<String, Value> {
    let mut meta = Map::new();
    meta.insert("querymt".to_string(), json!({ "profile_id": profile_id }));
    meta
}

/// A usable self-execution phase requires an available backend observation.
/// Validate portable cwd syntax; the CLI can supply its current directory on creation.
pub fn validate_cwd(cwd: Option<&Path>, required: bool) -> Result<(), &'static str> {
    match cwd {
        Some(path) if !is_absolute_server_cwd(path) => {
            Err("cwd must be an absolute path on the QueryMT host")
        }
        None if required => Err("a new session requires an absolute cwd on the QueryMT host"),
        _ => Ok(()),
    }
}

/// Shared static constraints for the prompt entry points and their batch preflight.
pub fn validate_prompt(
    new: bool,
    cwd: Option<&Path>,
    required: bool,
    profile: Option<&str>,
    delivery: Delivery,
) -> Result<(), &'static str> {
    if !new && cwd.is_some() {
        return Err(
            "cwd applies only when creating a session; an existing session keeps its own cwd",
        );
    }
    if !new && profile.is_some() {
        return Err("profile applies only when creating a session");
    }
    if new && !matches!(delivery, Delivery::Auto | Delivery::Prompt) {
        return Err("new sessions require auto or prompt delivery");
    }
    validate_cwd(cwd, new && required)
}

pub fn observed_run_id(runtime: &Value) -> Option<String> {
    runtime["active_run_id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
}

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

/// Add only the portion of the page that fits. A server cursor can advance
/// past the page only if all of its entries were returned.
pub fn append_session_page(
    sessions: &mut Vec<Value>,
    page: acp::ListSessionsResponse,
    limit: usize,
) -> (Option<String>, bool) {
    let remaining = limit.saturating_sub(sessions.len());
    let partial = page.sessions.len() > remaining;
    sessions.extend(page.sessions.iter().take(remaining).map(session_summary));
    (if partial { None } else { page.next_cursor }, partial)
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
    cwd: Option<&Path>,
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
        "cwd": cwd.map(path_string),
        "status": config_status(session_id, modes, load_value),
        "messages": messages,
        "tools": if include_tools {
            Value::Array(compact_tools(snapshot))
        } else {
            Value::Array(Vec::new())
        },
        "delegations": compact_delegations(session_id, snapshot),
        "errors": compact_errors(snapshot),
    })
}

/// The delegated children of a loaded session: the latest `subagent_update`
/// of each, from the snapshot's `subagentUpdates`, in the same shape as the
/// live `delegation` events.
fn compact_delegations(session_id: &str, snapshot: Option<&Value>) -> Vec<Value> {
    let parent = json!(session_id);
    snapshot
        .and_then(|snapshot| snapshot.get("subagentUpdates"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|update| events::compact_subagent_update(Some(&parent), update))
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

pub fn delivery_name(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::Auto => "auto",
        Delivery::Prompt => "prompt",
        Delivery::Steer => "steer",
        Delivery::Queue => "queue",
    }
}

pub async fn choose_delivery(
    client: &AcpClient,
    session_id: &str,
    requested: Delivery,
) -> Result<(Delivery, Option<String>), CliError> {
    if requested != Delivery::Auto {
        return Ok((requested, None));
    }
    let state = runtime(client, session_id.to_string()).await?;
    let phase = require_observed_phase(&state)?;
    let steerable = state
        .pointer("/runtime/steerable")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            CliError::rpc("runtime response has no valid steerable flag; input was not submitted")
        })?;
    let delivery = delivery_from_runtime(phase, steerable);
    let run_id = if delivery == Delivery::Steer {
        Some(observed_run_id(&state["runtime"]).ok_or_else(|| {
            CliError::rpc("runtime response has no active run ID; input was not submitted")
        })?)
    } else {
        None
    };
    Ok((delivery, run_id))
}

pub fn require_observed_phase(state: &Value) -> Result<&str, CliError> {
    observed_phase(&state["runtime"]).ok_or_else(|| {
        CliError::rpc(
            "session runtime observation is unavailable or invalid; execution state is unknown",
        )
        .with_recovery(json!({
            "sessionId": state["sessionId"], "stage": "observe",
            "observationState": "unavailable", "runtime": state["runtime"],
        }))
    })
}

pub fn delivery_from_runtime(phase: &str, steerable: bool) -> Delivery {
    if phase == "idle" {
        Delivery::Prompt
    } else if steerable {
        Delivery::Steer
    } else {
        Delivery::Queue
    }
}

pub async fn wait_until_idle(
    client: &AcpClient,
    session_id: &str,
    timeout: Duration,
) -> Result<Value, CliError> {
    wait_until_idle_every(client, session_id, timeout, Duration::from_millis(400)).await
}

pub async fn wait_until_idle_every(
    client: &AcpClient,
    session_id: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<Value, CliError> {
    let timeout = timeout.max(Duration::from_secs(1));
    wait_until_idle_by(
        client,
        session_id,
        tokio::time::Instant::now() + timeout,
        timeout,
        interval,
    )
    .await
}

async fn wait_until_idle_by(
    client: &AcpClient,
    session_id: &str,
    deadline: tokio::time::Instant,
    timeout: Duration,
    interval: Duration,
) -> Result<Value, CliError> {
    let mut last_runtime = Value::Null;
    tokio::time::timeout_at(deadline, async {
        loop {
            let value = runtime_observation(client, session_id.to_string(), true).await
                .map_err(|error| error.with_recovery(json!({"runtime": last_runtime})))?;
            last_runtime = value["runtime"].clone();
            if require_observed_phase(&value)? == "idle" {
                return Ok(value);
            }
            tokio::time::sleep(interval.max(Duration::from_millis(200))).await;
        }
    }).await.map_err(|_| CliError::Timeout(format!(
        "timed out after {}s waiting for session {session_id} to become idle (last phase: {}); remote work was not cancelled",
        timeout.as_secs(), last_runtime["phase"].as_str().unwrap_or("unknown"),
    )).with_recovery(json!({
        "sessionId": session_id, "stage": "observe", "observationState": "unavailable",
        "runtime": last_runtime,
    })))?
}

pub async fn runtime(client: &AcpClient, session_id: String) -> Result<Value, CliError> {
    runtime_observation(client, session_id, false).await
}

pub async fn runtime_observation(
    client: &AcpClient,
    session_id: String,
    wait_for_updates: bool,
) -> Result<Value, CliError> {
    let recovery = json!({
        "sessionId": session_id, "stage": "observe", "observationState": "unavailable",
    });
    client
        .initialize()
        .await
        .map_err(|error| CliError::from_request(error).with_recovery(recovery.clone()))?;
    let response = client
        .extension(
            "querymt/session/runtimeState",
            json!({ "session_id": session_id, "wait_for_updates": wait_for_updates }),
        )
        .await
        .map_err(|error| CliError::from_request(error).with_recovery(recovery))?;
    Ok(json!({
        "sessionId": session_id,
        "runtime": extension_payload(&response),
    }))
}

pub async fn apply_config(
    client: &AcpClient,
    session_id: &str,
    mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
) -> Result<(), CliError> {
    if let Some(mode) = mode {
        client
            .set_mode(session_id.to_string(), mode)
            .await
            .map_err(CliError::from_request)?;
    }
    if let Some(model) = model {
        client
            .set_config(session_id.to_string(), "model", &model)
            .await
            .map_err(CliError::from_request)?;
    }
    if let Some(effort) = effort {
        client
            .set_config(session_id.to_string(), "reasoning_effort", &effort)
            .await
            .map_err(CliError::from_request)?;
    }
    Ok(())
}

pub async fn open_session(
    client: &AcpClient,
    session_id: String,
    cwd: PathBuf,
) -> Result<OpenedSession, CliError> {
    let resumed_status = match client.resume_session(session_id.clone(), cwd.clone()).await {
        Ok((response, raw)) => {
            let status = config_status(&session_id, response.modes.as_ref(), &raw);
            if status.get("model").is_some_and(|value| !value.is_null()) {
                return Ok(OpenedSession { status });
            }
            Some(status)
        }
        Err(_) => None,
    };
    match client.load_session(session_id.clone(), cwd).await {
        Ok((loaded, raw)) => Ok(OpenedSession {
            status: config_status(&session_id, loaded.modes.as_ref(), &raw),
        }),
        Err(err) => match resumed_status {
            Some(status) => Ok(OpenedSession { status }),
            None => Err(CliError::from_request(err)),
        },
    }
}

/// Query current status after creation and configuration; the create response may be stale.
pub async fn created_session_status(client: &AcpClient, session_id: &str, cwd: PathBuf) -> Value {
    match open_session(client, session_id.to_owned(), cwd).await {
        Ok(opened) => opened.status,
        Err(error) => {
            json!({"sessionId": session_id, "observationState": "unavailable", "error": error.to_string()})
        }
    }
}

pub struct FollowResult {
    pub runtime: Value,
    pub text: String,
    pub truncated: bool,
}

/// Capture live updates from the beginning of attachment, then wait for delivered idle.
pub async fn follow(
    client: &AcpClient,
    session_id: &str,
    cwd: PathBuf,
    timeout: Duration,
    interval: Duration,
) -> Result<FollowResult, CliError> {
    let timeout = timeout.max(Duration::from_secs(1));
    let deadline = tokio::time::Instant::now() + timeout;
    let result =
        async {
            tokio::time::timeout_at(deadline, async {
            client.initialize().await.map_err(CliError::from_request)?;
            client.capture_assistant_text(Some(session_id)).await;
            open_session(client, session_id.to_owned(), cwd).await
        })
        .await
        .unwrap_or_else(|_| Err(CliError::Timeout(format!(
            "timed out after {}s opening session {session_id}; remote work was not cancelled",
            timeout.as_secs(),
        ))))
        .map_err(|error| error.with_recovery(json!({
            "sessionId": session_id, "stage": "open", "observationState": "unavailable",
        })))?;
            wait_until_idle_by(client, session_id, deadline, timeout, interval).await
        }
        .await;
    let (text, truncated) = client.finish_assistant_text().await;
    Ok(FollowResult {
        runtime: result?["runtime"].clone(),
        text,
        truncated,
    })
}

/// A cancelled prompt is already terminal. Another run may start immediately;
/// observing that run cannot change the outcome of this prompt.
pub async fn observe_prompt_completion(
    client: &AcpClient,
    session_id: &str,
    until_idle: bool,
    stop_reason: &str,
    timeout: Duration,
) -> Result<Option<Value>, CliError> {
    if until_idle && stop_reason != "cancelled" {
        return wait_until_idle(client, session_id, timeout).await.map(Some);
    }
    Ok(None)
}

pub fn extension_payload(response: &Value) -> Value {
    response
        .get("data")
        .cloned()
        .unwrap_or_else(|| response.clone())
}

pub fn stop_reason_name(reason: &acp::StopReason) -> String {
    match reason {
        acp::StopReason::EndTurn => "endTurn".to_string(),
        acp::StopReason::MaxTokens => "maxTokens".to_string(),
        acp::StopReason::MaxTurnRequests => "maxTurnRequests".to_string(),
        acp::StopReason::Refusal => "refusal".to_string(),
        acp::StopReason::Cancelled => "cancelled".to_string(),
        other => format!("{other:?}"),
    }
}

/// The turn usage a `session/prompt` response reported, for the prompt's
/// result. A response without `usage` (or a delivery that has no prompt
/// response) is `{"available": false}`: absent, never zero counts.
pub fn usage_summary(usage: Option<&acp::Usage>) -> Value {
    let Some(usage) = usage else {
        return json!({ "available": false });
    };
    let mut summary = json!({
        "available": true,
        "totalTokens": usage.total_tokens,
        "inputTokens": usage.input_tokens,
        "outputTokens": usage.output_tokens,
    });
    for (key, value) in [
        ("thoughtTokens", usage.thought_tokens),
        ("cachedReadTokens", usage.cached_read_tokens),
        ("cachedWriteTokens", usage.cached_write_tokens),
    ] {
        if let Some(value) = value {
            summary[key] = json!(value);
        }
    }
    summary
}

pub async fn discard_queued(
    client: &AcpClient,
    session_id: String,
    input_id: String,
) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let response = client
        .extension(
            "querymt/session/discardQueuedInput",
            json!({
                "session_id": session_id,
                "input_id": input_id,
            }),
        )
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({
        "sessionId": session_id,
        "inputId": input_id,
        "result": response,
    }))
}

pub async fn cancel(client: &AcpClient, session_id: String) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    client
        .cancel(session_id.clone())
        .await
        .map_err(CliError::from_notification)?;
    Ok(json!({
        "sessionId": session_id,
        "cancellationState": "notification_sent",
        "cancellationRequested": true,
    }))
}

pub async fn close(client: &AcpClient, session_id: String) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    client
        .close_session(session_id.clone())
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({
        "sessionId": session_id,
        "closed": true,
    }))
}

pub fn request_failure(error: anyhow::Error, mut recovery: Value) -> CliError {
    let state = if error
        .downcast_ref::<crate::client::RpcResponseError>()
        .is_some_and(crate::client::RpcResponseError::is_rejection)
    {
        "rejected"
    } else {
        "unknown"
    };
    let field = if recovery["stage"] == "create" {
        "creationState"
    } else {
        "deliveryState"
    };
    recovery[field] = json!(state);
    CliError::from_request(error).with_recovery(recovery)
}

/// An acknowledgement is not a turn outcome. Report the validated receipt status,
/// which can differ from the requested delivery (for example, queue -> started).
pub fn submission_stop_reason(receipt: &Value) -> &str {
    receipt["result"]["status"]
        .as_str()
        .expect("validated submission receipt")
}

pub async fn submit_text(
    client: &AcpClient,
    method: &str,
    session_id: String,
    run_id: Option<String>,
    text: String,
    on_submit: impl FnOnce(&str),
) -> Result<Value, CliError> {
    let not_submitted = json!({
        "sessionId": session_id, "stage": "submit", "deliveryState": "not_submitted",
    });
    client
        .initialize()
        .await
        .map_err(|error| CliError::from_request(error).with_recovery(not_submitted.clone()))?;
    let run_id = if method.ends_with("/steer") {
        let run_id = match run_id {
            Some(id) => Some(id),
            None => runtime(client, session_id.clone())
                .await
                .map_err(|error| error.with_recovery(not_submitted.clone()))?
                .get("runtime")
                .and_then(observed_run_id),
        };
        Some(run_id.filter(|id| !id.trim().is_empty()).ok_or_else(|| {
            CliError::rpc(
                "steer requires an active run or a nonempty run ID; input was not submitted",
            )
            .with_recovery(not_submitted.clone())
        })?)
    } else {
        run_id
    };
    let client_input_id = uuid::Uuid::new_v4().to_string();
    on_submit(&client_input_id);
    let mut recovery = json!({
        "sessionId": session_id, "clientInputId": client_input_id, "stage": "submit",
    });
    let mut params = json!({
        "session_id": session_id,
        "client_input_id": client_input_id,
        "prompt": [{ "type": "text", "text": text }],
    });
    if let Some(run_id) = run_id {
        params["expected_run_id"] = json!(run_id);
    }
    let response = client
        .extension(method, params)
        .await
        .map_err(|error| request_failure(error, recovery.clone()))?;
    let mut result = extension_payload(&response);
    let input_id = result.get("input_id").and_then(Value::as_str);
    let status_valid = matches!(
        response.get("status").and_then(Value::as_str),
        Some("queued" | "started" | "steered")
    );
    // A JSON-RPC result alone is not a correlated acceptance receipt.
    if input_id != Some(client_input_id.as_str())
        || !status_valid
        || response.get("ok") == Some(&Value::Bool(false))
        || result.get("ok") == Some(&Value::Bool(false))
    {
        recovery["deliveryState"] = json!("unknown");
        recovery["response"] = response;
        return Err(CliError::rpc(
            "invalid submission receipt; acceptance is unknown; inspect before retrying",
        )
        .with_recovery(recovery));
    }
    result["status"] = response["status"].clone();
    Ok(json!({
        "sessionId": session_id,
        "clientInputId": client_input_id,
        "deliveryState": "accepted",
        "result": result,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_server_paths_are_passed_through_on_any_client_os() {
        for raw in [
            "/srv/workspace",
            r"C:\repo\..\src",
            "C:/workspace",
            r"\\server\share\src",
        ] {
            let path = PathBuf::from(raw);
            assert_eq!(resolve_cwd(Some(path.clone())).unwrap().as_os_str(), raw);
            assert_eq!(existing_session_cwd(Some(path)).unwrap().as_os_str(), raw);
        }
        for raw in ["", ".", "relative", "C:relative", r"\relative", r"\\server"] {
            assert!(
                resolve_optional_cwd(Some(PathBuf::from(raw))).is_err(),
                "{raw:?}"
            );
        }
    }

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
        let compact = compact_inspect("s1", Some(Path::new("/repo")), None, &load, None, true);
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

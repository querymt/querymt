use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use qmacp::protocol::schema::v1 as acp;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use qmacp::{client::AcpClient, error::CliError, session};

use crate::output::{self, BatchState, OperationState};

const MIN_SECONDS: u64 = 1;
const MAX_SECONDS: u64 = 86_400;
const MIN_BATCH_CALLS: usize = 1;
const MAX_BATCH_CALLS: usize = 100;
// Match Rust's Unicode whitespace set, rather than a regex engine's differing \\s.
const NON_BLANK_TEXT_PATTERN: &str =
    r"[^\u0009-\u000d\u0020\u0085\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Auto,
    Prompt,
    Steer,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionFilter {
    Any,
    Open,
    Resolved,
}

impl ResolutionFilter {
    fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Open => "open",
            Self::Resolved => "resolved",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFilter {
    Any,
    Excluded,
    Only,
}

impl ArchiveFilter {
    fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Excluded => "excluded",
            Self::Only => "only",
        }
    }
}

#[derive(Clone, Copy)]
struct SessionFilterOptions {
    resolution: Option<ResolutionFilter>,
    archive: Option<ArchiveFilter>,
    pinned: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Protocol version, agent info, ACP session features, and QueryMT capabilities.
    Caps {},
    /// List QueryMT profiles advertised by the server.
    Profiles {},
    /// List models advertised by the server.
    Models {
        #[serde(default)]
        refresh: bool,
        query: Option<String>,
        provider: Option<String>,
    },
    /// Create a session without sending a prompt.
    New {
        cwd: Option<PathBuf>,
        profile: Option<String>,
        mode: Option<String>,
        model: Option<String>,
        effort: Option<String>,
    },
    /// List sessions with ACP pagination and optional lifecycle or pin filters.
    Sessions {
        cwd: Option<PathBuf>,
        cursor: Option<String>,
        /// Follow nextCursor until exhausted or the page cap is reached.
        #[serde(default)]
        all: bool,
        limit: Option<usize>,
        resolution: Option<ResolutionFilter>,
        archive: Option<ArchiveFilter>,
        pinned: Option<bool>,
    },
    /// Search sessions by title/id/cwd and optionally filter by phase, lifecycle, or pin.
    /// Scans at most 20 pages. Check truncated and scanLimitReached before treating results as exhaustive.
    Find {
        cwd: Option<PathBuf>,
        query: Option<String>,
        phase: Option<String>,
        #[serde(default = "default_limit")]
        limit: usize,
        resolution: Option<ResolutionFilter>,
        archive: Option<ArchiveFilter>,
        pinned: Option<bool>,
    },
    /// Read persisted lifecycle state for a session.
    GetLifecycle { session_id: String },
    /// Explicitly patch lifecycle state. At least one of resolved/archived is required.
    SetLifecycle {
        session_id: String,
        resolved: Option<bool>,
        archived: Option<bool>,
    },
    /// Pin a session.
    Pin { session_id: String },
    /// Unpin a session.
    Unpin { session_id: String },
    /// Read compact session history, or the full raw snapshot when full is true.
    Inspect {
        session_id: String,
        cwd: Option<PathBuf>,
        #[serde(default)]
        full: bool,
        messages: Option<usize>,
        #[serde(default = "default_true")]
        tools: bool,
    },
    /// Current mode/model/profile/effort for a session.
    Status {
        session_id: String,
        cwd: Option<PathBuf>,
    },
    /// Set session mode (build, plan, or review).
    SetMode { session_id: String, mode: String },
    /// Set session model id. Discover valid ids with models.
    SetModel { session_id: String, model: String },
    /// Set reasoning effort (auto, low, medium, high, or max).
    SetEffort { session_id: String, effort: String },
    /// Send text to a session, or create one with new=true. Auto delivery prompts when idle, steers when possible, otherwise queues.
    Prompt {
        #[serde(default)]
        new: bool,
        session_id: Option<String>,
        /// Literal prompt text. Stdin is reserved for the MCP protocol.
        text: String,
        cwd: Option<PathBuf>,
        profile: Option<String>,
        mode: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        /// Timeout in seconds for the entire operation, including waiting for idle.
        #[serde(default = "default_prompt_timeout")]
        timeout: u64,
        #[serde(default)]
        delivery: Delivery,
        /// Wait for idle after submission, not proof the submitted task succeeded.
        #[serde(default = "default_true")]
        until_idle: bool,
    },
    /// Current QueryMT runtime state for a session.
    Runtime { session_id: String },
    /// Collect compact session events until idle. Supply cwd for the session workspace.
    Follow {
        session_id: String,
        cwd: Option<PathBuf>,
        #[serde(default = "default_interval")]
        interval: u64,
        #[serde(default = "default_timeout")]
        timeout: u64,
    },
    /// Poll runtime state until idle or timeout. Set idle=false for one observation.
    Watch {
        session_id: String,
        #[serde(default = "default_interval")]
        interval: u64,
        #[serde(default = "default_timeout")]
        timeout: u64,
        #[serde(default = "default_true")]
        idle: bool,
    },
    /// Steer an in-flight turn. Omit run_id to discover it; a nonempty active run is required.
    Steer {
        session_id: String,
        run_id: Option<String>,
        text: String,
    },
    /// Queue text for after the current turn.
    Queue { session_id: String, text: String },
    /// Remove a queued input using the exact inputId/clientInputId from the server.
    DiscardQueued {
        session_id: String,
        input_id: String,
    },
    /// Run tool calls sequentially on one ACP connection. Stops at the first failure; prior calls are not rolled back. Nested exec is not allowed.
    Exec { calls: Vec<BatchCall> },
    /// Request cancellation of the current turn. Sends an ACP notification, not a completion acknowledgement.
    Cancel { session_id: String },
    /// Close a materialized session. Only use when explicitly requested.
    Close { session_id: String },
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Map<String, Value>,
}

fn default_true() -> bool {
    true
}
fn default_limit() -> usize {
    50
}
fn default_timeout() -> u64 {
    120
}
fn default_prompt_timeout() -> u64 {
    600
}
fn default_interval() -> u64 {
    1
}

pub fn decode(name: &str, mut arguments: Map<String, Value>) -> Result<Command> {
    if arguments.contains_key("command") {
        bail!("command is selected by the tool name, not arguments");
    }
    arguments.insert("command".into(), json!(name));
    let command: Command =
        serde_json::from_value(Value::Object(arguments)).context("invalid tool arguments")?;
    match &command {
        Command::Prompt {
            new,
            session_id,
            text,
            timeout,
            ..
        } => {
            if *new == session_id.is_some() {
                bail!("provide either new=true or session_id, not both");
            }
            if text.trim().is_empty() {
                bail!("text must not be empty");
            }
            validate_timeout(*timeout)?;
        }
        Command::Queue { text, .. } | Command::Steer { text, .. } if text.trim().is_empty() => {
            bail!("text must not be empty")
        }
        Command::Follow {
            interval, timeout, ..
        }
        | Command::Watch {
            interval, timeout, ..
        } => {
            validate_timeout(*timeout)?;
            if !(MIN_SECONDS..=MAX_SECONDS).contains(interval) {
                bail!("interval must be between {MIN_SECONDS} and {MAX_SECONDS} seconds");
            }
        }
        Command::SetLifecycle {
            resolved: None,
            archived: None,
            ..
        } => bail!("at least one of resolved or archived is required"),
        Command::Exec { calls } => {
            if !(MIN_BATCH_CALLS..=MAX_BATCH_CALLS).contains(&calls.len()) {
                bail!("exec requires between {MIN_BATCH_CALLS} and {MAX_BATCH_CALLS} calls");
            }
            // Validate every call before executing any side effects.
            for call in calls {
                if call.name == "exec" {
                    bail!("nested exec is not allowed");
                }
                decode(&call.name, call.arguments.clone())?;
            }
        }
        _ => {}
    }
    Ok(command)
}

fn validate_timeout(timeout: u64) -> Result<()> {
    if !(MIN_SECONDS..=MAX_SECONDS).contains(&timeout) {
        bail!("timeout must be between {MIN_SECONDS} and {MAX_SECONDS} seconds");
    }
    Ok(())
}

/// Build self-contained argument schemas from the deserialized types plus the
/// same bounds and semantic choices enforced by decode().
pub fn input_schemas() -> Result<Vec<(String, Map<String, Value>)>> {
    let mut settings = SchemaSettings::draft2020_12();
    settings.inline_subschemas = true;
    let schema = settings
        .into_generator()
        .into_root_schema_for::<Command>()
        .to_value();
    let variants = schema
        .get("oneOf")
        .and_then(Value::as_array)
        .context("command schema must contain oneOf")?;
    let mut inputs = Vec::with_capacity(variants.len());
    for variant in variants {
        let mut input = variant
            .as_object()
            .cloned()
            .context("command variant must be an object")?;
        let properties = input
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .context("command variant must have properties")?;
        let tag = properties
            .remove("command")
            .context("command variant must have a discriminator")?;
        let name = tag
            .get("const")
            .and_then(Value::as_str)
            .context("command discriminator must be constant")?
            .to_owned();
        if let Some(required) = input.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|field| field != "command");
        }
        constrain_input_schema(&name, &mut input);
        inputs.push((name, input));
    }
    // Each batch entry embeds the corresponding tool's full argument contract.
    // Exclude exec itself so its schema cannot advertise recursive batches.
    let entries: Vec<_> = inputs
        .iter()
        .filter(|(name, _)| name != "exec")
        .map(|(name, input)| {
            let mut entry = json!({
                "type": "object", "additionalProperties": false, "required": ["name"],
                "properties": {"name": {"const": name}, "arguments": input},
            });
            if decode(name, Map::new()).is_err() {
                entry["required"] = json!(["name", "arguments"]);
            }
            entry
        })
        .collect();
    let (_, exec) = inputs
        .iter_mut()
        .find(|(name, _)| name == "exec")
        .context("exec schema must exist")?;
    let calls = &mut exec
        .get_mut("properties")
        .context("exec properties must exist")?["calls"];
    calls["minItems"] = json!(MIN_BATCH_CALLS);
    calls["maxItems"] = json!(MAX_BATCH_CALLS);
    calls["items"] = json!({"oneOf": entries});
    Ok(inputs)
}

fn constrain_input_schema(name: &str, input: &mut Map<String, Value>) {
    let properties = input
        .get_mut("properties")
        .expect("derived properties exist");
    if matches!(name, "prompt" | "follow" | "watch") {
        properties["timeout"]["minimum"] = json!(MIN_SECONDS);
        properties["timeout"]["maximum"] = json!(MAX_SECONDS);
    }
    if matches!(name, "follow" | "watch") {
        properties["interval"]["minimum"] = json!(MIN_SECONDS);
        properties["interval"]["maximum"] = json!(MAX_SECONDS);
    }
    if matches!(name, "prompt" | "steer" | "queue") {
        properties["text"]["pattern"] = json!(NON_BLANK_TEXT_PATTERN);
    }
    if name == "prompt" {
        input.insert("oneOf".into(), json!([
            {"required": ["new"], "properties": {"new": {"const": true}, "session_id": {"type": "null"}}},
            {"required": ["session_id"], "properties": {"new": {"const": false}, "session_id": {"type": "string"}}},
        ]));
    } else if name == "set_lifecycle" {
        input.insert(
            "anyOf".into(),
            json!([
                {"required": ["resolved"], "properties": {"resolved": {"type": "boolean"}}},
                {"required": ["archived"], "properties": {"archived": {"type": "boolean"}}},
            ]),
        );
    }
}

impl Command {
    pub fn timeout(&self) -> Option<Duration> {
        match self {
            Self::Prompt { timeout, .. }
            | Self::Follow { timeout, .. }
            | Self::Watch { timeout, .. } => Some(Duration::from_secs(*timeout)),
            _ => None,
        }
    }
}

pub async fn run(
    client: &AcpClient,
    command: Command,
    state: &mut OperationState,
    deadline: Option<tokio::time::Instant>,
) -> Value {
    let deadline = deadline.or_else(|| {
        command
            .timeout()
            .map(|timeout| tokio::time::Instant::now() + timeout)
    });
    let is_batch = matches!(command, Command::Exec { .. });
    state.recovery.clear();
    let operation = async {
        client.clear_assistant_text().await;
        run_inner(client, command, state).await
    };
    let outcome = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, operation)
            .await
            .unwrap_or_else(|_| Err(operation_timeout())),
        None => operation.await,
    };
    let mut result = output::normalize(outcome.unwrap_or_else(output::failure));
    let truncated = client.text_truncated().await
        || (is_batch
            && state
                .batch
                .as_ref()
                .is_some_and(|batch| batch.text_truncated));
    result["textTruncated"] = json!(truncated);
    state.attach_recovery(result)
}

pub fn operation_timeout() -> CliError {
    CliError::Timeout("operation timed out; the remote turn may still be running; inspect runtime before cancelling or retrying".into())
}

async fn run_inner(
    client: &AcpClient,
    command: Command,
    state: &mut OperationState,
) -> Result<Value, CliError> {
    let result = match command {
        Command::Caps {} => caps(client).await?,
        Command::Profiles {} => profiles(client).await?,
        Command::Models {
            refresh,
            query,
            provider,
        } => models(client, refresh, query, provider).await?,
        Command::New {
            cwd,
            profile,
            mode,
            model,
            effort,
        } => new_session(client, state, cwd, profile, mode, model, effort).await?,
        Command::Sessions {
            cwd,
            cursor,
            all,
            limit,
            resolution,
            archive,
            pinned,
        } => {
            sessions(
                client,
                cwd,
                cursor,
                all,
                limit,
                SessionFilterOptions {
                    resolution,
                    archive,
                    pinned,
                },
            )
            .await?
        }
        Command::Find {
            cwd,
            query,
            phase,
            limit,
            resolution,
            archive,
            pinned,
        } => {
            find(
                client,
                cwd,
                query,
                phase,
                limit,
                SessionFilterOptions {
                    resolution,
                    archive,
                    pinned,
                },
            )
            .await?
        }
        Command::GetLifecycle { session_id } => get_lifecycle(client, session_id).await?,
        Command::SetLifecycle {
            session_id,
            resolved,
            archived,
        } => set_lifecycle(client, session_id, resolved, archived).await?,
        Command::Pin { session_id } => set_pinned(client, session_id, true).await?,
        Command::Unpin { session_id } => set_pinned(client, session_id, false).await?,
        Command::Inspect {
            session_id,
            cwd,
            full,
            messages,
            tools,
        } => inspect(client, session_id, cwd, full, messages, tools).await?,
        Command::Status { session_id, cwd } => status(client, session_id, cwd).await?,
        Command::SetMode { session_id, mode } => set_mode(client, session_id, mode).await?,
        Command::SetModel { session_id, model } => set_model(client, session_id, model).await?,
        Command::SetEffort { session_id, effort } => set_effort(client, session_id, effort).await?,
        Command::Prompt {
            new,
            session_id,
            text,
            cwd,
            profile,
            mode,
            model,
            effort,
            timeout: _,
            delivery,
            until_idle,
        } => {
            let args = PromptArgs {
                new,
                session_id,
                text,
                cwd,
                profile,
                mode,
                model,
                effort,
                delivery,
                until_idle,
            };
            prompt(client, state, args).await?
        }
        Command::Runtime { session_id } => runtime(client, session_id).await?,
        Command::Follow {
            session_id,
            cwd,
            interval,
            timeout,
        } => follow(client, session_id, cwd, interval, timeout).await?,
        Command::Watch {
            session_id,
            interval,
            timeout,
            idle,
        } => watch(client, session_id, interval, timeout, idle).await?,
        Command::Steer {
            session_id,
            run_id,
            text,
        } => {
            submit_input(
                client,
                state,
                "querymt/session/steer",
                session_id,
                run_id,
                text,
            )
            .await?
        }
        Command::Queue { session_id, text } => {
            submit_input(
                client,
                state,
                "querymt/session/queue",
                session_id,
                None,
                text,
            )
            .await?
        }
        Command::DiscardQueued {
            session_id,
            input_id,
        } => discard_queued(client, session_id, input_id).await?,
        Command::Exec { calls } => exec(client, calls, state).await?,
        Command::Cancel { session_id } => cancel(client, session_id).await?,
        Command::Close { session_id } => close(client, session_id).await?,
    };
    Ok(result)
}

async fn caps(client: &AcpClient) -> Result<Value, CliError> {
    let initialized = client.initialize().await.map_err(CliError::from_request)?;
    let querymt = match client.extension("querymt/capabilities", json!({})).await {
        Ok(value) => extension_payload(&value),
        Err(_) => Value::Null,
    };
    let agent = initialized.agent_info.as_ref();
    let session = &initialized.agent_capabilities.session_capabilities;
    Ok(json!({
        "protocolVersion": initialized.protocol_version,
        "agent": {
            "name": agent.map(|info| info.name.clone()),
            "title": agent.and_then(|info| info.title.clone()),
            "version": agent.map(|info| info.version.clone()),
        },
        "authMethods": initialized.auth_methods.iter().map(|method| method.id().to_string()).collect::<Vec<_>>(),
        "acp": {
            "loadSession": initialized.agent_capabilities.load_session,
            "list": session.list.is_some(),
            "resume": session.resume.is_some(),
            "close": session.close.is_some(),
            "delete": session.delete.is_some(),
        },
        "querymt": querymt,
    }))
}

async fn profiles(client: &AcpClient) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let response = client
        .extension("querymt/profiles", json!({}))
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({ "result": extension_payload(&response) }))
}

async fn models(
    client: &AcpClient,
    refresh: bool,
    query: Option<String>,
    provider: Option<String>,
) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let (method, params) = if refresh {
        (
            "querymt/refreshModels",
            json!({ "wait_for_completion": true }),
        )
    } else {
        ("querymt/models", json!({}))
    };
    let response = client
        .extension(method, params)
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({ "result": session::filter_models(
        extension_payload(&response),
        query.as_deref(),
        provider.as_deref(),
    ) }))
}

async fn new_session(
    client: &AcpClient,
    state: &mut OperationState,
    cwd: Option<PathBuf>,
    profile: Option<String>,
    mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let cwd = session::resolve_cwd(cwd).map_err(CliError::rpc)?;
    let (created, created_raw) = client
        .new_session(cwd.clone(), profile.as_deref())
        .await
        .map_err(CliError::from_request)?;
    let session_id = created.session_id.to_string();
    state.session(&session_id, &cwd, true);
    apply_config(client, &session_id, mode, model, effort).await?;
    let status = match open_session(client, session_id.clone(), cwd.clone()).await {
        Ok(opened) => opened.status,
        Err(_) => session::config_status(&session_id, created.modes.as_ref(), &created_raw),
    };
    Ok(json!({
        "sessionId": session_id,
        "cwd": cwd,
        "status": status,
    }))
}

async fn sessions(
    client: &AcpClient,
    cwd: Option<PathBuf>,
    cursor: Option<String>,
    all: bool,
    limit: Option<usize>,
    filter_options: SessionFilterOptions,
) -> Result<Value, CliError> {
    let SessionFilterOptions {
        resolution,
        archive,
        pinned,
    } = filter_options;
    client.initialize().await.map_err(CliError::from_request)?;
    let cwd = session::resolve_optional_cwd(cwd).map_err(CliError::rpc)?;
    let has_lifecycle_filters = resolution.is_some() || archive.is_some();
    let filters = session_filter_meta(resolution, archive, pinned);
    if has_lifecycle_filters {
        require_lifecycle_support(client, None).await?;
    }
    if pinned.is_some() {
        require_pin_support(client, None).await?;
    }
    let limit = limit.unwrap_or(if all { usize::MAX } else { 20 }).max(1);
    let mut sessions = Vec::new();
    let mut next = cursor;
    let mut pages = 0usize;
    loop {
        pages += 1;
        if pages > session::find_page_cap() {
            return Ok(json!({
                "cwd": cwd,
                "sessions": sessions,
                "nextCursor": next,
                "truncated": true,
            }));
        }
        let page = client
            .list_sessions(cwd.clone(), next.clone(), filters.clone())
            .await
            .map_err(CliError::from_request)?;
        for item in page.sessions {
            sessions.push(session::session_summary(&item));
        }
        if sessions.len() >= limit {
            return Ok(json!({
                "cwd": cwd,
                "sessions": sessions,
                "nextCursor": page.next_cursor,
                "truncated": page.next_cursor.is_some(),
            }));
        }
        match page.next_cursor {
            Some(cursor) if all => next = Some(cursor),
            next_cursor => {
                return Ok(json!({
                    "cwd": cwd,
                    "sessions": sessions,
                    "nextCursor": next_cursor,
                    "truncated": next_cursor.is_some(),
                }));
            }
        }
    }
}

async fn find(
    client: &AcpClient,
    cwd: Option<PathBuf>,
    query: Option<String>,
    phase: Option<String>,
    limit: usize,
    filter_options: SessionFilterOptions,
) -> Result<Value, CliError> {
    let SessionFilterOptions {
        resolution,
        archive,
        pinned,
    } = filter_options;
    client.initialize().await.map_err(CliError::from_request)?;
    let cwd = session::resolve_optional_cwd(cwd).map_err(CliError::rpc)?;
    let query = query.unwrap_or_default();
    let has_lifecycle_filters = resolution.is_some() || archive.is_some();
    let filters = session_filter_meta(resolution, archive, pinned);
    if has_lifecycle_filters {
        require_lifecycle_support(client, None).await?;
    }
    if pinned.is_some() {
        require_pin_support(client, None).await?;
    }
    let limit = limit.max(1);
    let mut matches = Vec::new();
    let mut next = None;
    let mut pages = 0usize;
    while pages < session::find_page_cap() {
        let page = client
            .list_sessions(cwd.clone(), next.clone(), filters.clone())
            .await
            .map_err(CliError::from_request)?;
        pages += 1;
        let mut listed = page.sessions.into_iter();
        while let Some(item) = listed.next() {
            let summary = session::session_summary(&item);
            if session::matches_query_and_phase(&summary, &query, phase.as_deref()) {
                matches.push(summary);
                if matches.len() >= limit {
                    let page_consumed = listed.len() == 0;
                    // A page cursor cannot represent a position inside the page.
                    let truncated = !page_consumed || page.next_cursor.is_some();
                    return Ok(json!({
                        "query": query, "phase": phase, "cwd": cwd, "sessions": matches,
                        "scannedPages": pages, "scanLimitReached": false, "truncated": truncated,
                        "nextCursor": if page_consumed { page.next_cursor } else { None },
                    }));
                }
            }
        }
        next = page.next_cursor;
        if next.is_none() {
            break;
        }
    }
    let scan_limit_reached = next.is_some();
    Ok(json!({
        "query": query, "phase": phase, "cwd": cwd, "sessions": matches,
        "scannedPages": pages, "scanLimitReached": scan_limit_reached,
        "truncated": scan_limit_reached, "nextCursor": next,
    }))
}

fn session_filter_meta(
    resolution: Option<ResolutionFilter>,
    archive: Option<ArchiveFilter>,
    pinned: Option<bool>,
) -> Option<Map<String, Value>> {
    let mut filters = Map::new();
    if let Some(resolution) = resolution {
        filters.insert("resolution".to_string(), json!(resolution.as_str()));
    }
    if let Some(archive) = archive {
        filters.insert("archive".to_string(), json!(archive.as_str()));
    }
    if let Some(pinned) = pinned {
        filters.insert("pinned".to_string(), json!(pinned));
    }
    if filters.is_empty() {
        return None;
    }

    let mut querymt = Map::new();
    querymt.insert("sessionFilters".to_string(), Value::Object(filters));
    let mut meta = Map::new();
    meta.insert("querymt".to_string(), Value::Object(querymt));
    Some(meta)
}

async fn require_lifecycle_support(
    client: &AcpClient,
    method: Option<&str>,
) -> Result<(), CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let response = client
        .extension("querymt/capabilities", json!({}))
        .await
        .map_err(|err| {
            CliError::from_request(err.context("cannot verify session lifecycle support"))
        })?;
    let capabilities = extension_payload(&response);
    validate_lifecycle_support(&capabilities, method)
}

fn validate_lifecycle_support(capabilities: &Value, method: Option<&str>) -> Result<(), CliError> {
    if supports_lifecycle(capabilities, method) {
        return Ok(());
    }
    let operation = method.unwrap_or("server-side lifecycle filtering");
    Err(CliError::rpc(format!(
        "server does not advertise support for {operation}"
    )))
}

fn supports_lifecycle(capabilities: &Value, method: Option<&str>) -> bool {
    let feature_supported = capabilities
        .get("features")
        .and_then(|features| features.get("session_lifecycle"))
        .and_then(Value::as_bool)
        == Some(true);
    let method_supported = method.is_none_or(|expected| {
        capabilities
            .get("methods")
            .and_then(Value::as_array)
            .is_some_and(|methods| methods.iter().any(|value| value.as_str() == Some(expected)))
    });
    feature_supported && method_supported
}

async fn require_pin_support(client: &AcpClient, method: Option<&str>) -> Result<(), CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let response = client
        .extension("querymt/capabilities", json!({}))
        .await
        .map_err(|err| CliError::from_request(err.context("cannot verify session pin support")))?;
    let capabilities = extension_payload(&response);
    validate_pin_support(&capabilities, method)
}

fn validate_pin_support(capabilities: &Value, method: Option<&str>) -> Result<(), CliError> {
    if supports_pin(capabilities, method) {
        return Ok(());
    }
    let operation = method.unwrap_or("server-side pin filtering");
    Err(CliError::rpc(format!(
        "server does not advertise support for {operation}"
    )))
}

fn supports_pin(capabilities: &Value, method: Option<&str>) -> bool {
    let feature_supported = capabilities
        .get("features")
        .and_then(|features| features.get("session_pins"))
        .and_then(Value::as_bool)
        == Some(true);
    let method_supported = method.is_none_or(|expected| {
        capabilities
            .get("methods")
            .and_then(Value::as_array)
            .is_some_and(|methods| methods.iter().any(|value| value.as_str() == Some(expected)))
    });
    feature_supported && method_supported
}

async fn get_lifecycle(client: &AcpClient, session_id: String) -> Result<Value, CliError> {
    require_lifecycle_support(client, Some("querymt/session/getLifecycle")).await?;
    let response = client
        .extension(
            "querymt/session/getLifecycle",
            json!({ "session_id": session_id }),
        )
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({ "result": extension_payload(&response) }))
}

fn lifecycle_patch_params(
    session_id: &str,
    resolved: Option<bool>,
    archived: Option<bool>,
) -> Result<Value, CliError> {
    if resolved.is_none() && archived.is_none() {
        return Err(CliError::Usage(
            "set-lifecycle requires --resolved and/or --archived".to_string(),
        ));
    }
    let mut params = json!({ "session_id": session_id });
    if let Some(resolved) = resolved {
        params["resolved"] = json!(resolved);
    }
    if let Some(archived) = archived {
        params["archived"] = json!(archived);
    }
    Ok(params)
}

async fn set_lifecycle(
    client: &AcpClient,
    session_id: String,
    resolved: Option<bool>,
    archived: Option<bool>,
) -> Result<Value, CliError> {
    let params = lifecycle_patch_params(&session_id, resolved, archived)?;
    require_lifecycle_support(client, Some("querymt/session/setLifecycle")).await?;
    let response = client
        .extension("querymt/session/setLifecycle", params)
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({ "result": extension_payload(&response) }))
}

async fn set_pinned(
    client: &AcpClient,
    session_id: String,
    pinned: bool,
) -> Result<Value, CliError> {
    require_pin_support(client, Some("querymt/session/setPinned")).await?;
    let response = client
        .extension(
            "querymt/session/setPinned",
            json!({ "session_id": session_id, "pinned": pinned }),
        )
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({ "result": extension_payload(&response) }))
}

async fn inspect(
    client: &AcpClient,
    session_id: String,
    cwd: Option<PathBuf>,
    full: bool,
    messages: Option<usize>,
    tools: bool,
) -> Result<Value, CliError> {
    let initialized = client.initialize().await.map_err(CliError::from_request)?;
    if !initialized.agent_capabilities.load_session {
        return Err(CliError::rpc("agent does not support session/load"));
    }
    let cwd = session::resolve_cwd(cwd).map_err(CliError::rpc)?;
    let (loaded, load_value) = client
        .load_session(session_id.clone(), cwd.clone())
        .await
        .map_err(CliError::from_request)?;
    if full {
        return Ok(json!({
            "sessionId": session_id,
            "cwd": cwd,
            "status": session::config_status(
                &session_id,
                loaded.modes.as_ref(),
                &load_value,
            ),
            "raw": load_value,
        }));
    }
    Ok(session::compact_inspect(
        &session_id,
        &cwd,
        loaded.modes.as_ref(),
        &load_value,
        messages,
        tools,
    ))
}

async fn status(
    client: &AcpClient,
    session_id: String,
    cwd: Option<PathBuf>,
) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let cwd = session::resolve_cwd(cwd).map_err(CliError::rpc)?;
    let opened = open_session(client, session_id.clone(), cwd).await?;
    Ok(opened.status)
}

async fn set_mode(client: &AcpClient, session_id: String, mode: String) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    client
        .set_mode(session_id.clone(), mode.clone())
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({
        "sessionId": session_id,
        "mode": mode,
    }))
}

async fn set_model(
    client: &AcpClient,
    session_id: String,
    model: String,
) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let (_, raw) = client
        .set_config(session_id.clone(), "model", &model)
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({
        "sessionId": session_id,
        "model": session::config_values_from_raw(&raw)
            .get("model")
            .cloned()
            .unwrap_or(json!(model)),
        "config": session::config_values_from_raw(&raw),
    }))
}

async fn set_effort(
    client: &AcpClient,
    session_id: String,
    effort: String,
) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let (_, raw) = client
        .set_config(session_id.clone(), "reasoning_effort", &effort)
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({
        "sessionId": session_id,
        "reasoningEffort": session::config_values_from_raw(&raw)
            .get("reasoning_effort")
            .cloned()
            .unwrap_or(json!(effort)),
        "config": session::config_values_from_raw(&raw),
    }))
}

struct PromptArgs {
    new: bool,
    session_id: Option<String>,
    text: String,
    cwd: Option<PathBuf>,
    profile: Option<String>,
    mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    delivery: Delivery,
    until_idle: bool,
}

async fn prompt(
    client: &AcpClient,
    state: &mut OperationState,
    args: PromptArgs,
) -> Result<Value, CliError> {
    let PromptArgs {
        new,
        session_id,
        text,
        cwd,
        profile,
        mode,
        model,
        effort,
        delivery,
        until_idle,
    } = args;
    let initialized = client.initialize().await.map_err(CliError::from_request)?;
    let cwd = session::resolve_cwd(cwd).map_err(CliError::rpc)?;
    let session_id = if new {
        let (created, _) = client
            .new_session(cwd.clone(), profile.as_deref())
            .await
            .map_err(CliError::from_request)?;
        let session_id = created.session_id.to_string();
        state.session(&session_id, &cwd, true);
        session_id
    } else {
        let session_id = session_id
            .ok_or_else(|| CliError::Usage("prompt requires session_id or new=true".into()))?;
        state.session(&session_id, &cwd, false);
        session_id
    };
    // Keep the session id observable even if configuration or the prompt later fails.
    client
        .emit(json!({ "type": "session", "sessionId": session_id, "cwd": cwd }))
        .await;
    state.recovery.insert("stage".into(), json!("configure"));
    state
        .recovery
        .insert("deliveryState".into(), json!("not_submitted"));
    let chosen = if new {
        Delivery::Prompt
    } else {
        choose_delivery(client, &session_id, delivery).await?
    };
    if !new && chosen == Delivery::Prompt {
        open_session(client, session_id.clone(), cwd.clone()).await?;
    }
    apply_config(client, &session_id, mode, model, effort).await?;
    client.clear_assistant_text().await;
    let mut submitted = None;
    let stop_reason = match chosen {
        Delivery::Prompt => {
            state.recovery.insert("stage".into(), json!("submit"));
            state
                .recovery
                .insert("deliveryState".into(), json!("unknown"));
            let response = client
                .prompt(session_id.clone(), text, Some(Duration::from_secs(86_400)))
                .await
                .map_err(CliError::from_request)?;
            stop_reason_name(&response.stop_reason)
        }
        Delivery::Steer | Delivery::Queue => {
            let method = if chosen == Delivery::Steer {
                "querymt/session/steer"
            } else {
                "querymt/session/queue"
            };
            submitted =
                Some(submit_input(client, state, method, session_id.clone(), None, text).await?);
            if chosen == Delivery::Steer {
                "steered".to_string()
            } else {
                "queued".to_string()
            }
        }
        Delivery::Auto => unreachable!("auto is resolved before send"),
    };
    state
        .recovery
        .insert("deliveryState".into(), json!("accepted"));
    state.recovery.insert("stage".into(), json!("observe"));
    if until_idle {
        wait_until_idle(client, &session_id, Duration::from_secs(u32::MAX.into())).await?;
    }
    let text = client.take_assistant_text().await;
    Ok(json!({
        "type": "done",
        "sessionId": session_id,
        "delivery": delivery_name(chosen),
        "stopReason": stop_reason,
        "text": text,
        "submitted": submitted,
        "deliveryState": "accepted",
        "observationState": if until_idle { "idle" } else if chosen == Delivery::Prompt { "prompt_returned" } else { "not_waited" },
        "loadSession": initialized.agent_capabilities.load_session,
    }))
}

fn delivery_name(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::Auto => "auto",
        Delivery::Prompt => "prompt",
        Delivery::Steer => "steer",
        Delivery::Queue => "queue",
    }
}

async fn choose_delivery(
    client: &AcpClient,
    session_id: &str,
    requested: Delivery,
) -> Result<Delivery, CliError> {
    if requested != Delivery::Auto {
        return Ok(requested);
    }
    let state = runtime(client, session_id.to_string()).await?;
    let phase = state
        .pointer("/runtime/phase")
        .and_then(Value::as_str)
        .filter(|phase| !phase.is_empty())
        .ok_or_else(|| {
            CliError::rpc("runtime response has no valid phase; input was not submitted")
        })?;
    let steerable = state
        .pointer("/runtime/steerable")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            CliError::rpc("runtime response has no valid steerable flag; input was not submitted")
        })?;
    Ok(delivery_from_runtime(phase, steerable))
}

fn delivery_from_runtime(phase: &str, steerable: bool) -> Delivery {
    if phase == "idle" {
        Delivery::Prompt
    } else if steerable {
        Delivery::Steer
    } else {
        Delivery::Queue
    }
}

async fn wait_until_idle(
    client: &AcpClient,
    session_id: &str,
    timeout: Duration,
) -> Result<(), CliError> {
    wait_until_idle_every(client, session_id, timeout, Duration::from_millis(400)).await
}

async fn wait_until_idle_every(
    client: &AcpClient,
    session_id: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<(), CliError> {
    let deadline = tokio::time::Instant::now() + timeout.max(Duration::from_secs(1));
    loop {
        let value = runtime(client, session_id.to_string()).await?;
        let phase = value
            .pointer("/runtime/phase")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if phase == "idle" {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(CliError::Timeout(format!(
                "timed out after {}s while session {session_id} was {phase}",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(interval.max(Duration::from_millis(200))).await;
    }
}

async fn follow(
    client: &AcpClient,
    session_id: String,
    cwd: Option<PathBuf>,
    interval: u64,
    timeout: u64,
) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let cwd = session::resolve_cwd(cwd).map_err(CliError::rpc)?;
    open_session(client, session_id.clone(), cwd.clone()).await?;
    client.clear_assistant_text().await;
    wait_until_idle_every(
        client,
        &session_id,
        Duration::from_secs(timeout),
        Duration::from_secs(interval),
    )
    .await?;
    let mut text = client.take_assistant_text().await;
    if text.is_empty()
        && let Ok(snapshot) =
            inspect(client, session_id.clone(), Some(cwd), false, Some(1), false).await
    {
        text = session::last_assistant_text(&snapshot).unwrap_or_default();
    }
    Ok(json!({
        "type": "done",
        "sessionId": session_id,
        "stopReason": "idle",
        "text": text,
    }))
}

async fn runtime(client: &AcpClient, session_id: String) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let response = client
        .extension(
            "querymt/session/runtimeState",
            json!({ "session_id": session_id }),
        )
        .await
        .map_err(CliError::from_request)?;
    Ok(json!({
        "sessionId": session_id,
        "runtime": extension_payload(&response),
    }))
}

async fn watch(
    client: &AcpClient,
    session_id: String,
    interval: u64,
    timeout: u64,
    wait_idle: bool,
) -> Result<Value, CliError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    loop {
        let value = runtime(client, session_id.clone()).await?;
        let phase = value
            .pointer("/runtime/phase")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        client.emit(json!({ "type": "runtime", "sessionId": session_id, "phase": phase, "runtime": value.get("runtime") })).await;
        if !wait_idle || phase == "idle" {
            return Ok(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(CliError::Timeout(format!(
                "watch timed out after {timeout}s while session {session_id} was {phase}"
            )));
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

async fn submit_input(
    client: &AcpClient,
    state: &mut OperationState,
    method: &str,
    session_id: String,
    run_id: Option<String>,
    text: String,
) -> Result<Value, CliError> {
    state.recovery.insert("sessionId".into(), json!(session_id));
    state.recovery.insert("stage".into(), json!("submit"));
    state
        .recovery
        .insert("deliveryState".into(), json!("not_submitted"));
    client.initialize().await.map_err(CliError::from_request)?;
    let delivery = if method.ends_with("/steer") {
        "steer"
    } else {
        "queue"
    };
    let run_id = if delivery == "steer" {
        let run_id = match run_id {
            Some(id) => Some(id),
            None => runtime(client, session_id.clone())
                .await?
                .pointer("/runtime/active_run_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        Some(run_id.filter(|id| !id.trim().is_empty()).ok_or_else(|| {
            CliError::rpc(
                "steer requires an active run or a nonempty run_id; input was not submitted",
            )
        })?)
    } else {
        run_id
    };
    let client_input_id = uuid::Uuid::new_v4().to_string();
    state.submission_started(&session_id, &client_input_id, delivery);
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
        .map_err(CliError::from_request)?;
    let acknowledgement = extension_payload(&response);
    if acknowledgement.get("input_id").and_then(Value::as_str) != Some(client_input_id.as_str())
        || !matches!(
            response.get("status").and_then(Value::as_str),
            Some("queued" | "started" | "steered")
        )
        || response.get("ok") == Some(&Value::Bool(false))
        || acknowledgement.get("ok") == Some(&Value::Bool(false))
    {
        return Err(CliError::rpc(
            "invalid submission receipt; acceptance is unknown; inspect before retrying",
        ));
    }
    state.submission_acknowledged(&acknowledgement);
    client
        .emit(json!({
            "type": "submitted", "sessionId": session_id, "clientInputId": client_input_id,
            "delivery": delivery, "deliveryState": "accepted", "result": acknowledgement,
        }))
        .await;
    Ok(json!({
        "sessionId": session_id,
        "clientInputId": client_input_id,
        "deliveryState": "accepted",
        "result": acknowledgement,
    }))
}

async fn exec(
    client: &AcpClient,
    calls: Vec<BatchCall>,
    state: &mut OperationState,
) -> Result<Value, CliError> {
    state.batch = Some(BatchState::default());
    for call in calls {
        state.batch.as_mut().expect("batch initialized").active_call = Some(call.name.clone());
        let command = decode(&call.name, call.arguments).map_err(CliError::rpc)?;
        let result = Box::pin(run(client, command, state, None)).await;
        let batch = state.batch.as_mut().expect("batch initialized");
        batch.text_truncated |= result["textTruncated"] == true;
        if output::failed(&result) {
            if result.get("stopReason").and_then(Value::as_str) == Some("cancelled") {
                batch
                    .results
                    .push(json!({ "name": call.name, "result": result }));
                return Ok(
                    json!({ "results": batch.results, "failedCall": call.name, "stopReason": "cancelled" }),
                );
            }
            return Ok(json!({
                "results": batch.results, "failedCall": call.name,
                "error": result.get("error"), "code": result.get("code"),
                "failedResult": result,
            }));
        }
        batch
            .results
            .push(json!({ "name": call.name, "result": result }));
    }
    let batch = state.batch.as_mut().expect("batch initialized");
    batch.active_call = None;
    Ok(json!({ "results": batch.results }))
}

async fn discard_queued(
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
        "result": extension_payload(&response),
    }))
}

async fn cancel(client: &AcpClient, session_id: String) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    client
        .cancel(session_id.clone())
        .await
        .map_err(CliError::from_notification)?;
    Ok(json!({
        "sessionId": session_id,
        "cancellationRequested": true,
    }))
}

async fn close(client: &AcpClient, session_id: String) -> Result<Value, CliError> {
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

async fn apply_config(
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

struct OpenedSession {
    status: Value,
}

async fn open_session(
    client: &AcpClient,
    session_id: String,
    cwd: PathBuf,
) -> Result<OpenedSession, CliError> {
    let resumed_status = match client.resume_session(session_id.clone(), cwd.clone()).await {
        Ok((response, raw)) => {
            let status = session::config_status(&session_id, response.modes.as_ref(), &raw);
            if status.get("model").is_some_and(|value| !value.is_null()) {
                return Ok(OpenedSession { status });
            }
            Some(status)
        }
        Err(_) => None,
    };
    match client.load_session(session_id.clone(), cwd).await {
        Ok((loaded, raw)) => Ok(OpenedSession {
            status: session::config_status(&session_id, loaded.modes.as_ref(), &raw),
        }),
        Err(err) => match resumed_status {
            Some(status) => Ok(OpenedSession { status }),
            None => Err(CliError::from_request(err)),
        },
    }
}

fn extension_payload(response: &Value) -> Value {
    response
        .get("data")
        .cloned()
        .unwrap_or_else(|| response.clone())
}

fn stop_reason_name(reason: &acp::StopReason) -> String {
    match reason {
        acp::StopReason::EndTurn => "endTurn".to_string(),
        acp::StopReason::MaxTokens => "maxTokens".to_string(),
        acp::StopReason::MaxTurnRequests => "maxTurnRequests".to_string(),
        acp::StopReason::Refusal => "refusal".to_string(),
        acp::StopReason::Cancelled => "cancelled".to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn json_arguments_are_strict_and_defaults_match_the_api() {
        let command = decode("prompt", args(json!({"new": true, "text": "-"}))).unwrap();
        assert!(
            matches!(command, Command::Prompt { text, timeout: 600, until_idle: true, delivery: Delivery::Auto, .. } if text == "-")
        );
        assert!(decode("prompt", args(json!({"text": "x"}))).is_err());
        assert!(
            decode(
                "prompt",
                args(json!({"new": true, "session_id": "s", "text": "x"}))
            )
            .is_err()
        );
        assert!(
            decode(
                "prompt",
                args(json!({"new": true, "text": "x", "timeout": 0}))
            )
            .is_err()
        );
        assert!(decode("caps", args(json!({"unknown": true}))).is_err());
        assert!(decode("caps", args(json!({"command": "close"}))).is_err());
    }

    #[test]
    fn batches_validate_before_execution_and_forbid_recursion() {
        assert!(
            decode(
                "exec",
                args(json!({"calls": [{"name": "caps"}, {"name": "runtime"}]}))
            )
            .is_err()
        );
        assert!(
            decode(
                "exec",
                args(json!({"calls": [{"name": "exec", "arguments": {"calls": []}}]}))
            )
            .is_err()
        );
        assert!(decode("exec", args(json!({"calls": [{"name": "caps"}]}))).is_ok());
    }

    #[test]
    fn lifecycle_and_pin_capabilities_require_feature_and_method() {
        let caps = json!({"features": {"session_lifecycle": true, "session_pins": true}, "methods": ["querymt/session/getLifecycle", "querymt/session/setPinned"]});
        assert!(supports_lifecycle(
            &caps,
            Some("querymt/session/getLifecycle")
        ));
        assert!(!supports_lifecycle(
            &caps,
            Some("querymt/session/setLifecycle")
        ));
        assert!(supports_pin(&caps, Some("querymt/session/setPinned")));
        assert!(!supports_pin(
            &json!({"features": {"session_pins": false}}),
            None
        ));
        assert!(validate_lifecycle_support(&json!({}), None).is_err());
        assert!(validate_pin_support(&json!({}), None).is_err());
    }

    #[test]
    fn lifecycle_and_filters_preserve_explicit_false() {
        assert_eq!(
            lifecycle_patch_params("s", Some(false), None).unwrap(),
            json!({"session_id": "s", "resolved": false})
        );
        assert!(lifecycle_patch_params("s", None, None).is_err());
        assert!(session_filter_meta(None, None, None).is_none());
        let filters = session_filter_meta(Some(ResolutionFilter::Any), None, Some(false)).unwrap();
        assert_eq!(filters["querymt"]["sessionFilters"]["resolution"], "any");
        assert_eq!(filters["querymt"]["sessionFilters"]["pinned"], false);
    }

    #[test]
    fn auto_delivery_respects_runtime() {
        assert_eq!(delivery_from_runtime("idle", false), Delivery::Prompt);
        assert_eq!(delivery_from_runtime("tools", true), Delivery::Steer);
        assert_eq!(delivery_from_runtime("tools", false), Delivery::Queue);
    }
}

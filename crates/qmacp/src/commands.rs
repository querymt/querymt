use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

use qmacp::client::AcpClient;
use qmacp::error::{CliError, ExitCode};
use qmacp::output;
use qmacp::session;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Delivery {
    #[default]
    Auto,
    Prompt,
    Steer,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Protocol version, agent info, ACP session features, and QueryMT capabilities.
    Caps,
    /// List QueryMT profiles advertised by the server.
    Profiles,
    /// List models advertised by the server.
    Models {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Create a session.
    New {
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        mode: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        /// Emit an early creation receipt before configuration (NDJSON).
        #[arg(long)]
        stream: bool,
    },
    /// List sessions with ACP pagination and optional lifecycle or pin filters.
    Sessions {
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        cursor: Option<String>,
        /// Follow nextCursor until exhausted or the page cap is reached.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, value_enum)]
        resolution: Option<ResolutionFilter>,
        #[arg(long, value_enum)]
        archive: Option<ArchiveFilter>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true, action = clap::ArgAction::Set)]
        pinned: Option<bool>,
    },
    /// Search sessions by title/id and optionally filter by phase, lifecycle, or pin.
    Find {
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        phase: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, value_enum)]
        resolution: Option<ResolutionFilter>,
        #[arg(long, value_enum)]
        archive: Option<ArchiveFilter>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true, action = clap::ArgAction::Set)]
        pinned: Option<bool>,
    },
    /// Read persisted lifecycle state for a session.
    GetLifecycle { session_id: String },
    /// Explicitly patch persisted lifecycle state for a session.
    SetLifecycle {
        session_id: String,
        #[arg(long, action = clap::ArgAction::Set)]
        resolved: Option<bool>,
        #[arg(long, action = clap::ArgAction::Set)]
        archived: Option<bool>,
    },
    /// Pin a session.
    Pin { session_id: String },
    /// Unpin a session.
    Unpin { session_id: String },
    /// Compact session history from session/load.
    Inspect {
        session_id: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        full: bool,
        #[arg(long)]
        messages: Option<usize>,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        tools: bool,
    },
    /// Current mode/model/profile/effort for a session.
    Status {
        session_id: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Set session mode (build|plan|review).
    SetMode { session_id: String, mode: String },
    /// Set session model id.
    SetModel { session_id: String, model: String },
    /// Set reasoning effort (auto|low|medium|high|max).
    SetEffort { session_id: String, effort: String },
    /// Send a prompt. `prompt --new TEXT` or `prompt SESSION_ID TEXT`.
    Prompt {
        /// Create a session, then prompt. Remaining args are the prompt text.
        #[arg(long)]
        new: bool,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        mode: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        timeout: Option<u64>,
        /// How to send text to an existing session.
        #[arg(long, value_enum, default_value_t = Delivery::Auto)]
        delivery: Delivery,
        /// Wait for self-session idle, including queued turns, not child/process completion.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        until_idle: bool,
        /// `SESSION_ID TEXT` or, with --new, just `TEXT`. Use `-` to read stdin.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Current QueryMT runtime state for a session.
    Runtime { session_id: String },
    /// Stream compact session events until self-session idle, not task success.
    Follow {
        session_id: String,
        #[arg(long, default_value_t = 1)]
        interval: u64,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Poll runtime state until self-session idle or timeout, not child/process completion.
    Watch {
        session_id: String,
        #[arg(long, default_value_t = 1)]
        interval: u64,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        idle: bool,
    },
    /// Steer an in-flight turn.
    Steer {
        session_id: String,
        #[arg(long)]
        run_id: Option<String>,
        /// Text to submit; use `-` explicitly to read stdin.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
    /// Queue input for after the current turn.
    Queue {
        session_id: String,
        /// Text to submit; use `-` explicitly to read stdin.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
    /// Remove an input that has not started from the session queue.
    DiscardQueued {
        session_id: String,
        input_id: String,
    },
    /// Run multiple commands on one connection. Lines from stdin or remaining args.
    Exec {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        commands: Vec<String>,
    },
    /// Send a cancellation request for the current turn; does not confirm termination.
    Cancel { session_id: String },
    /// Close a materialized session.
    Close { session_id: String },
}

impl Command {
    fn reads_stdin(&self) -> bool {
        let text = match self {
            Self::Prompt { new, args, .. } => {
                let start = usize::from(!new);
                args.get(start..).unwrap_or_default()
            }
            Self::Queue { text, .. } | Self::Steer { text, .. } => text.as_slice(),
            Self::Exec { commands } => {
                return commands.is_empty()
                    || commands.iter().any(|line| {
                        parse_exec_line(line).is_ok_and(|command| command.reads_stdin())
                    });
            }
            _ => return false,
        };
        text.is_empty() || (text.len() == 1 && text[0] == "-")
    }
}

pub struct CommandOutcome {
    pub exit: ExitCode,
}

pub async fn run(
    client: &AcpClient,
    command: Command,
    pretty: bool,
) -> Result<CommandOutcome, CliError> {
    match command {
        Command::Caps => write_ok(pretty, caps(client).await?),
        Command::Profiles => write_ok(pretty, profiles(client).await?),
        Command::Models {
            refresh,
            query,
            provider,
        } => write_ok(pretty, models(client, refresh, query, provider).await?),
        Command::New {
            cwd,
            profile,
            mode,
            model,
            effort,
            stream,
        } => write_ok(
            pretty,
            new_session(client, cwd, profile, mode, model, effort, stream).await?,
        ),
        Command::Sessions {
            cwd,
            cursor,
            all,
            limit,
            resolution,
            archive,
            pinned,
        } => write_ok(
            pretty,
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
            .await?,
        ),
        Command::Find {
            cwd,
            query,
            phase,
            limit,
            resolution,
            archive,
            pinned,
        } => write_ok(
            pretty,
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
            .await?,
        ),
        Command::GetLifecycle { session_id } => {
            write_ok(pretty, get_lifecycle(client, session_id).await?)
        }
        Command::SetLifecycle {
            session_id,
            resolved,
            archived,
        } => write_ok(
            pretty,
            set_lifecycle(client, session_id, resolved, archived).await?,
        ),
        Command::Pin { session_id } => {
            write_ok(pretty, set_pinned(client, session_id, true).await?)
        }
        Command::Unpin { session_id } => {
            write_ok(pretty, set_pinned(client, session_id, false).await?)
        }
        Command::Inspect {
            session_id,
            cwd,
            full,
            messages,
            tools,
        } => write_ok(
            pretty,
            inspect(client, session_id, cwd, full, messages, tools).await?,
        ),
        Command::Status { session_id, cwd } => {
            write_ok(pretty, status(client, session_id, cwd).await?)
        }
        Command::SetMode { session_id, mode } => {
            write_ok(pretty, set_mode(client, session_id, mode).await?)
        }
        Command::SetModel { session_id, model } => {
            write_ok(pretty, set_model(client, session_id, model).await?)
        }
        Command::SetEffort { session_id, effort } => {
            write_ok(pretty, set_effort(client, session_id, effort).await?)
        }
        Command::Prompt {
            new,
            cwd,
            profile,
            mode,
            model,
            effort,
            timeout,
            delivery,
            until_idle,
            args,
        } => {
            prompt(
                client,
                PromptArgs {
                    new,
                    cwd,
                    profile,
                    mode,
                    model,
                    effort,
                    timeout,
                    delivery,
                    until_idle,
                    args,
                    pretty,
                },
            )
            .await
        }
        Command::Runtime { session_id } => write_ok(pretty, runtime(client, session_id).await?),
        Command::Follow {
            session_id,
            interval,
            timeout,
        } => follow(client, session_id, interval, timeout, pretty).await,
        Command::Watch {
            session_id,
            interval,
            timeout,
            idle,
        } => watch(client, session_id, interval, timeout, idle, pretty).await,
        Command::Steer {
            session_id,
            run_id,
            text,
        } => write_submission_ok(
            submit_input(client, "querymt/session/steer", session_id, run_id, text).await?,
            |value| output::write_json(value, pretty),
        ),
        Command::Queue { session_id, text } => write_submission_ok(
            submit_input(client, "querymt/session/queue", session_id, None, text).await?,
            |value| output::write_json(value, pretty),
        ),
        Command::DiscardQueued {
            session_id,
            input_id,
        } => write_ok(pretty, discard_queued(client, session_id, input_id).await?),
        Command::Exec { commands } => exec(client, commands, pretty).await,
        Command::Cancel { session_id } => write_ok(pretty, cancel(client, session_id).await?),
        Command::Close { session_id } => write_ok(pretty, close(client, session_id).await?),
    }
}

fn write_ok(pretty: bool, value: Value) -> Result<CommandOutcome, CliError> {
    output::write_json(&output::ok(value), pretty).map_err(CliError::rpc)?;
    Ok(CommandOutcome { exit: ExitCode::Ok })
}

fn write_submission_ok(
    value: Value,
    write: impl FnOnce(&Value) -> Result<()>,
) -> Result<CommandOutcome, CliError> {
    let recovery = json!({
        "sessionId": value["sessionId"],
        "clientInputId": value["clientInputId"],
        "deliveryState": "accepted",
        "stage": "submit",
        "receipt": value,
    });
    write(&output::ok(value)).map_err(|error| CliError::rpc(error).with_recovery(recovery))?;
    Ok(CommandOutcome { exit: ExitCode::Ok })
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
    Ok(extension_payload(&response))
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
    Ok(session::filter_models(
        extension_payload(&response),
        query.as_deref(),
        provider.as_deref(),
    ))
}

async fn new_session(
    client: &AcpClient,
    cwd: Option<PathBuf>,
    profile: Option<String>,
    mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    stream: bool,
) -> Result<Value, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let cwd = session::resolve_cwd(cwd).map_err(CliError::rpc)?;
    let (created, created_raw) = client
        .new_session(cwd.clone(), profile.as_deref())
        .await
        .map_err(|error| request_failure(error, json!({"stage": "create"})))?;
    let session_id = created.session_id.to_string();
    let recovery = json!({
        "sessionId": session_id,
        "creationState": "created",
        "deliveryState": "not_submitted",
        "stage": "configure",
    });
    if stream {
        output::write_event(&json!({
            "type": "session", "sessionId": session_id, "cwd": cwd,
            "creationState": "created",
        }))
        .map_err(|error| CliError::rpc(error).with_recovery(recovery.clone()))?;
    }
    apply_config(client, &session_id, mode, model, effort)
        .await
        .map_err(|error| error.with_recovery(recovery))?;
    let status = match open_session(client, session_id.clone(), cwd.clone()).await {
        Ok(opened) => opened.status,
        Err(_) => session::config_status(&session_id, created.modes.as_ref(), &created_raw),
    };
    let mut result = json!({
        "sessionId": session_id,
        "cwd": cwd,
        "creationState": "created",
        "status": status,
    });
    if stream {
        result["type"] = json!("done");
    }
    Ok(result)
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
                    "truncated": false,
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
    let filters = session_filter_meta(resolution, archive, pinned);
    if resolution.is_some() || archive.is_some() {
        require_lifecycle_support(client, None).await?;
    }
    if pinned.is_some() {
        require_pin_support(client, None).await?;
    }
    let limit = limit.max(1);
    let mut matches = Vec::new();
    let mut next = None;
    let mut pages = 0usize;
    let (truncated, stop_reason) = 'pages: loop {
        let page = client
            .list_sessions(cwd.clone(), next.clone(), filters.clone())
            .await
            .map_err(CliError::from_request)?;
        pages += 1;
        let page_len = page.sessions.len();
        for (index, listed) in page.sessions.into_iter().enumerate() {
            let summary = session::session_summary(&listed);
            if session::matches_query_and_phase(&summary, &query, phase.as_deref()) {
                matches.push(summary);
                if matches.len() >= limit {
                    let more = index + 1 < page_len || page.next_cursor.is_some();
                    break 'pages (more, if more { "limit" } else { "exhausted" });
                }
            }
        }
        match page.next_cursor {
            Some(_) if pages >= session::find_page_cap() => break (true, "page_cap"),
            Some(cursor) => next = Some(cursor),
            None => break (false, "exhausted"),
        }
    };
    Ok(json!({
        "query": query,
        "phase": phase,
        "cwd": cwd,
        "sessions": matches,
        "pagesScanned": pages,
        "truncated": truncated,
        "pageCapReached": stop_reason == "page_cap",
        "stopReason": stop_reason,
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
    Ok(extension_payload(&response))
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
    Ok(extension_payload(&response))
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
    Ok(extension_payload(&response))
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
    cwd: Option<PathBuf>,
    profile: Option<String>,
    mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    timeout: Option<u64>,
    delivery: Delivery,
    until_idle: bool,
    args: Vec<String>,
    pretty: bool,
}

async fn prompt(client: &AcpClient, args: PromptArgs) -> Result<CommandOutcome, CliError> {
    let PromptArgs {
        new,
        cwd,
        profile,
        mode,
        model,
        effort,
        timeout,
        delivery,
        until_idle,
        args,
        pretty,
    } = args;
    let (session_id, text) = parse_prompt_args(new, args)?;
    let text = read_prompt_text(text)?;
    let cwd = session::resolve_cwd(cwd).map_err(CliError::rpc)?;
    let initialized = client.initialize().await.map_err(CliError::from_request)?;
    let session_id = if new {
        let (created, _) = client
            .new_session(cwd.clone(), profile.as_deref())
            .await
            .map_err(|error| request_failure(error, json!({"stage": "create"})))?;
        created.session_id.to_string()
    } else {
        session_id.ok_or_else(|| CliError::Usage("prompt requires SESSION_ID or --new".into()))?
    };
    let mut recovery = json!({
        "sessionId": session_id,
        "stage": "configure",
        "deliveryState": "not_submitted",
    });
    if new {
        recovery["creationState"] = json!("created");
        output::write_event(&json!({
            "type": "session", "sessionId": session_id, "cwd": cwd,
            "creationState": "created",
        }))
        .map_err(|error| CliError::rpc(error).with_recovery(recovery.clone()))?;
    }
    let chosen = if new {
        Delivery::Prompt
    } else {
        choose_delivery(client, &session_id, delivery)
            .await
            .map_err(|error| error.with_recovery(recovery.clone()))?
    };
    // Queue/steer do not need replay or runtime restoration, even when waiting later.
    if !new && chosen == Delivery::Prompt {
        open_session(client, session_id.clone(), cwd.clone())
            .await
            .map_err(|error| error.with_recovery(recovery.clone()))?;
    }
    apply_config(client, &session_id, mode, model, effort)
        .await
        .map_err(|error| error.with_recovery(recovery.clone()))?;
    recovery["delivery"] = json!(delivery_name(chosen));
    output::write_event(&json!({
        "type": "session", "sessionId": session_id, "cwd": cwd,
        "delivery": delivery_name(chosen),
    }))
    .map_err(|error| CliError::rpc(error).with_recovery(recovery.clone()))?;
    let _ = client.take_assistant_text().await;
    let timeout = timeout.map(Duration::from_secs);
    recovery["stage"] = json!("submit");
    let mut receipt = None;
    let stop_reason = match chosen {
        Delivery::Prompt => {
            let response = client
                .prompt(session_id.clone(), text, timeout)
                .await
                .map_err(|error| request_failure(error, recovery.clone()))?;
            stop_reason_name(&response.stop_reason)
        }
        Delivery::Steer | Delivery::Queue => {
            let method = if chosen == Delivery::Steer {
                "querymt/session/steer"
            } else {
                "querymt/session/queue"
            };
            let submitted = submit_text(client, method, session_id.clone(), None, text)
                .await
                .map_err(|error| error.with_recovery(recovery.clone()))?;
            recovery["clientInputId"] = submitted["clientInputId"].clone();
            recovery["receipt"] = submitted.clone();
            recovery["deliveryState"] = json!("accepted");
            output::write_event(&json!({
                "type": "submitted", "ok": true, "sessionId": session_id,
                "delivery": delivery_name(chosen), "deliveryState": "accepted",
                "clientInputId": submitted["clientInputId"], "receipt": submitted,
            }))
            .map_err(|error| CliError::rpc(error).with_recovery(recovery.clone()))?;
            receipt = Some(submitted);
            if chosen == Delivery::Steer {
                "steered"
            } else {
                "queued"
            }
            .to_string()
        }
        Delivery::Auto => unreachable!("auto is resolved before send"),
    };
    recovery["deliveryState"] = json!("accepted");
    recovery["stage"] = json!("observe");
    if chosen == Delivery::Prompt {
        recovery["stopReason"] = json!(stop_reason);
    }
    let mut text = client.take_assistant_text().await;
    let observed = if until_idle {
        let observed = wait_until_idle(
            client,
            &session_id,
            timeout.unwrap_or(Duration::from_secs(120)),
        )
        .await
        .map_err(|error| error.with_recovery(recovery.clone()))?;
        let more = client.snapshot_assistant_text().await;
        if !more.is_empty() {
            text = more;
        }
        Some(observed)
    } else {
        None
    };
    let mut done = json!({
        "type": "done", "ok": true, "sessionId": session_id,
        "delivery": delivery_name(chosen), "deliveryState": "accepted",
        "observationState": if until_idle { "idle" } else if chosen == Delivery::Prompt { "prompt_returned" } else { "not_waited" },
        "stopReason": stop_reason, "text": text,
        "loadSession": initialized.agent_capabilities.load_session,
    });
    if let Some(observed) = observed {
        done["runtime"] = observed["runtime"].clone();
    }
    if let Some(receipt) = receipt {
        done["clientInputId"] = receipt["clientInputId"].clone();
        done["receipt"] = receipt;
    }
    output::write_json(&done, pretty)
        .map_err(|error| CliError::rpc(error).with_recovery(recovery))?;
    Ok(CommandOutcome {
        exit: output::exit_for_stop_reason(&stop_reason),
    })
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
    let phase = require_observed_phase(&state)?;
    let steerable = state
        .pointer("/runtime/steerable")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            CliError::rpc("runtime response has no valid steerable flag; input was not submitted")
        })?;
    Ok(delivery_from_runtime(phase, steerable))
}

fn require_observed_phase(state: &Value) -> Result<&str, CliError> {
    session::observed_phase(&state["runtime"]).ok_or_else(|| {
        CliError::rpc(
            "session runtime observation is unavailable or invalid; execution state is unknown",
        )
        .with_recovery(json!({
            "sessionId": state["sessionId"], "stage": "observe",
            "observationState": "unavailable", "runtime": state["runtime"],
        }))
    })
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
) -> Result<Value, CliError> {
    wait_until_idle_every(client, session_id, timeout, Duration::from_millis(400)).await
}

async fn wait_until_idle_every(
    client: &AcpClient,
    session_id: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<Value, CliError> {
    let mut last_runtime = Value::Null;
    tokio::time::timeout(timeout.max(Duration::from_secs(1)), async {
        loop {
            let value = runtime(client, session_id.to_string()).await
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

async fn follow(
    client: &AcpClient,
    session_id: String,
    interval: u64,
    timeout: u64,
    pretty: bool,
) -> Result<CommandOutcome, CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let cwd = session::resolve_cwd(None).map_err(CliError::rpc)?;
    open_session(client, session_id.clone(), cwd)
        .await
        .map_err(|error| {
            error.with_recovery(json!({
                "sessionId": session_id, "stage": "open", "observationState": "unavailable",
            }))
        })?;
    let _ = client.take_assistant_text().await;
    let observed = wait_until_idle_every(
        client,
        &session_id,
        Duration::from_secs(timeout.max(1)),
        Duration::from_secs(interval.max(1)),
    )
    .await
    .map_err(|error| {
        error.with_recovery(json!({
            "sessionId": session_id, "stage": "observe", "observationState": "unavailable",
        }))
    })?;
    let text = client.take_assistant_text().await;
    let done = json!({
        "type": "done",
        "ok": true,
        "sessionId": session_id,
        "stopReason": "idle",
        "observationState": "idle",
        "runtime": observed["runtime"],
        "text": text,
        "interval": interval,
    });
    if pretty {
        output::write_json(&done, true).map_err(CliError::rpc)?;
    } else {
        output::write_event(&done).map_err(CliError::rpc)?;
    }
    Ok(CommandOutcome { exit: ExitCode::Ok })
}

async fn runtime(client: &AcpClient, session_id: String) -> Result<Value, CliError> {
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
            json!({ "session_id": session_id }),
        )
        .await
        .map_err(|error| CliError::from_request(error).with_recovery(recovery))?;
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
    pretty: bool,
) -> Result<CommandOutcome, CliError> {
    let interval = Duration::from_secs(interval.max(1));
    let mut last_runtime = Value::Null;
    tokio::time::timeout(Duration::from_secs(timeout.max(1)), async {
        loop {
            let value = runtime(client, session_id.clone()).await
                .map_err(|error| error.with_recovery(json!({"runtime": last_runtime})))?;
            last_runtime = value["runtime"].clone();
            output::write_event(&json!({
                "type": "runtime",
                "sessionId": session_id,
                "phase": session::observed_phase(&last_runtime).unwrap_or("unknown"),
                "runtime": last_runtime,
            }))
            .map_err(CliError::rpc)?;
            let phase = require_observed_phase(&value)?;
            if !wait_idle || phase == "idle" {
                return write_ok(pretty, value);
            }
            tokio::time::sleep(interval).await;
        }
    }).await.map_err(|_| CliError::Timeout(format!(
        "watch timed out after {timeout}s waiting for session {session_id}; remote work was not cancelled"
    )).with_recovery(json!({
        "sessionId": session_id, "stage": "observe", "observationState": "unavailable",
        "runtime": last_runtime,
    })))?
}

async fn submit_input(
    client: &AcpClient,
    method: &str,
    session_id: String,
    run_id: Option<String>,
    text: Vec<String>,
) -> Result<Value, CliError> {
    let text = join_prompt_args(text)
        .ok_or_else(|| CliError::Usage("text is required (pass TEXT or - for stdin)".into()))?;
    let text = read_prompt_text(Some(text))?;
    submit_text(client, method, session_id, run_id, text).await
}

async fn submit_text(
    client: &AcpClient,
    method: &str,
    session_id: String,
    run_id: Option<String>,
    text: String,
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
                .pointer("/runtime/active_run_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        Some(run_id.filter(|id| !id.trim().is_empty()).ok_or_else(|| {
            CliError::rpc(
                "steer requires an active run or a nonempty --run-id; input was not submitted",
            )
            .with_recovery(not_submitted.clone())
        })?)
    } else {
        run_id
    };
    let client_input_id = uuid::Uuid::new_v4().to_string();
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
    let result = extension_payload(&response);
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
    Ok(json!({
        "sessionId": session_id,
        "clientInputId": client_input_id,
        "deliveryState": "accepted",
        "result": result,
    }))
}

fn request_failure(error: anyhow::Error, mut recovery: Value) -> CliError {
    let state = if error
        .downcast_ref::<qmacp::client::RpcResponseError>()
        .is_some_and(qmacp::client::RpcResponseError::is_rejection)
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

async fn exec(
    client: &AcpClient,
    commands: Vec<String>,
    pretty: bool,
) -> Result<CommandOutcome, CliError> {
    let from_stdin = commands.is_empty();
    let lines = if from_stdin {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(CliError::rpc)?;
        buf.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        commands
    };
    if lines.is_empty() {
        return Err(CliError::Usage(
            "exec requires commands on stdin or as arguments".into(),
        ));
    }
    let commands = lines
        .iter()
        .map(|line| parse_exec_line(line))
        .collect::<Result<Vec<_>, _>>()?;
    if from_stdin && commands.iter().any(Command::reads_stdin) {
        return Err(CliError::Usage(
            "stdin payloads cannot be read inside a stdin exec batch".into(),
        ));
    }
    let mut last = CommandOutcome { exit: ExitCode::Ok };
    for command in commands {
        last = Box::pin(run(client, command, pretty)).await?;
        if last.exit != ExitCode::Ok {
            return Ok(last);
        }
    }
    Ok(last)
}

#[derive(Parser)]
#[command(name = "qmacp")]
struct ExecLine {
    #[command(subcommand)]
    command: Command,
}

fn parse_exec_line(line: &str) -> Result<Command, CliError> {
    let args = split_exec_args(line);
    let mut argv = vec!["qmacp".to_string()];
    argv.extend(args);
    ExecLine::try_parse_from(&argv)
        .map(|cli| cli.command)
        .map_err(|err| CliError::Usage(err.to_string()))
}

fn split_exec_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None::<char>;
    for ch in line.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
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
        "cancellationState": "notification_sent",
    }))
}

fn parse_prompt_args(
    new: bool,
    args: Vec<String>,
) -> Result<(Option<String>, Option<String>), CliError> {
    if new {
        return Ok((None, join_prompt_args(args)));
    }
    let mut args = args.into_iter();
    let Some(session_id) = args.next() else {
        return Err(CliError::Usage(
            "prompt requires SESSION_ID TEXT, or --new TEXT".into(),
        ));
    };
    Ok((Some(session_id), join_prompt_args(args.collect())))
}

fn join_prompt_args(args: Vec<String>) -> Option<String> {
    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
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

fn read_prompt_text(text: Option<String>) -> Result<String, CliError> {
    let explicit_stdin = text.as_deref() == Some("-");
    let text = match text {
        Some(text) if text != "-" => text,
        _ => {
            if !explicit_stdin && atty_stdin() {
                return Err(CliError::Usage(
                    "prompt text is required (pass TEXT or - for stdin)".into(),
                ));
            }
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("read stdin")
                .map_err(CliError::rpc)?;
            buf
        }
    };
    if text.trim().is_empty() {
        return Err(CliError::Usage("prompt text must not be empty".into()));
    }
    Ok(text)
}

fn atty_stdin() -> bool {
    std::io::IsTerminal::is_terminal(&io::stdin())
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

    #[test]
    fn lifecycle_capabilities_require_feature_and_operation_method() {
        let capabilities = json!({
            "features": { "session_lifecycle": true },
            "methods": [
                "querymt/session/getLifecycle",
                "querymt/session/setLifecycle"
            ]
        });
        assert!(supports_lifecycle(
            &capabilities,
            Some("querymt/session/getLifecycle")
        ));
        assert!(!supports_lifecycle(
            &capabilities,
            Some("querymt/session/unknown")
        ));
        assert!(!supports_lifecycle(
            &json!({ "features": { "session_lifecycle": false } }),
            None
        ));
    }

    #[test]
    fn pin_capabilities_require_feature_and_operation_method() {
        let capabilities = json!({
            "features": { "session_pins": true },
            "methods": ["querymt/session/setPinned"]
        });
        assert!(supports_pin(
            &capabilities,
            Some("querymt/session/setPinned")
        ));
        assert!(supports_pin(&capabilities, None));
        assert!(!supports_pin(
            &json!({ "features": { "session_pins": true } }),
            Some("querymt/session/setPinned")
        ));
        assert!(!supports_pin(
            &json!({ "features": { "session_pins": false } }),
            None
        ));
    }

    #[test]
    fn unsupported_lifecycle_capabilities_are_rejected_explicitly() {
        assert!(matches!(
            validate_lifecycle_support(
                &json!({ "features": { "session_lifecycle": false } }),
                None
            ),
            Err(CliError::Rpc(message))
                if message == "server does not advertise support for server-side lifecycle filtering"
        ));
        assert!(matches!(
            validate_lifecycle_support(
                &json!({
                    "features": { "session_lifecycle": true },
                    "methods": ["querymt/session/getLifecycle"]
                }),
                Some("querymt/session/setLifecycle")
            ),
            Err(CliError::Rpc(message))
                if message == "server does not advertise support for querymt/session/setLifecycle"
        ));
    }

    #[test]
    fn lifecycle_patch_preserves_explicit_false_and_rejects_empty_patch() {
        assert_eq!(
            lifecycle_patch_params("session-1", Some(false), None).unwrap(),
            json!({ "session_id": "session-1", "resolved": false })
        );
        assert!(matches!(
            lifecycle_patch_params("session-1", None, None),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn exec_line_parses_lifecycle_commands_and_explicit_false() {
        assert!(matches!(
            parse_exec_line("get-lifecycle session-1").unwrap(),
            Command::GetLifecycle { session_id } if session_id == "session-1"
        ));
        assert!(matches!(
            parse_exec_line("set-lifecycle session-1 --resolved false").unwrap(),
            Command::SetLifecycle { session_id, resolved: Some(false), archived: None }
                if session_id == "session-1"
        ));
        assert!(matches!(
            parse_exec_line("pin session-1").unwrap(),
            Command::Pin { session_id } if session_id == "session-1"
        ));
        assert!(matches!(
            parse_exec_line("unpin session-1").unwrap(),
            Command::Unpin { session_id } if session_id == "session-1"
        ));
        assert!(matches!(
            parse_exec_line("sessions --pinned").unwrap(),
            Command::Sessions {
                pinned: Some(true),
                ..
            }
        ));
        assert!(matches!(
            parse_exec_line("find --pinned=false").unwrap(),
            Command::Find {
                pinned: Some(false),
                ..
            }
        ));
        assert!(matches!(
            parse_exec_line("find --resolution open --archive excluded").unwrap(),
            Command::Find {
                resolution: Some(ResolutionFilter::Open),
                archive: Some(ArchiveFilter::Excluded),
                ..
            }
        ));
    }

    #[test]
    fn session_filters_omit_metadata_when_unspecified_and_preserve_pin_boolean() {
        assert!(session_filter_meta(None, None, None).is_none());
        let filters = session_filter_meta(Some(ResolutionFilter::Any), None, Some(false)).unwrap();
        assert_eq!(filters["querymt"]["sessionFilters"]["resolution"], "any");
        assert_eq!(filters["querymt"]["sessionFilters"]["pinned"], false);
        let pinned = session_filter_meta(None, None, Some(true)).unwrap();
        assert_eq!(pinned["querymt"]["sessionFilters"]["pinned"], true);
    }

    #[test]
    fn prompt_new_uses_remaining_args_as_text() {
        let (session_id, text) =
            parse_prompt_args(true, vec!["fix".into(), "the".into(), "build".into()]).unwrap();
        assert!(session_id.is_none());
        assert_eq!(text.as_deref(), Some("fix the build"));
    }

    #[test]
    fn prompt_existing_takes_session_then_text() {
        let (session_id, text) =
            parse_prompt_args(false, vec!["sess-1".into(), "continue".into()]).unwrap();
        assert_eq!(session_id.as_deref(), Some("sess-1"));
        assert_eq!(text.as_deref(), Some("continue"));
    }

    #[test]
    fn submission_output_failure_preserves_the_accepted_receipt() {
        let receipt = json!({
            "sessionId": "session-1", "clientInputId": "input-1",
            "deliveryState": "accepted", "result": {"input_id": "input-1"},
        });
        let error = write_submission_ok(receipt.clone(), |value| {
            assert_eq!(value, &output::ok(receipt.clone()));
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed output").into())
        })
        .err()
        .expect("output failure must remain an error");
        assert_eq!(error.exit_code(), ExitCode::Rpc);
        let recovery = error.recovery().unwrap();
        assert_eq!(recovery["sessionId"], "session-1");
        assert_eq!(recovery["clientInputId"], "input-1");
        assert_eq!(recovery["deliveryState"], "accepted");
        assert_eq!(recovery["stage"], "submit");
        assert_eq!(recovery["receipt"], receipt);
    }

    #[test]
    fn prompt_without_session_is_usage_error() {
        assert!(matches!(
            parse_prompt_args(false, Vec::new()),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn exec_line_parses_nested_command() {
        let command = parse_exec_line("sessions --cwd /tmp --limit 5").unwrap();
        match command {
            Command::Sessions { limit, .. } => assert_eq!(limit, Some(5)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn exec_line_parses_discard_queued_command() {
        let command = parse_exec_line("discard-queued session-1 input-1").unwrap();
        assert!(matches!(
            command,
            Command::DiscardQueued { session_id, input_id }
                if session_id == "session-1" && input_id == "input-1"
        ));
    }

    #[test]
    fn split_exec_args_keeps_quoted_text() {
        assert_eq!(
            split_exec_args(r#"prompt --new "fix the build""#),
            vec!["prompt", "--new", "fix the build"]
        );
    }

    #[test]
    fn auto_delivery_is_prompt_when_idle() {
        assert!(matches!(
            delivery_from_runtime("idle", false),
            Delivery::Prompt
        ));
    }

    #[test]
    fn auto_delivery_steers_when_steerable() {
        assert!(matches!(
            delivery_from_runtime("tools", true),
            Delivery::Steer
        ));
    }

    #[test]
    fn auto_delivery_queues_when_busy_and_not_steerable() {
        assert!(matches!(
            delivery_from_runtime("closing", false),
            Delivery::Queue
        ));
    }
}

//! `qmt-headless`: run one agent profile headlessly for one prompt, then
//! export what happened.
//!
//! Built for rollout harnesses (see `experiments/harness-adapter`). Two steps,
//! so the export works however the run ended (normal end, step limit, error,
//! a kill by the caller's timeout, an OOM):
//!
//! ```text
//! qmt-headless run <profile.toml> --cwd <dir> --instruction-file <file> --db <path>
//! qmt-headless export --db <path> [--session <id>] --atif <out.json> --summary <out.json>
//! ```
//!
//! `run` writes only the session database (and its WAL and run-lease files).
//! It has no time budget of its own: the caller bounds it. Exit status 0 when
//! the run ends normally or at a step limit; 1 for load, provider or runtime
//! errors.
//!
//! `export` only reads the database. It always writes the summary and writes
//! the ATIF trajectory whenever a session exists. It exits non-zero only when
//! the database cannot be read or no session can be chosen. Usage errors exit
//! with 2.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use querymt_agent::acp::protocol::{ContentBlock, PromptRequest, TextContent};
use querymt_agent::api::{Agent, AgentInfra};
use querymt_agent::config::load_config;
use querymt_agent::events::{AgentEventKind, StopType};
use querymt_agent::export::{AtifExportOptions, AtifSource, session_atif};
use querymt_agent::send_agent::SendAgent;
use querymt_agent::session::backend::StorageBackend;
use querymt_agent::session::projection::EventJournal;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long to wait for the run-completed event to be readable after the
/// prompt returns.
const EVENT_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(
    name = "qmt-headless",
    version,
    about = "Run one agent profile headlessly"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one session with one prompt; writes only the session database.
    Run(RunArgs),
    /// Export the ATIF trajectory and a run summary from a session database.
    Export(ExportArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Agent profile (TOML).
    profile: PathBuf,
    /// Workspace directory for the session.
    #[arg(long)]
    cwd: PathBuf,
    /// File holding the instruction sent as the only prompt.
    #[arg(long)]
    instruction_file: PathBuf,
    /// Session database to create or append to.
    #[arg(long)]
    db: PathBuf,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Session database written by `run`.
    #[arg(long)]
    db: PathBuf,
    /// Session to export; defaults to the single top-level session.
    #[arg(long)]
    session: Option<String>,
    /// Output path for the ATIF trajectory.
    #[arg(long)]
    atif: PathBuf,
    /// Output path for the run summary.
    #[arg(long)]
    summary: PathBuf,
}

/// What the last `RunCompleted` event said about the run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunEnd {
    outcome: String,
    stop_type: Option<StopType>,
    reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Classified {
    outcome: &'static str,
    stop_type: Option<StopType>,
    error: Option<String>,
}

/// Map the session's last `RunCompleted` onto the summary's `outcome`
/// (`end_turn`, `stopped`, `error` or `incomplete`).
fn classify(run_end: Option<&RunEnd>) -> Classified {
    let done = |outcome, stop_type, error| Classified {
        outcome,
        stop_type,
        error,
    };
    match run_end {
        None => done("incomplete", None, None),
        Some(end) => match end.outcome.as_str() {
            "completed" => done("end_turn", None, None),
            "stopped" => done("stopped", end.stop_type, None),
            "failed" => done(
                "error",
                None,
                Some(
                    end.reason
                        .clone()
                        .unwrap_or_else(|| "run failed".to_string()),
                ),
            ),
            // cancelled, interrupted, or anything unknown
            _ => done("incomplete", None, None),
        },
    }
}

/// The last `RunCompleted` in a session's durable event stream.
async fn last_run_end(journal: &dyn EventJournal, session_id: &str) -> Result<Option<RunEnd>> {
    let events = journal
        .load_session_stream(session_id, None, None)
        .await
        .map_err(|e| anyhow!("reading events: {e}"))?;
    Ok(events.iter().rev().find_map(|event| match &event.kind {
        AgentEventKind::RunCompleted {
            outcome,
            stop_type,
            reason,
            ..
        } => Some(RunEnd {
            outcome: outcome.clone(),
            stop_type: *stop_type,
            reason: reason.clone(),
        }),
        _ => None,
    }))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

fn summary_json(
    session_id: &str,
    classified: &Classified,
    error: Option<&str>,
    model_calls: u32,
) -> Value {
    json!({
        "session_id": session_id,
        "outcome": if error.is_some() { "error" } else { classified.outcome },
        "stop_type": classified.stop_type,
        "error": error.map(str::to_string).or_else(|| classified.error.clone()),
        "model_calls": model_calls,
    })
}

async fn run_command(args: RunArgs) -> Result<i32> {
    let instruction = std::fs::read_to_string(&args.instruction_file)
        .with_context(|| format!("reading {}", args.instruction_file.display()))?;
    let cwd = std::fs::canonicalize(&args.cwd)
        .with_context(|| format!("resolving --cwd {}", args.cwd.display()))?;
    if let Some(parent) = args.db.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let infra = AgentInfra::shared_with_db_path(Some(args.db.clone())).await?;
    let storage = infra
        .storage
        .clone()
        .ok_or_else(|| anyhow!("no session storage"))?;

    let config = load_config(args.profile.clone()).await?;
    let agent = Agent::builder_from_config_value(config, None)?
        .cwd(cwd)
        .infra(infra)
        .build()
        .await?;

    let session = agent.chat_session().await?;
    let session_id = session.id().to_string();
    let request = PromptRequest::new(
        session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(instruction))],
    );
    agent
        .handle()
        .prompt(request)
        .await
        .map_err(|e| anyhow!("prompt failed: {e}"))?;

    // The run's final event is journaled by the runtime; allow it a moment.
    let journal = storage.event_journal();
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    let end = loop {
        let end = last_run_end(journal.as_ref(), &session_id).await?;
        if end.is_some() || tokio::time::Instant::now() >= deadline {
            break end;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let classified = classify(end.as_ref());
    match classified.outcome {
        "end_turn" | "stopped" => Ok(0),
        other => {
            eprintln!(
                "qmt-headless: run ended with outcome `{other}`{}",
                classified
                    .error
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            );
            Ok(1)
        }
    }
}

async fn export_command(args: ExportArgs) -> Result<i32> {
    use querymt_agent::session::SqliteStorage;

    // Opening a missing path would create an empty database; refuse instead.
    if !args.db.is_file() {
        bail!("session database {} does not exist", args.db.display());
    }
    // `migrate = false`: export must not alter the database.
    let storage = SqliteStorage::connect_with_options(args.db.clone(), false)
        .await
        .map_err(|e| anyhow!("opening {}: {e}", args.db.display()))?;

    let store = storage.session_store();
    let session_id = match &args.session {
        Some(id) => {
            store
                .get_session(id)
                .await
                .map_err(|e| anyhow!("reading session {id}: {e}"))?
                .ok_or_else(|| anyhow!("session `{id}` not found"))?;
            id.clone()
        }
        None => {
            let sessions = store
                .list_sessions()
                .await
                .map_err(|e| anyhow!("listing sessions: {e}"))?;
            let top: Vec<_> = sessions
                .iter()
                .filter(|s| s.parent_session_id.is_none())
                .collect();
            match top.as_slice() {
                [only] => only.public_id.clone(),
                other => bail!(
                    "expected exactly one top-level session, found {}; pass --session",
                    other.len()
                ),
            }
        }
    };

    let end = last_run_end(storage.event_journal().as_ref(), &session_id).await?;
    let classified = classify(end.as_ref());

    let mut atif_error = None;
    let mut model_calls = 0;
    let exported = async {
        let view_store = storage
            .view_store()
            .ok_or_else(|| anyhow!("storage has no view store"))?;
        let options = AtifExportOptions {
            agent_name: Some("querymt".to_string()),
            agent_version: None,
            notes: None,
        };
        let atif = session_atif(view_store.as_ref(), &session_id, &options)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let calls = atif
            .steps
            .iter()
            .filter(|step| step.source == AtifSource::Agent)
            .count() as u32;
        write_json(&args.atif, &atif.to_json_value()?)?;
        Ok::<u32, anyhow::Error>(calls)
    }
    .await;
    match exported {
        Ok(calls) => model_calls = calls,
        Err(error) => atif_error = Some(format!("exporting ATIF: {error:#}")),
    }

    write_json(
        &args.summary,
        &summary_json(&session_id, &classified, atif_error.as_deref(), model_calls),
    )?;
    if let Some(error) = atif_error {
        eprintln!("qmt-headless: {error}");
        return Ok(1);
    }
    Ok(0)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run(args) => run_command(args).await,
        Command::Export(args) => export_command(args).await,
    };
    let code = match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("qmt-headless: {error:#}");
            1
        }
    };
    // Do not wait for lingering background tasks (for example a shell child).
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> Result<Cli, clap::Error> {
        let mut args = vec!["qmt-headless"];
        args.extend(list);
        Cli::try_parse_from(args)
    }

    #[test]
    fn run_requires_its_flags() {
        let cli = parse(&[
            "run",
            "p.toml",
            "--cwd",
            "/w",
            "--instruction-file",
            "i.txt",
            "--db",
            "s.db",
        ])
        .unwrap();
        let Command::Run(run) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(run.profile, PathBuf::from("p.toml"));
        assert_eq!(run.cwd, PathBuf::from("/w"));
        assert_eq!(run.instruction_file, PathBuf::from("i.txt"));
        assert_eq!(run.db, PathBuf::from("s.db"));

        for missing in [
            &["run", "--cwd", "/w", "--instruction-file", "i", "--db", "d"][..],
            &["run", "p.toml", "--instruction-file", "i", "--db", "d"][..],
            &["run", "p.toml", "--cwd", "/w", "--db", "d"][..],
            &["run", "p.toml", "--cwd", "/w", "--instruction-file", "i"][..],
        ] {
            assert!(parse(missing).is_err(), "{missing:?}");
        }
    }

    #[test]
    fn export_requires_its_flags_and_session_is_optional() {
        let cli = parse(&["export", "--db", "s.db", "--atif", "a", "--summary", "s"]).unwrap();
        let Command::Export(export) = cli.command else {
            panic!("expected export");
        };
        assert_eq!(export.session, None);

        let cli = parse(&[
            "export",
            "--db",
            "s.db",
            "--session",
            "abc",
            "--atif",
            "a",
            "--summary",
            "s",
        ])
        .unwrap();
        let Command::Export(export) = cli.command else {
            panic!("expected export");
        };
        assert_eq!(export.session.as_deref(), Some("abc"));

        for missing in [
            &["export", "--atif", "a", "--summary", "s"][..],
            &["export", "--db", "d", "--summary", "s"][..],
            &["export", "--db", "d", "--atif", "a"][..],
        ] {
            assert!(parse(missing).is_err(), "{missing:?}");
        }
    }

    #[test]
    fn unknown_flags_and_removed_options_are_rejected() {
        let base = ["export", "--db", "d", "--atif", "a", "--summary", "s"];
        for extra in ["--bogus", "--timeout-sec"] {
            let mut list = base.to_vec();
            list.push(extra);
            assert!(parse(&list).is_err(), "{extra}");
        }
        let err = parse(&[
            "run",
            "p",
            "--cwd",
            "w",
            "--instruction-file",
            "i",
            "--db",
            "d",
            "--timeout-sec",
            "5",
        ])
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(parse(&[]).is_err());
    }

    fn end(outcome: &str, stop_type: Option<StopType>, reason: Option<&str>) -> RunEnd {
        RunEnd {
            outcome: outcome.to_string(),
            stop_type,
            reason: reason.map(str::to_string),
        }
    }

    #[test]
    fn maps_run_completed_to_the_summary_outcome() {
        let c = classify(Some(&end("completed", None, None)));
        assert_eq!((c.outcome, c.stop_type, c.error), ("end_turn", None, None));

        let c = classify(Some(&end(
            "stopped",
            Some(StopType::StepLimit),
            Some("max"),
        )));
        assert_eq!(c.outcome, "stopped");
        assert_eq!(c.stop_type, Some(StopType::StepLimit));
        assert_eq!(c.error, None);

        let c = classify(Some(&end("failed", None, Some("provider down"))));
        assert_eq!(c.outcome, "error");
        assert_eq!(c.error.as_deref(), Some("provider down"));
        assert_eq!(
            classify(Some(&end("failed", None, None))).error.as_deref(),
            Some("run failed")
        );

        for outcome in ["cancelled", "interrupted"] {
            assert_eq!(
                classify(Some(&end(outcome, None, None))).outcome,
                "incomplete"
            );
        }
        assert_eq!(classify(None).outcome, "incomplete");
    }

    #[test]
    fn summary_json_has_the_documented_fields() {
        let classified = classify(Some(&end("stopped", Some(StopType::StepLimit), None)));
        let value = summary_json("s1", &classified, None, 3);
        assert_eq!(value["session_id"], "s1");
        assert_eq!(value["outcome"], "stopped");
        assert_eq!(value["stop_type"], "step_limit");
        assert_eq!(value["error"], Value::Null);
        assert_eq!(value["model_calls"], 3);

        let value = summary_json("s1", &classified, Some("late"), 0);
        assert_eq!(value["outcome"], "error");
        assert_eq!(value["error"], "late");
    }
}

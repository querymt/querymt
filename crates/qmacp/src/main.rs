mod commands;

use qmacp::{client, error, output, policy, url};

use std::process::ExitCode as ProcessExitCode;

use anyhow::Result;
use clap::Parser;

use crate::client::AcpClient;
use crate::commands::Command;
use crate::error::{CliError, ExitCode};
use crate::policy::PermissionPolicy;
use crate::url::{DEFAULT_HOST, normalize_acp_ws_url, safe_endpoint_label};

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("QMACP_GIT_SHA"), ")");

#[derive(Parser)]
#[command(name = "qmacp")]
#[command(about = "JSON CLI for a running QueryMT ACP WebSocket server")]
#[command(version = VERSION)]
struct Cli {
    /// ACP WebSocket URL or host[:port][/path]. Defaults to ws://127.0.0.1:3030/acp/ws
    #[arg(short, long, value_name = "url")]
    url: Option<String>,

    /// Allow plaintext ws:// connections to non-loopback hosts.
    #[arg(long)]
    allow_insecure: bool,

    /// Pretty-print JSON instead of compact JSON / NDJSON.
    #[arg(long)]
    pretty: bool,

    /// How to answer session/request_permission while a prompt is running.
    #[arg(long, value_enum, default_value_t = PermissionPolicy::AllowOnce)]
    permission: PermissionPolicy,

    /// Do not print connection logs to stderr.
    #[arg(long)]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[tokio::main]
async fn main() -> ProcessExitCode {
    let cli = Cli::parse();
    let pretty = cli.pretty;
    match run(cli).await {
        Ok(code) => ProcessExitCode::from(code.as_i32() as u8),
        Err(error) => {
            let _ = output::write_error(&error, pretty);
            ProcessExitCode::from(error.exit_code().as_i32() as u8)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, CliError> {
    let url = normalize_acp_ws_url(
        cli.url.as_deref().unwrap_or(DEFAULT_HOST),
        cli.allow_insecure,
    )
    .map_err(|err| CliError::Usage(err.to_string()))?;
    let endpoint = safe_endpoint_label(&url).map_err(|err| CliError::Usage(err.to_string()))?;
    let stream_events = matches!(
        cli.command,
        Command::Prompt { .. } | Command::Exec { .. } | Command::Follow { .. }
    );
    if !cli.quiet {
        eprintln!("connecting {endpoint}");
    }
    let client = AcpClient::connect(&url, &endpoint, cli.permission, stream_events)
        .await
        .map_err(|err| CliError::connection_with_cause(&endpoint, err))?;
    Ok(commands::run(&client, cli.command, cli.pretty).await?.exit)
}

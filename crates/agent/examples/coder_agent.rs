#![cfg(feature = "dashboard")]

//! Coder Agent Example
//!
//! Multi-mode agent that can run as ACP stdio server or web dashboard.
//! Requires the `dashboard` feature to be enabled.
//!
//! ## Usage
//!
//! ```bash
//! # ACP stdio mode
//! cargo run --example coder_agent --features dashboard -- --stdio
//!
//! # Web dashboard mode
//! cargo run --example coder_agent --features dashboard -- --dashboard
//! cargo run --example coder_agent --features dashboard -- --dashboard=0.0.0.0:8080
//!
//! # Running a built binary with embedded default config
//! ./coder_agent --dashboard
//! ```

use clap::{ArgGroup, Parser};
use querymt_agent::prelude::*;
use std::path::PathBuf;

const DEFAULT_DASHBOARD_ADDR: &str = "127.0.0.1:3000";
const EMBEDDED_CONFIG: &str = include_str!("confs/single_coder.toml");
const EMBEDDED_PROMPT: &str = include_str!("prompts/default_system.txt");
const EMBEDDED_PROMPT_REF: &str = r#"{ file = "../prompts/default_system.txt" }"#;

#[derive(Debug, Parser)]
#[command(name = "coder_agent")]
#[command(about = "Run QueryMT coder agent in ACP stdio mode or dashboard mode")]
#[command(
    after_help = "Examples:\n  coder_agent --stdio\n  coder_agent --dashboard\n  coder_agent --dashboard=0.0.0.0:8080\n  coder_agent path/to/config.toml --stdio"
)]
#[command(group(ArgGroup::new("mode").required(true).args(["stdio", "dashboard"])))]
struct Cli {
    /// Path to TOML config.
    ///
    /// If omitted, uses an embedded copy of `examples/confs/single_coder.toml`.
    config_file: Option<PathBuf>,

    /// Run as ACP stdio server (for subprocess spawning)
    #[arg(long)]
    stdio: bool,

    /// Run web dashboard; optionally set bind address
    #[arg(long, value_name = "addr", num_args = 0..=1, default_missing_value = DEFAULT_DASHBOARD_ADDR)]
    dashboard: Option<String>,
}

/// Setup logging for stdio mode - writes to stderr only to avoid corrupting stdout JSON-RPC.
fn setup_stdio_logging() {
    use tracing_log::LogTracer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, Registry, fmt};

    // Initialize log->tracing bridge so log:: macros from providers work.
    LogTracer::init().expect("Failed to set LogTracer");

    // Create fmt layer that writes to STDERR only (stdout is reserved for JSON-RPC).
    let fmt_layer = fmt::layer().with_writer(std::io::stderr).with_target(true);

    let filter = EnvFilter::from_default_env();

    let subscriber = Registry::default().with(filter).with(fmt_layer);

    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");
}

fn embedded_single_coder_config() -> String {
    let inline_prompt = format!("'''{}'''", EMBEDDED_PROMPT);
    EMBEDDED_CONFIG.replace(EMBEDDED_PROMPT_REF, &inline_prompt)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let is_stdio = cli.stdio;

    // Setup logging based on mode:
    // - Stdio mode: logs to stderr (stdout reserved for JSON-RPC)
    // - Dashboard mode: full telemetry with stdout
    if is_stdio {
        setup_stdio_logging();
    } else {
        querymt_utils::telemetry::setup_telemetry("querymt-coder-agent", env!("CARGO_PKG_VERSION"));
    }

    let runner = if let Some(config_path) = &cli.config_file {
        eprintln!("Loading agent from: {}", config_path.display());
        from_config(config_path).await?
    } else {
        eprintln!("Loading agent from embedded default config: single_coder.toml");
        let embedded_config = embedded_single_coder_config();
        from_config(ConfigSource::Toml(embedded_config)).await?
    };

    eprintln!("Agent loaded successfully!\n");

    if is_stdio {
        eprintln!("Starting ACP stdio server...");
        runner.acp("stdio").await?;
    } else {
        let addr = cli.dashboard.as_deref().unwrap_or(DEFAULT_DASHBOARD_ADDR);
        eprintln!("Starting dashboard at http://{}", addr);
        runner.dashboard().run(addr).await?;
    }

    Ok(())
}

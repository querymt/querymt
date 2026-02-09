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
//! ```

use querymt_agent::prelude::*;

/// Setup logging for stdio mode - writes to stderr only to avoid corrupting stdout JSON-RPC
fn setup_stdio_logging() {
    use tracing_log::LogTracer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, Registry, fmt};

    // Initialize log->tracing bridge so log:: macros from providers work
    LogTracer::init().expect("Failed to set LogTracer");

    // Create fmt layer that writes to STDERR only (stdout is reserved for JSON-RPC)
    let fmt_layer = fmt::layer().with_writer(std::io::stderr).with_target(true);

    let filter = EnvFilter::from_default_env();

    let subscriber = Registry::default().with(filter).with(fmt_layer);

    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse CLI args first to determine mode
    let args: Vec<String> = std::env::args().skip(1).collect();
    let is_stdio = args.contains(&"--stdio".to_string());

    // Setup logging based on mode:
    // - Stdio mode: logs to stderr (stdout reserved for JSON-RPC)
    // - Dashboard mode: full telemetry with stdout
    if is_stdio {
        setup_stdio_logging();
    } else {
        querymt_utils::telemetry::setup_telemetry("querymt-coder-agent", env!("CARGO_PKG_VERSION"));
    }

    // Default config path
    let config_path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(|s| s.as_str())
        .unwrap_or("examples/confs/coder_agent.toml");

    eprintln!("Loading agent from: {}", config_path);

    // Load agent from config
    let runner = from_config(config_path).await?;

    eprintln!("Agent loaded successfully!\n");

    // Determine mode from CLI args
    if args.contains(&"--stdio".to_string()) {
        eprintln!("Starting ACP stdio server...");
        runner.acp("stdio").await?;
    } else if let Some(dashboard_arg) = args.iter().find(|a| a.starts_with("--dashboard")) {
        let addr = if dashboard_arg.contains('=') {
            dashboard_arg.split('=').nth(1).unwrap()
        } else {
            args.iter()
                .position(|a| a == "--dashboard")
                .and_then(|pos| args.get(pos + 1))
                .map(|s| s.as_str())
                .unwrap_or("127.0.0.1:3000")
        };
        eprintln!("Starting dashboard at http://{}", addr);
        runner.dashboard().run(addr).await?;
    } else {
        // Print usage and exit
        eprintln!(
            "Usage: cargo run --example coder_agent [config_file] <--stdio|--dashboard[=addr]>"
        );
        eprintln!();
        eprintln!("Arguments:");
        eprintln!(
            "  config_file             Path to TOML config (default: examples/confs/coder_agent.toml)"
        );
        eprintln!("  --stdio                 Run as ACP stdio server (for subprocess spawning)");
        eprintln!("  --dashboard[=addr]      Run web dashboard (default: 127.0.0.1:3000)");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  cargo run --example coder_agent --stdio");
        eprintln!("  cargo run --example coder_agent --dashboard");
        eprintln!("  cargo run --example coder_agent --dashboard=0.0.0.0:8080");
        eprintln!("  cargo run --example coder_agent my_config.toml --stdio");
        std::process::exit(1);
    }

    Ok(())
}

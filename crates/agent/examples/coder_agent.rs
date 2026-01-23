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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Parse CLI args
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Only enable env_logger when running the dashboard.
    // In stdio mode, stdout must be reserved for ACP JSON-RPC traffic.
    // TODO: Clap?
    let is_dashboard = args.iter().any(|a| a.starts_with("--dashboard"));
    if is_dashboard {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .target(env_logger::Target::Stdout)
            .try_init();
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

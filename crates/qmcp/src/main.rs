use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use qmcp::{BridgeConfig, QmcpServer, policy::PermissionPolicy, transport};
use tracing_subscriber::EnvFilter;

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("QMCP_GIT_SHA"), ")");

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum Transport {
    #[default]
    Stdio,
    Http,
}

#[derive(Parser)]
#[command(name = "qmcp", version = VERSION, about = "MCP server for a running QueryMT ACP WebSocket server")]
struct Cli {
    /// MCP transport: stdio or Streamable HTTP.
    #[arg(long, value_enum, default_value = "stdio")]
    transport: Transport,

    /// ACP WebSocket endpoint (or host[:port][/path]).
    #[arg(
        short,
        long,
        env = "QMCP_ACP_URL",
        default_value = "ws://127.0.0.1:3030/acp/ws"
    )]
    url: String,

    /// Permit plaintext ACP WebSockets to a non-loopback host.
    #[arg(long)]
    allow_insecure: bool,

    /// ACP tool permission policy, delegated to qmacp. Use cancel to fail closed.
    #[arg(long, value_enum, default_value = "allow-once")]
    permission: PermissionPolicy,

    /// Streamable HTTP listen address. Non-loopback binds require authentication.
    #[arg(long, default_value = "127.0.0.1:3031")]
    bind: SocketAddr,

    /// Shared HTTP bearer token. Prefer the environment variable to keep it out of argv.
    #[arg(
        long,
        env = "QMCP_BEARER_TOKEN",
        hide_env_values = true,
        conflicts_with = "oauth_config"
    )]
    bearer_token: Option<String>,

    /// OAuth resource/provider JSON configuration (HTTP only; see README).
    #[arg(long, requires = "oauth_client_secret")]
    oauth_config: Option<PathBuf>,

    /// Introspection client secret. Prefer the environment variable.
    #[arg(
        long,
        env = "QMCP_OAUTH_CLIENT_SECRET",
        hide_env_values = true,
        requires = "oauth_config"
    )]
    oauth_client_secret: Option<String>,

    /// Additional allowed HTTP hostname or host:port (repeatable; requires auth).
    #[arg(long = "allowed-host", action = clap::ArgAction::Append)]
    allowed_hosts: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let server = QmcpServer::new(BridgeConfig::new(
        &cli.url,
        cli.allow_insecure,
        cli.permission,
    )?)?;
    match cli.transport {
        Transport::Stdio => {
            if cli.bearer_token.is_some()
                || cli.oauth_config.is_some()
                || !cli.allowed_hosts.is_empty()
            {
                bail!("HTTP authentication options require --transport http");
            }
            transport::serve_stdio(server).await
        }
        Transport::Http => {
            let token = cli
                .bearer_token
                .map(|value| transport::BearerToken::new(&value))
                .transpose()?;
            let router = if let Some(path) = cli.oauth_config {
                let bytes = tokio::fs::read(path)
                    .await
                    .context("read OAuth configuration")?;
                let config = serde_json::from_slice(&bytes).context("parse OAuth configuration")?;
                let secret = cli
                    .oauth_client_secret
                    .context("OAuth client secret is required")?;
                let auth = qmcp::oauth::OAuthAuth::new(config, secret)?;
                transport::http_router_with_oauth(server, cli.bind, auth, &cli.allowed_hosts)?
            } else {
                transport::http_router_with_auth(server, cli.bind, token, &cli.allowed_hosts)?
            };
            let listener = tokio::net::TcpListener::bind(cli.bind)
                .await
                .context("bind Streamable HTTP listener")?;
            tracing::info!(address = %listener.local_addr()?, "serving MCP at /mcp");
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown_signal())
                .await
                .context("serve Streamable HTTP")
        }
    }
}

async fn shutdown_signal() {
    // Keep the same Unix listener across both signals so an immediate second
    // interrupt is queued even before the forced-exit task is polled.
    #[cfg(unix)]
    let mut interrupts =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
            Ok(interrupts) => interrupts,
            Err(error) => {
                tracing::error!(%error, "cannot listen for shutdown interrupts");
                return;
            }
        };
    #[cfg(unix)]
    if interrupts.recv().await.is_none() {
        return;
    }
    #[cfg(not(unix))]
    if tokio::signal::ctrl_c().await.is_err() {
        return;
    }

    tokio::spawn(async move {
        #[cfg(unix)]
        let interrupted = interrupts.recv().await.is_some();
        #[cfg(not(unix))]
        let interrupted = tokio::signal::ctrl_c().await.is_ok();
        if interrupted {
            std::process::exit(130);
        }
    });
    tracing::warn!("shutting down; interrupt again to force exit");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_configuration_requires_a_secret_and_cannot_fall_back_to_a_static_token() {
        for args in [
            vec!["qmcp", "--oauth-config", "oauth.json"],
            vec!["qmcp", "--oauth-client-secret", "test-secret"],
            vec![
                "qmcp",
                "--oauth-config",
                "oauth.json",
                "--oauth-client-secret",
                "test-secret",
                "--bearer-token",
                "static-token",
            ],
        ] {
            let error = Cli::try_parse_from(args)
                .err()
                .expect("partial/conflicting OAuth must fail");
            assert!(!error.to_string().contains("test-secret"));
        }
        assert!(
            Cli::try_parse_from([
                "qmcp",
                "--transport",
                "http",
                "--oauth-config",
                "oauth.json",
                "--oauth-client-secret",
                "test-secret"
            ])
            .is_ok()
        );
    }
}

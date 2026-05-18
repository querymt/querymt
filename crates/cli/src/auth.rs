//! Console OAuth UI implementation
//!
//! This module provides a console-based implementation of the `OAuthUI` trait
//! from `querymt_utils::oauth`. It handles presenting OAuth flows to users via
//! the terminal with colored output, browser auto-opening, and optional callback
//! server support.

use anyhow::Result;
use async_trait::async_trait;
use colored::*;
use querymt_utils::oauth::{
    OAuthFlowData, OAuthFlowKind, OAuthProvider, OAuthUI, openai_callback_server,
};
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Duration;

/// Console-based OAuth UI implementation
///
/// This implementation:
/// - Uses colored terminal output for status messages
/// - Automatically opens the browser for authorization URLs
/// - Supports redirect callback servers for automatic code capture
/// - Falls back to manual code entry if callback server fails
pub struct ConsoleOAuthUI;

#[async_trait]
impl OAuthUI for ConsoleOAuthUI {
    async fn authorize(&self, _provider_name: &str, url: &str, _state: &str) -> Result<String> {
        println!(
            "\n{} Please visit this URL to authorize:",
            "🔐".bright_green()
        );
        println!("{}\n", url.bright_yellow());

        // Try to open browser automatically
        match open::that(url) {
            Ok(_) => println!("{} Browser opened automatically\n", "✓".bright_green()),
            Err(_) => println!(
                "{} Could not open browser automatically\n",
                "!".bright_yellow()
            ),
        }

        // Manual code entry
        manual_code_entry()
    }

    async fn authorize_and_exchange(
        &self,
        provider: &dyn OAuthProvider,
        flow: &OAuthFlowData,
    ) -> Result<Option<(querymt_utils::oauth::TokenSet, Option<String>)>> {
        // Device-poll flow: show URL, open browser, poll for token.
        if provider.flow_kind() == OAuthFlowKind::DevicePoll {
            println!(
                "\n{} Please visit this URL to authorize:",
                "🔐".bright_green()
            );
            println!("{}\n", flow.authorization_url.bright_yellow());

            match open::that(&flow.authorization_url) {
                Ok(_) => println!("{} Browser opened automatically\n", "✓".bright_green()),
                Err(_) => println!(
                    "{} Could not open browser automatically\n",
                    "!".bright_yellow()
                ),
            }

            println!("{} Waiting for device authorization...", "⏳".bright_cyan());
            println!(
                "   (Approve the request in your browser, then authentication will complete automatically)\n"
            );

            let tokens = provider
                .exchange_code("", &flow.state, &flow.verifier)
                .await?;

            println!("{} Device authorization complete!", "✓".bright_green());

            let api_key = provider
                .create_api_key(&tokens.access_token)
                .await
                .ok()
                .flatten();

            return Ok(Some((tokens, api_key)));
        }

        let Some(port) = provider.callback_port() else {
            return Ok(None);
        };

        println!(
            "\n{} Please visit this URL to authorize:",
            "🔐".bright_green()
        );
        println!("{}\n", flow.authorization_url.bright_yellow());

        // Try to open browser automatically
        match open::that(&flow.authorization_url) {
            Ok(_) => println!("{} Browser opened automatically\n", "✓".bright_green()),
            Err(_) => println!(
                "{} Could not open browser automatically\n",
                "!".bright_yellow()
            ),
        }

        println!(
            "{} Starting callback server on port {}...",
            "🌐".bright_blue(),
            port
        );
        println!("{} Waiting for OAuth callback...", "⏳".bright_cyan());
        println!("   (The browser should redirect automatically after you authorize)\n");

        match oauth_callback_server(provider, port, flow, Duration::from_secs(120)).await {
            Ok((tokens, api_key)) => {
                println!(
                    "{} Authorization and token exchange complete!",
                    "✓".bright_green()
                );
                Ok(Some((tokens, api_key)))
            }
            Err(e) => {
                println!("{} Callback server failed: {}", "⚠️".bright_yellow(), e);
                println!("Falling back to manual code entry...\n");
                Ok(None) // Fall back to manual entry
            }
        }
    }

    fn status(&self, message: &str) {
        println!("{}", message);
    }

    fn success(&self, message: &str) {
        println!("{} {}", "✓".bright_green(), message);
    }

    fn error(&self, message: &str) {
        println!("{} {}", "⚠️".bright_yellow(), message);
    }
}

async fn oauth_callback_server(
    provider: &dyn OAuthProvider,
    port: u16,
    flow: &OAuthFlowData,
    timeout: Duration,
) -> Result<(querymt_utils::oauth::TokenSet, Option<String>)> {
    if provider.name() == "codex" {
        let (tokens, api_key) =
            openai_callback_server(port, &flow.state, &flow.verifier, timeout).await?;
        let api_key = provider.api_key_name().and(api_key);
        return Ok((tokens, api_key));
    }

    let code = capture_callback_code(port, &flow.state, timeout).await?;
    let tokens = provider
        .exchange_code(&code, &flow.state, &flow.verifier)
        .await?;
    let api_key = match provider.api_key_name() {
        Some(_) => provider
            .create_api_key(&tokens.access_token)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    Ok((tokens, api_key))
}

async fn capture_callback_code(
    port: u16,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let expected_state = expected_state.to_string();

    let callback = async move {
        loop {
            let (stream, _) = listener.accept().await?;
            let mut buf = vec![0_u8; 8192];
            let n = stream
                .readable()
                .await
                .and_then(|_| stream.try_read(&mut buf))?;
            let request = String::from_utf8_lossy(&buf[..n]);
            let Some(request_line) = request.lines().next() else {
                continue;
            };
            let Some(target) = request_line.split_whitespace().nth(1) else {
                continue;
            };

            let params = parse_callback_params(target);
            let success = params.contains_key("code")
                && params
                    .get("state")
                    .is_some_and(|state| state == &expected_state);
            let body = if success {
                "<html><head><title>Authorization Successful</title></head><body><h1>Authorization Successful</h1><p>You can close this window.</p></body></html>"
            } else {
                "<html><head><title>Authorization Failed</title></head><body><h1>Authorization Failed</h1><p>You can close this window.</p></body></html>"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .writable()
                .await
                .and_then(|_| stream.try_write(response.as_bytes()))?;

            if let Some(error) = params.get("error") {
                anyhow::bail!("OAuth error: {}", error);
            }
            let Some(code) = params.get("code") else {
                anyhow::bail!("No authorization code received");
            };
            let Some(state) = params.get("state") else {
                anyhow::bail!("No OAuth state received");
            };
            if state != &expected_state {
                anyhow::bail!("State mismatch - possible CSRF attack");
            }
            return Ok(code.clone());
        }
    };

    tokio::time::timeout(timeout, callback)
        .await
        .map_err(|_| anyhow::anyhow!("Timeout waiting for OAuth callback"))?
}

fn parse_callback_params(target: &str) -> HashMap<String, String> {
    let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

/// Prompt user to manually enter the authorization code
fn manual_code_entry() -> Result<String> {
    print!("Paste the authorization response (code#state format): ");
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    let response = response.trim();

    // Try to extract code from query string if present
    // This handles cases where user pastes the full callback URL
    if response.contains('?') || response.contains('&') {
        if let Some(code) = querymt_utils::oauth::extract_code_from_query(response) {
            return Ok(code);
        }
    }

    Ok(response.to_string())
}
